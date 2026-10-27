//! Sticky scroll (VS Code, PyCharm): the header lines of the scopes around
//! the first visible line stay pinned at the top of the text, outermost
//! first, with their line numbers and syntax colours, over a subtle
//! shadow. Scopes are the foldable regions (`folds.rs`: definitions and
//! blocks from the syntax tree, else indentation), so folds and wrapped
//! rows are followed through the display map. The innermost header is
//! pushed up as its scope's last line scrolls under it. Clicking a header
//! scrolls to it and puts the cursor there.

use std::ops::Range;

use gpui::{
    App, Bounds, ContentMask, Context, Pixels, Point, ShapedLine, TextRun, Window, fill, point, px,
};

use syntax::Syntax;
use text::Buffer;

use super::{
    Editor, EditorLayout, LineHighlights,
    display_map::DisplayMap,
    folds::{Region, indentation_regions},
    line_runs,
    selections::{Selection, SelectionSet},
    semantic,
};
use crate::theme;

/// Lines past the first visible one whose regions are fetched together,
/// so scrolling reuses them.
const REGION_LOOKAHEAD: usize = 256;
/// The shadow under the pinned lines: its height in 1 px steps.
const SHADOW_STEPS: usize = 4;

/// A pinned header: its line and its top from the text's top (above the
/// slot it would take while pushed up).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct StickyHeader {
    pub line: usize,
    pub top: Pixels,
}

/// The scopes around `lines` as `(opening line, last line)`: the fold
/// regions (`Editor::fold_regions`), each from the line its construct
/// opens on (`def`/`fn` of a multi-line signature, not a comment or the
/// `):` line) rather than the line that stays shown when folded.
pub(super) fn scope_regions(
    buffer: &Buffer,
    syntax: Option<&Syntax>,
    lines: Range<usize>,
) -> Vec<Region> {
    match syntax {
        Some(syntax) if !syntax.language().folds_by_indentation() => syntax
            .scopes(lines)
            .into_iter()
            .map(|scope| (scope.opening, scope.last))
            .collect(),
        _ => indentation_regions(buffer, lines),
    }
}

/// Headers for the text scrolled to `scroll_y`, at most `max`: slot `i`
/// shows the outermost region inside slot `i - 1`'s that contains the
/// line under slot `i` (but does not start on it). A header moves up once
/// its region's last row would end above the slot's bottom.
pub(super) fn sticky_headers(
    display: &DisplayMap,
    regions: &[Region],
    scroll_y: Pixels,
    line_height: Pixels,
    max: usize,
) -> Vec<StickyHeader> {
    let mut headers = Vec::new();
    let mut outer: Option<Region> = None;
    let row_count = display.row_count();
    for slot in 0..max {
        let slot_top = line_height * slot as f32;
        let row = ((scroll_y + slot_top) / line_height).floor().max(0.) as usize;
        if row >= row_count {
            break;
        }
        let line = display.row_line(row);
        let region = regions
            .iter()
            .copied()
            .filter(|&(header, last)| header < line && line <= last)
            .filter(|&(header, last)| {
                outer.is_none_or(|(outer_header, outer_last)| {
                    header > outer_header && last <= outer_last
                })
            })
            .min_by_key(|&(header, last)| (header, std::cmp::Reverse(last)));
        let Some((header, last)) = region else {
            break;
        };
        // The bottom of the region's last row (folds and wraps included).
        let end = line_height * display.line_row(last + 1) as f32 - scroll_y;
        let top = slot_top.min(end - line_height);
        headers.push(StickyHeader { line: header, top });
        if top < slot_top {
            break;
        }
        outer = Some((header, last));
    }
    headers
}

/// A pinned header as laid out.
pub(super) struct StickyRow {
    pub line: usize,
    /// Window y of its top.
    pub top: Pixels,
    /// Buffer offset where `shaped` starts (the line's first row).
    start: usize,
    shaped: ShapedLine,
    number: ShapedLine,
}

pub(super) struct StickyLayout {
    pub rows: Vec<StickyRow>,
    /// Left edge of the gutter: the rows span it and the text.
    left: Pixels,
    right: Pixels,
    /// Window y where the pinned rows end.
    bottom: Pixels,
}

impl StickyLayout {
    /// The pinned rows' area.
    pub fn bounds(&self, top: Pixels) -> Bounds<Pixels> {
        Bounds::from_corners(point(self.left, top), point(self.right, self.bottom))
    }
}

#[derive(Default)]
pub(super) struct StickyState {
    /// Regions intersecting some lines, for a buffer version.
    regions: Option<(u64, Range<usize>, Vec<Region>)>,
    /// The pinned row under the mouse.
    hovered: Option<usize>,
}

impl Editor {
    /// Most headers pinned in a text area `viewport` tall: the setting,
    /// and at most half the rows (none in input fields).
    fn sticky_max(&self, line_height: Pixels, viewport: Pixels) -> usize {
        let settings = self.file_settings();
        let sticky = &settings.editor.sticky_scroll;
        if self.single_line || self.text_area || !sticky.enabled {
            return 0;
        }
        sticky
            .max_lines
            .min((viewport / line_height / 2.).floor().max(0.) as usize)
    }

    /// The headers pinned with the text scrolled to `scroll_y`.
    pub(super) fn sticky_lines(
        &mut self,
        scroll_y: Pixels,
        line_height: Pixels,
        viewport: Pixels,
    ) -> Vec<StickyHeader> {
        let max = self.sticky_max(line_height, viewport);
        if max == 0 || self.buffer.line_count() < 2 {
            return Vec::new();
        }
        let top_row = (scroll_y / line_height).floor().max(0.) as usize;
        let first = self.display.row_line(top_row);
        let last = self.display.row_line(top_row + max);
        let fresh = self
            .sticky
            .regions
            .as_ref()
            .is_some_and(|(version, lines, _)| {
                *version == self.version && lines.start <= first && last < lines.end
            });
        if !fresh {
            let lines = first..(last + 1 + REGION_LOOKAHEAD).min(self.buffer.line_count());
            let regions = scope_regions(&self.buffer, self.syntax.as_ref(), lines.clone());
            self.sticky.regions = Some((self.version, lines, regions));
        }
        let regions = &self.sticky.regions.as_ref().expect("filled above").2;
        sticky_headers(&self.display, regions, scroll_y, line_height, max)
    }

    /// The scroll that keeps the cursor's row (`cursor_row`) out from
    /// under the pinned headers, from `scroll_y` (which shows it).
    pub(super) fn sticky_autoscroll(
        &mut self,
        cursor_row: usize,
        scroll_y: Pixels,
        line_height: Pixels,
        viewport: Pixels,
    ) -> Pixels {
        let top = line_height * cursor_row as f32;
        let mut scroll_y = scroll_y;
        for _ in 0..=self.sticky_max(line_height, viewport) {
            let headers = self.sticky_lines(scroll_y, line_height, viewport);
            let covered = headers.last().map_or(px(0.), |h| h.top + line_height);
            if top < scroll_y || top >= scroll_y + covered {
                break;
            }
            scroll_y = (top - covered).max(px(0.));
        }
        scroll_y
    }

    /// Shapes the pinned headers for the resolved scroll. `left` is the
    /// gutter's left edge, `text` the text area.
    pub(super) fn layout_sticky(
        &mut self,
        window: &mut Window,
        left: Pixels,
        text: Bounds<Pixels>,
        line_height: Pixels,
    ) -> Option<StickyLayout> {
        let headers = self.sticky_lines(self.scroll.y, line_height, text.size.height);
        if headers.is_empty() {
            self.sticky.hovered = None;
            return None;
        }
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let text_system = window.text_system().clone();
        let base = TextRun {
            len: 0,
            font: style.font(),
            color: theme::text(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let theme = theme::current();
        let legend = self
            .lsp
            .as_ref()
            .and_then(|doc| doc.server.semantic_legend.as_ref());
        let styler = semantic::Styler::new(&theme, self.language(), legend);
        let cursor_line = self.buffer.line_of(self.primary().head);
        let mut cuts = Vec::new();
        let rows: Vec<StickyRow> = headers
            .iter()
            .map(|header| {
                let range = self
                    .display
                    .line_display_rows(&self.buffer, header.line)
                    .into_iter()
                    .next()
                    .map_or(
                        self.buffer.line_start(header.line)..self.buffer.line_end(header.line),
                        |row| row.range,
                    );
                let spans = self
                    .syntax
                    .as_ref()
                    .map(|syntax| syntax.highlights(&self.buffer, range.clone()))
                    .unwrap_or_default();
                let highlights = LineHighlights {
                    spans: &spans,
                    tokens: semantic::overlapping(&self.semantic.tokens, range.clone()),
                };
                let runs = line_runs(
                    range.clone(),
                    &highlights,
                    &styler,
                    None,
                    &[],
                    &base,
                    &mut cuts,
                );
                // Tabs render as one space, as in the text.
                let line_text = self.buffer.slice(range.clone()).replace('\t', " ");
                let shaped = text_system.shape_line(line_text.into(), font_size, &runs, None);
                let number = (header.line + 1).to_string();
                let color = if header.line == cursor_line {
                    theme::text()
                } else {
                    theme::muted()
                };
                let number_run = TextRun {
                    len: number.len(),
                    color,
                    ..base.clone()
                };
                let number = text_system.shape_line(number.into(), font_size, &[number_run], None);
                StickyRow {
                    line: header.line,
                    top: text.top() + header.top,
                    start: range.start,
                    shaped,
                    number,
                }
            })
            .collect();
        let bottom = rows
            .iter()
            .map(|row| row.top + line_height)
            .fold(text.top(), Pixels::max);
        if self.sticky.hovered.is_some_and(|index| index >= rows.len()) {
            self.sticky.hovered = None;
        }
        Some(StickyLayout {
            rows,
            left,
            right: text.right(),
            bottom,
        })
    }

    pub(super) fn paint_sticky(&self, layout: &EditorLayout, window: &mut Window, cx: &mut App) {
        let Some(sticky) = &layout.sticky else {
            return;
        };
        let line_height = layout.line_height;
        let text = layout.text_bounds;
        let area = Bounds::from_corners(point(sticky.left, text.top()), text.bottom_right());
        window.with_content_mask(Some(ContentMask { bounds: area }), |window| {
            // A pushed-up row slides under the ones above it.
            for (index, row) in sticky.rows.iter().enumerate().rev() {
                let background = if self.sticky.hovered == Some(index) {
                    theme::hover_row()
                } else {
                    theme::background()
                };
                window.paint_quad(fill(
                    Bounds::from_corners(
                        point(sticky.left, row.top),
                        point(sticky.right, row.top + line_height),
                    ),
                    background,
                ));
                let x = layout.numbers_right - row.number.width;
                row.number
                    .paint(point(x, row.top), line_height, window, cx)
                    .ok();
                window.with_content_mask(Some(ContentMask { bounds: text }), |window| {
                    let x = text.left() - self.scroll.x;
                    row.shaped
                        .paint(point(x, row.top), line_height, window, cx)
                        .ok();
                });
            }
            for step in 0..SHADOW_STEPS {
                let alpha = 0.2 * (SHADOW_STEPS - step) as f32 / SHADOW_STEPS as f32;
                let y = sticky.bottom + px(step as f32);
                window.paint_quad(fill(
                    Bounds::from_corners(point(sticky.left, y), point(sticky.right, y + px(1.))),
                    gpui::black().opacity(alpha),
                ));
            }
        });
    }

    /// The pinned row under `position` (rows above win where a pushed-up
    /// one slides under them).
    fn sticky_row_at(&self, position: Point<Pixels>) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        let sticky = layout.sticky.as_ref()?;
        if !sticky.bounds(layout.text_bounds.top()).contains(&position) {
            return None;
        }
        sticky
            .rows
            .iter()
            .position(|row| (row.top..row.top + layout.line_height).contains(&position.y))
    }

    pub(super) fn over_sticky(&self, position: Point<Pixels>) -> bool {
        self.sticky_row_at(position).is_some()
    }

    /// A click on a pinned header: scrolls so its line shows right under
    /// the headers above it, with the cursor where it was clicked (the
    /// line start from the gutter).
    pub(super) fn sticky_mouse_down(
        &mut self,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(index) = self.sticky_row_at(position) else {
            return false;
        };
        let Some(layout) = self.layout.as_ref() else {
            return false;
        };
        let Some(row) = layout.sticky.as_ref().map(|sticky| &sticky.rows[index]) else {
            return false;
        };
        let text_left = layout.text_bounds.left() - self.scroll.x;
        let offset = if position.x >= layout.text_bounds.left() {
            row.start + row.shaped.closest_index_for_x(position.x - text_left)
        } else {
            row.start
        };
        let line_height = layout.line_height;
        let line_row = self.display.line_row(row.line);
        self.scroll.y = (line_height * line_row as f32 - line_height * index as f32).max(px(0.));
        self.sticky.hovered = None;
        self.set_selections(SelectionSet::single(Selection::cursor(offset)), cx);
        true
    }

    pub(super) fn sticky_mouse_moved(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let hovered = self.sticky_row_at(position);
        if hovered != self.sticky.hovered {
            self.sticky.hovered = hovered;
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use gpui::{Modifiers, MouseButton, MouseDownEvent, TestAppContext};
    use syntax::{Language, Syntax};
    use text::Buffer;

    use super::*;
    use crate::editor::{
        display_map::Fold,
        tests::{editor, state, update},
    };

    const LH: Pixels = px(20.);

    struct Doc {
        buffer: Buffer,
        display: DisplayMap,
        regions: Vec<Region>,
    }

    fn doc(name: &str, text: &str) -> Doc {
        let buffer = Buffer::with_path(text, PathBuf::from(name));
        let language = Language::from_path(name.as_ref()).unwrap();
        let syntax = Syntax::new(language, &buffer).expect("grammar library");
        let regions = scope_regions(&buffer, Some(&syntax), 0..buffer.line_count());
        Doc {
            display: DisplayMap::new(&buffer),
            buffer,
            regions,
        }
    }

    impl Doc {
        /// Header lines and tops (in rows) with `row` at the top.
        fn at(&self, row: f32) -> Vec<(usize, f32)> {
            sticky_headers(&self.display, &self.regions, LH * row, LH, 5)
                .into_iter()
                .map(|h| (h.line, h.top / LH))
                .collect()
        }
    }

    const PYTHON: &str = "\
class Outer:
    def method(self):
        if ready:
            a = 1
            b = 2
            c = 3
        d = 4
        e = 5

    def other(self):
        return 6
x = 7
";

    #[test]
    fn nested_python_scopes_stack_outermost_first() {
        let doc = doc("a.py", PYTHON);
        assert_eq!(doc.at(0.), [], "the class header is at the top itself");
        // `def method` at the top: the class is pinned over it, and each
        // pinned header covers the next line, itself a header (VS Code).
        assert_eq!(doc.at(1.), [(0, 0.), (1, 1.), (2, 2.)]);
        assert_eq!(doc.at(3.), [(0, 0.), (1, 1.), (2, 2.)]);
        // `if ready` ends at line 5: half a row on, slot 2 would end below
        // line 5's row, so the header is half way up.
        assert_eq!(doc.at(3.5), [(0, 0.), (1, 1.), (2, 1.5)]);
        // At row 4 slot 2 covers line 6, outside the `if`: two levels.
        assert_eq!(doc.at(4.), [(0, 0.), (1, 1.)]);
        assert_eq!(doc.at(5.), [(0, 0.), (1, 1.)]);
        // `other` replaces `method` (which ended at line 7).
        assert_eq!(doc.at(9.), [(0, 0.), (9, 1.)]);
        // Top-level code after the class: nothing.
        assert_eq!(doc.at(11.), []);
    }

    #[test]
    fn headers_are_the_opening_lines_not_comments_docstrings_or_the_colon_line() {
        let doc = doc(
            "a.py",
            "\
class Inventory:
    \"\"\"Keeps track of stock.\"\"\"

    def receive(self, count):
        # Receive the items
        if count > 0:
            return count
        return 0

    def ship(self, count):
        \"\"\"Ship them.\"\"\"
        total = count
        return total

    @staticmethod
    def audit(
        first,
        second,
    ):
        # check
        check(first)
        return second
",
        );
        // The comment line at the top: `def receive` is pinned, not it.
        assert_eq!(doc.at(3.), [(0, 0.), (3, 1.)]);
        // Next slot down covers the `if` line, a scope of its own.
        assert_eq!(doc.at(4.), [(0, 0.), (3, 1.), (5, 2.)]);
        // The docstring at the top: `def ship`.
        assert_eq!(doc.at(9.), [(0, 0.), (9, 1.)]);
        assert_eq!(doc.at(10.), [(0, 0.), (9, 1.)]);
        // Inside a multi-line signature and its body: the `def` line,
        // neither the decorator nor the `):` line.
        assert_eq!(doc.at(15.), [(0, 0.), (15, 1.)]);
        assert_eq!(doc.at(19.), [(0, 0.), (15, 1.)]);
    }

    #[test]
    fn the_last_header_is_pushed_up_as_its_scope_ends() {
        let doc = doc("a.py", "def f():\n    a = 1\n    b = 2\n    c = 3\nz = 0\n");
        assert_eq!(doc.at(1.), [(0, 0.)]);
        assert_eq!(doc.at(2.), [(0, 0.)]);
        // Line 3 (`c = 3`, the last of the scope) at the top: the header
        // still fits on it exactly.
        assert_eq!(doc.at(3.), [(0, 0.)]);
        // A quarter row on, the header moves up with it.
        assert_eq!(doc.at(3.25), [(0, -0.25)]);
        assert_eq!(doc.at(3.99)[0].0, 0);
        // The scope is gone.
        assert_eq!(doc.at(4.), []);
    }

    const RUST: &str = "\
impl Widget {
    fn draw(&self) {
        for part in &self.parts {
            match part {
                Part::A => {
                    one();
                }
                Part::B => two(),
            }
        }
    }

    fn size(&self) -> usize {
        3
    }
}
";

    #[test]
    fn rust_impl_and_its_blocks() {
        let doc = doc("a.rs", RUST);
        // Every pinned header covers the next one: five levels, the limit.
        assert_eq!(doc.at(1.), [(0, 0.), (1, 1.), (2, 2.), (3, 3.), (4, 4.)]);
        // `Part::A => {` (line 4) under slot 0: the arm is not pinned.
        assert_eq!(doc.at(4.), [(0, 0.), (1, 1.), (2, 2.), (3, 3.)]);
        // A closing brace's line stays shown when folded, so scopes end on
        // the line before it: `match` ends at line 7, `for` at line 8.
        assert_eq!(doc.at(5.), [(0, 0.), (1, 1.), (2, 2.)]);
        assert_eq!(doc.at(7.), [(0, 0.), (1, 1.)]);
        // `draw` ends at line 9: pushed up half a row.
        assert_eq!(doc.at(8.5), [(0, 0.), (1, 0.5)]);
        // The second method.
        assert_eq!(doc.at(12.), [(0, 0.), (12, 1.)]);
        // A lower limit.
        let two = sticky_headers(&doc.display, &doc.regions, LH * 1., LH, 2);
        assert_eq!(two.iter().map(|h| h.line).collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn folds_and_wraps_follow_the_display() {
        let mut doc = doc("a.py", PYTHON);
        // Fold `if ready` (lines 3-5 hidden): rows shift up by three.
        let fold = Fold::lines(&doc.buffer, 2, 5);
        doc.display.fold(&doc.buffer, [fold]);
        // Row 2 is the folded header: it is not pinned itself.
        assert_eq!(doc.at(2.), [(0, 0.), (1, 1.)]);
        // Row 3 is line 6 (`d = 4`), still in `method`.
        assert_eq!(doc.at(3.), [(0, 0.), (1, 1.)]);
        // `method` ends at line 7 = row 4: half a row on, pushed up half.
        assert_eq!(doc.at(3.5), [(0, 0.), (1, 0.5)]);
        assert_eq!(doc.at(4.), [(0, 0.)]);

        // A wrapped header counts as one line; the push-up follows rows.
        let mut doc = self::doc(
            "a.py",
            "def f():\n    if a_long_condition_name_here:\n        x = 1\n        y = 2\nz = 0\n",
        );
        doc.display.set_wrap(&doc.buffer, Some(20));
        let if_row = doc.display.line_row(1);
        let if_rows = doc.display.line_row(2) - if_row;
        assert!(if_rows >= 2, "the `if` wraps");
        // The `if`'s first row at the top: slot 1 still shows the `if`.
        assert_eq!(doc.at(if_row as f32), [(0, 0.)]);
        // Its last row at the top: slot 1 shows line 2, inside it.
        let lines = doc.at((if_row + if_rows - 1) as f32);
        assert_eq!(lines.iter().map(|l| l.0).collect::<Vec<_>>(), [0, 1]);
        // Half way through the last line of `f` (line 3).
        let last_row = doc.display.line_row(3);
        assert_eq!(doc.at(last_row as f32 + 0.5), [(0, -0.5)]);
    }

    /// Ten top-level lines, then `class Outer` (line 10) / `def method`
    /// (11) / `if ready` (12) around 60 lines.
    fn python_editor(cx: &mut TestAppContext) -> gpui::WindowHandle<Editor> {
        let top: String = (0..10).map(|i| format!("a{i} = {i}\n")).collect();
        let body: String = (0..60)
            .map(|i| format!("            v{i} = {i}\n"))
            .collect();
        let text = format!(
            "|{top}class Outer:\n    def method(self):\n        if ready:\n{body}        done()\n"
        );
        editor("a.py", &text, cx)
    }

    fn line_height(window: gpui::WindowHandle<Editor>, cx: &mut TestAppContext) -> Pixels {
        window
            .read_with(cx, |e, _| e.layout.as_ref().expect("laid out").line_height)
            .unwrap()
    }

    fn pinned(window: gpui::WindowHandle<Editor>, cx: &mut TestAppContext) -> Vec<usize> {
        window
            .read_with(cx, |e, _| {
                let layout = e.layout.as_ref().unwrap();
                layout
                    .sticky
                    .as_ref()
                    .map(|s| s.rows.iter().map(|r| r.line).collect())
                    .unwrap_or_default()
            })
            .unwrap()
    }

    #[gpui::test]
    fn the_editor_pins_headers_and_a_click_scrolls_to_one(cx: &mut TestAppContext) {
        let window = python_editor(cx);
        let line_height = line_height(window, cx);
        update(window, cx, |e, _, cx| {
            e.scroll.y = line_height * 30.;
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(pinned(window, cx), [10, 11, 12]);
        // Click the second header (`def method`) in the text, after `m`.
        update(window, cx, |e, w, cx| {
            let layout = e.layout.as_ref().unwrap();
            let row = &layout.sticky.as_ref().unwrap().rows[1];
            let position = point(
                layout.text_bounds.left() + layout.em_width * 8.6,
                row.top + line_height / 2.,
            );
            assert!(e.over_sticky(position));
            e.on_mouse_down(
                &MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                    first_mouse: false,
                },
                w,
                cx,
            );
            assert!(e.drag.is_none(), "no text selection drag");
        });
        cx.run_until_parked();
        assert!(state(window, cx).contains("class Outer:\n    def m|ethod"));
        // `def method` shows where its pinned header was: right under the
        // class line, which is the top row (not pinned any more).
        let scroll = window.read_with(cx, |e, _| e.scroll.y).unwrap();
        assert_eq!(scroll, line_height * 10.);
        assert!(pinned(window, cx).is_empty());
    }

    #[gpui::test]
    fn moving_up_keeps_the_cursor_out_from_under_the_headers(cx: &mut TestAppContext) {
        let window = python_editor(cx);
        let line_height = line_height(window, cx);
        // Cursor on line 40, scrolled so it is the top row.
        update(window, cx, |e, _, cx| {
            let offset = e.buffer.line_start(40);
            e.selections = SelectionSet::single(Selection::cursor(offset));
            e.scroll.y = line_height * 40.;
            cx.notify();
        });
        cx.run_until_parked();
        // Up: line 39 would be under the three headers.
        update(window, cx, |e, w, cx| e.up(&super::super::Up, w, cx));
        cx.run_until_parked();
        window
            .read_with(cx, |e, _| {
                assert_eq!(e.buffer.line_of(e.primary().head), 39);
                assert_eq!(e.scroll.y, line_height * 36.);
                let layout = e.layout.as_ref().unwrap();
                let sticky = layout.sticky.as_ref().unwrap();
                let covered = sticky.bottom - layout.text_bounds.top();
                assert_eq!(covered, line_height * 3.);
            })
            .unwrap();
    }

    #[gpui::test]
    fn the_setting_and_input_fields_turn_it_off(cx: &mut TestAppContext) {
        let window = python_editor(cx);
        update(window, cx, |e, _, _| {
            let lh = px(20.);
            assert_eq!(e.sticky_lines(lh * 20., lh, px(400.)).len(), 3);
            // A short view pins at most half its rows.
            assert_eq!(e.sticky_lines(lh * 20., lh, px(80.)).len(), 2);
            e.text_area = true;
            assert!(e.sticky_lines(lh * 20., lh, px(400.)).is_empty());
        });
    }
}
