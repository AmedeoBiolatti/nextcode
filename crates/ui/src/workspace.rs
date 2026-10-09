//! Main window: docks with panels, editor tabs, and close/save confirmation.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use futures::StreamExt;

use gpui::{
    App, Context, Entity, FocusHandle, Focusable, KeyBinding, PromptLevel, SharedString,
    Subscription, Task, Window, actions, div, prelude::*, px, rgb,
};
use language::Location;
use syntax::Language;
use text::Buffer;
use watcher::{Batch, ChangeKind, ProjectWatcher};

use crate::{
    editor::{Editor, EditorEvent, ReloadOutcome},
    file_finder::{FileFinder, FileFinderEvent},
    file_tree::{FileTree, OpenFile},
    layout::{DockPosition, Layout},
    location_picker::{LocationPicker, LocationPickerEvent},
    lsp_store::{LspStore, LspStoreEvent, ServerStatus},
    problems::{ProblemsEvent, ProblemsPanel},
    search_panel::{SearchPanel, SearchPanelEvent},
    settings::Settings,
    terminal_panel::TerminalPanel,
    theme,
};

mod docks;

actions!(workspace, [CloseTab, ToggleFileFinder, GoBack, GoForward]);

/// Positions kept for Back/Forward.
const HISTORY_LIMIT: usize = 100;

pub fn bind_keys(cx: &mut App) {
    let ctx = Some("Workspace");
    cx.bind_keys([
        KeyBinding::new("secondary-w", CloseTab, ctx),
        KeyBinding::new("secondary-p", ToggleFileFinder, ctx),
        // VS Code and JetBrains defaults.
        KeyBinding::new("alt-left", GoBack, ctx),
        KeyBinding::new("alt-right", GoForward, ctx),
        KeyBinding::new("secondary-alt-left", GoBack, ctx),
        KeyBinding::new("secondary-alt-right", GoForward, ctx),
    ]);
    docks::bind_keys(cx);
}



/// A remembered cursor position for Back/Forward.
struct NavEntry {
    path: PathBuf,
    offset: usize,
}

struct Tab {
    editor: Entity<Editor>,
    path: PathBuf,
    title: SharedString,
    _subscription: Subscription,
}

pub struct Workspace {
    focus_handle: FocusHandle,
    root: PathBuf,
    file_tree: Entity<FileTree>,
    problems: Entity<ProblemsPanel>,
    terminal: Entity<TerminalPanel>,
    search: Entity<SearchPanel>,
    layout: Layout,
    /// Dock being resized by dragging its handle.
    resizing: Option<DockPosition>,
    panel_menu: Option<docks::PanelMenu>,
    file_finder: Option<(Entity<FileFinder>, Subscription)>,
    location_picker: Option<(Entity<LocationPicker>, Subscription)>,
    back: Vec<NavEntry>,
    forward: Vec<NavEntry>,
    lsp_store: Entity<LspStore>,
    tabs: Vec<Tab>,
    active: usize,
    notice: Option<SharedString>,
    /// Set once the user confirmed discarding unsaved changes on window close.
    close_confirmed: bool,
    /// Files with an open "changed on disk" prompt, so it is asked once.
    reload_prompts: HashSet<PathBuf>,
    _watcher: Option<(ProjectWatcher, Task<()>)>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let file_tree = cx.new(|_| FileTree::new(root.clone()));
        let tree_subscription = cx.subscribe_in(
            &file_tree,
            window,
            |this, _, event: &OpenFile, window, cx| {
                this.open(&event.0, window, cx);
            },
        );

        let (settings, settings_error) = Settings::load();
        let lsp_store = cx.new(|_| LspStore::new(root.clone(), Arc::new(settings)));
        let lsp_subscription = cx.subscribe(&lsp_store, |this, _, event, cx| {
            this.on_lsp_event(event, cx);
        });
        let problems = cx.new(|cx| ProblemsPanel::new(root.clone(), lsp_store.clone(), cx));
        let terminal = cx.new(|cx| TerminalPanel::new(root.clone(), cx));
        let search = cx.new(|cx| SearchPanel::new(root.clone(), cx));
        let search_subscription =
            cx.subscribe_in(&search, window, |this, _, event, window, cx| match event {
                SearchPanelEvent::Open { path, line, column } => {
                    this.record_position(cx);
                    this.forward.clear();
                    if let Some(editor) = this.open_editor(path, window, cx) {
                        editor.update(cx, |editor, cx| editor.go_to_line(*line, *column, cx));
                    }
                }
            });
        let problems_subscription = cx.subscribe_in(
            &problems,
            window,
            |this, _, event, window, cx| match event {
                ProblemsEvent::Open(location) => this.jump_to(location, window, cx),
            },
        );

        let workspace = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            workspace
                .update(cx, |this, cx| this.should_close(window, cx))
                .unwrap_or(true)
        });

        // Workspace key bindings (e.g. Ctrl+P) need focus inside the workspace.
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        let layout = Layout::load(&root);
        let mut notice = settings_error.map(|error| format!("Settings ignored: {error}").into());
        let watcher = match watcher::watch(&root, Duration::from_millis(150)) {
            Ok((watcher, mut batches)) => {
                let task = cx.spawn_in(window, async move |this, cx| {
                    while let Some(batch) = batches.next().await {
                        if this
                            .update_in(cx, |this, window, cx| {
                                this.on_files_changed(batch, window, cx)
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                });
                Some((watcher, task))
            }
            Err(err) => {
                notice = Some(format!("File watching unavailable: {err}").into());
                None
            }
        };
        Self {
            focus_handle,
            root,
            file_tree,
            problems,
            terminal,
            search,
            layout,
            resizing: None,
            panel_menu: None,
            file_finder: None,
            location_picker: None,
            back: Vec::new(),
            forward: Vec::new(),
            lsp_store,
            tabs: Vec::new(),
            active: 0,
            notice,
            close_confirmed: false,
            reload_prompts: HashSet::new(),
            _watcher: watcher,
            _subscriptions: vec![
                tree_subscription,
                lsp_subscription,
                problems_subscription,
                search_subscription,
            ],
        }
    }

    /// Applies external file changes: refreshes the tree, informs language
    /// servers, and reloads open files (asking first if they have edits).
    fn on_files_changed(&mut self, batch: Batch, window: &mut Window, cx: &mut Context<Self>) {
        let structural = batch
            .events
            .iter()
            .filter(|event| event.kind != ChangeKind::Modified)
            .map(|event| event.path.as_path());
        self.file_tree
            .update(cx, |tree, cx| tree.refresh(structural, batch.rescan, cx));
        self.lsp_store.read(cx).notify_file_changes(&batch.events);

        let affected: Vec<(Entity<Editor>, PathBuf, bool)> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let event = batch.events.iter().find(|event| event.path == tab.path);
                match event {
                    Some(event) => Some((
                        tab.editor.clone(),
                        tab.path.clone(),
                        event.kind == ChangeKind::Removed,
                    )),
                    None if batch.rescan => {
                        Some((tab.editor.clone(), tab.path.clone(), !tab.path.exists()))
                    }
                    None => None,
                }
            })
            .collect();
        for (editor, path, removed) in affected {
            if removed {
                self.notice = Some(format!("{} was deleted on disk", path.display()).into());
                cx.notify();
                continue;
            }
            let outcome = editor.update(cx, |editor, cx| editor.reload_from_disk(false, cx));
            if let ReloadOutcome::Conflict = outcome
                && self.reload_prompts.insert(path.clone())
            {
                self.prompt_reload(editor, path, window, cx);
            }
        }
    }

    fn prompt_reload(
        &mut self,
        editor: Entity<Editor>,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("{name} changed on disk"),
            Some("You have unsaved changes. Reload the file from disk and lose them?"),
            &["Keep My Changes", "Reload"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let reload = answer.await == Ok(1);
            this.update_in(cx, |this, _, cx| {
                this.reload_prompts.remove(&path);
                if reload {
                    editor.update(cx, |editor, cx| editor.reload_from_disk(true, cx));
                }
            })
            .ok();
        })
        .detach();
    }

    fn on_lsp_event(&mut self, event: &LspStoreEvent, cx: &mut Context<Self>) {
        match event {
            &LspStoreEvent::ServerStarted(language) => {
                let Some(server) = self
                    .lsp_store
                    .update(cx, |store, cx| store.server(language, cx))
                else {
                    return;
                };
                for editor in self.editors_for(language, cx) {
                    editor.update(cx, |editor, _| editor.attach_server(server.clone()));
                }
            }
            &LspStoreEvent::ServerStopped(language) => {
                for editor in self.editors_for(language, cx) {
                    editor.update(cx, |editor, _| editor.detach_server(false));
                }
            }
            &LspStoreEvent::StatusChanged(language) => {
                let status = self.lsp_status(language, cx);
                for editor in self.editors_for(language, cx) {
                    editor.update(cx, |editor, cx| editor.set_lsp_status(status.clone(), cx));
                }
            }
            LspStoreEvent::DiagnosticsUpdated(path) => self.refresh_diagnostics(path, cx),
        }
    }

    fn refresh_diagnostics(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter().find(|tab| tab.path == path) else {
            return;
        };
        let store = self.lsp_store.read(cx);
        let diagnostics = store.diagnostics(path).to_vec();
        tab.editor
            .update(cx, |editor, cx| editor.set_diagnostics(&diagnostics, cx));
    }

    fn editors_for(&self, language: Language, cx: &App) -> Vec<Entity<Editor>> {
        self.tabs
            .iter()
            .filter(|tab| tab.editor.read(cx).language() == Some(language))
            .map(|tab| tab.editor.clone())
            .collect()
    }

    /// Status-bar text and color for `language`'s server.
    fn lsp_status(&self, language: Language, cx: &App) -> Option<(SharedString, u32)> {
        let (name, status) = self.lsp_store.read(cx).status(language)?;
        Some(match status {
            ServerStatus::Starting => (format!("{name}: starting…").into(), theme::MUTED),
            ServerStatus::Ready => (name, theme::MUTED),
            ServerStatus::Busy(work) => (format!("{name}: {work}").into(), theme::MUTED),
            ServerStatus::Disabled => (format!("{name}: disabled").into(), theme::MUTED),
            ServerStatus::NotFound {
                command,
                install_hint,
            } => (
                match install_hint {
                    Some(hint) => format!("{command} not found (install: {hint})"),
                    None => format!("{command} not found"),
                }
                .into(),
                theme::ERROR,
            ),
            ServerStatus::Failed(error) => (format!("{name}: {error}").into(), theme::ERROR),
        })
    }

    pub fn open(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.tabs.iter().position(|tab| tab.path == path) {
            self.activate(index, window, cx);
            return;
        }
        let buffer = match Buffer::open(path) {
            Ok(buffer) => buffer,
            Err(err) => {
                self.notice = Some(format!("Cannot open {}: {err}", path.display()).into());
                cx.notify();
                return;
            }
        };
        let editor = cx.new(|cx| Editor::new(buffer, cx));
        let subscription =
            cx.subscribe_in(&editor, window, |this, _, event, window, cx| match event {
                EditorEvent::Navigate { locations, title } => {
                    this.navigate(locations.clone(), title.clone(), window, cx);
                }
                EditorEvent::DirtyChanged | EditorEvent::Edited => cx.notify(),
            });
        if let Some(language) = editor.read(cx).language() {
            let server = self
                .lsp_store
                .update(cx, |store, cx| store.server(language, cx));
            let status = self.lsp_status(language, cx);
            editor.update(cx, |editor, cx| {
                if let Some(server) = server {
                    editor.attach_server(server);
                }
                editor.set_lsp_status(status, cx);
            });
        }
        let title = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into();
        self.tabs.push(Tab {
            editor,
            path: path.to_path_buf(),
            title,
            _subscription: subscription,
        });
        self.refresh_diagnostics(path, cx);
        self.activate(self.tabs.len() - 1, window, cx);
    }

    fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.active = index;
        self.notice = None;
        let tab = &self.tabs[index];
        window.focus(&tab.editor.focus_handle(cx));
        self.file_tree
            .update(cx, |tree, cx| tree.select(&tab.path, cx));
        cx.notify();
    }

    /// Jumps to the only location, or lets the user pick one.
    fn navigate(
        &mut self,
        locations: Vec<Location>,
        title: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let [location] = locations.as_slice() {
            self.jump_to(location, window, cx);
            return;
        }
        let root = self.root.clone();
        let picker = cx.new(|cx| LocationPicker::new(title, locations, &root, cx));
        let subscription =
            cx.subscribe_in(&picker, window, |this, _, event, window, cx| match event {
                LocationPickerEvent::Open(location) => {
                    this.location_picker = None;
                    this.jump_to(location, window, cx);
                }
                LocationPickerEvent::Dismissed => {
                    this.location_picker = None;
                    this.focus_active(window, cx);
                }
            });
        window.focus(&picker.focus_handle(cx));
        self.location_picker = Some((picker, subscription));
        cx.notify();
    }

    fn jump_to(&mut self, location: &Location, window: &mut Window, cx: &mut Context<Self>) {
        self.record_position(cx);
        self.forward.clear();
        if let Some(editor) = self.open_editor(&location.path, window, cx) {
            editor.update(cx, |editor, cx| editor.go_to(location.range.start, cx));
        }
    }

    /// Opens (or activates) `path` and returns its editor.
    fn open_editor(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Editor>> {
        self.open(path, window, cx);
        self.tabs
            .iter()
            .find(|tab| tab.path == path)
            .map(|tab| tab.editor.clone())
    }

    fn current_position(&self, cx: &App) -> Option<NavEntry> {
        let tab = self.tabs.get(self.active)?;
        Some(NavEntry {
            path: tab.path.clone(),
            offset: tab.editor.read(cx).cursor(),
        })
    }

    fn record_position(&mut self, cx: &App) {
        if let Some(entry) = self.current_position(cx) {
            self.back.push(entry);
            if self.back.len() > HISTORY_LIMIT {
                self.back.remove(0);
            }
        }
    }

    fn go_back(&mut self, _: &GoBack, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.back.pop() else {
            return;
        };
        if let Some(current) = self.current_position(cx) {
            self.forward.push(current);
        }
        self.restore(&entry, window, cx);
    }

    fn go_forward(&mut self, _: &GoForward, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.forward.pop() else {
            return;
        };
        if let Some(current) = self.current_position(cx) {
            self.back.push(current);
        }
        self.restore(&entry, window, cx);
    }

    fn restore(&mut self, entry: &NavEntry, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.open_editor(&entry.path, window, cx) {
            editor.update(cx, |editor, cx| editor.set_cursor(entry.offset, cx));
        }
    }

    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.tabs.get(self.active) {
            Some(tab) => window.focus(&tab.editor.focus_handle(cx)),
            None => window.focus(&self.focus_handle),
        }
        cx.notify();
    }

    fn toggle_file_finder(
        &mut self,
        _: &ToggleFileFinder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.file_finder.is_some() {
            self.dismiss_file_finder(window, cx);
            return;
        }
        let finder = cx.new(|cx| FileFinder::new(self.root.clone(), cx));
        let subscription =
            cx.subscribe_in(&finder, window, |this, _, event, window, cx| match event {
                FileFinderEvent::Open(path) => {
                    this.file_finder = None;
                    this.open(path, window, cx);
                }
                FileFinderEvent::Dismissed => this.dismiss_file_finder(window, cx),
            });
        window.focus(&finder.focus_handle(cx));
        self.file_finder = Some((finder, subscription));
        cx.notify();
    }

    fn dismiss_file_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.file_finder = None;
        match self.tabs.get(self.active) {
            Some(tab) => window.focus(&tab.editor.focus_handle(cx)),
            None => window.focus(&self.focus_handle),
        }
        cx.notify();
    }

    fn close_active(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tabs.is_empty() {
            self.close_tab(self.active, window, cx);
        }
    }

    /// Closes a tab, asking to save first if it has unsaved changes.
    fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.tabs[index].editor.clone();
        if !editor.read(cx).buffer().is_dirty() {
            self.remove_tab(index, window, cx);
            return;
        }
        let message = format!("Save changes to {}?", self.tabs[index].title);
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            None,
            &["Save", "Don't Save", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(answer) = answer.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                let close = match answer {
                    0 => editor.update(cx, |editor, cx| editor.save(cx)),
                    1 => true,
                    _ => false,
                };
                // The tab list may have changed while the prompt was open.
                if close && let Some(index) = this.tabs.iter().position(|tab| tab.editor == editor)
                {
                    this.remove_tab(index, window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn remove_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let tab = self.tabs.remove(index);
        tab.editor
            .update(cx, |editor, _| editor.detach_server(true));
        if self.tabs.is_empty() {
            self.active = 0;
            window.focus(&self.focus_handle);
            cx.notify();
        } else {
            let next = if self.active > index {
                self.active - 1
            } else {
                self.active
            };
            self.activate(next.min(self.tabs.len() - 1), window, cx);
        }
    }

    fn should_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let dirty = self
            .tabs
            .iter()
            .filter(|tab| tab.editor.read(cx).buffer().is_dirty())
            .count();
        if dirty == 0 || self.close_confirmed {
            return true;
        }
        let message = format!("{dirty} file(s) have unsaved changes. Close anyway?");
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            None,
            &["Discard Changes", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update_in(cx, |this, window, _| {
                this.close_confirmed = true;
                window.remove_window();
            })
            .ok();
        })
        .detach();
        false
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .h(px(34.))
            .bg(rgb(theme::PANEL))
            .border_b_1()
            .border_color(rgb(theme::BORDER))
            .text_sm()
            .children(self.tabs.iter().enumerate().map(|(index, tab)| {
                let active = index == self.active;
                let dirty = tab.editor.read(cx).buffer().is_dirty();
                div()
                    .id(("tab", index))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .border_r_1()
                    .border_color(rgb(theme::BORDER))
                    .cursor_pointer()
                    .text_color(rgb(if active { theme::TEXT } else { theme::MUTED }))
                    .when(active, |tab| {
                        tab.bg(rgb(theme::BACKGROUND))
                            .border_b_2()
                            .border_color(rgb(theme::ACCENT))
                    })
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.activate(index, window, cx)),
                    )
                    .child(tab.title.clone())
                    .child(
                        div()
                            .id(("close", index))
                            .w(px(14.))
                            .text_color(rgb(theme::MUTED))
                            .hover(|style| style.text_color(rgb(theme::TEXT)))
                            .child(if dirty { "●" } else { "×" })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_tab(index, window, cx);
                            })),
                    )
            }))
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let main = match self.tabs.get(self.active) {
            Some(tab) => div().flex_1().min_h_0().child(tab.editor.clone()),
            None => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(theme::MUTED))
                .child("Open a file from the project tree, or press Ctrl+P"),
        };

        div()
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::close_active))
            .on_action(cx.listener(Self::toggle_file_finder))
            .on_action(cx.listener(Self::go_back))
            .on_action(cx.listener(Self::go_forward))
            .on_action(cx.listener(Self::toggle_files))
            .on_action(cx.listener(Self::toggle_search))
            .on_action(cx.listener(Self::toggle_terminal))
            .on_action(cx.listener(Self::toggle_problems))
            .on_action(cx.listener(Self::toggle_bottom_dock))
            .on_action(cx.listener(Self::save_layout_as_default))
            .on_action(cx.listener(Self::reset_layout))
            .on_mouse_move(cx.listener(Self::on_resize_move))
            .on_mouse_up(gpui::MouseButton::Left, cx.listener(Self::on_resize_end))
            .relative()
            .size_full()
            .flex()
            .bg(rgb(theme::BACKGROUND))
            .text_color(rgb(theme::TEXT))
            .children(self.render_dock(DockPosition::Left, window, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .when(!self.tabs.is_empty(), |column| {
                        column.child(self.render_tabs(cx))
                    })
                    .children(self.notice.clone().map(|notice| {
                        div()
                            .px_3()
                            .py_1()
                            .text_sm()
                            .text_color(rgb(theme::ERROR))
                            .child(notice)
                    }))
                    .child(main)
                    .children(self.render_dock(DockPosition::Bottom, window, cx)),
            )
            .children(self.render_dock(DockPosition::Right, window, cx))
            .children(self.render_panel_menu(cx))
            .children(self.file_finder.as_ref().map(|(finder, _)| {
                div()
                    .absolute()
                    .top(px(72.))
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(finder.clone())
            }))
            .children(self.location_picker.as_ref().map(|(picker, _)| {
                div()
                    .absolute()
                    .top(px(72.))
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(picker.clone())
            }))
    }
}
