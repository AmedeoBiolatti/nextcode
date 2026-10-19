//! A project's content: docks with panels, editor tabs, and the project's
//! commands. The window around it (menus, project tabs) is `shell::Shell`.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use futures::StreamExt;

use gpui::{
    AnyView, App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    KeyBinding, PromptLevel, SharedString, Subscription, Task, Window, actions, div, prelude::*,
    px,
};
use language::Location;
use syntax::Language;
use text::Buffer;
use watcher::{Batch, ChangeKind};

use crate::{
    call_hierarchy::{CallHierarchyEvent, CallHierarchyPanel},
    commit_panel::{CommitPanel, CommitPanelEvent, PreloadedRepository, SyncAction},
    compare_view::{CompareEvent, CompareView},
    connecting::PendingConnection,
    diff_view::{DiffSource, DiffView, DiffViewEvent},
    editor::{Editor, EditorEvent, ReloadOutcome},
    file_finder::{FileFinder, FileFinderEvent},
    file_tree::{FileTree, FileTreeEvent},
    git_log::GitLogPanel,
    layout::{DockPosition, Layout, PanelId},
    location_picker::{LocationPicker, LocationPickerEvent},
    lsp_store::{LspStore, LspStoreEvent, ServerStatus},
    notebook::view::{NotebookView, NotebookViewEvent, OpenAsNotebook},
    outline_panel::OutlinePanel,
    pinpoint::{self, Pin},
    ports::{Ports, PortsEvent, panel::PortsPanel},
    problems::{ProblemsEvent, ProblemsPanel},
    project::{self, Project},
    python_panel::{PythonPanel, PythonPanelEvent, RunRequest},
    recent,
    revision_picker::{RevisionPicker, RevisionPickerEvent},
    search_panel::{SearchPanel, SearchPanelEvent},
    selectable::selectable,
    settings::Settings,
    symbol_picker::SymbolPicker,
    terminal_panel::{TerminalPanel, TerminalPanelEvent, TerminalView},
    theme,
    worktree_picker::{WorktreePicker, WorktreePickerEvent},
};

mod debug;
mod docks;
mod edits;
mod files;
mod folders;
mod git_flows;
mod git_gutter;
mod log_window;
mod notebooks;
mod notes;
mod panes;
mod pinpoint_text;
mod runs;
mod session;
mod stash_flows;
mod symbols;
#[cfg(test)]
mod tests;

use panes::{Pane, PaneId, PaneNode};

actions!(
    workspace,
    [
        CloseTab,
        ToggleFileFinder,
        GoBack,
        GoForward,
        SaveAll,
        NewTerminal,
        ReopenInContainer,
        AddFolderToWorkspace,
        SaveWorkspaceAs
    ]
);

/// Git commands for menus and the command palette.
pub mod git_commands {
    gpui::actions!(
        git,
        [
            Branches,
            Worktrees,
            ShowLog,
            ShowFileHistory,
            Compare,
            Fetch,
            Pull,
            Push,
            Refresh
        ]
    );
}

/// uv project commands for menus and the command palette.
pub mod python_commands {
    gpui::actions!(
        python,
        [Sync, Lock, RunFile, RunTests, RunTestFile, RunTestAtCursor]
    );
}

/// Positions kept for Back/Forward.
const HISTORY_LIMIT: usize = 100;

pub fn bind_keys(cx: &mut App) {
    let ctx = Some("Workspace");
    cx.bind_keys([
        KeyBinding::new("secondary-w", CloseTab, ctx),
        KeyBinding::new("secondary-p", ToggleFileFinder, ctx),
        // VS Code defaults (Ctrl+Alt+Left/Right move tabs between panes).
        KeyBinding::new("alt-left", GoBack, ctx),
        KeyBinding::new("alt-right", GoForward, ctx),
    ]);
    docks::bind_keys(cx);
    panes::bind_keys(cx);
    symbols::bind_keys(cx);
    runs::bind_keys(cx);
    debug::bind_keys(cx);
    git_flows::bind_keys(cx);
}

/// Work for the first window started before gpui (whose platform setup
/// takes a few hundred milliseconds): connecting to WSL or a dev
/// container, or reading a local project's git status.
pub enum Preload {
    Local(PreloadedRepository),
    Remote(PendingConnection),
}

impl Preload {
    /// Starts the work for `location` on a thread.
    pub fn start(location: &project::Location) -> Self {
        match location {
            project::Location::Local(root) => {
                let (tx, rx) = futures::channel::oneshot::channel();
                let project = Project::local(root.clone());
                std::thread::spawn(move || tx.send(project.open_repository()).ok());
                Self::Local(rx)
            }
            remote => Self::Remote(PendingConnection::start(remote.clone())),
        }
    }
}

/// A remembered cursor position for Back/Forward.
struct NavEntry {
    path: PathBuf,
    offset: usize,
}

#[derive(Clone)]
enum TabContent {
    Editor(Entity<Editor>),
    Diff(Entity<DiffView>),
    Compare(Entity<CompareView>),
    Notebook(Entity<NotebookView>),
}

impl TabContent {
    fn entity_id(&self) -> gpui::EntityId {
        match self {
            Self::Editor(editor) => editor.entity_id(),
            Self::Diff(diff) => diff.entity_id(),
            Self::Compare(view) => view.entity_id(),
            Self::Notebook(view) => view.entity_id(),
        }
    }

    /// Writes a file tab's unsaved changes; the task says whether that
    /// succeeded (an editor's write runs off the UI thread).
    fn save(&self, cx: &mut App) -> Task<bool> {
        match self {
            Self::Editor(editor) => editor.update(cx, |editor, cx| editor.save(cx)),
            Self::Notebook(view) => Task::ready(view.update(cx, |view, cx| view.save_document(cx))),
            Self::Diff(_) | Self::Compare(_) => Task::ready(true),
        }
    }
}

/// What a revision picker was opened for.
enum RevisionPurpose {
    /// A new comparison of the working tree with the picked revision.
    NewCompare,
    Base(Entity<CompareView>),
    Target(Entity<CompareView>),
}

struct Tab {
    content: TabContent,
    /// The edited file, the file a diff is about, or the project root.
    path: PathBuf,
    title: SharedString,
    /// The view of its file that holds the language server document (one
    /// per file, following focus between panes).
    lsp_owner: bool,
    _subscription: Subscription,
}

impl Tab {
    fn editor(&self) -> Option<&Entity<Editor>> {
        match &self.content {
            TabContent::Editor(editor) => Some(editor),
            _ => None,
        }
    }

    fn notebook(&self) -> Option<&Entity<NotebookView>> {
        match &self.content {
            TabContent::Notebook(view) => Some(view),
            _ => None,
        }
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.content {
            TabContent::Editor(editor) => editor.focus_handle(cx),
            TabContent::Diff(diff) => diff.focus_handle(cx),
            TabContent::Compare(view) => view.focus_handle(cx),
            TabContent::Notebook(view) => view.focus_handle(cx),
        }
    }

    fn view(&self) -> AnyView {
        match &self.content {
            TabContent::Editor(editor) => editor.clone().into(),
            TabContent::Diff(diff) => diff.clone().into(),
            TabContent::Compare(view) => view.clone().into(),
            TabContent::Notebook(view) => view.clone().into(),
        }
    }

    fn entity_id(&self) -> gpui::EntityId {
        self.content.entity_id()
    }

    fn is_dirty(&self, cx: &App) -> bool {
        match &self.content {
            TabContent::Editor(editor) => editor.read(cx).buffer().is_dirty(),
            TabContent::Notebook(view) => view.read(cx).is_dirty(cx),
            TabContent::Diff(_) | TabContent::Compare(_) => false,
        }
    }

    /// The stable pinpoint key of the tab (`tab:<key>`) and of its
    /// content's wrapper: the file as `pinpoint::file_key` names it; a
    /// diff adds which versions it compares.
    fn pin_key(&self, project: &Project, cx: &App) -> String {
        match &self.content {
            TabContent::Editor(_) | TabContent::Notebook(_) => {
                pinpoint::file_key(project, &self.path)
            }
            TabContent::Diff(diff) => format!(
                "{}@{}",
                pinpoint::file_key(project, &self.path),
                pinpoint::kebab(&diff.read(cx).source().sides())
            ),
            TabContent::Compare(_) => format!(
                "compare:{}",
                pinpoint::kebab(self.title.trim_start_matches("Compare "))
            ),
        }
    }

    /// The pinpoint id of the wrapper its owner pins the content inside.
    fn content_pin(&self, key: &str) -> String {
        match &self.content {
            TabContent::Editor(_) => format!("editor:{key}"),
            TabContent::Notebook(_) => format!("notebook:{key}"),
            TabContent::Diff(_) => format!("diff:{key}"),
            // Already `compare:…`.
            TabContent::Compare(_) => key.to_owned(),
        }
    }
}

/// A suggestion above the editor, with an action button and "Not now".
struct Offer {
    kind: OfferKind,
    message: SharedString,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OfferKind {
    /// The folder has this `devcontainer.json`.
    ReopenInContainer { config: String },
    /// `.venv` is out of date with the lock or `pyproject.toml`.
    PythonSync,
    /// No LLDB debug adapter: download CodeLLDB.
    DownloadCodeLldb,
    /// A forwarded port's application listens: open it in the browser.
    PortAvailable { port: u16, url: String },
    /// The active tab shows a marimo notebook as text.
    OpenAsNotebook { path: PathBuf },
    /// An agent wrote to the person on this note: show it.
    NoteMessage {
        root: usize,
        file: String,
        id: String,
    },
    /// The folder is not trusted yet: its settings run no programs (see
    /// `trust`).
    TrustFolder,
}

impl OfferKind {
    fn action(&self) -> &'static str {
        match self {
            Self::ReopenInContainer { .. } => "Reopen in Container",
            Self::PythonSync => "Sync",
            Self::DownloadCodeLldb => "Download CodeLLDB",
            Self::PortAvailable { .. } => "Open in Browser",
            Self::OpenAsNotebook { .. } => "Open as Notebook",
            Self::NoteMessage { .. } => "Show Note",
            Self::TrustFolder => "Trust",
        }
    }

    /// Its pinpoint id, `offer:<key>`.
    fn pin_id(&self) -> String {
        match self {
            Self::ReopenInContainer { .. } => "offer:reopen-in-container".into(),
            Self::PythonSync => "offer:python-sync".into(),
            Self::DownloadCodeLldb => "offer:download-codelldb".into(),
            Self::PortAvailable { port, .. } => format!("offer:port-{port}"),
            Self::OpenAsNotebook { .. } => "offer:open-as-notebook".into(),
            Self::NoteMessage { id, .. } => format!("offer:note-{id}"),
            Self::TrustFolder => "offer:trust-folder".into(),
        }
    }
}

pub struct Workspace {
    focus_handle: FocusHandle,
    project: Project,
    file_tree: Entity<FileTree>,
    problems: Entity<ProblemsPanel>,
    /// Sticky notes: the project's, and the Notes panel.
    note_store: Entity<crate::note_store::NoteStore>,
    notes_panel: Entity<crate::notes_panel::NotesPanel>,
    /// Forwarded ports (WSL and container projects); the panel holds the
    /// model.
    ports_panel: Entity<PortsPanel>,
    terminal: Entity<TerminalPanel>,
    search: Entity<SearchPanel>,
    /// The Commit tool window.
    commit: Entity<CommitPanel>,
    /// The Git tool window (the Log).
    git_log: Entity<GitLogPanel>,
    python: Entity<PythonPanel>,
    call_hierarchy: Entity<CallHierarchyPanel>,
    /// Follows the active editor (set while rendering).
    outline: Entity<OutlinePanel>,
    /// Run configurations, the Tests panel and the configuration picker.
    runs: runs::Runs,
    /// Debug sessions, breakpoints, the Debug panel and console.
    debug: debug::DebugState,
    layout: Layout,
    /// Dock being resized by dragging its handle.
    resizing: Option<DockPosition>,
    panel_menu: Option<docks::PanelMenu>,
    file_finder: Option<(Entity<FileFinder>, Subscription)>,
    location_picker: Option<(Entity<LocationPicker>, Subscription)>,
    /// Go to Symbol (file/workspace) and breadcrumb lists.
    symbol_picker: Option<(Entity<SymbolPicker>, Subscription)>,
    revision_picker: Option<(Entity<RevisionPicker>, Subscription)>,
    /// The worktree picker, or a git dialog (new branch, push).
    git_modal: Option<(AnyView, Subscription)>,
    /// The title-bar branch widget's popup.
    branches_popup: Option<git_flows::BranchesPopupState>,
    back: Vec<NavEntry>,
    forward: Vec<NavEntry>,
    lsp_store: Entity<LspStore>,
    /// Editor panes in reading order (never empty); see `panes`.
    panes: Vec<Pane>,
    pane_tree: PaneNode,
    active_pane: PaneId,
    /// Source of pane and split ids.
    next_id: usize,
    /// Split whose handle is being dragged.
    pane_resize: Option<usize>,
    /// `restore_session` from settings.json.
    restore_session: bool,
    /// Pending debounced session write.
    session_save: Option<Task<()>>,
    notice: Option<SharedString>,
    /// Suggestions with buttons, above the editor.
    offers: Vec<Offer>,
    /// Why the WSL agent connection was lost; stays until restart.
    connection_error: Option<SharedString>,
    /// Files with an open "changed on disk" prompt, so it is asked once.
    reload_prompts: HashSet<PathBuf>,
    /// marimo notebooks the user chose to see as text (Open as Text) this
    /// session: they open in text editors.
    text_notebooks: HashSet<PathBuf>,
    /// Owns the watch: dropping it stops watching.
    _watcher: Task<()>,
    _disconnects: Option<Task<()>>,
    /// Change-marker bases and blame of the open editors.
    git_gutters: git_gutter::GitGutters,
    _subscriptions: Vec<Subscription>,
}

/// Requests for the window around the workspace.
pub enum WorkspaceEvent {
    /// Open another project (in a new project tab).
    OpenProject(project::Location),
    /// Show this project another way (inside its dev container): open
    /// that, and close this one unless it has unsaved edits.
    ReplaceWith(project::Location),
    /// The same project with other folders (added, removed, or saved as a
    /// workspace file): open it in this tab instead. Its session was
    /// saved for it.
    Reopen(project::Location),
}

impl EventEmitter<WorkspaceEvent> for Workspace {}

impl Workspace {
    pub fn new(
        project: Project,
        git_preload: Option<PreloadedRepository>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        crate::startup::mark("workspace start");
        window.on_next_frame(|_, _| crate::startup::mark("first frame"));
        let file_tree = cx.new(|cx| FileTree::new(project.clone(), cx));
        let tree_subscription = cx.subscribe_in(
            &file_tree,
            window,
            |this, _, event: &FileTreeEvent, window, cx| {
                this.on_file_tree_event(event, window, cx);
            },
        );

        let (settings, settings_error) = Settings::load();
        let restore_session = settings.restore_session;
        let ports = cx.new(|cx| Ports::new(project.clone(), &settings.ports, cx));
        let ports_subscription = cx.subscribe(&ports, |this, _, event, cx| {
            this.on_ports_event(event, cx);
        });
        let ports_panel = cx.new(|cx| PortsPanel::new(ports.clone(), cx));
        let lsp_store = cx.new(|_| LspStore::new(project.clone(), Arc::new(settings)));
        let lsp_subscription = cx.subscribe(&lsp_store, |this, _, event, cx| {
            this.on_lsp_event(event, cx);
        });
        let problems = cx.new(|cx| ProblemsPanel::new(project.clone(), lsp_store.clone(), cx));
        let (note_store, notes_panel, notes_subscriptions) =
            Self::notes_setup(&project, window, cx);
        let terminal = cx.new(|cx| TerminalPanel::new(project.clone(), cx));
        let search = cx.new(|cx| SearchPanel::new(project.clone(), cx));
        let workspace = cx.entity().downgrade();
        search.update(cx, |search, _| {
            search.set_unsaved_documents(std::rc::Rc::new(move |cx| {
                workspace
                    .upgrade()
                    .map(|workspace| workspace.read(cx).unsaved_documents(cx))
                    .unwrap_or_default()
            }));
        });
        let commit = cx.new(|cx| CommitPanel::new(project.clone(), git_preload, cx));
        let commit_subscription =
            cx.subscribe_in(&commit, window, |this, _, event, window, cx| match event {
                CommitPanelEvent::OpenDiff { repository, source } => {
                    this.open_diff(repository.clone(), source.clone(), window, cx)
                }
                CommitPanelEvent::OpenFile(path) => this.open(path, window, cx),
                CommitPanelEvent::Push(repository) => {
                    this.open_push_dialog(repository.clone(), window, cx)
                }
                CommitPanelEvent::Stash {
                    repository,
                    paths,
                    include_untracked,
                } => this.stash_dialog(
                    repository.clone(),
                    Some(paths.clone()),
                    *include_untracked,
                    window,
                    cx,
                ),
                CommitPanelEvent::Shelve {
                    repository,
                    paths,
                    name,
                } => {
                    this.shelve_dialog(repository.clone(), paths.clone(), name.clone(), window, cx)
                }
                CommitPanelEvent::Refreshed => {
                    this.reload_diffs(cx);
                    this.refresh_git_gutters(cx);
                }
            });
        let git_log = cx.new(|cx| GitLogPanel::new(project.clone(), cx));
        let git_log_subscription = cx.subscribe_in(&git_log, window, Self::on_git_log_event);
        let python = cx.new(|cx| PythonPanel::new(project.clone(), cx));
        let python_subscription =
            cx.subscribe_in(&python, window, |this, _, event, window, cx| match event {
                PythonPanelEvent::Run(request) => this.run_python(request.clone(), window, cx),
                PythonPanelEvent::EnvironmentChanged => this
                    .lsp_store
                    .update(cx, |store, cx| store.refresh_python_interpreter(cx)),
                PythonPanelEvent::SyncNeeded(true) => this.offer(
                    OfferKind::PythonSync,
                    "The Python environment (.venv) is out of date with uv.lock / pyproject.toml.",
                    cx,
                ),
                PythonPanelEvent::SyncNeeded(false) => {
                    this.withdraw_offer(&OfferKind::PythonSync, cx)
                }
            });
        let terminal_subscription = cx.subscribe_in(
            &terminal,
            window,
            |this, _, event: &TerminalPanelEvent, window, cx| match event {
                TerminalPanelEvent::OpenFile { path, position } => {
                    this.record_position(cx);
                    this.forward.clear();
                    if let Some(editor) = this.open_editor(path, window, cx)
                        && let Some((line, column)) = *position
                    {
                        editor.update(cx, |editor, cx| editor.go_to_line(line, column, cx));
                    }
                }
            },
        );
        let search_subscription =
            cx.subscribe_in(&search, window, |this, _, event, window, cx| match event {
                SearchPanelEvent::Open { path, line, column } => {
                    this.record_position(cx);
                    this.forward.clear();
                    if let Some(editor) = this.open_editor(path, window, cx) {
                        editor.update(cx, |editor, cx| editor.go_to_line(*line, *column, cx));
                    }
                }
                SearchPanelEvent::Replace { replacer, files } => {
                    let report = this.apply_search_replace(replacer, files, cx);
                    this.search
                        .update(cx, |search, cx| search.replaced(report, cx));
                }
            });
        let problems_subscription = cx.subscribe_in(
            &problems,
            window,
            |this, _, event, window, cx| match event {
                ProblemsEvent::Open(location) => this.jump_to(location, window, cx),
            },
        );
        let call_hierarchy = cx.new(CallHierarchyPanel::new);
        let call_hierarchy_subscription = cx.subscribe_in(
            &call_hierarchy,
            window,
            |this, _, event, window, cx| match event {
                CallHierarchyEvent::Jump(location) => this.jump_to(location, window, cx),
            },
        );
        let runs = runs::Runs::new(project.clone(), window, cx);

        // Workspace key bindings (e.g. Ctrl+P) need focus inside the workspace.
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        let layout = Layout::load(&project.state_key());
        let debug = debug::DebugState::new(window, cx);
        let notice = settings_error.map(|error| format!("Settings ignored: {error}").into());
        // Starting the watcher can take seconds (a container's bind mount
        // from a Windows folder, a large tree): never on the UI thread.
        let watch_project = project.clone();
        let watcher = cx.spawn_in(window, async move |this, cx| {
            let started = cx
                .background_spawn(async move { watch_project.watch(Duration::from_millis(150)) })
                .await;
            let (_handle, mut batches) = match started {
                Ok(started) => started,
                Err(err) => {
                    this.update(cx, |this, cx| {
                        this.notice = Some(format!("File watching unavailable: {err}").into());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
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
        let disconnects = project.disconnects().map(|mut reasons| {
            cx.spawn(async move |this, cx| {
                if let Some(reason) = reasons.next().await {
                    this.update(cx, |this, cx| {
                        this.connection_error = Some(
                            format!(
                                "{reason}. Files can no longer be opened or saved; \
                                 restart next_code to reconnect (unsaved edits stay open)."
                            )
                            .into(),
                        );
                        cx.notify();
                    })
                    .ok();
                }
            })
        });
        // A folder with a dev container: offer to reopen it inside.
        let configs_project = project.clone();
        cx.spawn(async move |this, cx| {
            let configs = cx
                .background_spawn(async move { configs_project.devcontainer_configs() })
                .await
                .unwrap_or_default();
            if let Some(config) = configs.into_iter().next() {
                this.update(cx, |this, cx| {
                    this.offer(
                        OfferKind::ReopenInContainer { config },
                        "This folder has a dev container. Reopen it inside the container to \
                         use its tools, language servers and terminals.",
                        cx,
                    )
                })
                .ok();
            }
        })
        .detach();
        recent::record(&project.location());
        cx.defer_in(window, |this, _, cx| this.load_folder_settings(cx));
        if let Some(question) = crate::trust::prompt(&project) {
            cx.defer_in(window, move |this, _, cx| {
                this.offer(OfferKind::TrustFolder, &question, cx)
            });
        }
        // The session is written when the project closes (its tab or
        // window, or the app quits) and restored once the workspace exists.
        let release_subscription = cx.on_release(|this, cx| {
            this.save_session(cx).ok();
            this.shutdown_notebooks(cx);
        });
        let quit_subscription = cx.on_app_quit(|this, cx| {
            this.save_session(cx).ok();
            // No kernel outlives the app.
            this.shutdown_notebooks(cx);
            // Language servers get `shutdown`/`exit` and a grace to leave
            // (gpui bounds the wait; the rest are ended with the app).
            this.lsp_store
                .update(cx, |store, cx| store.shutdown_all(lsp::SHUTDOWN_GRACE, cx))
        });
        cx.defer_in(window, |this, window, cx| this.restore_session(window, cx));
        crate::startup::mark("workspace built");
        let first_pane = PaneId(1);
        Self {
            focus_handle,
            project,
            file_tree,
            problems,
            note_store,
            notes_panel,
            ports_panel,
            terminal,
            search,
            commit,
            git_log,
            python,
            call_hierarchy,
            outline: cx.new(OutlinePanel::new),
            runs,
            debug,
            layout,
            resizing: None,
            panel_menu: None,
            file_finder: None,
            location_picker: None,
            symbol_picker: None,
            revision_picker: None,
            git_modal: None,
            branches_popup: None,
            back: Vec::new(),
            forward: Vec::new(),
            lsp_store,
            panes: vec![Pane::new(first_pane)],
            pane_tree: PaneNode::Leaf(first_pane),
            active_pane: first_pane,
            next_id: first_pane.0,
            pane_resize: None,
            restore_session,
            session_save: None,
            notice,
            offers: Vec::new(),
            connection_error: None,
            reload_prompts: HashSet::new(),
            text_notebooks: HashSet::new(),
            _watcher: watcher,
            _disconnects: disconnects,
            git_gutters: Default::default(),
            _subscriptions: vec![
                tree_subscription,
                lsp_subscription,
                problems_subscription,
                search_subscription,
                commit_subscription,
                git_log_subscription,
                python_subscription,
                call_hierarchy_subscription,
                terminal_subscription,
                release_subscription,
                quit_subscription,
                ports_subscription,
            ]
            .into_iter()
            .chain(notes_subscriptions)
            .collect(),
        }
    }

    /// Tells the user about forwarded applications (`onAutoForward`).
    fn on_ports_event(&mut self, event: &PortsEvent, cx: &mut Context<Self>) {
        match event {
            PortsEvent::Available {
                port,
                url,
                label,
                action,
            } => match action {
                devcontainer::ports::OnAutoForward::OpenBrowser => cx.open_url(url),
                devcontainer::ports::OnAutoForward::Notify => {
                    let what = match label {
                        Some(label) => format!("Your application ({label})"),
                        None => "Your application".to_owned(),
                    };
                    let local = url.rsplit(':').next().unwrap_or_default();
                    let message = if local == port.to_string() {
                        format!("{what} running on port {port} is available.")
                    } else {
                        format!("{what} running on port {port} is available at localhost:{local}.")
                    };
                    self.offer(
                        OfferKind::PortAvailable {
                            port: *port,
                            url: url.clone(),
                        },
                        &message,
                        cx,
                    );
                }
                _ => {}
            },
            PortsEvent::Released { target } if target.host.is_none() => {
                let port = target.port;
                self.offers.retain(|offer| {
                    !matches!(&offer.kind, OfferKind::PortAvailable { port: p, .. } if *p == port)
                });
                cx.notify();
            }
            _ => {}
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
        // Worktree edits change status as much as commits do.
        self.commit
            .update(cx, |commit, cx| commit.schedule_refresh(cx));
        let python_files = ["pyproject.toml", "uv.lock", ".python-version"];
        if batch.rescan
            || batch.events.iter().any(|event| {
                event
                    .path
                    .file_name()
                    .is_some_and(|name| python_files.iter().any(|file| name == *file))
            })
        {
            self.python.update(cx, |python, cx| python.refresh(cx));
        }
        if batch.repository || batch.rescan {
            self.git_log
                .update(cx, |log, cx| log.repository_changed(cx));
        }
        self.run_files_changed(&batch, cx);
        self.note_store.update(cx, |store, cx| {
            let paths = batch.events.iter().map(|event| event.path.as_path());
            store.files_changed(paths, batch.rescan, cx)
        });
        // An edited `.next_code/words.txt` (project dictionary).
        crate::editor::spell::files_changed(
            batch.events.iter().map(|event| event.path.as_path()),
            batch.rescan,
            cx,
        );
        if self
            .project
            .folder_settings_changed(batch.events.iter().map(|event| event.path.as_path()))
        {
            self.load_folder_settings(cx);
        }

        // One view per file: views of a file share its buffer.
        let mut seen = HashSet::new();
        let affected: Vec<(Entity<Editor>, PathBuf, bool)> = self
            .all_tabs()
            .filter(|tab| tab.editor().is_some() && seen.insert(tab.path.clone()))
            .filter_map(|tab| {
                let editor = tab.editor()?.clone();
                let event = batch.events.iter().find(|event| event.path == tab.path);
                match event {
                    Some(event) => {
                        Some((editor, tab.path.clone(), event.kind == ChangeKind::Removed))
                    }
                    None if batch.rescan => {
                        let removed = matches!(self.project.kind(&tab.path), Ok(None));
                        Some((editor, tab.path.clone(), removed))
                    }
                    None => None,
                }
            })
            .collect();
        for (editor, path, removed) in affected {
            // The Files tree's own renames/deletes: tabs already followed.
            if self.file_tree.read(cx).is_own_change(&path) {
                continue;
            }
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
        self.notebooks_changed_on_disk(&batch, window, cx);
    }

    /// Opens (or activates) a diff tab.
    pub(crate) fn open_diff(
        &mut self,
        repository: git::Repository,
        source: DiffSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = self.project.repository_path(&repository, source.path());
        let existing = self.find_tab(|tab| match &tab.content {
            TabContent::Diff(diff) => *diff.read(cx).source() == source,
            _ => false,
        });
        if let Some((pane, index)) = existing {
            if let Some(TabContent::Diff(diff)) = self.tab_content(pane, index) {
                diff.update(cx, |diff, cx| diff.reload(cx));
            }
            return self.activate(pane, index, window, cx);
        }
        let project = self.project.clone();
        let view = cx.new(|cx| DiffView::new(project, repository, source, cx));
        let subscription =
            cx.subscribe_in(&view, window, |this, _, event, window, cx| match event {
                DiffViewEvent::OpenFile { path, line } => {
                    this.record_position(cx);
                    this.forward.clear();
                    if let Some(editor) = this.open_editor(path, window, cx) {
                        editor.update(cx, |editor, cx| editor.go_to_line(*line, 0, cx));
                    }
                }
                DiffViewEvent::RepositoryChanged => this.repository_changed(None, cx),
            });
        let title = view.read(cx).title();
        self.push_tab(
            TabContent::Diff(view),
            path,
            title,
            subscription,
            window,
            cx,
        );
    }

    fn push_tab(
        &mut self,
        content: TabContent,
        path: PathBuf,
        title: SharedString,
        subscription: Subscription,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.active_pane;
        let Some(tabs) = self.pane_mut(pane).map(|pane| &mut pane.tabs) else {
            return;
        };
        tabs.push(Tab {
            content,
            path,
            title,
            lsp_owner: false,
            _subscription: subscription,
        });
        let index = tabs.len() - 1;
        self.activate(pane, index, window, cx);
    }

    /// The first tab (in pane order) matching `predicate`.
    fn find_tab(&self, predicate: impl Fn(&Tab) -> bool) -> Option<(PaneId, usize)> {
        self.panes.iter().find_map(|pane| {
            let index = pane.tabs.iter().position(&predicate)?;
            Some((pane.id, index))
        })
    }

    fn tab_content(&self, pane: PaneId, index: usize) -> Option<&TabContent> {
        Some(&self.pane(pane)?.tabs.get(index)?.content)
    }

    /// Opens (or activates) a comparison of `base` with `target`.
    pub(crate) fn open_compare(
        &mut self,
        repository: git::Repository,
        base: String,
        target: git::Side,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = self.find_tab(|tab| match &tab.content {
            TabContent::Compare(view) => view.read(cx).sides() == (&base, &target),
            _ => false,
        });
        if let Some((pane, index)) = existing {
            if let Some(TabContent::Compare(view)) = self.tab_content(pane, index) {
                view.update(cx, |view, cx| view.reload(cx));
            }
            return self.activate(pane, index, window, cx);
        }
        let view = cx.new(|cx| CompareView::new(repository, base, target, cx));
        let subscription =
            cx.subscribe_in(&view, window, |this, view, event, window, cx| match event {
                CompareEvent::OpenDiff { repository, source } => {
                    this.open_diff(repository.clone(), source.clone(), window, cx)
                }
                CompareEvent::ChangeBase(repository) => this.pick_revision(
                    repository.clone(),
                    RevisionPurpose::Base(view.clone()),
                    window,
                    cx,
                ),
                CompareEvent::ChangeTarget(repository) => this.pick_revision(
                    repository.clone(),
                    RevisionPurpose::Target(view.clone()),
                    window,
                    cx,
                ),
                CompareEvent::TitleChanged => {
                    let tab =
                        this.panes.iter_mut().flat_map(|pane| &mut pane.tabs).find(
                            |tab| match &tab.content {
                                TabContent::Compare(compare) => compare == view,
                                _ => false,
                            },
                        );
                    if let Some(tab) = tab {
                        tab.title = view.read(cx).title();
                    }
                    cx.notify();
                }
            });
        let title = view.read(cx).title();
        let root = self.project.root().to_path_buf();
        self.push_tab(
            TabContent::Compare(view),
            root,
            title,
            subscription,
            window,
            cx,
        );
    }

    /// Asks for a revision (branch, tag, commit, or the working tree).
    fn pick_revision(
        &mut self,
        repository: git::Repository,
        purpose: RevisionPurpose,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let allow_worktree = matches!(purpose, RevisionPurpose::Target(_));
        let title = match &purpose {
            RevisionPurpose::NewCompare => "Compare the working tree with…",
            RevisionPurpose::Base(_) => "Compare from (base)…",
            RevisionPurpose::Target(_) => "Compare to…",
        };
        let picker =
            cx.new(|cx| RevisionPicker::new(repository.clone(), title, allow_worktree, cx));
        let subscription = cx.subscribe_in(&picker, window, move |this, _, event, window, cx| {
            this.revision_picker = None;
            this.focus_active(window, cx);
            let RevisionPickerEvent::Picked(side) = event else {
                return;
            };
            match &purpose {
                RevisionPurpose::NewCompare => {
                    if let git::Side::Revision(base) = side {
                        this.open_compare(
                            repository.clone(),
                            base.clone(),
                            git::Side::Worktree,
                            window,
                            cx,
                        );
                    }
                }
                RevisionPurpose::Base(view) => {
                    if let git::Side::Revision(base) = side {
                        view.update(cx, |view, cx| view.set_base(base.clone(), cx));
                    }
                }
                RevisionPurpose::Target(view) => {
                    view.update(cx, |view, cx| view.set_target(side.clone(), cx));
                }
            }
        });
        window.focus(&picker.focus_handle(cx));
        self.revision_picker = Some((picker, subscription));
        cx.notify();
    }

    fn open_worktree_picker(
        &mut self,
        repository: git::Repository,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let picker = cx.new(|cx| WorktreePicker::new(repository, cx));
        let subscription = cx.subscribe_in(&picker, window, |this, _, event, window, cx| {
            this.git_modal = None;
            this.focus_active(window, cx);
            match event {
                WorktreePickerEvent::Open(path) => {
                    this.repository_changed(None, cx);
                    match this.project.location_of(path) {
                        Some(location) => cx.emit(WorkspaceEvent::OpenProject(location)),
                        None => {
                            this.notice = Some(
                                format!(
                                    "{path} is inside the dev container; open it from a terminal \
                                     or add it to the container's workspace instead"
                                )
                                .into(),
                            );
                            cx.notify();
                        }
                    }
                }
                WorktreePickerEvent::Changed(message) => {
                    this.repository_changed(Some(message.clone()), cx)
                }
                WorktreePickerEvent::Dismissed => {}
            }
        });
        window.focus(&picker.focus_handle(cx));
        self.git_modal = Some((picker.into(), subscription));
        cx.notify();
    }

    /// Reconciles status after returning to the project, including when
    /// filesystem notifications were missed while it was in the background.
    pub(crate) fn refresh_git_status(&mut self, cx: &mut Context<Self>) {
        self.commit.update(cx, |commit, cx| commit.refresh(cx));
    }

    /// Re-reads git state everywhere after an operation from a view or
    /// popup; `message` is reported in the Commit window.
    fn repository_changed(&mut self, message: Option<String>, cx: &mut Context<Self>) {
        self.commit.update(cx, |commit, cx| {
            if let Some(message) = message {
                commit.report(message, cx);
            }
            commit.refresh(cx);
        });
        self.git_log
            .update(cx, |log, cx| log.repository_changed(cx));
    }

    /// Recomputes open diffs and comparisons after the repository or the
    /// working tree changed.
    fn reload_diffs(&mut self, cx: &mut Context<Self>) {
        for tab in self.all_tabs() {
            match &tab.content {
                TabContent::Diff(diff) => diff.update(cx, |diff, cx| diff.reload(cx)),
                TabContent::Compare(view) => view.update(cx, |view, cx| view.reload(cx)),
                TabContent::Editor(_) | TabContent::Notebook(_) => {}
            }
        }
    }

    /// A save found the file changed on disk (`EditorEvent::SaveConflict`):
    /// overwrite it, reload it (losing the edits), or leave both as they are.
    fn prompt_overwrite(
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
            Some(
                "The file was changed outside the editor since you opened or saved it. \
                 Overwrite it with your changes, or reload it and lose them?",
            ),
            &["Overwrite", "Reload", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let answer = answer.await;
            this.update_in(cx, |this, _, cx| {
                this.reload_prompts.remove(&path);
                match answer {
                    Ok(0) => editor
                        .update(cx, |editor, cx| editor.save_overwriting(cx))
                        .detach(),
                    Ok(1) => {
                        editor.update(cx, |editor, cx| editor.reload_from_disk(true, cx));
                    }
                    _ => {}
                }
            })
            .ok();
        })
        .detach();
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
            &LspStoreEvent::ServerStarted(language, _) => self.attach_servers(language, cx),
            &LspStoreEvent::ServerStopped(language) => {
                for editor in self.editors_for(language, true, cx) {
                    let exited = editor
                        .read(cx)
                        .server()
                        .is_some_and(|server| server.connection.has_exited());
                    if exited {
                        editor.update(cx, |editor, _| editor.detach_server(false));
                    }
                }
            }
            &LspStoreEvent::StatusChanged(language) => {
                // A folder whose first server failed to start tries its own.
                if self.project.is_multi_root() {
                    self.attach_servers(language, cx);
                }
                for editor in self.editors_for(language, false, cx) {
                    let status = editor
                        .read(cx)
                        .buffer()
                        .path()
                        .and_then(|path| self.lsp_status(language, path, cx));
                    editor.update(cx, |editor, cx| editor.set_lsp_status(status, cx));
                }
            }
            LspStoreEvent::DiagnosticsUpdated(path) => self.refresh_diagnostics(path, cx),
            &LspStoreEvent::SemanticTokensRefresh(language) => {
                for editor in self.editors_for(language, true, cx) {
                    editor.update(cx, |editor, cx| editor.refresh_semantic_tokens(cx));
                }
            }
            &LspStoreEvent::InlayHintsRefresh(language) => {
                for editor in self.editors_for(language, true, cx) {
                    editor.update(cx, |editor, cx| editor.refresh_inlay_hints(cx));
                }
            }
            LspStoreEvent::ApplyEdit(request) => self.apply_edit_request(request, cx),
        }
    }

    /// Gives every view of `path` the server's diagnostics.
    fn refresh_diagnostics(&mut self, path: &Path, cx: &mut Context<Self>) {
        let editors: Vec<Entity<Editor>> = self
            .all_tabs()
            .filter(|tab| tab.path == path)
            .filter_map(Tab::editor)
            .cloned()
            .collect();
        if editors.is_empty() {
            return;
        }
        let diagnostics = self.lsp_store.read(cx).diagnostics(path).to_vec();
        for editor in editors {
            editor.update(cx, |editor, cx| editor.set_diagnostics(&diagnostics, cx));
        }
    }

    /// Editors of `language`; with `owners`, only the view of each file that
    /// holds (or would hold) its server document.
    fn editors_for(&self, language: Language, owners: bool, cx: &App) -> Vec<Entity<Editor>> {
        self.all_tabs()
            .filter(|tab| tab.lsp_owner || !owners)
            .filter_map(Tab::editor)
            .filter(|editor| editor.read(cx).language() == Some(language))
            .cloned()
            .collect()
    }

    /// Opens the documents of `language` on the server serving each (one
    /// per folder when a multi-root project's server cannot serve them
    /// all), starting it if needed.
    fn attach_servers(&mut self, language: Language, cx: &mut Context<Self>) {
        for editor in self.editors_for(language, true, cx) {
            let Some(path) = editor.read(cx).buffer().path().map(Path::to_path_buf) else {
                continue;
            };
            let server = self
                .lsp_store
                .update(cx, |store, cx| store.server(language, &path, cx));
            if let Some(server) = server {
                editor.update(cx, |editor, cx| editor.attach_server(server, cx));
            }
        }
    }

    /// Status-bar text and color for the `language` server serving `path`.
    fn lsp_status(
        &self,
        language: Language,
        path: &Path,
        cx: &App,
    ) -> Option<(SharedString, fn() -> Hsla)> {
        let (name, status) = self.lsp_store.read(cx).status(language, path)?;
        Some(match status {
            ServerStatus::Starting => (format!("{name}: starting…").into(), theme::muted),
            ServerStatus::Ready => (name, theme::muted),
            ServerStatus::Busy(work) => (format!("{name}: {work}").into(), theme::muted),
            ServerStatus::Disabled => (format!("{name}: disabled").into(), theme::muted),
            ServerStatus::NotFound {
                command,
                install_hint,
            } => (
                match install_hint {
                    Some(hint) => format!("{command} not found (install: {hint})"),
                    None => format!("{command} not found"),
                }
                .into(),
                theme::error,
            ),
            ServerStatus::Failed(error) => (format!("{name}: {error}").into(), theme::error),
        })
    }

    /// Opens (or activates) `path` in the active pane.
    pub fn open(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        self.open_editor(path, window, cx);
    }

    /// Opens (or activates) `path` in the active pane and returns its
    /// editor. A file open in another pane gets a second view of the same
    /// buffer there. Notebooks open in notebook tabs instead (`None`); one
    /// that cannot be read as a notebook opens as text.
    fn open_editor(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Editor>> {
        let pane = self.active_pane;
        if let Some(index) = self.active_pane().editor_index(path) {
            self.activate(pane, index, window, cx);
            return self.active_tab().and_then(Tab::editor).cloned();
        }
        // Notebooks open in notebook tabs, sharing the open document.
        if self.has_notebook_tab(path) {
            if let Err(err) = self.open_notebook(path, None, window, cx) {
                self.notice = Some(err.into());
                cx.notify();
            }
            return None;
        }
        let existing = self
            .all_tabs()
            .filter(|tab| tab.path == path)
            .find_map(Tab::editor)
            .cloned();
        let mut notebook_error = None;
        let buffer = match &existing {
            Some(editor) => editor.read(cx).buffer().link(),
            None => {
                // Taken before the read: a file that changes in between
                // looks changed to the first save instead of being
                // overwritten. (The read itself stays on the UI thread:
                // every caller uses the editor as soon as it opens.)
                let stamp = crate::editor::save::disk_stamp(&self.project, path);
                match self.project.read_text(path) {
                    Ok(text) => {
                        if self.opens_as_notebook(path, &text) {
                            match self.open_notebook(path, Some(&text), window, cx) {
                                Ok(_) => return None,
                                Err(err) => notebook_error = Some(err),
                            }
                        }
                        let mut buffer = Buffer::with_path(&text, path.to_path_buf());
                        buffer.set_disk_stamp(stamp);
                        buffer
                    }
                    Err(err) => {
                        self.notice = Some(format!("Cannot open {}: {err}", path.display()).into());
                        cx.notify();
                        return None;
                    }
                }
            }
        };
        let project = self.project.clone();
        let editor = cx.new(|cx| Editor::for_file(buffer, project, cx));
        let subscription =
            cx.subscribe_in(
                &editor,
                window,
                |this, editor, event, window, cx| match event {
                    EditorEvent::Navigate { locations, title } => {
                        this.navigate(locations.clone(), title.clone(), window, cx);
                    }
                    EditorEvent::CallHierarchy { server, items } => {
                        this.show_call_hierarchy(server.clone(), items.clone(), window, cx);
                    }
                    EditorEvent::DirtyChanged | EditorEvent::Edited => {
                        this.sync_views(editor, cx);
                        cx.notify();
                    }
                    EditorEvent::Saved => {
                        let path = this
                            .all_tabs()
                            .find(|tab| tab.editor() == Some(editor))
                            .map(|tab| tab.path.clone());
                        if let Some(path) = path {
                            this.run_file_saved(&path, cx);
                        }
                    }
                    EditorEvent::SaveConflict => {
                        if let Some(path) = editor.read(cx).buffer().path().map(Path::to_path_buf)
                            && this.reload_prompts.insert(path.clone())
                        {
                            this.prompt_overwrite(editor.clone(), path, window, cx);
                        }
                    }
                    EditorEvent::SymbolsChanged => {}
                    EditorEvent::ApplyEdit {
                        edit,
                        server,
                        command,
                        label,
                    } => this.apply_editor_edit(
                        Some(editor),
                        edit.clone(),
                        server.clone(),
                        command.clone(),
                        label,
                        cx,
                    ),
                    EditorEvent::Pick(request) => {
                        this.open_pick_request(editor.clone(), request.clone(), window, cx);
                    }
                    EditorEvent::ToggleBreakpoint { .. }
                    | EditorEvent::SetBreakpoint { .. }
                    | EditorEvent::RemoveBreakpoint { .. }
                    | EditorEvent::BreakpointsMoved(_) => {
                        this.on_editor_debug_event(editor, event, cx)
                    }
                    EditorEvent::GutterMarkerClicked { line, kind } => {
                        this.gutter_marker_clicked(editor, *line, *kind, cx)
                    }
                    EditorEvent::StageHunk { .. }
                    | EditorEvent::ShowChangeDiff { .. }
                    | EditorEvent::LoadBlame
                    | EditorEvent::ShowCommit(_) => {
                        this.on_editor_git_event(editor, event, window, cx)
                    }
                    EditorEvent::Spelling(problems) => {
                        if let Some(path) = editor.read(cx).buffer().path().map(Path::to_path_buf) {
                            this.problems
                                .update(cx, |panel, cx| panel.set_spelling(&path, problems, cx));
                        }
                    }
                    EditorEvent::Close { discard } => {
                        if let Some((pane, index)) = this.find_view(editor.entity_id()) {
                            if *discard {
                                this.remove_tab(pane, index, window, cx);
                            } else {
                                this.close_tab(pane, index, window, cx);
                            }
                        }
                    }
                    // Only notebook cell editors emit it.
                    EditorEvent::CellBoundary { .. } => {}
                },
            );
        if let Some(language) = editor.read(cx).language() {
            // A second view gets the server document when it takes focus.
            let server = match existing {
                Some(_) => None,
                None => self
                    .lsp_store
                    .update(cx, |store, cx| store.server(language, path, cx)),
            };
            let status = self.lsp_status(language, path, cx);
            editor.update(cx, |editor, cx| {
                if let Some(server) = server {
                    editor.attach_server(server, cx);
                }
                editor.set_lsp_status(status, cx);
            });
        }
        self.debug_editor_opened(&editor, path, cx);
        self.show_notes_in(&editor, cx);
        self.set_test_markers(&editor, path, cx);
        let title = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into();
        let tabs = &mut self.pane_mut(pane)?.tabs;
        tabs.push(Tab {
            content: TabContent::Editor(editor.clone()),
            path: path.to_path_buf(),
            title,
            lsp_owner: existing.is_none(),
            _subscription: subscription,
        });
        let index = tabs.len() - 1;
        self.refresh_diagnostics(path, cx);
        self.activate(pane, index, window, cx);
        if let Some(err) = notebook_error {
            self.notice = Some(err.into());
        }
        self.refresh_git_gutters(cx);
        Some(editor)
    }

    /// Brings the other views of `source`'s file up to date with its edits.
    fn sync_views(&mut self, source: &Entity<Editor>, cx: &mut Context<Self>) {
        let Some(path) = self
            .all_tabs()
            .find(|tab| tab.editor() == Some(source))
            .map(|tab| tab.path.clone())
        else {
            return;
        };
        let others: Vec<Entity<Editor>> = self
            .all_tabs()
            .filter(|tab| tab.path == path)
            .filter_map(Tab::editor)
            .filter(|editor| *editor != source)
            .cloned()
            .collect();
        for other in others {
            other.update(cx, |editor, cx| editor.sync_buffer(cx));
        }
    }

    /// Shows tab `index` of `pane` and focuses it.
    fn activate(
        &mut self,
        pane: PaneId,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.pane_mut(pane).and_then(|target| {
            let tab = target.tabs.get(index)?;
            target.active = index;
            Some(tab)
        }) else {
            return;
        };
        window.focus(&tab.focus_handle(cx));
        let path = tab.path.clone();
        self.active_pane = pane;
        self.notice = None;
        self.file_tree.update(cx, |tree, cx| tree.select(&path, cx));
        self.claim_server(pane, index, cx);
        self.offer_notebook_view(pane, index, cx);
        self.schedule_session_save(cx);
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
        // Locations in a notebook's virtual document read as its cells.
        let store = self.lsp_store.read(cx);
        let labels: Vec<Option<(String, String)>> = locations
            .iter()
            .map(|location| {
                let notebook = store.notebook(&location.path)?;
                let notebook = notebook.borrow();
                let target = notebook.cell_position(location.range.start)?;
                let name = notebook.notebook_path().file_name()?.to_string_lossy();
                let preview = notebook.line_text(location.range.start.line);
                Some((
                    format!(
                        "{name} [cell {}]:{}",
                        target.number,
                        target.position.line + 1
                    ),
                    preview.unwrap_or_default().trim().to_owned(),
                ))
            })
            .collect();
        let picker =
            cx.new(|cx| LocationPicker::with_labels(title, locations, labels, &self.project, cx));
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
        if self.jump_to_notebook(location, window, cx) {
            return;
        }
        if let Some(editor) = self.open_editor(&location.path, window, cx) {
            editor.update(cx, |editor, cx| editor.go_to(location.range.start, cx));
        }
    }

    fn current_position(&self, cx: &App) -> Option<NavEntry> {
        let tab = self.active_tab()?;
        Some(NavEntry {
            path: tab.path.clone(),
            offset: tab.editor()?.read(cx).cursor(),
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

    /// Focuses the active tab (or the workspace when none is open).
    pub fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_tab() {
            Some(tab) => window.focus(&tab.focus_handle(cx)),
            None => window.focus(&self.focus_handle),
        }
        cx.notify();
    }

    /// Shows a suggestion (once per kind).
    pub(crate) fn offer(&mut self, kind: OfferKind, message: &str, cx: &mut Context<Self>) {
        self.offers.retain(|offer| offer.kind != kind);
        self.offers.push(Offer {
            kind,
            message: message.to_owned().into(),
        });
        cx.notify();
    }

    /// Removes a suggestion, e.g. once its condition is gone.
    pub(crate) fn withdraw_offer(&mut self, kind: &OfferKind, cx: &mut Context<Self>) {
        let before = self.offers.len();
        self.offers.retain(|offer| &offer.kind != kind);
        if self.offers.len() != before {
            cx.notify();
        }
    }

    fn accept_offer(&mut self, kind: OfferKind, window: &mut Window, cx: &mut Context<Self>) {
        self.withdraw_offer(&kind, cx);
        match kind {
            OfferKind::PythonSync => self.python.update(cx, |python, cx| python.sync(cx)),
            OfferKind::ReopenInContainer { config } => self.reopen_in(config, cx),
            OfferKind::DownloadCodeLldb => self.download_codelldb(window, cx),
            OfferKind::PortAvailable { url, .. } => cx.open_url(&url),
            OfferKind::OpenAsNotebook { path } => self.reopen_as_notebook(&path, window, cx),
            OfferKind::NoteMessage { root, file, id } => {
                self.open_note(root, &file, &id, window, cx)
            }
            OfferKind::TrustFolder => {
                if let Err(err) = crate::trust::grant(&self.project) {
                    self.notice = Some(format!("Cannot save the trusted folders: {err}").into());
                }
                // Folder settings are layered again, now with their programs.
                self.load_folder_settings(cx);
            }
        }
    }

    /// Asks the window to show this folder inside its dev container instead.
    fn reopen_in(&mut self, config: String, cx: &mut Context<Self>) {
        self.offers
            .retain(|offer| !matches!(offer.kind, OfferKind::ReopenInContainer { .. }));
        cx.emit(WorkspaceEvent::ReplaceWith(
            project::Location::DevContainer {
                folder: Box::new(self.project.location()),
                config: Some(config),
            },
        ));
    }

    fn reopen_in_container(
        &mut self,
        _: &ReopenInContainer,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let project::Location::DevContainer { .. } = self.project.location() {
            self.notice = Some("This project is already open in its dev container".into());
            return cx.notify();
        }
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            let configs = cx
                .background_spawn(async move { project.devcontainer_configs() })
                .await;
            this.update(cx, |this, cx| match configs {
                Ok(configs) if !configs.is_empty() => {
                    this.reopen_in(configs.into_iter().next().unwrap_or_default(), cx)
                }
                Ok(_) => {
                    this.notice = Some(
                        "This folder has no dev container (.devcontainer/devcontainer.json)".into(),
                    );
                    cx.notify();
                }
                Err(err) => {
                    this.notice = Some(format!("Could not look for a dev container: {err}").into());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn save_all(&mut self, _: &SaveAll, _: &mut Window, cx: &mut Context<Self>) {
        // Once per file, through the view holding its server document (a
        // notebook's first view).
        let dirty: Vec<TabContent> = self
            .file_tabs()
            .into_iter()
            .filter(|tab| tab.is_dirty(cx))
            .map(|tab| tab.content.clone())
            .collect();
        for content in dirty {
            content.save(cx).detach();
        }
    }

    fn new_terminal(&mut self, _: &NewTerminal, window: &mut Window, cx: &mut Context<Self>) {
        self.show_panel(PanelId::Terminal, cx);
        let near = self.active_tab().map(|tab| tab.path.clone());
        self.terminal.update(cx, |terminal, cx| {
            terminal.new_terminal(near.as_deref(), window, cx)
        });
    }

    /// The repository git commands act on (the active editor's file's,
    /// else the first one), or a notice saying there is none (yet).
    fn repository_or_notice(&mut self, cx: &mut Context<Self>) -> Option<git::Repository> {
        let repository = self.active_repository(cx);
        if repository.is_none() {
            self.notice = Some("No git repository here (or its status is still loading)".into());
            cx.notify();
        }
        repository
    }

    fn git_branches(
        &mut self,
        _: &git_commands::Branches,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_branches_popup(window, cx);
    }

    fn git_worktrees(
        &mut self,
        _: &git_commands::Worktrees,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(repository) = self.repository_or_notice(cx) {
            self.open_worktree_picker(repository, window, cx);
        }
    }

    fn git_compare(
        &mut self,
        _: &git_commands::Compare,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(repository) = self.repository_or_notice(cx) {
            self.pick_revision(repository, RevisionPurpose::NewCompare, window, cx);
        }
    }

    fn git_sync(&mut self, action: SyncAction, cx: &mut Context<Self>) {
        if self.repository_or_notice(cx).is_some() {
            self.show_panel(PanelId::Commit, cx);
            self.commit.update(cx, |commit, cx| commit.sync(action, cx));
        }
    }

    fn git_fetch(&mut self, _: &git_commands::Fetch, _: &mut Window, cx: &mut Context<Self>) {
        self.git_sync(SyncAction::Fetch, cx);
    }

    fn git_pull(&mut self, _: &git_commands::Pull, _: &mut Window, cx: &mut Context<Self>) {
        self.git_sync(SyncAction::Pull, cx);
    }

    fn git_push(&mut self, _: &git_commands::Push, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(repository) = self.repository_or_notice(cx) {
            self.open_push_dialog(repository, window, cx);
        }
    }

    fn git_refresh(&mut self, _: &git_commands::Refresh, _: &mut Window, cx: &mut Context<Self>) {
        self.commit.update(cx, |commit, cx| commit.refresh(cx));
    }

    fn python_sync(&mut self, _: &python_commands::Sync, _: &mut Window, cx: &mut Context<Self>) {
        self.show_panel(PanelId::Python, cx);
        self.python.update(cx, |python, cx| python.sync(cx));
    }

    fn python_lock(&mut self, _: &python_commands::Lock, _: &mut Window, cx: &mut Context<Self>) {
        self.show_panel(PanelId::Python, cx);
        self.python.update(cx, |python, cx| python.lock(cx));
    }

    fn run_file(
        &mut self,
        _: &python_commands::RunFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_python(RunRequest::CurrentFile, window, cx);
    }

    fn run_tests(
        &mut self,
        _: &python_commands::RunTests,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_python(RunRequest::Tests, window, cx);
    }

    fn run_test_file(
        &mut self,
        _: &python_commands::RunTestFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_python(RunRequest::TestFile, window, cx);
    }

    fn run_test_at_cursor(
        &mut self,
        _: &python_commands::RunTestAtCursor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_python(RunRequest::TestAtCursor, window, cx);
    }

    fn render_offers(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        self.offers
            .iter()
            .enumerate()
            .map(|(index, offer)| {
                let button = |id: &'static str, label: &'static str| {
                    div()
                        .id((id, index))
                        .px_2()
                        .rounded_sm()
                        .border_1()
                        .border_color(theme::border())
                        .text_xs()
                        .cursor_pointer()
                        .hover(|style| style.bg(theme::hover_row()))
                        .child(label)
                };
                let (accept_kind, dismiss_kind) = (offer.kind.clone(), offer.kind.clone());
                div()
                    .px_3()
                    .py_1()
                    .flex()
                    .items_center()
                    .gap_2()
                    .bg(theme::panel())
                    .border_b_1()
                    .border_color(theme::border())
                    .text_sm()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(selectable(("offer", index), offer.message.clone())),
                    )
                    .child(
                        button("offer-accept", offer.kind.action())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.accept_offer(accept_kind.clone(), window, cx)
                            }))
                            .pin("accept")
                            .label(offer.kind.action()),
                    )
                    .child(
                        button("offer-dismiss", "Not now")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.withdraw_offer(&dismiss_kind, cx)
                            }))
                            .pin("dismiss")
                            .label("Not now"),
                    )
                    .pin(offer.kind.pin_id())
                    .label(offer.message.clone())
                    .into_any_element()
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn file_finder_query(&self, cx: &App) -> Option<Entity<Editor>> {
        let (finder, _) = self.file_finder.as_ref()?;
        Some(finder.read(cx).query().clone())
    }

    #[cfg(test)]
    pub(crate) fn active_editor(&self) -> Option<Entity<Editor>> {
        self.active_tab().and_then(Tab::editor).cloned()
    }

    /// The files of every tab, pane by pane.
    #[cfg(test)]
    pub(crate) fn tab_paths(&self) -> Vec<PathBuf> {
        self.all_tabs().map(|tab| tab.path.clone()).collect()
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
        let finder = cx.new(|cx| FileFinder::new(self.project.clone(), cx));
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
        match self.active_tab() {
            Some(tab) => window.focus(&tab.focus_handle(cx)),
            None => window.focus(&self.focus_handle),
        }
        cx.notify();
    }

    fn close_active(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active_pane(window, cx);
        let pane = self.active_pane();
        if !pane.tabs.is_empty() {
            self.close_tab(pane.id, pane.active, window, cx);
        }
    }

    /// Closes a tab, asking to save first if its unsaved changes would be
    /// lost (no other pane shows the file).
    fn close_tab(
        &mut self,
        pane: PaneId,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.pane(pane).and_then(|pane| pane.tabs.get(index)) else {
            return;
        };
        let Some(content) =
            (tab.is_dirty(cx) && !self.has_other_view(tab)).then(|| tab.content.clone())
        else {
            self.remove_tab(pane, index, window, cx);
            return;
        };
        let message = format!("Save changes to {}?", tab.title);
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
            let close = match answer {
                // Closes once the write is done; a failed save stays open.
                0 => match this.update(cx, |_, cx| content.save(cx)) {
                    Ok(save) => save.await,
                    Err(_) => return,
                },
                1 => true,
                _ => false,
            };
            if !close {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                // The tabs may have changed while the prompt was open.
                if let Some((pane, index)) = this.find_view(content.entity_id()) {
                    this.remove_tab(pane, index, window, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Open files with unsaved changes (a file open in two panes counts once).
    pub fn dirty_count(&self, cx: &App) -> usize {
        self.file_tabs()
            .into_iter()
            .filter(|tab| tab.is_dirty(cx))
            .count()
    }

    /// What `next_code context` reads (see `pinpoint::context`): handles
    /// only, the caller reads what it reports.
    pub(crate) fn context_parts(&self, window: &Window, cx: &App) -> pinpoint::context::Parts {
        use pinpoint::context::{ActiveView, TabPart};
        let active_id = self.active_tab().map(Tab::entity_id);
        let tabs = self
            .all_tabs()
            .map(|tab| TabPart {
                path: tab.path.clone(),
                title: tab.title.to_string(),
                kind: match tab.content {
                    TabContent::Editor(_) => "editor",
                    TabContent::Notebook(_) => "notebook",
                    TabContent::Diff(_) => "diff",
                    TabContent::Compare(_) => "compare",
                },
                dirty: tab.is_dirty(cx),
                active: Some(tab.entity_id()) == active_id,
            })
            .collect();
        let active = self.active_tab().map(|tab| match &tab.content {
            TabContent::Editor(editor) => ActiveView::Editor(editor.clone()),
            TabContent::Notebook(view) => ActiveView::Notebook(view.clone()),
            TabContent::Diff(_) | TabContent::Compare(_) => ActiveView::Other,
        });
        let open_panels: Vec<PanelId> = [&self.layout.left, &self.layout.right, &self.layout.bottom]
            .into_iter()
            .filter(|dock| dock.open)
            .filter_map(|dock| dock.active)
            .collect();
        // Focus inside a tab or a panel names the area; a panel without its
        // own focus handle (Problems) never does.
        let focused = if self
            .active_tab()
            .is_some_and(|tab| tab.focus_handle(cx).contains_focused(window, cx))
        {
            Some("editor".to_owned())
        } else {
            open_panels
                .iter()
                .copied()
                .find(|panel| {
                    let handle = match panel {
                        PanelId::Files => self.file_tree.focus_handle(cx),
                        PanelId::Search => self.search.focus_handle(cx),
                        PanelId::Commit => self.commit.focus_handle(cx),
                        PanelId::Python => self.python.focus_handle(cx),
                        PanelId::Terminal => {
                            let (view, _) = self.terminal.read(cx).active_view();
                            match view {
                                Some(view) => view.focus_handle(cx),
                                None => self.terminal.focus_handle(cx),
                            }
                        }
                        PanelId::Problems | PanelId::Notes => return false,
                        PanelId::GitLog => self.git_log.focus_handle(cx),
                        PanelId::Ports => self.ports_panel.focus_handle(cx),
                        PanelId::Outline => self.outline.focus_handle(cx),
                        PanelId::CallHierarchy => self.call_hierarchy.focus_handle(cx),
                        PanelId::Debug => self.debug.panel.focus_handle(cx),
                        PanelId::DebugConsole => self.debug.console.focus_handle(cx),
                        PanelId::Tests => self.runs.tests.focus_handle(cx),
                    };
                    handle.contains_focused(window, cx)
                })
                .map(|panel| format!("panel:{}", panel.key()))
        };
        pinpoint::context::Parts {
            project: self.project.clone(),
            tabs,
            active,
            focused,
            open_panels,
            terminal: self.terminal.clone(),
            problems: self.problems.clone(),
            tests: self.runs.tests.clone(),
            debug_session: self.debug_session(),
        }
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::startup::mark_once("first workspace render");
        // The Outline shows the active editor's symbols.
        let active_editor = self.active_tab().and_then(Tab::editor).cloned();
        self.outline
            .update(cx, |outline, cx| outline.set_editor(active_editor, cx));
        // Flex, not block, down to the panes' content: in block containers
        // a right dock left inputs over an editor (go to line, rename) laid
        // out zero wide. See also the editor area in `Shell::render`.
        let main = div()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .flex()
            .child(self.render_panes(cx));

        div()
            .key_context(self.workspace_key_context(cx))
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::close_active))
            .on_action(cx.listener(Self::toggle_file_finder))
            .on_action(cx.listener(Self::save_all))
            .on_action(cx.listener(Self::new_terminal))
            .on_action(cx.listener(Self::new_file))
            .on_action(cx.listener(Self::new_python_file))
            .on_action(cx.listener(Self::reopen_in_container))
            .on_action(cx.listener(Self::add_folder_to_workspace))
            .on_action(cx.listener(Self::save_workspace_as))
            .on_action(cx.listener(Self::git_branches))
            .on_action(cx.listener(Self::git_worktrees))
            .on_action(cx.listener(Self::git_compare))
            .on_action(cx.listener(Self::git_fetch))
            .on_action(cx.listener(Self::git_pull))
            .on_action(cx.listener(Self::git_push))
            .on_action(cx.listener(Self::git_refresh))
            .map(|element| self.on_git_actions(element, cx))
            .map(|element| self.on_log_actions(element, cx))
            .map(|element| self.on_stash_actions(element, cx))
            .on_action(cx.listener(Self::python_sync))
            .on_action(cx.listener(Self::python_lock))
            .on_action(cx.listener(Self::run_file))
            .on_action(cx.listener(Self::run_tests))
            .on_action(cx.listener(Self::run_test_file))
            .on_action(cx.listener(Self::run_test_at_cursor))
            .on_action(cx.listener(Self::open_as_notebook))
            .on_action(cx.listener(Self::go_back))
            .on_action(cx.listener(Self::go_forward))
            .on_action(cx.listener(Self::toggle_files))
            .on_action(cx.listener(Self::toggle_search))
            .on_action(cx.listener(Self::replace_in_files))
            .on_action(cx.listener(Self::toggle_commit))
            .on_action(cx.listener(Self::toggle_terminal))
            .on_action(cx.listener(Self::toggle_python))
            .on_action(cx.listener(Self::toggle_problems))
            .on_action(cx.listener(Self::toggle_notes))
            .on_action(cx.listener(Self::toggle_ports))
            .on_action(cx.listener(Self::forward_port))
            .on_action(cx.listener(Self::toggle_call_hierarchy))
            .on_action(cx.listener(Self::toggle_outline))
            .on_action(cx.listener(Self::go_to_workspace_symbol))
            .on_action(cx.listener(Self::toggle_bottom_dock))
            .on_action(cx.listener(Self::save_layout_as_default))
            .on_action(cx.listener(Self::reset_layout))
            .on_action(cx.listener(Self::split_right))
            .on_action(cx.listener(Self::split_down))
            .on_action(cx.listener(Self::focus_next_pane))
            .on_action(cx.listener(Self::focus_previous_pane))
            .on_action(cx.listener(Self::focus_first_pane))
            .on_action(cx.listener(Self::focus_second_pane))
            .on_action(cx.listener(Self::focus_third_pane))
            .on_action(cx.listener(Self::move_tab_to_next_pane))
            .on_action(cx.listener(Self::move_tab_to_previous_pane))
            .on_action(cx.listener(Self::close_pane_action))
            .on_action(cx.listener(Self::run_configuration))
            .on_action(cx.listener(Self::debug_configuration))
            .on_action(cx.listener(Self::select_configuration))
            .on_action(cx.listener(Self::edit_configurations))
            .on_action(cx.listener(Self::run_all_tests))
            .on_action(cx.listener(Self::run_failed_tests))
            .on_action(cx.listener(Self::toggle_tests))
            .map(|element| self.on_debug_actions(element, cx))
            .on_mouse_move(cx.listener(Self::on_resize_move))
            .on_mouse_up(gpui::MouseButton::Left, cx.listener(Self::on_resize_end))
            .relative()
            .size_full()
            .flex()
            .bg(theme::background())
            .text_color(theme::text())
            .children(self.render_dock(DockPosition::Left, window, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .children(
                        self.connection_error
                            .iter()
                            .map(|notice| ("connection-error", notice))
                            .chain(self.notice.iter().map(|notice| ("notice", notice)))
                            .enumerate()
                            .map(|(index, (pin, notice))| {
                                div()
                                    .px_3()
                                    .py_1()
                                    .text_sm()
                                    .text_color(theme::error())
                                    .child(selectable(("notice", index), notice.clone()))
                                    .pin(pin)
                                    .interactive(false)
                            }),
                    )
                    .children(self.render_offers(cx))
                    .children(self.render_debug_bar(cx))
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
            .children(self.symbol_picker.as_ref().map(|(picker, _)| {
                div()
                    .absolute()
                    .top(px(72.))
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(picker.clone())
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
            .children(self.revision_picker.as_ref().map(|(picker, _)| {
                div()
                    .absolute()
                    .top(px(72.))
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(picker.clone())
            }))
            .children(self.git_modal.as_ref().map(|(picker, _)| {
                div()
                    .absolute()
                    .top(px(72.))
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    .child(picker.clone())
            }))
            .children(self.render_branches_popup(cx))
            .children(self.render_configuration_picker())
            .pin("workspace")
    }
}
