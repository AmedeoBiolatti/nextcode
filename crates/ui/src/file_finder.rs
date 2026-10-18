//! Fuzzy file finder: a modal that indexes the project and matches paths as you type.

use std::{
    ops::Range,
    path::{MAIN_SEPARATOR, PathBuf},
    sync::Arc,
};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle,
    KeyBinding, MouseButton, ScrollStrategy, SharedString, Subscription, Task,
    UniformListScrollHandle, Window, actions, div, prelude::*, px, uniform_list,
};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};

use crate::{
    editor::{self, Editor, EditorEvent},
    pinpoint::Pin,
    project::{Project, root_label},
    selectable::selectable,
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

/// An indexed file: matched and shown as `label`, which is the
/// folder-relative path, after `folder › ` when the project has several.
struct FileEntry {
    root: usize,
    label: String,
    /// Where the folder-relative path starts in `label`.
    relative: usize,
}

impl AsRef<str> for FileEntry {
    fn as_ref(&self) -> &str {
        &self.label
    }
}

/// The files of every folder of `project`, in folder order.
fn list_all(project: &Project) -> (Vec<FileEntry>, Option<String>) {
    let multi_root = project.is_multi_root();
    let mut files = Vec::new();
    let mut errors = Vec::new();
    for (root, folder) in project.roots().iter().enumerate() {
        match project.list_files(root) {
            Ok(listed) => files.extend(listed.into_iter().map(|relative| {
                if multi_root {
                    let label = root_label(&folder.name, &relative);
                    FileEntry {
                        root,
                        relative: label.len() - relative.len(),
                        label,
                    }
                } else {
                    FileEntry {
                        root,
                        label: relative,
                        relative: 0,
                    }
                }
            })),
            Err(err) if multi_root => errors.push(format!("{}: {err}", folder.name)),
            Err(err) => errors.push(err.to_string()),
        }
    }
    let error = (!errors.is_empty()).then(|| format!("Cannot list files: {}", errors.join("; ")));
    (files, error)
}

pub struct FileFinder {
    project: Project,
    query: Entity<Editor>,
    /// Every folder's files, sorted within each folder.
    files: Arc<Vec<FileEntry>>,
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
    #[cfg(test)]
    pub fn query(&self) -> &Entity<Editor> {
        &self.query
    }

    pub fn new(project: Project, cx: &mut Context<Self>) -> Self {
        let query = cx.new(Editor::single_line);
        let subscription = cx.subscribe(&query, |this, _, event, cx| {
            if let EditorEvent::Edited = event {
                this.search(cx);
            }
        });
        let walk_project = project.clone();
        let index = cx.spawn(async move |this, cx| {
            let (files, error) = cx
                .background_spawn(async move { list_all(&walk_project) })
                .await;
            this.update(cx, |this, cx| {
                this.files = Arc::new(files);
                this.error = error;
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
            let file = &self.files[m.index];
            let path = self
                .project
                .resolve_in(file.root, &file.label[file.relative..]);
            cx.emit(FileFinderEvent::Open(path));
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(FileFinderEvent::Dismissed);
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let m = &self.matches[index];
        let file = &self.files[m.index];
        let path = &file.label;
        // Local paths use the platform separator, WSL paths `/`; the folder
        // label before the relative path is part of the directory.
        let name_start = path[file.relative..]
            .rfind(['/', MAIN_SEPARATOR])
            .map_or(file.relative, |i| file.relative + i + 1);
        let highlight = HighlightStyle {
            color: Some(theme::accent_text()),
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
        // As `pinpoint::file_key` names the file.
        let relative = path[file.relative..].replace('\\', "/");
        let key = if self.project.is_multi_root() {
            let roots = self.project.roots();
            format!("{}/{relative}", roots[file.root].name)
        } else {
            relative
        };

        div()
            .id(index)
            .h(px(ROW_HEIGHT))
            .w_full()
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .when(index == self.selected, |row| row.bg(theme::active_row()))
            .when(index != self.selected, |row| {
                row.hover(|style| style.bg(theme::hover_row()))
            })
            .on_click(cx.listener(move |this, _, _, cx| this.open(index, cx)))
            .child(div().text_color(theme::text()).child(
                selectable("name", name).with_highlights(highlights(name_start..path.len())),
            ))
            .child(
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(selectable("dir", dir).with_highlights(highlights(0..name_start))),
            )
            .pin(format!("item:{key}"))
            .label(path.clone())
    }
}

/// Best `MAX_RESULTS` matches for `query` among the paths (labels) of
/// `files`; an empty query lists files in order.
fn rank<F: AsRef<str>>(files: &[F], query: &str) -> Vec<PathMatch> {
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
            let score = pattern.score(Utf32Str::new(path.as_ref(), &mut chars), &mut matcher)?;
            Some((score, index))
        })
        .collect();
    // Higher score first; ties go to shorter, then alphabetically earlier paths.
    scored.sort_unstable_by(|(a_score, a), (b_score, b)| {
        b_score
            .cmp(a_score)
            .then(files[*a].as_ref().len().cmp(&files[*b].as_ref().len()))
            .then(a.cmp(b))
    });
    scored.truncate(MAX_RESULTS);

    let mut char_indices = Vec::new();
    scored
        .into_iter()
        .map(|(_, index)| {
            let path = files[index].as_ref();
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
            .bg(theme::panel())
            .border_1()
            .border_color(theme::border())
            .rounded_md()
            .shadow_lg()
            .text_sm()
            .child(
                div()
                    .h(px(36.))
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(self.query.clone())
                    .pin("input")
                    .label("File name"),
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
                    .border_color(theme::border())
                    .text_xs()
                    .text_color(theme::muted())
                    .child(selectable("status", status))
                    .pin("status")
                    .interactive(false),
            )
            // Keep clicks inside the modal from reaching the editor below.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .pin("file-finder")
            .label("Go to File")
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

    /// A two-folder workspace over fresh `api` and `web` temp dirs.
    fn two_roots(name: &str) -> (PathBuf, Project) {
        let base =
            std::env::temp_dir().join(format!("next_code_finder_{name}_{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let folders = ["api", "web"].map(|folder| base.join(folder));
        for folder in &folders {
            std::fs::create_dir_all(folder.join("src")).unwrap();
            std::fs::write(folder.join("src").join("main.rs"), "").unwrap();
            std::fs::write(folder.join("README.md"), "").unwrap();
        }
        let project = Project::local_folders(&[&folders[0], &folders[1]]);
        (base, project)
    }

    #[test]
    fn files_of_every_folder_are_labelled_with_it_and_resolve_there() {
        let (base, project) = two_roots("index");
        let (files, error) = list_all(&project);
        assert_eq!(error, None);
        let main = format!("src{MAIN_SEPARATOR}main.rs");
        let labels: Vec<&str> = files.iter().map(|file| file.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "api › README.md".to_owned(),
                format!("api › {main}"),
                "web › README.md".to_owned(),
                format!("web › {main}"),
            ]
        );
        let web_main = &files[3];
        assert_eq!(&web_main.label[web_main.relative..], main);
        assert_eq!(
            project.resolve_in(web_main.root, &web_main.label[web_main.relative..]),
            base.join("web").join("src").join("main.rs")
        );
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn the_folder_name_narrows_a_multi_root_query() {
        let files = [
            "api › src/main.rs",
            "api › README.md",
            "web › src/main.rs",
            "web › README.md",
        ];
        let ranked =
            |query| -> Vec<&str> { rank(&files, query).iter().map(|m| files[m.index]).collect() };
        assert_eq!(ranked("main"), ["api › src/main.rs", "web › src/main.rs"]);
        assert_eq!(ranked("web main"), ["web › src/main.rs"]);
        assert_eq!(ranked("webmain")[0], "web › src/main.rs");
        assert!(!ranked("webmain").contains(&"api › src/main.rs"));
    }
}
