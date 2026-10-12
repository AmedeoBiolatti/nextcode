//! Fuzzy file finder: a modal that indexes the project and matches paths as you type.

use std::{
    ops::Range,
    path::{MAIN_SEPARATOR, PathBuf},
    sync::Arc,
};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle,
    KeyBinding, MouseButton, ScrollStrategy, SharedString, StyledText, Subscription, Task,
    UniformListScrollHandle, Window, actions, div, prelude::*, px, rgb, uniform_list,
};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};

use crate::{
    editor::{self, Editor, EditorEvent},
    project::Project,
    theme,
};

actions!(file_finder, [Dismiss]);

const CONTEXT: &str = "FileFinder";
const MAX_RESULTS: usize = 200;
const ROW_HEIGHT: f32 = 28.;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Dismiss, Some(CONTEXT))]);
}

pub enum FileFinderEvent {
    Open(PathBuf),
    Dismissed,
}

impl EventEmitter<FileFinderEvent> for FileFinder {}

/// A ranked path with the byte offsets of the matched characters.
struct PathMatch {
    index: usize,
    positions: Vec<usize>,
}

pub struct FileFinder {
    project: Project,
    query: Entity<Editor>,
    /// Project-relative paths, sorted.
    files: Arc<Vec<String>>,
    indexing: bool,
    /// Why the project could not be listed.
    error: Option<String>,
    matches: Vec<PathMatch>,
    selected: usize,
    scroll: UniformListScrollHandle,
    _index: Task<()>,
    _search: Task<()>,
    _subscription: Subscription,
}

impl FileFinder {
    pub fn new(project: Project, cx: &mut Context<Self>) -> Self {
        let query = cx.new(Editor::single_line);
        let subscription = cx.subscribe(&query, |this, _, event, cx| {
            if let EditorEvent::Edited = event {
                this.search(cx);
            }
        });
        let walk_project = project.clone();
        let index = cx.spawn(async move |this, cx| {
            let files = cx
                .background_spawn(async move { walk_project.list_files() })
                .await;
            this.update(cx, |this, cx| {
                match files {
                    Ok(files) => this.files = Arc::new(files),
                    Err(err) => this.error = Some(format!("Cannot list files: {err}")),
                }
                this.indexing = false;
                this.search(cx);
            })
            .ok();
        });
        Self {
            project,
            query,
            files: Arc::default(),
            indexing: true,
            error: None,
            matches: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            _index: index,
            _search: Task::ready(()),
            _subscription: subscription,
        }
    }

    /// Re-ranks off the main thread; a newer search replaces (cancels) the pending one.
    fn search(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text();
        let files = self.files.clone();
        self._search = cx.spawn(async move |this, cx| {
            let matches = cx
                .background_spawn(async move { rank(&files, &query) })
                .await;
            this.update(cx, |this, cx| {
                this.matches = matches;
                this.selected = 0;
                this.scroll.scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            })
            .ok();
        });
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected = index;
        self.scroll.scroll_to_item(index, ScrollStrategy::Center);
        cx.notify();
    }

    fn select_prev(&mut self, _: &editor::Up, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected.checked_sub(1) {
            self.select(index, cx);
        }
    }

    fn select_next(&mut self, _: &editor::Down, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected + 1 < self.matches.len() {
            self.select(self.selected + 1, cx);
        }
    }

    fn confirm(&mut self, _: &editor::Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.open(self.selected, cx);
    }

    fn open(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(m) = self.matches.get(index) {
            let path = self.project.resolve(&self.files[m.index]);
            cx.emit(FileFinderEvent::Open(path));
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(FileFinderEvent::Dismissed);
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let m = &self.matches[index];
        let path = &self.files[m.index];
        // Local paths use the platform separator, WSL paths `/`.
        let name_start = path.rfind(['/', MAIN_SEPARATOR]).map_or(0, |i| i + 1);
        let highlight = HighlightStyle {
            color: Some(rgb(theme::ACCENT_TEXT).into()),
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        };
        let highlights = |range: Range<usize>| {
            let offset = range.start;
            m.positions
                .iter()
                .filter(move |&&pos| range.contains(&pos))
                .map(move |&pos| {
                    let start = pos - offset;
                    let len = path[pos..].chars().next().map_or(1, char::len_utf8);
                    (start..start + len, highlight)
                })
                .collect::<Vec<_>>()
        };
        let name = SharedString::from(path[name_start..].to_owned());
        let dir = SharedString::from(path[..name_start].to_owned());

        div()
            .id(index)
            .h(px(ROW_HEIGHT))
            .w_full()
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .when(index == self.selected, |row| row.bg(rgb(theme::ACTIVE_ROW)))
            .when(index != self.selected, |row| {
                row.hover(|style| style.bg(rgb(theme::HOVER_ROW)))
            })
            .on_click(cx.listener(move |this, _, _, cx| this.open(index, cx)))
            .child(
                div().text_color(rgb(theme::TEXT)).child(
                    StyledText::new(name).with_highlights(highlights(name_start..path.len())),
                ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(StyledText::new(dir).with_highlights(highlights(0..name_start))),
            )
    }
}

/// Best `MAX_RESULTS` matches for `query`; an empty query lists files in order.
fn rank(files: &[String], query: &str) -> Vec<PathMatch> {
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    if pattern.atoms.is_empty() {
        return (0..files.len().min(MAX_RESULTS))
            .map(|index| PathMatch {
                index,
                positions: Vec::new(),
            })
            .collect();
    }
    let mut config = Config::DEFAULT;
    config.set_match_paths();
    let mut matcher = Matcher::new(config);
    let mut chars = Vec::new();

    let mut scored: Vec<(u32, usize)> = files
        .iter()
        .enumerate()
        .filter_map(|(index, path)| {
            let score = pattern.score(Utf32Str::new(path, &mut chars), &mut matcher)?;
            Some((score, index))
        })
        .collect();
    // Higher score first; ties go to shorter, then alphabetically earlier paths.
    scored.sort_unstable_by(|(a_score, a), (b_score, b)| {
        b_score
            .cmp(a_score)
            .then(files[*a].len().cmp(&files[*b].len()))
            .then(a.cmp(b))
    });
    scored.truncate(MAX_RESULTS);

    let mut char_indices = Vec::new();
    scored
        .into_iter()
        .map(|(_, index)| {
            let path = &files[index];
            char_indices.clear();
            pattern.indices(
                Utf32Str::new(path, &mut chars),
                &mut matcher,
                &mut char_indices,
            );
            char_indices.sort_unstable();
            char_indices.dedup();
            let mut wanted = char_indices.iter().map(|&i| i as usize).peekable();
            let positions = path
                .char_indices()
                .enumerate()
                .filter_map(|(char_index, (byte, _))| wanted.next_if_eq(&char_index).map(|_| byte))
                .collect();
            PathMatch { index, positions }
        })
        .collect()
}

impl Focusable for FileFinder {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for FileFinder {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = if self.indexing {
            "Indexing…".to_owned()
        } else if let Some(error) = &self.error {
            error.clone()
        } else {
            format!("{} of {} files", self.matches.len(), self.files.len())
        };

        div()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .on_mouse_down_out(
                cx.listener(|this, _, window, cx| this.dismiss(&Dismiss, window, cx)),
            )
            .w(px(640.))
            .flex()
            .flex_col()
            .bg(rgb(theme::PANEL))
            .border_1()
            .border_color(rgb(theme::BORDER))
            .rounded_md()
            .shadow_lg()
            .text_sm()
            .child(
                div()
                    .h(px(36.))
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(theme::BORDER))
                    .child(self.query.clone()),
            )
            .child(
                uniform_list(
                    "file-finder-matches",
                    self.matches.len(),
                    cx.processor(|this, range: Range<usize>, _, cx| {
                        range.map(|index| this.render_row(index, cx)).collect()
                    }),
                )
                .track_scroll(self.scroll.clone())
                .h(px(ROW_HEIGHT * 14.)),
            )
            .child(
                div()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(rgb(theme::BORDER))
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(status),
            )
            // Keep clicks inside the modal from reaching the editor below.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_are_byte_offsets_of_matched_chars() {
        // 'é' takes two bytes, so char and byte indices diverge after it.
        let files = ["é/main.rs".to_owned()];
        let matches = rank(&files, "mr");
        assert_eq!(matches[0].positions, [3, 8]);
    }

    #[test]
    fn exact_file_name_outranks_partial_and_nested_matches() {
        let files = [
            "src/deep/other_main.rs",
            "src/domain.rs",
            "main.rs",
            "lib.rs",
        ]
        .map(String::from);
        let ranked: Vec<&str> = rank(&files, "main")
            .iter()
            .map(|m| files[m.index].as_str())
            .collect();
        assert_eq!(ranked[0], "main.rs");
        assert!(!ranked.contains(&"lib.rs"));
    }
}
