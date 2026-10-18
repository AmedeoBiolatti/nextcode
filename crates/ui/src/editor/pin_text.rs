//! Pinpoint for the editor's text, down to the letter: the `text` node and
//! its visible text, the rects of code ranges agents highlight (file
//! editors and notebook cells), the range under the picker, revealing a
//! range without moving the cursor, and Copy Reference.

use gpui::{ClipboardItem, MouseDownEvent, Size, anchored, deferred};

use super::*;
use crate::{
    pinpoint::{self, CodeWant, PickText, code_reference},
    pinpoint_text::{self, CodeSpan},
};

actions!(editor, [CopyReference]);

/// Larger visible text (huge folds) is not copied into the node.
const MAX_NODE_TEXT: usize = 64 * 1024;

/// The file (or notebook cell) an editor shows, as references name it.
struct CodeOwner {
    /// Project-relative (the pin key), or the full path outside the project.
    file: String,
    /// The full path, `/`-separated.
    absolute: Option<String>,
    cell: Option<usize>,
}

impl CodeOwner {
    fn matches(&self, want: &CodeWant) -> bool {
        if want.cell != self.cell {
            return false;
        }
        let wanted = normalize(&want.file);
        let file = normalize(&self.file);
        wanted == file
            || self.absolute.as_deref().map(normalize).is_some_and(|absolute| {
                absolute == wanted || absolute.ends_with(&format!("/{wanted}"))
            })
            // An absolute path (another machine's view of the project)
            // ending in this file's project path.
            || wanted.ends_with(&format!("/{file}"))
    }
}

fn normalize(path: &str) -> String {
    let path = path.replace('\\', "/");
    let path = path.trim_start_matches("./");
    if cfg!(windows) {
        path.to_lowercase()
    } else {
        path.to_owned()
    }
}

impl Editor {
    /// After layout in prepaint: records `<prefix>/text` with its visible
    /// text, reports the rects of highlighted ranges, and the range under
    /// the picker.
    pub(super) fn record_text_pins(&mut self, prefix: &SharedString, window: &Window) {
        self.pin_prefix = prefix.clone();
        if prefix.is_empty() {
            return;
        }
        let Some(layout) = &self.layout else {
            return;
        };
        let path: SharedString = format!("{prefix}/text").into();
        let mask = window.content_mask().bounds;
        pinpoint::record(window, path.clone(), None, layout.text_bounds);
        let shown = match (layout.rows.first(), layout.rows.last()) {
            (Some(first), Some(last)) => first.row.range.start..last.row.range.end,
            _ => 0..0,
        };
        if shown.len() <= MAX_NODE_TEXT {
            pinpoint::set_text(window, &path, self.buffer.slice(shown.clone()).into());
            for (id, range) in pinpoint::wanted_text(window, &path) {
                let range = (shown.start + range.start).min(shown.end)
                    ..(shown.start + range.end).min(shown.end);
                let (rects, clipped) = self.range_rects(range, mask);
                pinpoint::report(window, id, rects, clipped);
            }
        }
        let wants = pinpoint::wanted_code(window);
        let owner = (!wants.is_empty() || pinpoint::picking(window).is_some())
            .then(|| self.code_owner())
            .flatten();
        if let Some(owner) = &owner {
            for (id, want) in wants.iter().filter(|(_, want)| owner.matches(want)) {
                if let Some(range) = CodeSpan::from(want).offsets(&self.buffer) {
                    let (rects, clipped) = self.range_rects(range, mask);
                    pinpoint::report(window, *id, rects, clipped);
                }
            }
        }
        if let (Some(owner), Some(pointer)) = (owner, pinpoint::picking(window))
            && layout.text_bounds.contains(&pointer)
            && mask.contains(&pointer)
            && let Some(range) = self.pick_range(pointer)
        {
            let span = CodeSpan::of_range(&self.buffer, range.clone());
            let (rects, _) = self.range_rects(range, mask);
            pinpoint::report_pick(
                window,
                PickText {
                    reference: code_reference(&span.want(owner.file, owner.cell)),
                    path,
                    rects,
                },
            );
        }
    }

    /// The file this editor shows: its buffer's path, or for a notebook
    /// cell the enclosing `notebook:<key>/cell:<n>` pins.
    fn code_owner(&self) -> Option<CodeOwner> {
        if self.notebook_cell {
            let (head, tail) = self.pin_prefix.rsplit_once("/cell:")?;
            let cell = tail.split('/').next()?.parse().ok()?;
            let (_, file) = head.rsplit_once("notebook:")?;
            return Some(CodeOwner {
                file: file.to_owned(),
                absolute: None,
                cell: Some(cell),
            });
        }
        if self.single_line || self.text_area {
            return None;
        }
        let path = self.buffer.path()?;
        let file = match &self.project {
            Some(project) => pinpoint::file_key(project, path),
            None => path.to_string_lossy().replace('\\', "/"),
        };
        Some(CodeOwner {
            file,
            absolute: Some(path.to_string_lossy().into_owned()),
            cell: None,
        })
    }

    /// Window rects of `range` as drawn now, one per screen row it covers
    /// (clipped to the text area and `mask`), and whether part of it is
    /// not shown: scrolled away, cut off, or folded (the fold's `⋯` stands
    /// for the hidden part).
    pub(super) fn range_rects(
        &self,
        range: Range<usize>,
        mask: Bounds<Pixels>,
    ) -> (Vec<Bounds<Pixels>>, bool) {
        let Some(layout) = &self.layout else {
            return (Vec::new(), true);
        };
        let clip = layout.text_bounds.intersect(&mask);
        let mark = layout.em_width / 2.;
        let height = layout.line_height;
        let folds: Vec<usize> = self
            .display
            .folds()
            .iter()
            .filter(|fold| fold.start < range.end && range.start < fold.end)
            .map(|fold| fold.start)
            .collect();
        let mut rects = Vec::new();
        let (mut start_shown, mut end_shown) = (false, false);
        for (i, row) in layout.rows.iter().enumerate() {
            let (start, end) = (row.row.range.start, row.row.range.end);
            let top = layout.row_top(i, self.scroll.y);
            if top >= clip.top() && top + height <= clip.bottom() {
                start_shown |= row.row.shows(range.start, false);
                end_shown |= row.row.shows(range.end, true);
            }
            let left = layout.text_left(row, self.scroll.x);
            let (lo, hi) = (range.start.max(start), range.end.min(end));
            let span = if lo < hi {
                Some((row.x_for(lo), row.x_for(hi)))
            } else if (range.is_empty() && row.row.shows(range.start, false))
                || (start == end && row.row.first && range.start <= start && start < range.end)
            {
                // A position, or an empty line inside the range: a mark.
                let x = row.x_for(lo.clamp(start, end));
                Some((x, x + mark))
            } else {
                None
            };
            if let Some((x0, x1)) = span {
                rects.push(Bounds::from_corners(
                    point(left + x0, top),
                    point(left + x1, top + height),
                ));
            }
            if row.folded.is_some_and(|fold| folds.contains(&fold)) {
                rects.push(layout.placeholder_bounds(i, self.scroll));
            }
        }
        let (rects, clipped) = pinpoint_text::clip_rects(rects, clip);
        (
            rects,
            clipped || !start_shown || !end_shown || !folds.is_empty(),
        )
    }

    /// What the picker takes at `pointer`: a selection under it, else the
    /// word (or the single character) under it.
    fn pick_range(&self, pointer: Point<Pixels>) -> Option<Range<usize>> {
        let layout = self.layout.as_ref()?;
        let (_, row) = layout.row_layout_at(pointer.y, self.scroll.y)?;
        let offset = row.offset_under(pointer.x - layout.text_left(row, self.scroll.x))?;
        if let Some(selection) = self
            .selections
            .all()
            .iter()
            .find(|s| !s.is_empty() && s.range().contains(&offset))
        {
            return Some(selection.range());
        }
        let line = self.buffer.line_of(offset);
        let start = self.buffer.line_start(line);
        let word = pinpoint_text::word_at(&self.buffer.line_text(line), offset - start);
        Some(start + word.start..start + word.end)
    }

    /// Shows the code of `span` in the middle of the view on the next
    /// layout (opening folds over it); the cursor stays.
    pub(crate) fn pinpoint_reveal(&mut self, span: CodeSpan, cx: &mut Context<Self>) {
        let Some(range) = span.offsets(&self.buffer) else {
            return;
        };
        self.display.unfold(&self.buffer, |fold| {
            fold.start < range.end && range.start < fold.end
        });
        self.pin_reveal = Some(range);
        cx.notify();
    }

    /// The scroll position showing `range` in the middle of `viewport`
    /// (its top when taller), and its start column when it is sideways
    /// out of view of unwrapped text.
    pub(super) fn reveal_scroll(
        &self,
        range: Range<usize>,
        line_height: Pixels,
        em_width: Pixels,
        viewport: Size<Pixels>,
        wrap: Option<usize>,
    ) -> Point<Pixels> {
        let (first, start_column) = self.display.point(&self.buffer, range.start, false);
        let (last, end_column) = self.display.point(&self.buffer, range.end, true);
        let top = line_height * first as f32;
        let height = line_height * (last.max(first) + 1 - first) as f32;
        let y = if height >= viewport.height {
            top
        } else {
            top - (viewport.height - height) / 2.
        };
        let mut x = self.scroll.x;
        if wrap.is_none() {
            let left = em_width * start_column as f32;
            let right = em_width * end_column.max(start_column + 1) as f32;
            if left < x || right > x + viewport.width {
                x = (left - viewport.width / 4.).max(px(0.));
            }
        }
        point(x, y.max(px(0.)))
    }

    /// The reference of the selections (each on a line), or of the
    /// cursor's line: `src/main.rs:12:5-12:18`, `src/main.rs:12`.
    pub(super) fn reference(&self) -> Option<String> {
        let owner = self.code_owner()?;
        let references: Vec<String> = self
            .selections
            .all()
            .iter()
            .map(|s| {
                let span = CodeSpan::of_range(&self.buffer, s.range());
                code_reference(&span.want(owner.file.clone(), owner.cell))
            })
            .collect();
        Some(references.join("\n"))
    }

    pub(super) fn copy_reference(
        &mut self,
        _: &CopyReference,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.text_menu = None;
        if let Some(reference) = self.reference() {
            cx.write_to_clipboard(ClipboardItem::new_string(reference));
        }
        cx.notify();
    }

    /// Right-click on the text: a menu with Copy Reference. The cursor
    /// moves there unless the click is in a selection.
    pub(super) fn on_text_right_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.single_line || self.text_area && !self.notebook_cell {
            return;
        }
        let Some(layout) = &self.layout else {
            return;
        };
        if !layout.text_bounds.contains(&event.position) {
            return;
        }
        let offset = self.offset_for_position(event.position);
        let in_selection = self
            .selections
            .all()
            .iter()
            .any(|s| !s.is_empty() && s.start() <= offset && offset <= s.end());
        if !in_selection {
            self.selections = SelectionSet::single(Selection::cursor(offset));
        }
        cx.stop_propagation();
        self.hover = None;
        self.gutter_menu = None;
        self.text_menu = Some(event.position);
        cx.notify();
    }

    pub(super) fn render_text_menu(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let position = self.text_menu?;
        let item = |id: &'static str, label: &'static str, shortcut: Option<&'static str>| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .flex()
                .justify_between()
                .gap_4()
                .rounded_sm()
                .cursor_pointer()
                .hover(|style| style.bg(theme::active_row()))
                .child(label)
                .children(shortcut.map(|keys| div().text_color(theme::muted()).child(keys)))
        };
        let selected = self.selections.all().iter().any(|s| !s.is_empty());
        Some(deferred(
            anchored().position(position).snap_to_window().child(
                div()
                    .id("text-menu")
                    .occlude()
                    .min_w(px(240.))
                    .p_1()
                    .flex()
                    .flex_col()
                    .font_family(theme::UI_FONT)
                    .text_sm()
                    .text_color(theme::text())
                    .bg(theme::panel())
                    .border_1()
                    .border_color(theme::border())
                    .rounded_md()
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.text_menu = None;
                        cx.notify();
                    }))
                    .when(selected, |menu| {
                        menu.child(
                            item("text-menu-copy", "Copy", None)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.text_menu = None;
                                    this.copy(&Copy, window, cx);
                                    cx.notify();
                                }))
                                .pin("item:copy"),
                        )
                    })
                    .child(
                        item(
                            "text-menu-copy-reference",
                            "Copy Reference",
                            Some("Ctrl+Alt+Shift+C"),
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.copy_reference(&CopyReference, window, cx);
                        }))
                        .pin("item:copy-reference")
                        .label("Copy Reference"),
                    )
                    .when(self.notes_enabled(), |menu| {
                        menu.child(
                            item("text-menu-add-note", "Add Note", Some("Ctrl+K Ctrl+N"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.text_menu = None;
                                    this.add_note(&super::AddNote, window, cx);
                                }))
                                .pin("item:add-note")
                                .label("Add Note"),
                        )
                    })
                    .pin("text-menu")
                    .label("Text menu"),
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, WindowHandle};

    use super::{
        super::{
            display_map::{Fold, Inlay},
            tests::{editor, update},
        },
        *,
    };

    /// Rects of `range` as (column, row, width) in em widths and line
    /// heights from the text area's corner (halves kept), and whether part
    /// of it is not shown.
    fn rects(
        window: WindowHandle<Editor>,
        range: Range<usize>,
        cx: &mut TestAppContext,
    ) -> (Vec<(f32, f32, f32)>, bool) {
        window
            .read_with(cx, |e, _| {
                let l = e.layout.as_ref().expect("laid out");
                let half = |v: f32| (v * 2.).round() / 2.;
                let (rects, clipped) = e.range_rects(range, l.bounds);
                let rects = rects
                    .iter()
                    .map(|r| {
                        (
                            half((r.left() - l.text_bounds.left()) / l.em_width),
                            half((r.top() - l.text_bounds.top()) / l.line_height),
                            half(r.size.width / l.em_width),
                        )
                    })
                    .collect();
                (rects, clipped)
            })
            .unwrap()
    }

    fn offsets(
        window: WindowHandle<Editor>,
        span: CodeSpan,
        cx: &mut TestAppContext,
    ) -> Range<usize> {
        window
            .read_with(cx, |e, _| span.offsets(&e.buffer).unwrap())
            .unwrap()
    }

    fn span(
        line: u32,
        column: Option<u32>,
        end_line: Option<u32>,
        end_column: Option<u32>,
    ) -> CodeSpan {
        CodeSpan {
            line,
            column,
            end_line,
            end_column,
        }
    }

    #[gpui::test]
    fn words_multibyte_and_emoji_get_their_cells(cx: &mut TestAppContext) {
        let window = editor("a.rs", "|fn main() {\n    let é = \"日本😀\";\n}", cx);
        let main = offsets(window, span(1, Some(4), None, Some(7)), cx);
        assert_eq!(rects(window, main, cx), (vec![(3., 0., 4.)], false));
        let e_acute = offsets(window, span(2, Some(9), None, Some(9)), cx);
        assert_eq!(rects(window, e_acute, cx), (vec![(8., 1., 1.)], false));
        // An emoji is two cells wide.
        let string = offsets(window, span(2, Some(13), None, Some(17)), cx);
        assert_eq!(rects(window, string, cx), (vec![(12., 1., 6.)], false));
    }

    #[gpui::test]
    fn a_block_gets_one_rect_per_line_and_marks_empty_lines(cx: &mut TestAppContext) {
        let window = editor("a.rs", "|fn a() {\n\n    b();\n}\nc", cx);
        let block = offsets(window, span(1, None, Some(4), None), cx);
        assert_eq!(
            rects(window, block, cx),
            (
                vec![(0., 0., 8.), (0., 1., 0.5), (0., 2., 8.), (0., 3., 1.)],
                false
            )
        );
        // From a column of the first line to a column of the last.
        let part = offsets(window, span(1, Some(4), Some(3), Some(6)), cx);
        assert_eq!(
            rects(window, part, cx).0,
            [(3., 0., 5.), (0., 1., 0.5), (0., 2., 6.)]
        );
    }

    #[gpui::test]
    fn wrapped_lines_get_one_rect_per_row(cx: &mut TestAppContext) {
        let window = editor("a.txt", "|aaaa bbbb cccc dddd\nnext", cx);
        update(window, cx, |e, _, cx| {
            e.word_wrap = Some(true);
            e.wrap_columns = Some(10);
            cx.notify();
        });
        cx.run_until_parked();
        let line = offsets(window, span(1, None, None, None), cx);
        let (rows, clipped) = rects(window, line, cx);
        assert!(!clipped);
        assert_eq!(
            rows.iter().map(|r| r.1).collect::<Vec<_>>(),
            [0., 1.],
            "{rows:?}"
        );
        // `cccc` sits on the second row (after the continuation indent).
        let word = offsets(window, span(1, Some(11), None, Some(14)), cx);
        assert_eq!(rects(window, word, cx).0, [(0., 1., 4.)]);
        // A range across the wrap: a rect on each row.
        let across = offsets(window, span(1, Some(6), None, Some(14)), cx);
        assert_eq!(rects(window, across, cx).0.len(), 2);
    }

    #[gpui::test]
    fn folded_text_shows_as_its_placeholder_and_counts_as_hidden(cx: &mut TestAppContext) {
        let window = editor("a.rs", "|fn a() {\n    b();\n    c();\n}\nd", cx);
        update(window, cx, |e, _, cx| {
            let fold = Fold::lines(&e.buffer, 0, 2);
            e.display.fold(&e.buffer, [fold]);
            cx.notify();
        });
        cx.run_until_parked();
        // The header line is shown; `}` moved up to the second row.
        let header = offsets(window, span(1, Some(4), None, Some(4)), cx);
        assert_eq!(rects(window, header, cx), (vec![(3., 0., 1.)], false));
        let close = offsets(window, span(4, None, None, None), cx);
        assert_eq!(rects(window, close, cx), (vec![(0., 1., 1.)], false));
        // Hidden lines: the `⋯` stands for them, and they need revealing.
        let hidden = offsets(window, span(2, None, Some(3), None), cx);
        let (rows, clipped) = rects(window, hidden.clone(), cx);
        assert!(clipped);
        assert_eq!(rows.len(), 1, "the placeholder: {rows:?}");
        assert_eq!(rows[0].1, 0.);
        // Revealing opens the fold.
        update(window, cx, |e, _, cx| {
            e.pinpoint_reveal(span(2, None, Some(3), None), cx)
        });
        cx.run_until_parked();
        let (rows, clipped) = rects(window, hidden, cx);
        assert_eq!((rows.len(), clipped), (2, false));
    }

    #[gpui::test]
    fn off_screen_ranges_are_clipped_until_revealed(cx: &mut TestAppContext) {
        let text: String = (1..=400).map(|i| format!("line {i}\n")).collect();
        let window = editor("a.txt", &format!("|{text}"), cx);
        let far = offsets(window, span(300, Some(6), None, Some(8)), cx);
        assert_eq!(rects(window, far.clone(), cx), (vec![], true));
        update(window, cx, |e, _, cx| {
            e.pinpoint_reveal(span(300, Some(6), None, Some(8)), cx)
        });
        cx.run_until_parked();
        let (rows, clipped) = rects(window, far, cx);
        assert!(!clipped, "{rows:?}");
        assert_eq!(rows.len(), 1);
        // In the middle of the view; the cursor did not move.
        let (row, visible_rows, cursor) = window
            .read_with(cx, |e, _| {
                let l = e.layout.as_ref().unwrap();
                (
                    rows[0].1,
                    l.text_bounds.size.height / l.line_height,
                    e.cursor(),
                )
            })
            .unwrap();
        assert!(
            (row - visible_rows / 2.).abs() <= 1.,
            "{row} of {visible_rows}"
        );
        assert_eq!(cursor, 0);

        // A range partly below the view: the visible part, clipped.
        // The last row wholly in view.
        let last_line = window
            .read_with(cx, |e, _| {
                let l = e.layout.as_ref().unwrap();
                let cut = (0..l.rows.len())
                    .find(|&i| l.row_top(i, e.scroll.y) + l.line_height > l.text_bounds.bottom())
                    .unwrap();
                l.rows[cut - 1].row.line
            })
            .unwrap() as u32;
        let first = last_line;
        let across = offsets(window, span(first + 1, None, Some(first + 3), None), cx);
        let (rows, clipped) = rects(window, across, cx);
        assert!(clipped);
        assert!(!rows.is_empty() && rows.len() < 3, "{rows:?}");
    }

    #[gpui::test]
    fn long_lines_scroll_sideways_to_reveal(cx: &mut TestAppContext) {
        let long = format!("|{}target", "x".repeat(600));
        let window = editor("a.txt", &long, cx);
        let target = offsets(window, span(1, Some(601), None, Some(606)), cx);
        assert!(rects(window, target.clone(), cx).1, "off to the right");
        update(window, cx, |e, _, cx| {
            e.pinpoint_reveal(span(1, Some(601), None, Some(606)), cx)
        });
        cx.run_until_parked();
        let (rows, clipped) = rects(window, target, cx);
        assert!(!clipped);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2, 6.);
    }

    #[gpui::test]
    fn inlay_hints_push_the_rects_of_text_after_them(cx: &mut TestAppContext) {
        let window = editor("a.rs", "|let x = 1;", cx);
        update(window, cx, |e, _, cx| {
            let hint = Inlay {
                offset: 5,
                label: ": i32".into(),
                padding_left: false,
                padding_right: false,
            };
            e.display.set_inlays(&e.buffer, vec![hint]);
            cx.notify();
        });
        cx.run_until_parked();
        let x = offsets(window, span(1, Some(5), None, Some(5)), cx);
        assert_eq!(rects(window, x, cx).0, [(4., 0., 1.)]);
        let one = offsets(window, span(1, Some(9), None, Some(9)), cx);
        assert_eq!(rects(window, one, cx).0, [(13., 0., 1.)], "after `: i32`");
    }

    #[gpui::test]
    fn copy_reference_names_the_selections_or_the_cursor_line(cx: &mut TestAppContext) {
        let window = editor("src/a.rs", "fn «main»() {\n    |b();\n}", cx);
        let copied = |window, cx: &mut TestAppContext| {
            update(window, cx, |e, w, cx| {
                e.copy_reference(&CopyReference, w, cx)
            });
            cx.read_from_clipboard().and_then(|item| item.text())
        };
        assert_eq!(
            copied(window, cx).as_deref(),
            Some("src/a.rs:1:4-1:7\nsrc/a.rs:2")
        );
        let window = editor("src/a.rs", "fn main() {\n    b();|\n}", cx);
        assert_eq!(copied(window, cx).as_deref(), Some("src/a.rs:2"));
        let window = editor("src/a.rs", "«fn main() {\n    b();\n»}", cx);
        assert_eq!(copied(window, cx).as_deref(), Some("src/a.rs:1-2"));
    }

    #[gpui::test]
    fn the_picker_takes_the_word_or_the_selection_under_the_pointer(cx: &mut TestAppContext) {
        let window = editor("a.rs", "|let total = a_b + 1;", cx);
        let at = |column: f32, window: WindowHandle<Editor>, cx: &mut TestAppContext| {
            window
                .read_with(cx, |e: &Editor, _| {
                    let l = e.layout.as_ref().unwrap();
                    let pointer = point(
                        l.text_bounds.left() + l.em_width * column,
                        l.text_bounds.top() + l.line_height / 2.,
                    );
                    e.pick_range(pointer).map(|r| e.buffer.slice(r))
                })
                .unwrap()
        };
        assert_eq!(at(5.5, window, cx).as_deref(), Some("total"));
        assert_eq!(at(13.2, window, cx).as_deref(), Some("a_b"));
        assert_eq!(at(16.5, window, cx).as_deref(), Some("+"));
        assert_eq!(at(40., window, cx), None, "past the text");
        update(window, cx, |e, _, cx| e.select_text_range(4, 15, cx));
        assert_eq!(at(5.5, window, cx).as_deref(), Some("total = a_b"));
    }
}
