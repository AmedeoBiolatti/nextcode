//! Annotate with Git Blame (PyCharm): a gutter column left of the line
//! numbers with the date and author of the commit that last changed each
//! line, written once per run of lines from the same commit. Hovering
//! shows the commit (hash, author, date, full message); a click shows it
//! in the Git log. With `git.inline_blame` the cursor's line also gets the
//! author, age and summary after its end (VS Code).
//!
//! The workspace runs `git blame` on the buffer's text and hands it over
//! with that text; edits are followed by diffing that text with the
//! buffer (with the change markers), so lines shift with the code and
//! edited lines read "Not committed".

use std::time::Instant;

use git::BlameCommit;
use gpui::{anchored, deferred};

use super::*;
use crate::git_log::{absolute_time, relative_time};

actions!(editor, [ToggleBlame]);

/// Width of the blame column, in em widths.
pub(super) const BLAME_COLUMNS: f32 = 26.;
/// Author characters shown in the column.
const AUTHOR_CHARS: usize = 13;
/// Commits newer than this are drawn brighter.
const RECENT_SECONDS: i64 = 30 * 86_400;

#[derive(Default)]
pub(super) struct BlameState {
    pub(super) blame: Option<Arc<git::Blame>>,
    /// The text `blame` describes.
    pub(super) text: Option<Arc<str>>,
    /// Per current line: its line in `text`, `None` when edited since.
    pub(super) map: Vec<Option<usize>>,
    /// The column is on (Annotate).
    pub(super) shown: bool,
    /// Line whose annotation the mouse is over.
    hover: Option<usize>,
}

/// What blame says about a line.
pub(super) enum LineBlame<'a> {
    Commit(&'a BlameCommit),
    NotCommitted,
    /// Past the blamed lines (the empty line after a final line break).
    Unknown,
}

/// Maps each line of `current` to its line in `blamed` (`None` for lines
/// added or changed since).
pub(super) fn line_map(
    blamed: &str,
    current: &str,
    deadline: Option<Instant>,
) -> Vec<Option<usize>> {
    let count = current.split('\n').count();
    let mut map = Vec::with_capacity(count);
    let (mut old, mut new) = (0, 0);
    for change in git::line_changes(blamed, current, deadline) {
        while new < change.new.start {
            map.push(Some(old));
            old += 1;
            new += 1;
        }
        map.extend(change.new.clone().map(|_| None));
        new = change.new.end;
        old = change.old.end;
    }
    while new < count {
        map.push(Some(old));
        old += 1;
        new += 1;
    }
    map
}

impl Editor {
    /// Blame of `text` (this buffer's text when blame ran); lines follow
    /// edits made since. Ignored when annotations were closed meanwhile.
    pub fn set_blame(&mut self, blame: git::Blame, text: Arc<str>, cx: &mut Context<Self>) {
        if !self.wants_blame() {
            return;
        }
        let lines = text.split('\n').count();
        self.blame.blame = Some(Arc::new(blame));
        self.blame.text = Some(text);
        self.blame.map = (0..lines).map(Some).collect();
        self.schedule_git_diff(Duration::ZERO, cx);
        cx.notify();
    }

    /// Blame could not run (e.g. the file is not committed yet).
    pub fn blame_failed(&mut self, error: &str, cx: &mut Context<Self>) {
        self.blame.shown = false;
        self.notice = Some(format!("Cannot annotate: {error}").into());
        cx.notify();
    }

    /// Whether blame data is wanted: the column is on, or inline blame.
    pub fn wants_blame(&self) -> bool {
        !self.single_line
            && !self.text_area
            && (self.blame.shown || crate::settings::current().git.inline_blame)
    }

    pub fn has_blame(&self) -> bool {
        self.blame.blame.is_some()
    }

    /// Annotate with Git Blame / Close Annotations.
    pub(super) fn toggle_blame(&mut self, _: &ToggleBlame, _: &mut Window, cx: &mut Context<Self>) {
        self.set_blame_shown(!self.blame.shown, cx);
    }

    pub(super) fn set_blame_shown(&mut self, shown: bool, cx: &mut Context<Self>) {
        self.blame.shown = shown;
        self.blame.hover = None;
        if shown {
            if self.blame.blame.is_none() {
                cx.emit(EditorEvent::LoadBlame);
            }
        } else if !self.wants_blame() {
            self.blame = BlameState::default();
        }
        cx.notify();
    }

    pub(super) fn line_blame(&self, line: usize) -> LineBlame<'_> {
        let Some(blame) = &self.blame.blame else {
            return LineBlame::Unknown;
        };
        match self.blame.map.get(line) {
            Some(Some(blamed)) => match blame.lines.get(*blamed) {
                Some(Some(index)) => LineBlame::Commit(&blame.commits[*index]),
                Some(None) => LineBlame::NotCommitted,
                None => LineBlame::Unknown,
            },
            Some(None) => LineBlame::NotCommitted,
            None => LineBlame::Unknown,
        }
    }

    /// The commit id of `line`'s annotation group (`""` when not
    /// committed), for grouping.
    fn group_key(&self, line: usize) -> Option<&str> {
        match self.line_blame(line) {
            LineBlame::Commit(commit) => Some(&commit.id),
            LineBlame::NotCommitted => Some(""),
            LineBlame::Unknown => None,
        }
    }

    /// Width of the blame column (zero when off or not loaded).
    pub(super) fn blame_width(&self, em_width: Pixels) -> Pixels {
        if self.blame.shown && self.blame.blame.is_some() {
            em_width * BLAME_COLUMNS
        } else {
            px(0.)
        }
    }

    /// The annotation of `line` when it starts a group: text and color.
    pub(super) fn blame_label(&self, line: usize) -> Option<(String, Hsla)> {
        if !self.blame.shown {
            return None;
        }
        let key = self.group_key(line)?;
        if line > 0 && self.group_key(line - 1) == Some(key) {
            return None;
        }
        Some(match self.line_blame(line) {
            LineBlame::Commit(commit) => {
                let date = absolute_time(commit.time);
                let mut author: String = commit.author.chars().take(AUTHOR_CHARS).collect();
                if commit.author.chars().count() > AUTHOR_CHARS {
                    author.pop();
                    author.push('…');
                }
                let recent = crate::git_log::now() - commit.time < RECENT_SECONDS;
                let color = if recent {
                    theme::text()
                } else {
                    theme::muted()
                };
                (format!("{} {author}", &date[..10]), color)
            }
            _ => ("Not committed".to_owned(), theme::git_modified()),
        })
    }

    /// The muted text after the cursor's line with `git.inline_blame`.
    pub(super) fn inline_blame_text(&self) -> Option<String> {
        if self.single_line || self.text_area || !crate::settings::current().git.inline_blame {
            return None;
        }
        match self.line_blame(self.cursor_line()) {
            LineBlame::Commit(commit) => Some(format!(
                "{}, {} • {}",
                commit.author,
                relative_time(commit.time),
                commit.summary
            )),
            LineBlame::NotCommitted => Some("Not committed yet".to_owned()),
            LineBlame::Unknown => None,
        }
    }

    /// The line whose annotation is at `position`, if over the column.
    fn blame_line_at(&self, position: Point<Pixels>) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        let left = layout.blame_left?;
        let width = layout.em_width * BLAME_COLUMNS;
        if !layout.bounds.contains(&position) || !(left..left + width).contains(&position.x) {
            return None;
        }
        let (_, row) = layout.row_layout_at(position.y, self.scroll.y)?;
        Some(row.row.line)
    }

    /// A click on an annotation shows its commit in the Git log.
    pub(super) fn blame_click(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some(line) = self.blame_line_at(position) else {
            return false;
        };
        if let LineBlame::Commit(commit) = self.line_blame(line) {
            cx.emit(EditorEvent::ShowCommit(commit.id.clone()));
        }
        true
    }

    pub(super) fn blame_mouse_moved(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let hover = self
            .blame_line_at(position)
            .filter(|&line| matches!(self.line_blame(line), LineBlame::Commit(_)));
        if hover != self.blame.hover {
            self.blame.hover = hover;
            cx.notify();
        }
    }

    pub(super) fn clear_blame_hover(&mut self, cx: &mut Context<Self>) {
        if self.blame.hover.take().is_some() {
            cx.notify();
        }
    }

    /// The hovered annotation's commit, beside the column.
    pub(super) fn render_blame_hover(&self) -> Option<impl IntoElement + use<>> {
        let line = self.blame.hover?;
        let LineBlame::Commit(commit) = self.line_blame(line) else {
            return None;
        };
        let layout = self.layout.as_ref()?;
        let left = layout.blame_left?;
        let (_, top) = self.screen_position(self.buffer.line_start(line))?;
        let origin = point(
            left + layout.em_width * BLAME_COLUMNS,
            layout.bounds.top() + top + layout.line_height,
        );
        let header = format!(
            "{}  {} <{}>",
            crate::commit_panel::short_revision(&commit.id),
            commit.author,
            commit.author_mail
        );
        let date = format!(
            "{} ({})",
            absolute_time(commit.time),
            relative_time(commit.time)
        );
        Some(deferred(
            anchored().position(origin).snap_to_window().child(
                div()
                    .id("blame-hover")
                    .max_w(px(520.))
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .bg(theme::panel())
                    .border_1()
                    .border_color(theme::border())
                    .rounded_md()
                    .shadow_lg()
                    .font_family(theme::UI_FONT)
                    .text_xs()
                    .text_color(theme::text())
                    .child(selectable("blame-header", header))
                    .child(
                        div()
                            .text_color(theme::muted())
                            .child(selectable("blame-date", date)),
                    )
                    .child(
                        div()
                            .pt_1()
                            .border_t_1()
                            .border_color(theme::border())
                            .child(selectable("blame-message", commit.message.clone())),
                    )
                    .child(
                        div()
                            .text_color(theme::muted())
                            .child(selectable("blame-id", commit.id.clone())),
                    )
                    .pin("blame-hover")
                    .label("Commit details"),
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::super::tests::{editor, type_text, update};
    use super::*;

    fn commit(id: &str, author: &str) -> BlameCommit {
        BlameCommit {
            id: id.repeat(40),
            author: author.into(),
            author_mail: format!("{author}@example.com"),
            time: 1_700_000_000,
            summary: format!("{author}'s change"),
            message: format!("{author}'s change\n\ndetails"),
        }
    }

    fn annotations(window: gpui::WindowHandle<Editor>, cx: &mut TestAppContext) -> Vec<String> {
        window
            .read_with(cx, |editor, _| {
                (0..editor.buffer.line_count())
                    .map(|line| match editor.line_blame(line) {
                        LineBlame::Commit(commit) => commit.author.clone(),
                        LineBlame::NotCommitted => "-".into(),
                        LineBlame::Unknown => "?".into(),
                    })
                    .collect()
            })
            .unwrap()
    }

    #[gpui::test]
    fn blame_lines_shift_with_edits_and_edited_lines_are_not_committed(cx: &mut TestAppContext) {
        let text = "one\ntwo\nthree\n";
        let window = editor("a.py", &format!("|{text}"), cx);
        update(window, cx, |editor, _, cx| {
            editor.blame.shown = true;
            editor.set_blame(
                git::Blame {
                    commits: vec![commit("a", "Ann"), commit("b", "Bob")],
                    lines: vec![Some(0), Some(1), Some(1)],
                },
                text.into(),
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(annotations(window, cx), ["Ann", "Bob", "Bob", "?"]);
        // Grouped: one label per run of the same commit.
        let labels = window
            .read_with(cx, |editor, _| {
                (0..3)
                    .map(|line| editor.blame_label(line).map(|(text, _)| text))
                    .collect::<Vec<_>>()
            })
            .unwrap();
        assert_eq!(
            labels,
            [
                Some("2023-11-14 Ann".to_owned()),
                Some("2023-11-14 Bob".to_owned()),
                None
            ]
        );
        // A new first line shifts the others down.
        type_text(window, "zero\n", cx);
        cx.executor().advance_clock(Duration::from_millis(150));
        cx.run_until_parked();
        assert_eq!(annotations(window, cx), ["-", "Ann", "Bob", "Bob", "?"]);
        // Editing a line makes it uncommitted.
        update(window, cx, |editor, _, cx| {
            let start = editor.text().find("two").unwrap();
            editor.commit(
                vec![(start..start + 3, "TWO".into())],
                |_| SelectionSet::single(Selection::cursor(0)),
                cx,
            );
        });
        cx.executor().advance_clock(Duration::from_millis(150));
        cx.run_until_parked();
        assert_eq!(annotations(window, cx), ["-", "Ann", "-", "Bob", "?"]);
    }

    #[gpui::test]
    fn turning_annotations_on_asks_for_blame_once(cx: &mut TestAppContext) {
        let window = editor("a.py", "|one\n", cx);
        let asked = std::rc::Rc::new(std::cell::Cell::new(0));
        let sink = asked.clone();
        cx.update(|cx| {
            cx.subscribe(&window.entity(cx).unwrap(), move |_, event, _| {
                if let EditorEvent::LoadBlame = event {
                    sink.set(sink.get() + 1);
                }
            })
            .detach()
        });
        update(window, cx, |editor, window, cx| {
            editor.toggle_blame(&ToggleBlame, window, cx)
        });
        assert_eq!(asked.get(), 1);
        update(window, cx, |editor, _, cx| {
            editor.set_blame(
                git::Blame {
                    commits: vec![commit("a", "Ann")],
                    lines: vec![Some(0)],
                },
                "one\n".into(),
                cx,
            );
        });
        // Off, then on again: the data was dropped, so blame runs again
        // (the commit may have changed meanwhile).
        update(window, cx, |editor, window, cx| {
            editor.toggle_blame(&ToggleBlame, window, cx);
            assert!(!editor.has_blame());
            editor.toggle_blame(&ToggleBlame, window, cx);
        });
        assert_eq!(asked.get(), 2);
    }
}
