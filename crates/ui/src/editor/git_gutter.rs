//! Git change markers (VS Code / PyCharm): coloured bars in the gutter for
//! added and modified lines and a triangle where lines were deleted,
//! against a base version the workspace sets (HEAD, or the index with
//! `git.staging_area`). Recomputed after edits (debounced, longer for
//! large files) on a background thread.
//!
//! A click on a marker opens a popup with the base's lines and actions:
//! Rollback (one undo step), Stage Hunk (index base only), Show Diff,
//! previous/next change. Next/Previous Change and Rollback Lines also run
//! from the keyboard.

use std::time::Instant;

use git::{LineChange, LineChangeKind};
use gpui::{PathBuilder, anchored, deferred};

use super::*;

actions!(
    editor,
    [NextChange, PreviousChange, RollbackLines, CloseHunkPopup]
);

/// Width of the change-bar column, between the line numbers and the fold
/// chevrons.
pub(super) const CHANGE_COLUMN: Pixels = px(7.);
const BAR_WIDTH: Pixels = px(3.);
/// Base and current lines the hunk popup shows at most.
const POPUP_LINES: usize = 12;
/// Diffs stop refining after this long (the result stays correct).
const DIFF_DEADLINE: Duration = Duration::from_millis(300);

/// The version change markers compare the buffer with.
#[derive(Clone)]
pub struct GitBase {
    pub text: Arc<str>,
    /// Staged but not committed (HEAD base): every line counts as added.
    pub new_file: bool,
    /// The base is git's index, so hunks can be staged.
    pub index: bool,
}

#[derive(Default)]
pub(super) struct GitGutterState {
    pub(super) base: Option<GitBase>,
    /// Sorted by `new.start`.
    pub(super) changes: Vec<LineChange>,
    task: Option<Task<()>>,
    /// The hunk popup: a line its change touches.
    popup: Option<HunkPopup>,
}

struct HunkPopup {
    line: usize,
    /// Base lines and current lines of the change (cut at
    /// `POPUP_LINES`), with how many were left out.
    old: Vec<String>,
    new: Vec<String>,
    more_old: usize,
    more_new: usize,
}

/// The gutter's right-click menu: annotations, and the line's change.
pub(super) struct GutterMenu {
    position: Point<Pixels>,
    line: usize,
}

/// Wait after an edit before diffing again: small files follow typing
/// closely, large ones wait for a pause.
pub(super) fn diff_delay(len: usize) -> Duration {
    match len {
        0..=262_144 => Duration::from_millis(100),
        262_145..=2_097_152 => Duration::from_millis(400),
        _ => Duration::from_millis(1000),
    }
}

impl Editor {
    /// Sets (or with `None` clears) the base the change markers compare
    /// with, and recomputes them now.
    pub fn set_git_base(&mut self, base: Option<GitBase>, cx: &mut Context<Self>) {
        let cleared = base.is_none();
        self.git.base = base;
        if cleared {
            self.git.changes.clear();
            self.git.popup = None;
            cx.notify();
        }
        self.schedule_git_diff(Duration::ZERO, cx);
    }

    /// Whether markers are shown (a base is set).
    pub fn has_git_base(&self) -> bool {
        self.git.base.is_some()
    }

    #[cfg(test)]
    pub(crate) fn line_changes(&self) -> &[LineChange] {
        &self.git.changes
    }

    /// The text changed: diff again after a pause.
    pub(super) fn git_text_edited(&mut self, cx: &mut Context<Self>) {
        if self.git.base.is_some() || self.blame.blame.is_some() {
            self.schedule_git_diff(diff_delay(self.buffer.len()), cx);
        }
    }

    /// Diffs the text with the base (and the blamed text) after `delay`,
    /// replacing any pending diff.
    pub(super) fn schedule_git_diff(&mut self, delay: Duration, cx: &mut Context<Self>) {
        if self.git.base.is_none() && self.blame.blame.is_none() {
            self.git.task = None;
            return;
        }
        self.git.task = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let Ok((text, base, blamed)) = this.update(cx, |this, _| {
                (this.text(), this.git.base.clone(), this.blame.text.clone())
            }) else {
                return;
            };
            let (changes, map) = cx
                .background_spawn(async move {
                    let deadline = Some(Instant::now() + DIFF_DEADLINE);
                    let changes = match &base {
                        Some(base) if base.new_file => git::all_added(&text),
                        Some(base) => git::line_changes(&base.text, &text, deadline),
                        None => Vec::new(),
                    };
                    let map = blamed.map(|blamed| blame::line_map(&blamed, &text, deadline));
                    (changes, map)
                })
                .await;
            this.update(cx, |this, cx| {
                this.git.changes = changes;
                if let Some(map) = map {
                    this.blame.map = map;
                }
                this.refresh_hunk_popup();
                cx.notify();
            })
            .ok();
        }));
    }

    /// The change touching `line`, if any.
    fn change_at(&self, line: usize) -> Option<&LineChange> {
        let count = self.buffer.line_count();
        self.git
            .changes
            .iter()
            .find(|change| change.touches(line, count))
    }

    /// Paints the change marker of `row` (every row of a changed line).
    pub(super) fn paint_change_marker(
        &self,
        layout: &EditorLayout,
        row: &RowLayout,
        top: Pixels,
        window: &mut Window,
    ) {
        let Some(left) = layout.change_left else {
            return;
        };
        let line = row.row.line;
        let changes = &self.git.changes;
        let first = changes.partition_point(|change| change.new.end < line);
        let count = self.buffer.line_count();
        let height = layout.line_height;
        for change in changes[first..]
            .iter()
            .take_while(|change| change.new.start <= line + 1)
        {
            match change.kind {
                LineChangeKind::Added | LineChangeKind::Modified if change.new.contains(&line) => {
                    let color = if change.kind == LineChangeKind::Added {
                        theme::git_added()
                    } else {
                        theme::git_modified()
                    };
                    window.paint_quad(fill(
                        Bounds::new(point(left, top), size(BAR_WIDTH, height)),
                        color,
                    ));
                }
                LineChangeKind::Deleted => {
                    // At the top of the line after the deleted ones, or
                    // under the last line when they ended the file.
                    let y = if change.new.start == line && row.row.first {
                        top
                    } else if change.new.start >= count && line + 1 == count && row.row.last {
                        top + height
                    } else {
                        continue;
                    };
                    let half = (height * 0.28).max(px(3.));
                    let mut path = PathBuilder::fill();
                    path.move_to(point(left, y - half));
                    path.line_to(point(left + half * 1.4, y));
                    path.line_to(point(left, y + half));
                    path.close();
                    if let Ok(path) = path.build() {
                        window.paint_path(path, theme::git_deleted());
                    }
                }
                _ => {}
            }
        }
    }

    /// Whether `position` is over the change-marker column.
    fn in_change_column(&self, position: Point<Pixels>) -> bool {
        self.layout.as_ref().is_some_and(|layout| {
            layout.change_left.is_some_and(|left| {
                layout.bounds.contains(&position)
                    && (left - px(2.)..left + CHANGE_COLUMN).contains(&position.x)
            })
        })
    }

    /// A click on a change marker opens its popup.
    pub(super) fn change_marker_click(
        &mut self,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.in_change_column(position) {
            return false;
        }
        let Some(layout) = &self.layout else {
            return false;
        };
        let Some((_, row)) = layout.row_layout_at(position.y, self.scroll.y) else {
            return false;
        };
        let line = row.row.line;
        if self.change_at(line).is_none() {
            return false;
        }
        self.open_hunk_popup(line, cx);
        true
    }

    fn open_hunk_popup(&mut self, line: usize, cx: &mut Context<Self>) {
        self.hover = None;
        self.completion = None;
        self.git.popup = Some(HunkPopup {
            line,
            old: Vec::new(),
            new: Vec::new(),
            more_old: 0,
            more_new: 0,
        });
        self.refresh_hunk_popup();
        cx.notify();
    }

    /// Re-reads the popup's lines after the changes were recomputed; closes
    /// it when its change is gone.
    fn refresh_hunk_popup(&mut self) {
        let Some(line) = self.git.popup.as_ref().map(|popup| popup.line) else {
            return;
        };
        let (Some(change), Some(base)) = (self.change_at(line).cloned(), &self.git.base) else {
            self.git.popup = None;
            return;
        };
        let old: Vec<String> = base
            .text
            .split('\n')
            .skip(change.old.start)
            .take(change.old.len().min(POPUP_LINES))
            .map(|line| line.trim_end_matches('\r').to_owned())
            .collect();
        let new: Vec<String> = change
            .new
            .clone()
            .take(POPUP_LINES)
            .map(|line| self.buffer.line_text(line))
            .map(|line| line.trim_end_matches(['\r', '\n']).to_owned())
            .collect();
        self.git.popup = Some(HunkPopup {
            line,
            more_old: change.old.len() - old.len(),
            more_new: change.new.len() - new.len(),
            old,
            new,
        });
    }

    pub(super) fn hunk_popup_open(&self) -> bool {
        self.git.popup.is_some()
    }

    pub(super) fn close_hunk_popup(
        &mut self,
        _: &CloseHunkPopup,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.git.popup.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn next_change(&mut self, _: &NextChange, _: &mut Window, cx: &mut Context<Self>) {
        let line = self.cursor_line();
        self.go_to_change(line, true, false, cx);
    }

    pub(super) fn previous_change(
        &mut self,
        _: &PreviousChange,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line = self.cursor_line();
        self.go_to_change(line, false, false, cx);
    }

    /// Moves the cursor to the change after (or before) `line`, wrapping
    /// around; with `popup` the hunk popup follows.
    fn go_to_change(&mut self, line: usize, forward: bool, popup: bool, cx: &mut Context<Self>) {
        let count = self.buffer.line_count();
        let start = |change: &LineChange| change.new.start.min(count.saturating_sub(1));
        let current = self.change_at(line).map(start);
        let starts: Vec<usize> = self.git.changes.iter().map(start).collect();
        let target = if forward {
            let after = current.unwrap_or(line);
            starts
                .iter()
                .copied()
                .find(|&s| s > after)
                .or_else(|| starts.first().copied())
        } else {
            let before = current.unwrap_or(line);
            starts
                .iter()
                .rev()
                .copied()
                .find(|&s| s < before)
                .or_else(|| starts.last().copied())
        };
        let Some(target) = target else {
            self.notice = Some("No changes".into());
            cx.notify();
            return;
        };
        self.go_to_line(target, 0, cx);
        if popup {
            self.open_hunk_popup(target, cx);
        }
    }

    /// Ctrl+Alt+Z: rolls back every change touching the selected lines.
    pub(super) fn rollback_lines(
        &mut self,
        _: &RollbackLines,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.buffer.line_count();
        let blocks: Vec<Range<usize>> = self
            .selections
            .all()
            .iter()
            .map(|s| {
                let first = self.buffer.line_of(s.start());
                let last = self.buffer.line_of(s.end());
                first..last + 1
            })
            .collect();
        let changes: Vec<LineChange> = self
            .git
            .changes
            .iter()
            .filter(|change| {
                blocks
                    .iter()
                    .any(|lines| change.touches_range(lines, count))
            })
            .cloned()
            .collect();
        if changes.is_empty() {
            self.notice = Some("No changes to roll back here".into());
            cx.notify();
            return;
        }
        self.rollback(&changes, cx);
    }

    /// Restores the base's lines for `changes` (sorted) in one undo step.
    fn rollback(&mut self, changes: &[LineChange], cx: &mut Context<Self>) {
        let Some(base) = self.git.base.clone() else {
            return;
        };
        let current = self.text();
        let mut edits: Vec<(Range<usize>, String)> = changes
            .iter()
            .map(|change| {
                git::minimal_edit(&current, &git::revert_change(&current, &base.text, change))
            })
            .collect();
        edits.sort_by_key(|(range, _)| range.start);
        let overlapping = edits.windows(2).any(|pair| pair[0].0.end > pair[1].0.start);
        if overlapping {
            // Adjacent edits: one edit over the whole difference.
            let mut text = current.clone();
            for change in changes.iter().rev() {
                text = git::revert_change(&text, &base.text, change);
            }
            edits = vec![git::minimal_edit(&current, &text)];
        }
        self.git.popup = None;
        self.apply_edits_keeping_selections(edits, cx);
        self.schedule_git_diff(Duration::ZERO, cx);
    }

    fn popup_change(&self) -> Option<LineChange> {
        self.change_at(self.git.popup.as_ref()?.line).cloned()
    }

    fn rollback_popup_change(&mut self, cx: &mut Context<Self>) {
        if let Some(change) = self.popup_change() {
            self.rollback(&[change], cx);
        }
    }

    /// Puts the popup's change (only) into the index.
    fn stage_popup_change(&mut self, cx: &mut Context<Self>) {
        let (Some(change), Some(base)) = (self.popup_change(), self.git.base.clone()) else {
            return;
        };
        let contents = git::apply_change(&base.text, &self.text(), &change);
        self.git.popup = None;
        cx.emit(EditorEvent::StageHunk { contents });
        cx.notify();
    }

    fn show_popup_diff(&mut self, cx: &mut Context<Self>) {
        if let Some(change) = self.popup_change() {
            self.git.popup = None;
            cx.emit(EditorEvent::ShowChangeDiff {
                line: change.new.start,
            });
            cx.notify();
        }
    }

    /// Rollback Lines / Show Diff for the gutter menu's line.
    pub(super) fn rollback_line(&mut self, line: usize, cx: &mut Context<Self>) {
        if let Some(change) = self.change_at(line).cloned() {
            self.rollback(&[change], cx);
        }
    }

    pub(super) fn line_has_change(&self, line: usize) -> bool {
        self.change_at(line).is_some()
    }

    /// The hunk popup, under its change's last line.
    pub(super) fn render_hunk_popup(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let popup = self.git.popup.as_ref()?;
        let change = self.change_at(popup.line)?.clone();
        let layout = self.layout.as_ref()?;
        let base = self.git.base.as_ref()?;
        let count = self.buffer.line_count();
        let below = if change.new.is_empty() {
            change.new.start.saturating_sub(1)
        } else {
            change.new.end - 1
        }
        .min(count.saturating_sub(1));
        let top = self
            .screen_position(self.buffer.line_start(below))
            .map_or(px(0.), |(_, top)| top + layout.line_height);
        let origin = point(
            layout.change_left.unwrap_or(layout.bounds.left()),
            layout.bounds.top() + top,
        );
        let title = match change.kind {
            LineChangeKind::Added => format!("{} added", lines_word(change.new.len())),
            LineChangeKind::Deleted => format!("{} deleted", lines_word(change.old.len())),
            LineChangeKind::Modified if change.old.len() == change.new.len() => {
                format!("{} changed", lines_word(change.new.len()))
            }
            LineChangeKind::Modified => format!(
                "{} changed to {}",
                lines_word(change.old.len()),
                lines_word(change.new.len())
            ),
        };
        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px_2()
                .rounded_sm()
                .cursor_pointer()
                .text_xs()
                .font_family(theme::UI_FONT)
                .text_color(theme::accent_text())
                .hover(|style| style.bg(theme::hover_row()))
                .child(label)
        };
        let code_line = |id: (&'static str, usize), text: &str, sign: &'static str, bg: Hsla| {
            div()
                .flex()
                .bg(bg)
                .px_1()
                .whitespace_nowrap()
                .overflow_hidden()
                .child(div().w(px(14.)).flex_none().child(sign))
                // A space keeps an empty line one line high.
                .child(selectable(
                    id,
                    if text.is_empty() {
                        " ".to_owned()
                    } else {
                        text.replace('\t', "    ")
                    },
                ))
        };
        let more = |id: &'static str, count: usize| {
            (count > 0).then(|| {
                div()
                    .px_1()
                    .text_color(theme::muted())
                    .child(selectable(id, format!("⋯ {count} more")))
            })
        };
        let index = base.index;
        Some(deferred(
            anchored().position(origin).snap_to_window().child(
                div()
                    .id("hunk-popup")
                    .occlude()
                    .w(px(560.))
                    .flex()
                    .flex_col()
                    .bg(theme::panel())
                    .border_1()
                    .border_color(theme::border())
                    .rounded_md()
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down_out(cx.listener(|this, event: &MouseDownEvent, _, cx| {
                        if !this.in_change_column(event.position) {
                            this.git.popup = None;
                            cx.notify();
                        }
                    }))
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .flex()
                            .items_center()
                            .gap_1()
                            .border_b_1()
                            .border_color(theme::border())
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .font_family(theme::UI_FONT)
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(selectable("hunk-title", title)),
                            )
                            .child(button("hunk-previous", "↑").on_click(cx.listener(
                                |this, _, _, cx| {
                                    if let Some(line) = this.git.popup.as_ref().map(|p| p.line) {
                                        this.go_to_change(line, false, true, cx);
                                    }
                                },
                            )))
                            .child(button("hunk-next", "↓").on_click(cx.listener(
                                |this, _, _, cx| {
                                    if let Some(line) = this.git.popup.as_ref().map(|p| p.line) {
                                        this.go_to_change(line, true, true, cx);
                                    }
                                },
                            )))
                            .child(button("hunk-rollback", "Rollback").on_click(
                                cx.listener(|this, _, _, cx| this.rollback_popup_change(cx)),
                            ))
                            .when(index, |row| {
                                row.child(button("hunk-stage", "Stage Hunk").on_click(
                                    cx.listener(|this, _, _, cx| this.stage_popup_change(cx)),
                                ))
                            })
                            .child(
                                button("hunk-diff", "Show Diff").on_click(
                                    cx.listener(|this, _, _, cx| this.show_popup_diff(cx)),
                                ),
                            )
                            .child(button("hunk-close", "✕").on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.git.popup = None;
                                    cx.notify();
                                },
                            ))),
                    )
                    .child(
                        div()
                            .py_1()
                            .flex()
                            .flex_col()
                            .font_family(theme::MONO_FONT)
                            .text_size(px(theme::EDITOR_FONT_SIZE - 1.))
                            .text_color(theme::text())
                            .children(popup.old.iter().enumerate().map(|(i, text)| {
                                code_line(("hunk-old", i), text, "-", theme::diff_removed())
                            }))
                            .children(more("hunk-more-old", popup.more_old))
                            .children(popup.new.iter().enumerate().map(|(i, text)| {
                                code_line(("hunk-new", i), text, "+", theme::diff_added())
                            }))
                            .children(more("hunk-more-new", popup.more_new)),
                    ),
            ),
        ))
    }
}

impl Editor {
    /// Right-click in the gutter (outside the breakpoint column).
    pub(super) fn open_gutter_menu(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(layout) = &self.layout else {
            return;
        };
        if !layout.bounds.contains(&position) || position.x >= layout.text_bounds.left() {
            return;
        }
        let Some((_, row)) = layout.row_layout_at(position.y, self.scroll.y) else {
            return;
        };
        cx.stop_propagation();
        self.hover = None;
        self.git.popup = None;
        self.gutter_menu = Some(GutterMenu {
            position,
            line: row.row.line,
        });
        cx.notify();
    }

    pub(super) fn render_gutter_menu(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let menu = self.gutter_menu.as_ref()?;
        let line = menu.line;
        let changed = self.line_has_change(line);
        let item = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(|style| style.bg(theme::active_row()))
                .child(label)
        };
        let annotate = if self.blame.shown {
            "Close Annotations"
        } else {
            "Annotate with Git Blame"
        };
        Some(deferred(
            anchored().position(menu.position).snap_to_window().child(
                div()
                    .id("gutter-menu")
                    .occlude()
                    .min_w(px(220.))
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
                        this.gutter_menu = None;
                        cx.notify();
                    }))
                    .child(item("gutter-annotate", annotate).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.gutter_menu = None;
                            let shown = !this.blame.shown;
                            this.set_blame_shown(shown, cx);
                        },
                    )))
                    .when(changed, |menu| {
                        menu.child(
                            item("gutter-rollback", "Rollback Lines").on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.gutter_menu = None;
                                    this.rollback_line(line, cx);
                                },
                            )),
                        )
                        .child(
                            item("gutter-show-change", "Show Change").on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.gutter_menu = None;
                                    this.open_hunk_popup(line, cx);
                                },
                            )),
                        )
                    }),
            ),
        ))
    }
}

fn lines_word(count: usize) -> String {
    format!("{count} line{}", if count == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::super::tests::{editor, state, type_text, update};
    use super::*;

    fn base(text: &str) -> Option<GitBase> {
        Some(GitBase {
            text: text.into(),
            new_file: false,
            index: false,
        })
    }

    fn kinds(
        window: gpui::WindowHandle<Editor>,
        cx: &mut TestAppContext,
    ) -> Vec<(LineChangeKind, Range<usize>)> {
        window
            .read_with(cx, |editor, _| {
                editor
                    .line_changes()
                    .iter()
                    .map(|change| (change.kind, change.new.clone()))
                    .collect()
            })
            .unwrap()
    }

    #[gpui::test]
    fn markers_follow_typing_after_a_short_pause(cx: &mut TestAppContext) {
        let window = editor("a.py", "one\ntwo|\nthree\nfour\n", cx);
        update(window, cx, |editor, _, cx| {
            editor.set_git_base(base("one\ntwo\nthree\nfour\n"), cx)
        });
        cx.run_until_parked();
        assert!(kinds(window, cx).is_empty());
        type_text(window, "!\nnew", cx);
        // Not yet: the diff waits for a pause in typing.
        cx.run_until_parked();
        assert!(kinds(window, cx).is_empty());
        cx.executor().advance_clock(Duration::from_millis(150));
        cx.run_until_parked();
        assert_eq!(
            kinds(window, cx),
            [(LineChangeKind::Modified, 1..3)],
            "a changed line and an inserted one form one modified run"
        );
        // Deleting an unchanged line marks a deletion before the next line.
        update(window, cx, |editor, _, cx| {
            let text = editor.text();
            let start = text.find("four").unwrap();
            editor.commit(
                vec![(start..start + "four\n".len(), String::new())],
                |_| SelectionSet::single(Selection::cursor(0)),
                cx,
            );
        });
        cx.executor().advance_clock(Duration::from_millis(150));
        cx.run_until_parked();
        assert_eq!(
            kinds(window, cx),
            [
                (LineChangeKind::Modified, 1..3),
                (LineChangeKind::Deleted, 4..4)
            ]
        );
    }

    #[gpui::test]
    fn large_files_wait_longer_before_diffing(cx: &mut TestAppContext) {
        let text: String = (0..300_000).map(|i| format!("line {i}\n")).collect();
        let window = editor("big.txt", &format!("|{text}"), cx);
        update(window, cx, |editor, _, cx| {
            editor.set_git_base(base(&text), cx)
        });
        cx.run_until_parked();
        type_text(window, "x", cx);
        cx.executor().advance_clock(Duration::from_millis(150));
        cx.run_until_parked();
        assert!(kinds(window, cx).is_empty(), "still waiting");
        cx.executor().advance_clock(Duration::from_millis(1000));
        cx.run_until_parked();
        assert_eq!(kinds(window, cx), [(LineChangeKind::Modified, 0..1)]);
    }

    #[gpui::test]
    fn rollback_lines_restores_the_base_in_one_undo_step(cx: &mut TestAppContext) {
        let window = editor("a.py", "zero\none\nTWO|\nthree\nfour\n", cx);
        update(window, cx, |editor, _, cx| {
            editor.set_git_base(base("one\ntwo\nthree\n"), cx)
        });
        cx.run_until_parked();
        assert_eq!(
            kinds(window, cx),
            [
                (LineChangeKind::Added, 0..1),
                (LineChangeKind::Modified, 2..3),
                (LineChangeKind::Added, 4..5)
            ]
        );
        // Only the change under the cursor; the cursor stays on its line.
        update(window, cx, |editor, window, cx| {
            editor.rollback_lines(&RollbackLines, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(state(window, cx), "zero\none\ntwo|\nthree\nfour\n");
        assert_eq!(
            kinds(window, cx),
            [(LineChangeKind::Added, 0..1), (LineChangeKind::Added, 4..5)]
        );
        // All of them from a selection over everything.
        update(window, cx, |editor, window, cx| {
            editor.select_all_text(cx);
            editor.rollback_lines(&RollbackLines, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            window.read_with(cx, |editor, _| editor.text()).unwrap(),
            "one\ntwo\nthree\n"
        );
        assert!(kinds(window, cx).is_empty());
        update(window, cx, |editor, window, cx| {
            editor.undo(&Undo, window, cx)
        });
        assert_eq!(
            window.read_with(cx, |editor, _| editor.text()).unwrap(),
            "zero\none\ntwo\nthree\nfour\n"
        );
    }

    #[gpui::test]
    fn crlf_buffers_roll_back_with_crlf(cx: &mut TestAppContext) {
        let window = editor("a.txt", "a\r\nB|\r\nc", cx);
        update(window, cx, |editor, _, cx| {
            editor.set_git_base(base("a\nb\nc\nd"), cx)
        });
        cx.run_until_parked();
        assert_eq!(
            kinds(window, cx),
            [
                (LineChangeKind::Modified, 1..2),
                (LineChangeKind::Deleted, 3..3)
            ]
        );
        update(window, cx, |editor, window, cx| {
            editor.select_all_text(cx);
            editor.rollback_lines(&RollbackLines, window, cx)
        });
        assert_eq!(
            window.read_with(cx, |editor, _| editor.text()).unwrap(),
            "a\r\nb\r\nc\r\nd"
        );
    }

    #[gpui::test]
    fn next_and_previous_change_wrap_around(cx: &mut TestAppContext) {
        let window = editor("a.py", "|a\nB\nc\nd\nE\nf\n", cx);
        update(window, cx, |editor, _, cx| {
            editor.set_git_base(base("a\nb\nc\nd\ne\nf\n"), cx)
        });
        cx.run_until_parked();
        let line = |cx: &mut TestAppContext| {
            window
                .read_with(cx, |editor, _| editor.cursor_line())
                .unwrap()
        };
        let next = |cx: &mut TestAppContext| {
            update(window, cx, |editor, window, cx| {
                editor.next_change(&NextChange, window, cx)
            })
        };
        next(cx);
        assert_eq!(line(cx), 1);
        next(cx);
        assert_eq!(line(cx), 4);
        next(cx);
        assert_eq!(line(cx), 1, "wraps to the first change");
        update(window, cx, |editor, window, cx| {
            editor.previous_change(&PreviousChange, window, cx)
        });
        assert_eq!(line(cx), 4, "wraps to the last change");
    }

    #[gpui::test]
    fn stage_hunk_sends_the_base_with_only_that_change(cx: &mut TestAppContext) {
        let window = editor("a.py", "one\nTWO\nthree\nfour|\n", cx);
        update(window, cx, |editor, _, cx| {
            editor.set_git_base(
                Some(GitBase {
                    text: "one\ntwo\nthree\n".into(),
                    new_file: false,
                    index: true,
                }),
                cx,
            )
        });
        cx.run_until_parked();
        let staged = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = staged.clone();
        cx.update(|cx| {
            cx.subscribe(&window.entity(cx).unwrap(), move |_, event, _| {
                if let EditorEvent::StageHunk { contents } = event {
                    sink.borrow_mut().push(contents.clone());
                }
            })
            .detach()
        });
        update(window, cx, |editor, _, cx| {
            editor.open_hunk_popup(3, cx);
            editor.stage_popup_change(cx);
        });
        assert_eq!(*staged.borrow(), ["one\ntwo\nthree\nfour\n"]);
    }
}
