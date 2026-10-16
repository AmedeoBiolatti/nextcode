//! Modal symbol pickers: Go to Symbol in File (`Ctrl+Shift+O`), Go to
//! Symbol in Workspace (`Ctrl+T`), and the breadcrumbs' lists (a symbol's
//! siblings, a folder's entries). Fuzzy-filtered with nucleo like the file
//! finder; workspace symbols come from `workspace/symbol` per query.

use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle, Hsla,
    KeyBinding, MouseButton, ScrollStrategy, SharedString, Subscription, Task,
    UniformListScrollHandle, Window, actions, div, prelude::*, px, uniform_list,
};
use language::{Location, symbols as model};
use lsp::types::{WorkspaceSymbolParams, request::WorkspaceSymbolRequest};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};
use syntax::Language;

use crate::{
    editor::{self, Editor, EditorEvent, SymbolsStatus, symbol_kind_badge},
    lsp_store::{RunningServer, request_retrying},
    project::Project,
    selectable::selectable,
    theme,
};

actions!(symbol_picker, [Dismiss]);

const CONTEXT: &str = "SymbolPicker";
const ROW_HEIGHT: f32 = 28.;
/// Pause after typing before asking servers for workspace symbols.
const WORKSPACE_DEBOUNCE: Duration = Duration::from_millis(150);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Dismiss, Some(CONTEXT))]);
}

pub enum SymbolPickerEvent {
    /// Move `editor`'s cursor to `offset` (a symbol of its file).
    Reveal {
        editor: Entity<Editor>,
        offset: usize,
    },
    /// Open a workspace symbol (range in its server's coordinates).
    Open(Location),
    OpenFile(PathBuf),
    Dismissed,
}

impl EventEmitter<SymbolPickerEvent> for SymbolPicker {}

/// Where the list comes from.
enum Source {
    Document {
        editor: Entity<Editor>,
        /// Breadcrumb: only this symbol's siblings.
        siblings_of: Option<usize>,
        generation: u64,
    },
    Workspace {
        servers: Vec<(Language, Arc<RunningServer>)>,
        project: Project,
    },
    Directory {
        project: Project,
        /// `None`: the folders of a multi-root project.
        dir: Option<PathBuf>,
    },
}

#[derive(Clone)]
enum Target {
    Offset(usize),
    Location(Location),
    File(PathBuf),
    Dir(PathBuf),
    /// Up from a folder of a multi-root project to the list of folders.
    Folders,
}

struct Item {
    badge: &'static str,
    color: Hsla,
    name: String,
    /// Tree depth, indented while the list is unfiltered.
    depth: usize,
    /// Symbol detail or container, muted after the name.
    detail: SharedString,
    /// Right-aligned: line, path.
    hint: SharedString,
    target: Target,
}

/// A ranked item with the byte offsets of the matched characters.
#[derive(Debug, PartialEq, Eq)]
struct Match {
    index: usize,
    positions: Vec<usize>,
}

pub struct SymbolPicker {
    title: SharedString,
    query: Entity<Editor>,
    source: Source,
    items: Vec<Item>,
    matches: Vec<Match>,
    selected: usize,
    /// The list is in tree order (unfiltered file symbols): indent rows.
    indent: bool,
    /// Shown instead of the counts (loading, errors, empty states).
    status: Option<SharedString>,
    scroll: UniformListScrollHandle,
    _load: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl SymbolPicker {
    fn new(title: &str, source: Source, cx: &mut Context<Self>) -> Self {
        let query = cx.new(Editor::single_line);
        let subscription = cx.subscribe(&query, |this, _, event, cx| {
            if let EditorEvent::Edited = event {
                this.query_changed(cx);
            }
        });
        Self {
            title: title.to_owned().into(),
            query,
            source,
            items: Vec::new(),
            matches: Vec::new(),
            selected: 0,
            indent: false,
            status: None,
            scroll: UniformListScrollHandle::new(),
            _load: Task::ready(()),
            _subscriptions: vec![subscription],
        }
    }

    /// The symbols of `editor`'s file, or (from a breadcrumb) the siblings
    /// of symbol `siblings_of`. Fills in when the symbols arrive.
    pub fn document(
        editor: Entity<Editor>,
        siblings_of: Option<usize>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&editor, |this, editor, cx| {
            let generation = editor.read(cx).symbols_generation();
            // A breadcrumb's sibling list is a snapshot (its symbol index
            // is only valid for the list it came from).
            if let Source::Document {
                generation: seen,
                siblings_of: None,
                ..
            } = &this.source
                && *seen != generation
            {
                this.load_document(cx);
            }
        });
        let title = if siblings_of.is_some() {
            "Symbols"
        } else {
            "Go to Symbol in File"
        };
        let mut picker = Self::new(
            title,
            Source::Document {
                editor,
                siblings_of,
                generation: u64::MAX,
            },
            cx,
        );
        picker._subscriptions.push(observe);
        picker.load_document(cx);
        picker
    }

    /// `workspace/symbol` on `servers` as the user types.
    pub fn workspace(
        servers: Vec<(Language, Arc<RunningServer>)>,
        project: Project,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut picker = Self::new(
            "Go to Symbol in Workspace",
            Source::Workspace { servers, project },
            cx,
        );
        picker.status = Some(if picker.workspace_servers().is_empty() {
            "No running language server lists workspace symbols".into()
        } else {
            "Type to search symbols".into()
        });
        picker
    }

    /// The entries of `dir` (through the project, so remote projects
    /// work), `selected` preselected.
    pub fn directory(
        project: Project,
        dir: PathBuf,
        selected: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut picker = Self::new(
            "Files",
            Source::Directory {
                project,
                dir: Some(dir),
            },
            cx,
        );
        picker.load_directory(selected, cx);
        picker
    }

    fn workspace_servers(&self) -> Vec<(Language, Arc<RunningServer>)> {
        match &self.source {
            Source::Workspace { servers, .. } => servers
                .iter()
                .filter(|(_, server)| {
                    server
                        .capabilities
                        .workspace_symbol_provider
                        .as_ref()
                        .is_some_and(|provider| !matches!(provider, lsp::types::OneOf::Left(false)))
                })
                .cloned()
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Re-reads the editor's symbols, keeping the selected one.
    fn load_document(&mut self, cx: &mut Context<Self>) {
        let Source::Document {
            editor,
            siblings_of,
            generation,
        } = &mut self.source
        else {
            return;
        };
        let editor = editor.read(cx);
        *generation = editor.symbols_generation();
        let symbols = editor.symbols();
        let language = editor.language();
        let siblings_of = *siblings_of;
        let indices: Vec<usize> = match siblings_of {
            Some(index) if index < symbols.len() => model::siblings(symbols, index),
            _ => (0..symbols.len()).collect(),
        };
        let selected_offset = siblings_of
            .and_then(|index| symbols.get(index))
            .map(|symbol| symbol.selection_range.start);
        self.items = indices
            .into_iter()
            .map(|index| {
                let symbol = &symbols[index];
                let (badge, color) = symbol_kind_badge(symbol.kind, language);
                let detail = symbol
                    .detail
                    .clone()
                    .or_else(|| symbol.parent.map(|parent| symbols[parent].name.clone()))
                    .unwrap_or_default();
                let line = editor.buffer().line_of(symbol.selection_range.start) + 1;
                Item {
                    badge,
                    color,
                    name: symbol.name.clone(),
                    depth: symbol.depth,
                    detail: detail.lines().next().unwrap_or_default().to_owned().into(),
                    hint: format!("{line}").into(),
                    target: Target::Offset(symbol.selection_range.start),
                }
            })
            .collect();
        self.status = match editor.symbols_status() {
            SymbolsStatus::Loading => Some("Loading symbols…".into()),
            SymbolsStatus::NoServer => Some("No language server for this file".into()),
            SymbolsStatus::Unsupported => Some("The language server does not list symbols".into()),
            SymbolsStatus::Ready if self.items.is_empty() => Some("No symbols".into()),
            SymbolsStatus::Ready => None,
        };
        let selected = selected_offset.and_then(|offset| {
            self.items
                .iter()
                .position(|item| matches!(item.target, Target::Offset(o) if o == offset))
        });
        self.filter(cx);
        if let Some(index) = selected.filter(|_| self.query.read(cx).text().is_empty()) {
            self.select(index, cx);
        }
    }

    /// Lists the directory off the main thread (it may go through the
    /// agent).
    fn load_directory(&mut self, selected: Option<String>, cx: &mut Context<Self>) {
        let Source::Directory { project, dir } = &self.source else {
            return;
        };
        let (project, dir) = (project.clone(), dir.clone());
        self.items.clear();
        self.matches.clear();
        let Some(dir) = dir else {
            self._load = Task::ready(());
            self.items = project
                .roots()
                .iter()
                .map(|root| Item {
                    badge: "▸",
                    color: theme::muted(),
                    name: root.name.clone(),
                    depth: 0,
                    detail: SharedString::default(),
                    hint: root.path.display().to_string().into(),
                    target: Target::Dir(root.path.clone()),
                })
                .collect();
            self.status = None;
            self.title = "Folders".into();
            self.filter(cx);
            if let Some(index) =
                selected.and_then(|name| self.items.iter().position(|item| item.name == name))
            {
                self.select(index, cx);
            }
            return cx.notify();
        };
        self.status = Some("Loading…".into());
        cx.notify();
        self._load = cx.spawn(async move |this, cx| {
            let list_project = project.clone();
            let list_dir = dir.clone();
            let entries = cx
                .background_spawn(async move { list_project.read_dir(&list_dir) })
                .await;
            this.update(cx, |this, cx| {
                let entries = match entries {
                    Ok(entries) => entries,
                    Err(err) => {
                        this.status = Some(format!("Cannot list {}: {err}", dir.display()).into());
                        return cx.notify();
                    }
                };
                let mut items = Vec::new();
                let up = |target| Item {
                    badge: "▴",
                    color: theme::muted(),
                    name: "..".into(),
                    depth: 0,
                    detail: SharedString::default(),
                    hint: SharedString::default(),
                    target,
                };
                // Up within the folder holding `dir`, then to the folders.
                let root = &project.root_for(&dir).path;
                if dir != *root
                    && let Some(parent) = dir.parent().filter(|parent| parent.starts_with(root))
                {
                    items.push(up(Target::Dir(parent.to_path_buf())));
                } else if project.is_multi_root() && project.root_index_of(&dir).is_some() {
                    items.push(up(Target::Folders));
                }
                items.extend(entries.into_iter().map(|entry| {
                    let path = project.join(&dir, &entry.name);
                    let (badge, target) = if entry.is_dir {
                        ("▸", Target::Dir(path))
                    } else {
                        ("·", Target::File(path))
                    };
                    Item {
                        badge,
                        color: theme::muted(),
                        name: entry.name,
                        depth: 0,
                        detail: SharedString::default(),
                        hint: SharedString::default(),
                        target,
                    }
                }));
                this.status = items.is_empty().then(|| "Empty folder".into());
                this.items = items;
                this.title = path_label(&project, &dir).into();
                this.filter(cx);
                if let Some(index) =
                    selected.and_then(|name| this.items.iter().position(|item| item.name == name))
                {
                    this.select(index, cx);
                }
            })
            .ok();
        });
    }

    fn query_changed(&mut self, cx: &mut Context<Self>) {
        if matches!(self.source, Source::Workspace { .. }) {
            self.search_workspace(cx);
        } else {
            self.filter(cx);
        }
    }

    /// Sends the query to every server after a pause; a newer query
    /// replaces (cancels) the pending one, so stale answers are dropped.
    fn search_workspace(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text();
        let servers = self.workspace_servers();
        let Source::Workspace { project, .. } = &self.source else {
            return;
        };
        let project = project.clone();
        if query.trim().is_empty() || servers.is_empty() {
            self._load = Task::ready(());
            self.items.clear();
            self.matches.clear();
            self.status = Some(if servers.is_empty() {
                "No running language server lists workspace symbols".into()
            } else {
                "Type to search symbols".into()
            });
            return cx.notify();
        }
        self._load = cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            executor.timer(WORKSPACE_DEBOUNCE).await;
            this.update(cx, |this, cx| {
                this.status = Some("Searching…".into());
                cx.notify();
            })
            .ok();
            let params = WorkspaceSymbolParams {
                query: query.clone(),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            let requests = servers.iter().map(|(_, server)| {
                request_retrying::<WorkspaceSymbolRequest>(
                    server,
                    params.clone(),
                    &executor,
                    1,
                    Duration::from_millis(400),
                )
            });
            let results = futures::future::join_all(requests).await;
            this.update(cx, |this, cx| {
                let mut items = Vec::new();
                let mut errors = Vec::new();
                for ((language, _), result) in servers.iter().zip(results) {
                    match result {
                        Ok(response) => items.extend(
                            response
                                .map(model::workspace_symbols)
                                .unwrap_or_default()
                                .into_iter()
                                .map(|symbol| workspace_item(symbol, &project, *language)),
                        ),
                        Err(err) => errors.push(err.to_string()),
                    }
                }
                this.status = if !errors.is_empty() && items.is_empty() {
                    Some(format!("Workspace symbols failed: {}", errors.join("; ")).into())
                } else if items.is_empty() {
                    Some("No symbols found".into())
                } else {
                    None
                };
                this.items = items;
                this.filter(cx);
            })
            .ok();
        });
    }

    /// Ranks the items against the query.
    fn filter(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text();
        let names: Vec<&str> = self.items.iter().map(|item| item.name.as_str()).collect();
        // Servers match workspace queries their own way: keep their
        // results, ours first.
        let keep_unmatched = matches!(self.source, Source::Workspace { .. });
        self.matches = rank(&names, &query, keep_unmatched);
        self.indent = query.trim().is_empty()
            && matches!(
                self.source,
                Source::Document {
                    siblings_of: None,
                    ..
                }
            );
        self.selected = 0;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
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
        self.choose(self.selected, cx);
    }

    fn choose(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(item) = self.matches.get(row).map(|m| &self.items[m.index]) else {
            return;
        };
        match item.target.clone() {
            Target::Offset(offset) => {
                if let Source::Document { editor, .. } = &self.source {
                    cx.emit(SymbolPickerEvent::Reveal {
                        editor: editor.clone(),
                        offset,
                    });
                }
            }
            Target::Location(location) => cx.emit(SymbolPickerEvent::Open(location)),
            Target::File(path) => cx.emit(SymbolPickerEvent::OpenFile(path)),
            Target::Dir(path) => {
                if let Source::Directory { dir, .. } = &mut self.source {
                    *dir = Some(path);
                }
                self.query.update(cx, |query, cx| query.set_text("", cx));
                self.load_directory(None, cx);
            }
            Target::Folders => {
                // Preselect the folder we leave.
                let mut left = None;
                if let Source::Directory { project, dir } = &mut self.source {
                    left = dir.take().map(|dir| project.root_for(&dir).name.clone());
                }
                self.query.update(cx, |query, cx| query.set_text("", cx));
                self.load_directory(left, cx);
            }
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(SymbolPickerEvent::Dismissed);
    }

    fn render_row(&self, row: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let m = &self.matches[row];
        let item = &self.items[m.index];
        let highlight = HighlightStyle {
            color: Some(theme::accent_text()),
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        };
        let highlights: Vec<_> = m
            .positions
            .iter()
            .map(|&pos| {
                let len = item.name[pos..].chars().next().map_or(1, char::len_utf8);
                (pos..pos + len, highlight)
            })
            .collect();
        div()
            .id(row)
            .h(px(ROW_HEIGHT))
            .w_full()
            .pr_3()
            .pl(px(12.
                + if self.indent {
                    14. * item.depth as f32
                } else {
                    0.
                }))
            .flex()
            .items_center()
            .gap_2()
            .overflow_hidden()
            .whitespace_nowrap()
            .cursor_pointer()
            .when(row == self.selected, |row| row.bg(theme::active_row()))
            .when(row != self.selected, |row| {
                row.hover(|style| style.bg(theme::hover_row()))
            })
            .on_click(cx.listener(move |this, _, _, cx| this.choose(row, cx)))
            .child(
                div()
                    .w(px(16.))
                    .flex_none()
                    .text_xs()
                    .text_color(item.color)
                    .child(item.badge),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(theme::text())
                    .child(selectable("name", item.name.clone()).with_highlights(highlights)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(selectable("detail", item.detail.clone())),
            )
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(selectable("hint", item.hint.clone())),
            )
    }
}

/// `path` relative to `root` when inside it, `/`-separated.
fn relative(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(inside) if inside.as_os_str().is_empty() => ".".to_owned(),
        Ok(inside) => inside
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => path.display().to_string(),
    }
}

/// How the picker names `path`: relative to the root, after its folder's
/// name (`api › src`) when the project has several.
fn path_label(project: &Project, path: &Path) -> String {
    if project.is_multi_root() {
        project.display_path(path)
    } else {
        relative(project.root(), path)
    }
}

fn workspace_item(
    symbol: model::WorkspaceSymbolItem,
    project: &Project,
    language: Language,
) -> Item {
    let (badge, color) = symbol_kind_badge(symbol.kind, Some(language));
    let line = symbol
        .range
        .map(|range| format!(":{}", range.start.line + 1))
        .unwrap_or_default();
    Item {
        badge,
        color,
        hint: format!("{}{line}", path_label(project, &symbol.path)).into(),
        detail: symbol.container.unwrap_or_default().into(),
        name: symbol.name,
        depth: 0,
        target: Target::Location(Location {
            path: symbol.path,
            range: symbol.range.unwrap_or_default(),
        }),
    }
}

/// Fuzzy-ranks `names` against `query`: best score first, ties to the
/// shorter name, then list order. An empty query keeps the list order;
/// `keep_unmatched` appends the names the query does not match, in order.
fn rank(names: &[&str], query: &str, keep_unmatched: bool) -> Vec<Match> {
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let all = || {
        (0..names.len()).map(|index| Match {
            index,
            positions: Vec::new(),
        })
    };
    if pattern.atoms.is_empty() {
        return all().collect();
    }
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut chars = Vec::new();
    let mut scored: Vec<(u32, usize)> = names
        .iter()
        .enumerate()
        .filter_map(|(index, name)| {
            Some((
                pattern.score(Utf32Str::new(name, &mut chars), &mut matcher)?,
                index,
            ))
        })
        .collect();
    scored.sort_by(|(a_score, a), (b_score, b)| {
        b_score
            .cmp(a_score)
            .then(names[*a].len().cmp(&names[*b].len()))
            .then(a.cmp(b))
    });
    let mut char_indices = Vec::new();
    let mut matches: Vec<Match> = scored
        .iter()
        .map(|&(_, index)| {
            let name = names[index];
            char_indices.clear();
            pattern.indices(
                Utf32Str::new(name, &mut chars),
                &mut matcher,
                &mut char_indices,
            );
            char_indices.sort_unstable();
            char_indices.dedup();
            let mut wanted = char_indices.iter().map(|&i| i as usize).peekable();
            let positions = name
                .char_indices()
                .enumerate()
                .filter_map(|(char_index, (byte, _))| wanted.next_if_eq(&char_index).map(|_| byte))
                .collect();
            Match { index, positions }
        })
        .collect();
    if keep_unmatched {
        let matched: std::collections::HashSet<usize> =
            scored.iter().map(|&(_, index)| index).collect();
        matches.extend(all().filter(|m| !matched.contains(&m.index)));
    }
    matches
}

impl Focusable for SymbolPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for SymbolPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self
            .status
            .clone()
            .unwrap_or_else(|| format!("{} of {}", self.matches.len(), self.items.len()).into());
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
                    .child(self.query.clone()),
            )
            .child(
                uniform_list(
                    "symbol-picker-matches",
                    self.matches.len(),
                    cx.processor(|this, range: Range<usize>, _, cx| {
                        range.map(|row| this.render_row(row, cx)).collect()
                    }),
                )
                .track_scroll(self.scroll.clone())
                .h(px(ROW_HEIGHT * 14.)),
            )
            .child(
                div()
                    .px_3()
                    .py_1()
                    .flex()
                    .justify_between()
                    .gap_4()
                    .border_t_1()
                    .border_color(theme::border())
                    .text_xs()
                    .text_color(theme::muted())
                    .child(
                        div()
                            .truncate()
                            .child(selectable("title", self.title.clone())),
                    )
                    .child(div().flex_none().child(selectable("status", status))),
            )
            // Keep clicks inside the modal from reaching the editor below.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(matches: &[Match]) -> Vec<usize> {
        matches.iter().map(|m| m.index).collect()
    }

    #[test]
    fn rank_orders_by_score_then_length() {
        let names = ["render_row", "new", "render"];
        // Empty query: list order.
        assert_eq!(order(&rank(&names, "", false)), [0, 1, 2]);
        // The exact name first, the longer one after, non-matches gone.
        let ranked = rank(&names, "render", false);
        assert_eq!(order(&ranked), [2, 0]);
        assert_eq!(ranked[0].positions, [0, 1, 2, 3, 4, 5]);
        // Workspace results the server matched its own way stay, last.
        assert_eq!(order(&rank(&names, "render", true)), [2, 0, 1]);
        // Smart case: an upper-case query letter must match exactly.
        assert_eq!(order(&rank(&["Render", "render"], "Ren", false)), [0]);
    }

    #[test]
    fn relative_paths_use_slashes() {
        let root = Path::new("/home/me/project");
        assert_eq!(
            relative(root, Path::new("/home/me/project/src/a.rs")),
            "src/a.rs"
        );
        assert_eq!(relative(root, root), ".");
        assert_eq!(relative(root, Path::new("/usr/lib/x.rs")), "/usr/lib/x.rs");
    }

    #[test]
    fn multi_root_paths_are_labelled_with_their_folder() {
        let (api, web) = (PathBuf::from("/ws/api"), PathBuf::from("/ws/web"));
        let project = Project::local_folders(&[&api, &web]);
        assert_eq!(
            path_label(&project, &web.join("src").join("a.rs")),
            "web › src/a.rs"
        );
        assert_eq!(path_label(&project, &api), "api");
        let single = Project::local(api.clone());
        assert_eq!(
            path_label(&single, &api.join("src").join("a.rs")),
            "src/a.rs"
        );
        assert_eq!(path_label(&single, &api), ".");
    }

    #[gpui::test]
    fn folders_are_the_top_level_of_a_multi_root_directory_list(cx: &mut gpui::TestAppContext) {
        let base =
            std::env::temp_dir().join(format!("next_code_symbol_dirs_{}", std::process::id()));
        let (api, web) = (base.join("api"), base.join("web"));
        std::fs::create_dir_all(api.join("src")).unwrap();
        std::fs::create_dir_all(&web).unwrap();
        std::fs::write(web.join("index.ts"), "").unwrap();
        let project = Project::local_folders(&[&api, &web]);
        let picker = cx.new(|cx| SymbolPicker::directory(project, api.join("src"), None, cx));
        cx.run_until_parked();
        let shown = |cx: &mut gpui::TestAppContext| {
            picker.read_with(cx, |picker, _| {
                let names = picker
                    .matches
                    .iter()
                    .map(|m| picker.items[m.index].name.clone());
                (
                    picker.title.to_string(),
                    names.collect::<Vec<_>>(),
                    picker.selected,
                )
            })
        };
        assert_eq!(shown(cx), ("api › src".into(), vec!["..".to_owned()], 0));

        // Up to the folder, then to the list of folders (the left one selected).
        picker.update(cx, |picker, cx| picker.choose(0, cx));
        cx.run_until_parked();
        assert_eq!(
            shown(cx),
            ("api".into(), vec!["..".into(), "src".into()], 0)
        );
        picker.update(cx, |picker, cx| picker.choose(0, cx));
        cx.run_until_parked();
        assert_eq!(
            shown(cx),
            ("Folders".into(), vec!["api".into(), "web".into()], 0)
        );

        // Into the other folder.
        picker.update(cx, |picker, cx| picker.choose(1, cx));
        cx.run_until_parked();
        assert_eq!(
            shown(cx),
            ("web".into(), vec!["..".into(), "index.ts".into()], 0)
        );
        picker.update(cx, |picker, cx| picker.choose(0, cx));
        cx.run_until_parked();
        assert_eq!(
            shown(cx),
            ("Folders".into(), vec!["api".into(), "web".into()], 1)
        );
        std::fs::remove_dir_all(base).ok();
    }
}
