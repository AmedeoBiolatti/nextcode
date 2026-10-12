//! Main window: docks with panels, editor tabs, and close/save confirmation.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use futures::StreamExt;

use gpui::{
    AnyView, App, AppContext, Bounds, Context, Entity, FocusHandle, Focusable, KeyBinding,
    PathPromptOptions, PromptLevel, SharedString, Subscription, Task, TitlebarOptions, Window,
    WindowBounds, WindowOptions, actions, div, prelude::*, px, rgb, size,
};
use language::Location;
use syntax::Language;
use text::Buffer;
use watcher::{Batch, ChangeKind};

use crate::{
    branch_picker::{BranchPicker, BranchPickerEvent},
    compare_view::{CompareEvent, CompareView},
    connecting::{Connecting, PreloadedConnection},
    diff_view::{DiffSource, DiffView, DiffViewEvent},
    editor::{Editor, EditorEvent, ReloadOutcome},
    file_finder::{FileFinder, FileFinderEvent},
    file_tree::{FileTree, OpenFile},
    git_panel::{GitPanel, GitPanelEvent, PreloadedRepository},
    history_view::{HistoryEvent, HistoryView},
    layout::{DockPosition, Layout},
    location_picker::{LocationPicker, LocationPickerEvent},
    lsp_store::{LspStore, LspStoreEvent, ServerStatus},
    problems::{ProblemsEvent, ProblemsPanel},
    project::{self, Project, WatchHandle},
    project_picker::{ProjectPicker, ProjectPickerEvent},
    recent,
    revision_picker::{RevisionPicker, RevisionPickerEvent},
    search_panel::{SearchPanel, SearchPanelEvent},
    settings::Settings,
    terminal_panel::TerminalPanel,
    theme,
    worktree_picker::{WorktreePicker, WorktreePickerEvent},
};

mod docks;

actions!(
    workspace,
    [
        CloseTab,
        ToggleFileFinder,
        GoBack,
        GoForward,
        OpenFolder,
        OpenRecent
    ]
);

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
        KeyBinding::new("secondary-o", OpenFolder, ctx),
        KeyBinding::new("secondary-r", OpenRecent, ctx),
    ]);
    docks::bind_keys(cx);
}

/// Work for the first window started before gpui (whose platform setup
/// takes a few hundred milliseconds): connecting to WSL, or reading a local
/// project's git status.
pub enum Preload {
    Local(PreloadedRepository),
    Wsl(PreloadedConnection),
}

impl Preload {
    /// Starts the work for `location` on a thread.
    pub fn start(location: &project::Location) -> Self {
        use futures::channel::oneshot;
        match location {
            project::Location::Local(root) => {
                let (tx, rx) = oneshot::channel();
                let project = Project::local(root.clone());
                std::thread::spawn(move || tx.send(project.open_repository()).ok());
                Self::Local(rx)
            }
            project::Location::Wsl { distro, path } => {
                let (tx, rx) = oneshot::channel();
                let (distro, path) = (distro.clone(), path.clone());
                std::thread::spawn(move || tx.send(Project::connect_wsl(&distro, &path)).ok());
                Self::Wsl(rx)
            }
        }
    }
}

/// Opens `location` in a new window, or brings forward the window that
/// already shows it. Each window is an independent workspace.
pub fn open_project(location: project::Location, cx: &mut App) {
    open_preloaded(location, None, cx);
}

/// [`open_project`] with work started by [`Preload::start`] for the same
/// location.
pub fn open_preloaded(location: project::Location, preload: Option<Preload>, cx: &mut App) {
    let location = match location {
        project::Location::Local(root) => {
            project::Location::Local(std::path::absolute(&root).unwrap_or(root))
        }
        wsl => wsl,
    };
    for window in cx.windows() {
        let shows = if let Some(workspace) = window.downcast::<Workspace>() {
            workspace
                .read(cx)
                .is_ok_and(|workspace| workspace.project.location() == location)
        } else if let Some(connecting) = window.downcast::<Connecting>() {
            connecting
                .read(cx)
                .is_ok_and(|connecting| connecting.location() == location)
        } else {
            false
        };
        if shows {
            window
                .update(cx, |_, window, _| window.activate_window())
                .ok();
            return;
        }
    }

    let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(format!("{location} — next_code").into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let opened = match (location, preload) {
        (project::Location::Local(root), preload) => {
            let git = match preload {
                Some(Preload::Local(git)) => Some(git),
                _ => None,
            };
            cx.open_window(options, |window, cx| {
                cx.new(|cx| Workspace::new(Project::local(root), git, window, cx))
            })
            .map(drop)
        }
        (project::Location::Wsl { distro, path }, preload) => {
            let connection = match preload {
                Some(Preload::Wsl(connection)) => Some(connection),
                _ => None,
            };
            cx.open_window(options, |window, cx| {
                cx.new(|cx| Connecting::new(distro, path, connection, window, cx))
            })
            .map(drop)
        }
    };
    opened.expect("failed to open window");
}

/// A remembered cursor position for Back/Forward.
struct NavEntry {
    path: PathBuf,
    offset: usize,
}

enum TabContent {
    Editor(Entity<Editor>),
    Diff(Entity<DiffView>),
    History(Entity<HistoryView>),
    Compare(Entity<CompareView>),
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
    _subscription: Subscription,
}

impl Tab {
    fn editor(&self) -> Option<&Entity<Editor>> {
        match &self.content {
            TabContent::Editor(editor) => Some(editor),
            _ => None,
        }
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.content {
            TabContent::Editor(editor) => editor.focus_handle(cx),
            TabContent::Diff(diff) => diff.focus_handle(cx),
            TabContent::History(view) => view.focus_handle(cx),
            TabContent::Compare(view) => view.focus_handle(cx),
        }
    }

    fn view(&self) -> AnyView {
        match &self.content {
            TabContent::Editor(editor) => editor.clone().into(),
            TabContent::Diff(diff) => diff.clone().into(),
            TabContent::History(view) => view.clone().into(),
            TabContent::Compare(view) => view.clone().into(),
        }
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.editor()
            .is_some_and(|editor| editor.read(cx).buffer().is_dirty())
    }
}

pub struct Workspace {
    focus_handle: FocusHandle,
    project: Project,
    file_tree: Entity<FileTree>,
    problems: Entity<ProblemsPanel>,
    terminal: Entity<TerminalPanel>,
    search: Entity<SearchPanel>,
    git: Entity<GitPanel>,
    layout: Layout,
    /// Dock being resized by dragging its handle.
    resizing: Option<DockPosition>,
    panel_menu: Option<docks::PanelMenu>,
    file_finder: Option<(Entity<FileFinder>, Subscription)>,
    location_picker: Option<(Entity<LocationPicker>, Subscription)>,
    project_picker: Option<(Entity<ProjectPicker>, Subscription)>,
    revision_picker: Option<(Entity<RevisionPicker>, Subscription)>,
    /// The branch or worktree picker.
    git_modal: Option<(AnyView, Subscription)>,
    back: Vec<NavEntry>,
    forward: Vec<NavEntry>,
    lsp_store: Entity<LspStore>,
    tabs: Vec<Tab>,
    active: usize,
    notice: Option<SharedString>,
    /// Why the WSL agent connection was lost; stays until restart.
    connection_error: Option<SharedString>,
    /// Set once the user confirmed discarding unsaved changes on window close.
    close_confirmed: bool,
    /// Files with an open "changed on disk" prompt, so it is asked once.
    reload_prompts: HashSet<PathBuf>,
    _watcher: Option<(WatchHandle, Task<()>)>,
    _disconnects: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(
        project: Project,
        git_preload: Option<PreloadedRepository>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        crate::startup::mark("workspace start");
        window.on_next_frame(|_, _| crate::startup::mark("first frame"));
        let root = project.root().to_path_buf();
        let file_tree = cx.new(|_| FileTree::new(project.clone()));
        let tree_subscription = cx.subscribe_in(
            &file_tree,
            window,
            |this, _, event: &OpenFile, window, cx| {
                this.open(&event.0, window, cx);
            },
        );

        let (settings, settings_error) = Settings::load();
        let lsp_store = cx.new(|_| LspStore::new(project.clone(), Arc::new(settings)));
        let lsp_subscription = cx.subscribe(&lsp_store, |this, _, event, cx| {
            this.on_lsp_event(event, cx);
        });
        let problems = cx.new(|cx| ProblemsPanel::new(root.clone(), lsp_store.clone(), cx));
        let terminal = cx.new(|cx| TerminalPanel::new(project.clone(), cx));
        let search = cx.new(|cx| SearchPanel::new(project.clone(), cx));
        let git = cx.new(|cx| GitPanel::new(project.clone(), git_preload, cx));
        let git_subscription =
            cx.subscribe_in(&git, window, |this, _, event, window, cx| match event {
                GitPanelEvent::OpenDiff { repository, source } => {
                    this.open_diff(repository.clone(), source.clone(), window, cx)
                }
                GitPanelEvent::OpenFile(path) => this.open(path, window, cx),
                GitPanelEvent::OpenHistory(repository) => {
                    this.open_history(repository.clone(), window, cx)
                }
                GitPanelEvent::Compare(repository) => {
                    this.pick_revision(repository.clone(), RevisionPurpose::NewCompare, window, cx)
                }
                GitPanelEvent::Branches(repository) => {
                    this.open_branch_picker(repository.clone(), None, window, cx)
                }
                GitPanelEvent::Worktrees(repository) => {
                    this.open_worktree_picker(repository.clone(), window, cx)
                }
                GitPanelEvent::Refreshed => this.reload_diffs(cx),
            });
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
        let layout = Layout::load(&project.state_key());
        let mut notice = settings_error.map(|error| format!("Settings ignored: {error}").into());
        let watcher = match project.watch(Duration::from_millis(150)) {
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
        recent::record(&project.location());
        crate::startup::mark("workspace built");
        Self {
            focus_handle,
            project,
            file_tree,
            problems,
            terminal,
            search,
            git,
            layout,
            resizing: None,
            panel_menu: None,
            file_finder: None,
            location_picker: None,
            project_picker: None,
            revision_picker: None,
            git_modal: None,
            back: Vec::new(),
            forward: Vec::new(),
            lsp_store,
            tabs: Vec::new(),
            active: 0,
            notice,
            connection_error: None,
            close_confirmed: false,
            reload_prompts: HashSet::new(),
            _watcher: watcher,
            _disconnects: disconnects,
            _subscriptions: vec![
                tree_subscription,
                lsp_subscription,
                problems_subscription,
                search_subscription,
                git_subscription,
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
        // Worktree edits change status as much as commits do.
        self.git.update(cx, |git, cx| git.schedule_refresh(cx));
        if batch.repository || batch.rescan {
            self.reload_history(cx);
        }

        let affected: Vec<(Entity<Editor>, PathBuf, bool)> = self
            .tabs
            .iter()
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

    /// Opens (or activates) a diff tab.
    pub(crate) fn open_diff(
        &mut self,
        repository: git::Repository,
        source: DiffSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = self.project.repository_path(&repository, source.path());
        let existing = self.tabs.iter().position(|tab| match &tab.content {
            TabContent::Diff(diff) => *diff.read(cx).source() == source,
            _ => false,
        });
        if let Some(index) = existing {
            if let TabContent::Diff(diff) = &self.tabs[index].content {
                diff.update(cx, |diff, cx| diff.reload(cx));
            }
            return self.activate(index, window, cx);
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
        self.tabs.push(Tab {
            content,
            path,
            title,
            _subscription: subscription,
        });
        self.activate(self.tabs.len() - 1, window, cx);
    }

    /// Opens (or activates) the commit history tab.
    fn open_history(
        &mut self,
        repository: git::Repository,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| matches!(tab.content, TabContent::History(_)))
        {
            return self.activate(index, window, cx);
        }
        let view = cx.new(|cx| HistoryView::new(repository, cx));
        let subscription =
            cx.subscribe_in(&view, window, |this, _, event, window, cx| match event {
                HistoryEvent::OpenDiff { repository, source } => {
                    this.open_diff(repository.clone(), source.clone(), window, cx)
                }
                HistoryEvent::Compare {
                    repository,
                    base,
                    target,
                } => {
                    this.open_compare(repository.clone(), base.clone(), target.clone(), window, cx)
                }
                HistoryEvent::CreateBranch { repository, start } => {
                    this.open_branch_picker(repository.clone(), Some(start.clone()), window, cx)
                }
            });
        let root = self.project.root().to_path_buf();
        self.push_tab(
            TabContent::History(view),
            root,
            "Git History".into(),
            subscription,
            window,
            cx,
        );
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
        let existing = self.tabs.iter().position(|tab| match &tab.content {
            TabContent::Compare(view) => view.read(cx).sides() == (&base, &target),
            _ => false,
        });
        if let Some(index) = existing {
            if let TabContent::Compare(view) = &self.tabs[index].content {
                view.update(cx, |view, cx| view.reload(cx));
            }
            return self.activate(index, window, cx);
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
                    if let Some(tab) = this.tabs.iter_mut().find(|tab| match &tab.content {
                        TabContent::Compare(compare) => compare == view,
                        _ => false,
                    }) {
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

    fn open_branch_picker(
        &mut self,
        repository: git::Repository,
        start: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let picker = cx.new(|cx| BranchPicker::new(repository, start, cx));
        let subscription = cx.subscribe_in(&picker, window, |this, _, event, window, cx| {
            this.git_modal = None;
            this.focus_active(window, cx);
            if let BranchPickerEvent::Changed(message) = event {
                this.repository_changed(Some(message.clone()), cx);
            }
        });
        window.focus(&picker.focus_handle(cx));
        self.git_modal = Some((picker.into(), subscription));
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
                    let location = this.project.location_of(path);
                    this.repository_changed(None, cx);
                    open_project(location, cx);
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

    /// Re-reads git state everywhere after an operation from a view or
    /// picker; `message` is reported in the Git panel.
    fn repository_changed(&mut self, message: Option<String>, cx: &mut Context<Self>) {
        self.git.update(cx, |git, cx| {
            if let Some(message) = message {
                git.report(message, cx);
            }
            git.refresh(cx);
        });
        self.reload_history(cx);
    }

    /// Recomputes open diffs and comparisons after the repository or the
    /// working tree changed.
    fn reload_diffs(&mut self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            match &tab.content {
                TabContent::Diff(diff) => diff.update(cx, |diff, cx| diff.reload(cx)),
                TabContent::Compare(view) => view.update(cx, |view, cx| view.reload(cx)),
                TabContent::Editor(_) | TabContent::History(_) => {}
            }
        }
    }

    /// Re-reads history tabs after commits, checkouts or ref changes.
    fn reload_history(&mut self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            if let TabContent::History(view) = &tab.content {
                view.update(cx, |view, cx| view.reload(cx));
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
        let Some(editor) = self.editor_tab(path).and_then(Tab::editor) else {
            return;
        };
        let store = self.lsp_store.read(cx);
        let diagnostics = store.diagnostics(path).to_vec();
        editor.update(cx, |editor, cx| editor.set_diagnostics(&diagnostics, cx));
    }

    /// The editor tab of `path`, if open.
    fn editor_tab(&self, path: &Path) -> Option<&Tab> {
        self.tabs
            .iter()
            .find(|tab| tab.path == path && tab.editor().is_some())
    }

    fn editors_for(&self, language: Language, cx: &App) -> Vec<Entity<Editor>> {
        self.tabs
            .iter()
            .filter_map(Tab::editor)
            .filter(|editor| editor.read(cx).language() == Some(language))
            .cloned()
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
        if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.path == path && tab.editor().is_some())
        {
            self.activate(index, window, cx);
            return;
        }
        let buffer = match self.project.read_text(path) {
            Ok(text) => Buffer::with_path(&text, path.to_path_buf()),
            Err(err) => {
                self.notice = Some(format!("Cannot open {}: {err}", path.display()).into());
                cx.notify();
                return;
            }
        };
        let project = self.project.clone();
        let editor = cx.new(|cx| Editor::for_file(buffer, project, cx));
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
            content: TabContent::Editor(editor),
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
        window.focus(&tab.focus_handle(cx));
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
        let picker = cx.new(|cx| LocationPicker::new(title, locations, &self.project, cx));
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
        self.editor_tab(path).and_then(Tab::editor).cloned()
    }

    fn current_position(&self, cx: &App) -> Option<NavEntry> {
        let tab = self.tabs.get(self.active)?;
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

    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.tabs.get(self.active) {
            Some(tab) => window.focus(&tab.focus_handle(cx)),
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

    /// `Ctrl+O`: picks a folder and opens it in its own window. A
    /// `\\wsl.localhost\...` folder opens as a WSL project.
    fn open_folder(&mut self, _: &OpenFolder, _: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open Folder".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(location) = paths
                .first()
                .and_then(|path| project::Location::parse(&path.to_string_lossy()))
            else {
                return;
            };
            this.update(cx, |this, cx| this.open_location(location, cx))
                .ok();
        })
        .detach();
    }

    /// Opens another project, checking local folders exist first (WSL
    /// paths are checked by the agent while connecting).
    fn open_location(&mut self, location: project::Location, cx: &mut Context<Self>) {
        if let project::Location::Local(root) = &location
            && !root.is_dir()
        {
            self.notice = Some(format!("{} is not a folder", root.display()).into());
            cx.notify();
            return;
        }
        open_project(location, cx);
    }

    /// `Ctrl+R`: recent projects, or a typed location.
    fn open_recent(&mut self, _: &OpenRecent, window: &mut Window, cx: &mut Context<Self>) {
        let picker = cx.new(ProjectPicker::new);
        let subscription = cx.subscribe_in(&picker, window, |this, _, event, window, cx| {
            this.project_picker = None;
            this.focus_active(window, cx);
            if let ProjectPickerEvent::Open(location) = event {
                this.open_location(location.clone(), cx);
            }
        });
        window.focus(&picker.focus_handle(cx));
        self.project_picker = Some((picker, subscription));
        cx.notify();
    }

    fn dismiss_file_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.file_finder = None;
        match self.tabs.get(self.active) {
            Some(tab) => window.focus(&tab.focus_handle(cx)),
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
        let Some(editor) = self.tabs[index]
            .editor()
            .filter(|editor| editor.read(cx).buffer().is_dirty())
            .cloned()
        else {
            self.remove_tab(index, window, cx);
            return;
        };
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
                if close
                    && let Some(index) = this
                        .tabs
                        .iter()
                        .position(|tab| tab.editor() == Some(&editor))
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
        if let Some(editor) = tab.editor() {
            editor.update(cx, |editor, _| editor.detach_server(true));
        }
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
        let dirty = self.tabs.iter().filter(|tab| tab.is_dirty(cx)).count();
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
                let dirty = tab.is_dirty(cx);
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
        crate::startup::mark_once("first workspace render");
        let main = match self.tabs.get(self.active) {
            Some(tab) => div().flex_1().min_h_0().child(tab.view()),
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
            .on_action(cx.listener(Self::open_folder))
            .on_action(cx.listener(Self::open_recent))
            .on_action(cx.listener(Self::go_back))
            .on_action(cx.listener(Self::go_forward))
            .on_action(cx.listener(Self::toggle_files))
            .on_action(cx.listener(Self::toggle_search))
            .on_action(cx.listener(Self::toggle_git))
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
                    .children(
                        self.connection_error
                            .iter()
                            .chain(&self.notice)
                            .cloned()
                            .map(|notice| {
                                div()
                                    .px_3()
                                    .py_1()
                                    .text_sm()
                                    .text_color(rgb(theme::ERROR))
                                    .child(notice)
                            }),
                    )
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
            .children(self.project_picker.as_ref().map(|(picker, _)| {
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
    }
}
