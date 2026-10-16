//! The Commit tool window (PyCharm's): local changes as a tree with a
//! checkbox per file, folder and group; Commit commits exactly the checked
//! files (`git commit --only`, untracked ones added first). With the
//! `git.staging_area` setting it shows git's index instead (Staged /
//! Unstaged groups with stage and unstage). A second tab lists the shelf
//! (`shelf_view`). Git runs where the project lives (locally, in WSL or in
//! a dev container).
//!
//! Each folder of a multi-root project may be in its own repository (or
//! share one, or have none). With several repositories the changes are
//! grouped under one collapsible header per repository (PyCharm's
//! multi-repository Commit window); Commit commits every repository with
//! chosen files using the same message, and Fetch/Pull act on all of them.

use std::{
    collections::{HashSet, VecDeque},
    io,
    ops::Range,
    path::Path,
    time::Duration,
};

use git::{Change, Entry, FileChange, OperationState, Repository, Side, Status};
use gpui::{
    App, ClickEvent, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    KeyBinding, MouseButton, MouseDownEvent, Pixels, Point, PromptLevel, SharedString,
    Subscription, Task, Window, actions, anchored, deferred, div, prelude::*, px, uniform_list,
};

use crate::{
    change_tree::{self, Node},
    debugger::panel::Tooltip,
    diff_view::DiffSource,
    editor::{self, Editor},
    project::Project,
    selectable::{SelectableText, selectable},
    settings,
    shelf_view::{ShelfView, ShelfViewEvent},
    theme,
};

pub(crate) const ROW_HEIGHT: f32 = 22.;
pub(crate) const INDENT: f32 = 16.;

actions!(
    commit_panel,
    [
        /// Ctrl+Enter in the commit message.
        CommitChanges,
        ShowDiff,
        JumpToSource,
        SelectPrevious,
        SelectNext,
        ToggleChecked,
        OpenSelected,
    ]
);

pub fn bind_keys(cx: &mut App) {
    let tree = Some("CommitPanel && !Editor");
    cx.bind_keys([
        KeyBinding::new(
            "secondary-enter",
            CommitChanges,
            Some("CommitMessage > Editor"),
        ),
        KeyBinding::new("secondary-enter", CommitChanges, Some("CommitPanel")),
        KeyBinding::new("f4", JumpToSource, Some("CommitPanel")),
        KeyBinding::new("secondary-d", ShowDiff, tree),
        KeyBinding::new("up", SelectPrevious, tree),
        KeyBinding::new("down", SelectNext, tree),
        KeyBinding::new("space", ToggleChecked, tree),
        KeyBinding::new("enter", OpenSelected, tree),
    ]);
}

/// [`Project::open_repository`] started before the window (see
/// `workspace::Preload`).
pub type PreloadedRepository =
    futures::channel::oneshot::Receiver<io::Result<Option<(Repository, Status)>>>;

pub enum CommitPanelEvent {
    OpenDiff {
        repository: Repository,
        source: DiffSource,
    },
    /// Open a file (Jump to Source, or a conflicted file to resolve).
    OpenFile(std::path::PathBuf),
    /// Commit and Push: the commit is done, confirm the push (one
    /// repository at a time, see [`CommitPanel::next_push`]).
    Push(Repository),
    /// Stash Changes… for these files of `repository` (the Stash dialog
    /// asks for the message).
    Stash {
        repository: Repository,
        paths: Vec<String>,
        include_untracked: bool,
    },
    /// Shelve Changes… for these files of `repository`; `name` prefills
    /// the dialog.
    Shelve {
        repository: Repository,
        paths: Vec<String>,
        name: String,
    },
    /// Status was re-read; open diffs may be stale.
    Refreshed,
}

impl EventEmitter<CommitPanelEvent> for CommitPanel {}

/// A node of the changes tree directly under the root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Group {
    Conflicts,
    /// Every tracked change (no staging area).
    Changes,
    /// Staging area: index vs HEAD.
    Staged,
    /// Staging area: working tree vs index.
    Unstaged,
    Unversioned,
}

impl Group {
    fn title(self) -> &'static str {
        match self {
            Self::Conflicts => "Merge Conflicts",
            Self::Changes => "Changes",
            Self::Staged => "Staged",
            Self::Unstaged => "Unstaged",
            Self::Unversioned => "Unversioned Files",
        }
    }

    fn all(staging: bool) -> &'static [Self] {
        if staging {
            &[
                Self::Conflicts,
                Self::Staged,
                Self::Unstaged,
                Self::Unversioned,
            ]
        } else {
            &[Self::Conflicts, Self::Changes, Self::Unversioned]
        }
    }

    /// Groups whose files have checkboxes (what Commit takes).
    fn checkable(self) -> bool {
        matches!(self, Self::Changes | Self::Unversioned)
    }

    /// The change an entry shows in this group, if it belongs here.
    fn change(self, entry: &Entry) -> Option<Change> {
        let special = matches!(entry.unstaged, Some(Change::Conflicted | Change::Untracked));
        match self {
            Self::Conflicts => entry.unstaged.filter(|&c| c == Change::Conflicted),
            Self::Unversioned => entry.unstaged.filter(|&c| c == Change::Untracked),
            Self::Changes => (!special).then(|| entry.combined_change()),
            Self::Staged => entry.staged.filter(|_| !special),
            Self::Unstaged => entry.unstaged.filter(|_| !special),
        }
    }
}

/// Tri-state of a checkbox over several files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    Checked,
    Unchecked,
    Partial,
}

/// Which files Commit takes: everything except what the user unchecked, so
/// new changes start checked (and stay as chosen across refreshes).
#[derive(Debug, Default, Clone)]
pub struct Inclusion {
    excluded: HashSet<String>,
}

impl Inclusion {
    pub fn includes(&self, path: &str) -> bool {
        !self.excluded.contains(path)
    }

    pub fn state<'a>(&self, paths: impl IntoIterator<Item = &'a str>) -> Check {
        let (mut checked, mut unchecked) = (false, false);
        for path in paths {
            if self.includes(path) {
                checked = true;
            } else {
                unchecked = true;
            }
        }
        match (checked, unchecked) {
            (true, false) => Check::Checked,
            (true, true) => Check::Partial,
            _ => Check::Unchecked,
        }
    }

    /// Checks all of `paths`, or unchecks them all when they all are.
    pub fn toggle(&mut self, paths: &[&str]) {
        if self.state(paths.iter().copied()) == Check::Checked {
            self.excluded
                .extend(paths.iter().map(|path| (*path).to_owned()));
        } else {
            for path in paths {
                self.excluded.remove(*path);
            }
        }
    }

    /// Checks exactly `paths` among `all`.
    pub fn only<'a>(&mut self, all: impl IntoIterator<Item = &'a str>, paths: &[&str]) {
        self.excluded = all
            .into_iter()
            .filter(|path| !paths.contains(path))
            .map(str::to_owned)
            .collect();
    }

    /// Forgets files that no longer have changes, so they start checked if
    /// they change again.
    pub fn retain(&mut self, present: &HashSet<&str>) {
        self.excluded.retain(|path| present.contains(path.as_str()));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A repository's header, above its groups (only with several
    /// repositories).
    Repo { count: usize, collapsed: bool },
    Group {
        group: Group,
        count: usize,
        collapsed: bool,
    },
    Dir {
        group: Group,
        path: String,
        label: String,
        depth: usize,
        collapsed: bool,
    },
    File {
        group: Group,
        /// Index into the status entries.
        entry: usize,
        change: Change,
        depth: usize,
        /// Containing folder, shown dimmed after the name.
        dir: String,
    },
}

impl Row {
    /// The group the row is in; `None` for a repository header.
    fn group(&self) -> Option<Group> {
        match self {
            Self::Repo { .. } => None,
            Self::Group { group, .. } | Self::Dir { group, .. } | Self::File { group, .. } => {
                Some(*group)
            }
        }
    }

    /// What identifies the row across rebuilds (within its repository).
    fn key(&self, status: &Status) -> (Option<Group>, String, bool) {
        match self {
            Self::Repo { .. } => (None, String::new(), true),
            Self::Group { group, .. } => (Some(*group), String::new(), true),
            Self::Dir { group, path, .. } => (Some(*group), path.clone(), true),
            Self::File { group, entry, .. } => {
                (Some(*group), status.entries[*entry].path.clone(), false)
            }
        }
    }
}

/// Collapsed nodes: `(group, "")` for a group, `(group, path)` for a folder.
type Collapsed = HashSet<(Group, String)>;

/// The tree rows for `status`: groups with their folders (compacted, when
/// grouping by directory) and files.
pub fn build_rows(
    status: &Status,
    staging: bool,
    by_directory: bool,
    collapsed: &Collapsed,
) -> Vec<Row> {
    let mut rows = Vec::new();
    for &group in Group::all(staging) {
        let files: Vec<(usize, Change)> = status
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| Some((index, group.change(entry)?)))
            .collect();
        if files.is_empty() && group != Group::Changes {
            continue;
        }
        let group_collapsed = collapsed.contains(&(group, String::new()));
        rows.push(Row::Group {
            group,
            count: files.len(),
            collapsed: group_collapsed,
        });
        if group_collapsed {
            continue;
        }
        let paths: Vec<&str> = files
            .iter()
            .map(|&(entry, _)| status.entries[entry].path.as_str())
            .collect();
        let nodes = if by_directory {
            let folders: HashSet<String> = collapsed
                .iter()
                .filter(|(g, path)| *g == group && !path.is_empty())
                .map(|(_, path)| path.clone())
                .collect();
            change_tree::tree(&paths, &folders)
        } else {
            change_tree::list(&paths)
        };
        rows.extend(nodes.into_iter().map(|node| match node {
            Node::Dir {
                path,
                label,
                depth,
                collapsed,
            } => Row::Dir {
                group,
                path,
                label,
                depth: depth + 1,
                collapsed,
            },
            Node::File {
                item, depth, dir, ..
            } => Row::File {
                group,
                entry: files[item].0,
                change: files[item].1,
                depth: depth + 1,
                dir,
            },
        }));
    }
    rows
}

/// Every folder path of every group (for Collapse All).
fn all_folders(status: &Status, staging: bool) -> Collapsed {
    let mut folders = Collapsed::new();
    for &group in Group::all(staging) {
        folders.insert((group, String::new()));
        let paths: Vec<&str> = status
            .entries
            .iter()
            .filter(|entry| group.change(entry).is_some())
            .map(|entry| entry.path.as_str())
            .collect();
        for node in change_tree::tree(&paths, &HashSet::new()) {
            if let Node::Dir { path, .. } = node {
                folders.insert((group, path));
            }
        }
    }
    folders
}

/// How a repository's top level (as git prints it) is compared: Windows
/// drive paths (`C:/…`) case-insensitively, like `git worktree list`'s.
pub fn repository_key(root: &str) -> String {
    let root = root.trim_end_matches('/');
    if root.as_bytes().get(1) == Some(&b':') {
        root.to_lowercase()
    } else {
        root.to_owned()
    }
}

/// A repository's folder name (what multi-repository lists show).
pub fn repository_name(repository: &Repository) -> &str {
    let root = repository.root().trim_end_matches('/');
    root.rsplit(['/', '\\']).next().unwrap_or(root)
}

/// The branch, a short commit id when detached, else `HEAD`.
pub fn branch_label(status: &Status) -> String {
    match (&status.branch, &status.head) {
        (Some(branch), _) => branch.clone(),
        (None, Some(head)) => short_revision(head).to_owned(),
        (None, None) => "HEAD".to_owned(),
    }
}

/// Groups folders by repository: `keys[i]` is folder `i`'s
/// [`repository_key`] (`None` without a repository). Returns per
/// repository the folders inside it (in folder order), and per folder
/// its repository's index; folders without one are left out.
pub fn group_folders(keys: &[Option<String>]) -> (Vec<Vec<usize>>, Vec<Option<usize>>) {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut of_folder = vec![None; keys.len()];
    for (folder, key) in keys.iter().enumerate() {
        let Some(key) = key else {
            continue;
        };
        let group = match groups
            .iter()
            .position(|group| keys[group[0]].as_ref() == Some(key))
        {
            Some(group) => group,
            None => {
                groups.push(Vec::new());
                groups.len() - 1
            }
        };
        groups[group].push(folder);
        of_folder[folder] = Some(group);
    }
    (groups, of_folder)
}

/// One repository of the project: its status and what the panel shows of
/// it (checkboxes and collapsed nodes survive refreshes).
pub struct Repo {
    key: String,
    /// Discovered from its first folder.
    repository: Repository,
    status: Status,
    /// The merge, rebase, cherry-pick or revert in progress.
    operation: OperationState,
    /// The project folders inside it, each with its own discovery (whose
    /// prefix is that folder's place in the repository).
    folders: Vec<(usize, Repository)>,
    inclusion: Inclusion,
    collapsed: Collapsed,
    /// Its header is collapsed (several repositories only).
    folded: bool,
}

impl Repo {
    pub fn repository(&self) -> &Repository {
        &self.repository
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub fn operation(&self) -> OperationState {
        self.operation
    }

    pub fn name(&self) -> &str {
        repository_name(&self.repository)
    }

    /// Whether `repository` is this one (any folder's discovery of it).
    pub fn is(&self, repository: &Repository) -> bool {
        self.key == repository_key(repository.root())
    }

    /// Entries Commit can take: changes and unversioned files
    /// (conflicted files once resolved).
    fn checked_entries(&self) -> Vec<Entry> {
        self.status
            .entries
            .iter()
            .filter(|entry| checkable(entry) && self.inclusion.includes(&entry.path))
            .cloned()
            .collect()
    }
}

/// Files with a checkbox: under Changes or Unversioned Files.
fn checkable(entry: &Entry) -> bool {
    Group::Changes.change(entry).is_some() || Group::Unversioned.change(entry).is_some()
}

/// What a folder's part of a refresh read.
enum FolderRead {
    /// Its repository (discovered, or known) with status and the
    /// operation in progress.
    Opened(io::Result<Option<(Repository, Status, OperationState)>>),
    /// A known repository whose status an earlier folder reads.
    Shares(Repository),
}

/// What a refresh read: repositories deduplicated in folder order, per
/// folder its repository, and the folders git refused or failed on.
#[derive(Default)]
struct Loaded {
    repos: Vec<Repo>,
    roots: Vec<Option<usize>>,
    untrusted: Vec<SharedString>,
    errors: Vec<SharedString>,
}

/// A folder's repository and status, as [`Project::open_repository_in`]
/// returns them.
type Opened = io::Result<Option<(Repository, Status)>>;

/// Reads every folder's repository where the project lives, all folders
/// at once: status again for `known` repositories (once per repository),
/// discovery for the others (`first` is folder 0's, already read).
/// Blocks.
fn load(
    project: &Project,
    known: &[Option<Repository>],
    first: Option<Opened>,
    operations: bool,
) -> Loaded {
    let operation = |repository: &Repository| {
        if operations {
            repository.operation_state().unwrap_or(OperationState::None)
        } else {
            OperationState::None
        }
    };
    let opened = |result: Opened| {
        FolderRead::Opened(result.map(|found| {
            found.map(|(repository, status)| {
                let operation = operation(&repository);
                (repository, status, operation)
            })
        }))
    };
    let keys: Vec<Option<String>> = known
        .iter()
        .map(|repository| repository.as_ref().map(|r| repository_key(r.root())))
        .collect();
    let mut first = first;
    let reads: Vec<FolderRead> = std::thread::scope(|scope| {
        let mut pending = Vec::new();
        for (index, folder) in known.iter().enumerate() {
            let shared = keys[index]
                .as_ref()
                .is_some_and(|key| keys[..index].contains(&Some(key.clone())));
            pending.push(match folder {
                Some(repository) if shared => Err(FolderRead::Shares(repository.clone())),
                _ if index == 0 && first.is_some() => Err(opened(first.take().unwrap_or(Ok(None)))),
                Some(repository) => Ok(scope.spawn(move || {
                    opened(
                        repository
                            .status()
                            .map(|status| Some((repository.clone(), status))),
                    )
                })),
                None => Ok(scope.spawn(move || opened(project.open_repository_in(index)))),
            });
        }
        pending
            .into_iter()
            .map(|read| match read {
                Err(read) => read,
                Ok(thread) => thread.join().unwrap_or_else(|_| {
                    FolderRead::Opened(Err(io::Error::other("git status panicked")))
                }),
            })
            .collect()
    });
    assemble(project, reads)
}

/// A folder's repository, with its status unless another folder already
/// read it.
type FoundRepository = Option<(Repository, Option<(Status, OperationState)>)>;

/// Groups the folders' reads by repository (see [`group_folders`]).
fn assemble(project: &Project, reads: Vec<FolderRead>) -> Loaded {
    let mut loaded = Loaded::default();
    let mut found: Vec<FoundRepository> = Vec::new();
    for (index, read) in reads.into_iter().enumerate() {
        found.push(match read {
            FolderRead::Shares(repository) => Some((repository, None)),
            FolderRead::Opened(Ok(found)) => {
                found.map(|(repository, status, operation)| (repository, Some((status, operation))))
            }
            FolderRead::Opened(Err(err)) => {
                match git::unsafe_repository(&err) {
                    Some(refusal) => {
                        let path: SharedString = refusal.path.clone().into();
                        if !loaded.untrusted.contains(&path) {
                            loaded.untrusted.push(path);
                        }
                    }
                    None if project.is_multi_root() => loaded
                        .errors
                        .push(format!("{}: {err}", project.roots()[index].name).into()),
                    None => loaded.errors.push(err.to_string().into()),
                }
                None
            }
        });
    }
    let keys: Vec<Option<String>> = found
        .iter()
        .map(|found| found.as_ref().map(|(r, _)| repository_key(r.root())))
        .collect();
    let (groups, _) = group_folders(&keys);
    loaded.roots = vec![None; found.len()];
    for folders in groups {
        // A folder sharing a known repository has no status of its own:
        // the status is the first member's that read one (none if that
        // read failed, which is reported above).
        let Some((status, operation)) = folders
            .iter()
            .find_map(|&folder| found[folder].as_mut().and_then(|(_, read)| read.take()))
        else {
            continue;
        };
        let members: Vec<(usize, Repository)> = folders
            .iter()
            .filter_map(|&folder| Some((folder, found[folder].take()?.0)))
            .collect();
        for &(folder, _) in &members {
            loaded.roots[folder] = Some(loaded.repos.len());
        }
        let repository = members[0].1.clone();
        loaded.repos.push(Repo {
            key: repository_key(repository.root()),
            repository,
            status,
            operation,
            folders: members,
            inclusion: Inclusion::default(),
            collapsed: Collapsed::new(),
            folded: false,
        });
    }
    loaded
}

/// The Commit window's tabs (PyCharm's Local Changes / Shelf).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Changes,
    Shelf,
}

/// The open right-click menu: where, and the files (of repository `repo`)
/// it acts on.
struct ContextMenu {
    position: Point<Pixels>,
    repo: usize,
    entries: Vec<usize>,
}

#[derive(Debug, Clone, Copy)]
pub enum SyncAction {
    Fetch,
    Pull,
}

/// A git operation on one repository, run off the main thread.
type Job = Box<dyn FnOnce(&Repository) -> io::Result<Option<String>> + Send>;

fn job(operation: impl FnOnce(&Repository) -> io::Result<Option<String>> + Send + 'static) -> Job {
    Box::new(operation)
}

/// What identifies a row across refreshes: its repository's key and the
/// row's key there.
type RowKey = (String, Option<Group>, String, bool);

pub struct CommitPanel {
    project: Project,
    /// The first status read has finished.
    loaded: bool,
    repos: Vec<Repo>,
    /// Per project folder: its repository (index into `repos`).
    root_repos: Vec<Option<usize>>,
    /// Repositories git refuses (their folder belongs to another user).
    untrusted: Vec<SharedString>,
    /// Folders whose repository could not be read.
    errors: Vec<SharedString>,
    /// Rows with their repository (index into `repos`).
    rows: Vec<(usize, Row)>,
    /// The staging-area setting the rows were built for.
    staging: bool,
    message: Entity<Editor>,
    /// Result of the last operation, with its color (errors in red).
    feedback: Option<(SharedString, fn() -> Hsla)>,
    busy: bool,
    /// The next commit replaces the last one.
    amend: bool,
    /// Files grouped in folders (else a flat list).
    by_directory: bool,
    selected: Option<usize>,
    menu: Option<ContextMenu>,
    tab: Tab,
    shelf: Entity<ShelfView>,
    /// The repository whose shelves the Shelf tab lists.
    shelf_repo: usize,
    /// Commit and Push in several repositories: the pushes left to confirm.
    pushes: VecDeque<Repository>,
    focus_handle: FocusHandle,
    _refresh: Task<()>,
    refresh_running: bool,
    refresh_requested: bool,
    refresh_scheduled: bool,
    _operation: Task<()>,
    _subscription: Subscription,
    _shelf_subscription: Subscription,
}

pub fn change_color(change: Change) -> Hsla {
    match change {
        Change::Added => theme::git_added(),
        Change::Untracked => theme::git_unversioned(),
        Change::Deleted => theme::git_deleted(),
        Change::Conflicted => theme::git_conflicted(),
        Change::Renamed | Change::Copied => theme::git_renamed(),
        Change::Modified | Change::TypeChanged => theme::git_modified(),
    }
}

/// Commit ids are shortened; branch and tag names are kept.
pub fn short_revision(revision: &str) -> &str {
    let hex = revision.len() >= 20 && revision.bytes().all(|b| b.is_ascii_hexdigit());
    if hex { &revision[..8] } else { revision }
}

/// Selectable text in a row: double-click (diff / expand) and the
/// right-click menu belong to the row.
pub(crate) fn row_text(id: &'static str, text: impl Into<SharedString>) -> SelectableText {
    selectable(id, text)
        .double_click_through()
        .right_click_through()
}

/// A strip above the changes: an operation in progress, a refused
/// repository.
fn banner() -> gpui::Div {
    div()
        .px_2()
        .py_1()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .text_xs()
        .bg(theme::active_row())
}

/// A checkbox drawn in the theme's colors (also the workflow inputs
/// form's).
pub(crate) fn checkbox(check: Check) -> gpui::Div {
    let on = check != Check::Unchecked;
    div()
        .size(px(13.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .border_1()
        .border_color(if on { theme::accent() } else { theme::muted() })
        .when(on, |boxed| boxed.bg(theme::accent()))
        .text_color(theme::on_accent())
        .text_size(px(10.))
        .line_height(px(10.))
        .child(match check {
            Check::Checked => "✓",
            Check::Partial => "–",
            Check::Unchecked => "",
        })
}

impl CommitPanel {
    pub fn new(
        project: Project,
        preloaded: Option<PreloadedRepository>,
        cx: &mut Context<Self>,
    ) -> Self {
        let message = cx.new(|cx| {
            let mut message = Editor::text_area(cx);
            message.set_commit_message();
            message
        });
        let subscription = cx.subscribe(&message, |_, _, _: &editor::EditorEvent, cx| cx.notify());
        let shelf = cx.new(|cx| ShelfView::new(project.clone(), cx));
        let shelf_subscription = cx.subscribe(&shelf, |this, _, event, cx| match event {
            ShelfViewEvent::OpenDiff { repository, source } => {
                cx.emit(CommitPanelEvent::OpenDiff {
                    repository: repository.clone(),
                    source: source.clone(),
                })
            }
            ShelfViewEvent::Changed { message, conflicts } => {
                this.feedback = Some((message.clone().into(), theme::muted));
                // Conflicts are resolved in the Changes tab.
                if *conflicts {
                    this.tab = Tab::Changes;
                }
                this.refresh(cx);
            }
            ShelfViewEvent::Failed(error) => {
                this.feedback = Some((error.clone().into(), theme::error));
                this.refresh(cx);
            }
        });
        let mut panel = Self {
            root_repos: vec![None; project.roots().len()],
            project,
            loaded: false,
            repos: Vec::new(),
            untrusted: Vec::new(),
            errors: Vec::new(),
            rows: Vec::new(),
            staging: settings::current().git.staging_area,
            message,
            feedback: None,
            busy: false,
            amend: false,
            by_directory: true,
            selected: None,
            menu: None,
            tab: Tab::Changes,
            shelf,
            shelf_repo: 0,
            pushes: VecDeque::new(),
            focus_handle: cx.focus_handle(),
            _refresh: Task::ready(()),
            refresh_running: false,
            refresh_requested: false,
            refresh_scheduled: false,
            _operation: Task::ready(()),
            _subscription: subscription,
            _shelf_subscription: shelf_subscription,
        };
        match preloaded {
            Some(preloaded) => panel.show_preloaded(preloaded, cx),
            None => panel.refresh(cx),
        }
        panel
    }

    /// Focuses the commit message.
    pub fn focus(&self, window: &mut Window, cx: &App) {
        window.focus(&self.message.focus_handle(cx));
    }

    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx)
    }

    /// The project's repositories in folder order, once their status has
    /// been read.
    pub fn repos(&self) -> &[Repo] {
        &self.repos
    }

    /// The repository of the folder holding `path`; `None` outside every
    /// folder, or in a folder without one.
    pub fn repo_index_for(&self, path: &Path) -> Option<usize> {
        let root = self.project.root_index_of(path)?;
        self.root_repos.get(root).copied().flatten()
    }

    /// The repository git commands act on (PyCharm's): `active`'s (the
    /// active editor's file), else the first one.
    pub fn active_repo(&self, active: Option<&Path>) -> Option<&Repo> {
        let index = active.and_then(|path| self.repo_index_for(path));
        self.repos.get(index.unwrap_or(0))
    }

    /// The repository `repository` is (whichever folder discovered it).
    pub fn repo_of(&self, repository: &Repository) -> Option<&Repo> {
        self.repos.iter().find(|repo| repo.is(repository))
    }

    /// `path`'s repository and its `/`-separated path there, when its
    /// folder is in one.
    pub fn file_in_repository(&self, path: &Path) -> Option<(Repository, String)> {
        let root = self.project.root_index_of(path)?;
        let repo = &self.repos[self.root_repos.get(root).copied().flatten()?];
        let (_, folder) = repo.folders.iter().find(|(index, _)| *index == root)?;
        let inside = path.strip_prefix(&self.project.roots()[root].path).ok()?;
        let parts: Vec<String> = inside
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect();
        (!parts.is_empty()).then(|| {
            (
                repo.repository.clone(),
                format!("{}{}", folder.prefix(), parts.join("/")),
            )
        })
    }

    /// Commit and Push in several repositories: the next push to confirm
    /// (after the previous Push dialog closed).
    pub fn next_push(&mut self) -> Option<Repository> {
        self.pushes.pop_front()
    }

    /// Refreshes after [`REFRESH_DEBOUNCE`], coalescing bursts of changes.
    pub fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
        if self.refresh_running {
            self.refresh_requested = true;
            return;
        }
        // Keep the first deadline: continuous file activity must not
        // postpone the next status read indefinitely.
        if self.refresh_scheduled {
            return;
        }
        self.refresh_scheduled = true;
        self._refresh = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REFRESH_DEBOUNCE).await;
            this.update(cx, |this, cx| this.refresh(cx)).ok();
        });
    }

    /// Re-reads every repository's status (discovering repositories of
    /// folders without a known one first) and the operations in progress.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        // Dropping a running task discards its result even though the
        // background git process continues. Let it finish and queue one
        // follow-up read for notifications received while it was running.
        if self.refresh_running {
            self.refresh_requested = true;
            return;
        }
        self.refresh_scheduled = false;
        self.refresh_running = true;
        let project = self.project.clone();
        let known = self.known_repositories();
        self._refresh = cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move { load(&project, &known, None, true) })
                .await;
            this.update(cx, |this, cx| {
                this.show(loaded, cx);
                this.finish_refresh(cx);
            })
            .ok();
        });
    }

    /// Per folder, its repository as last read (`None`: discover again).
    fn known_repositories(&self) -> Vec<Option<Repository>> {
        let mut known = vec![None; self.project.roots().len()];
        for repo in &self.repos {
            for (folder, repository) in &repo.folders {
                if let Some(slot) = known.get_mut(*folder) {
                    *slot = Some(repository.clone());
                }
            }
        }
        known
    }

    /// Shows a repository read before the panel existed (at startup); reads
    /// it now if that read was abandoned. The operations in progress are
    /// read afterwards, off the startup path.
    fn show_preloaded(&mut self, preloaded: PreloadedRepository, cx: &mut Context<Self>) {
        self.refresh_running = true;
        let project = self.project.clone();
        self._refresh = cx.spawn(async move |this, cx| {
            let opened = match preloaded.await {
                Ok(opened) => opened,
                Err(_) => {
                    this.update(cx, |this, cx| {
                        this.refresh_running = false;
                        this.refresh_requested = false;
                        this.refresh(cx);
                    })
                    .ok();
                    return;
                }
            };
            let loaded = cx
                .background_spawn(async move {
                    let known = vec![None; project.roots().len()];
                    load(&project, &known, Some(opened), false)
                })
                .await;
            let repositories: Vec<Repository> = this
                .update(cx, |this, cx| {
                    this.show(loaded, cx);
                    this.repos
                        .iter()
                        .map(|repo| repo.repository.clone())
                        .collect()
                })
                .unwrap_or_default();
            let operations: Vec<(String, OperationState)> = cx
                .background_spawn(async move {
                    repositories
                        .iter()
                        .map(|repository| {
                            let operation = repository.operation_state();
                            (
                                repository_key(repository.root()),
                                operation.unwrap_or(OperationState::None),
                            )
                        })
                        .collect()
                })
                .await;
            this.update(cx, |this, cx| {
                for (key, operation) in operations {
                    if let Some(repo) = this.repos.iter_mut().find(|repo| repo.key == key) {
                        repo.operation = operation;
                    }
                }
                this.finish_refresh(cx);
                cx.notify();
            })
            .ok();
        });
    }

    fn finish_refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_running = false;
        if std::mem::take(&mut self.refresh_requested) {
            self.schedule_refresh(cx);
        }
    }

    fn show(&mut self, loaded: Loaded, cx: &mut Context<Self>) {
        crate::startup::mark_once("git status shown");
        let selected = self.selected_key();
        let mut previous = std::mem::take(&mut self.repos);
        self.repos = loaded.repos;
        for repo in &mut self.repos {
            if let Some(old) = previous.iter_mut().find(|old| old.key == repo.key) {
                repo.inclusion = std::mem::take(&mut old.inclusion);
                repo.collapsed = std::mem::take(&mut old.collapsed);
                repo.folded = old.folded;
            }
            let present: HashSet<&str> = repo
                .status
                .entries
                .iter()
                .map(|e| e.path.as_str())
                .collect();
            repo.inclusion.retain(&present);
        }
        self.root_repos = loaded.roots;
        self.untrusted = loaded.untrusted;
        self.errors = loaded.errors;
        self.loaded = true;
        self.rebuild_rows();
        self.selected = selected.and_then(|key| self.find_row(&key));
        if self.shelf_repo >= self.repos.len() {
            self.shelf_repo = 0;
        }
        self.update_shelf(cx);
        cx.emit(CommitPanelEvent::Refreshed);
        cx.notify();
    }

    /// Points the Shelf tab at its repository (reading the shelves again
    /// when the tab shows).
    fn update_shelf(&mut self, cx: &mut Context<Self>) {
        let repository = self
            .repos
            .get(self.shelf_repo)
            .map(|repo| repo.repository.clone());
        let shelf_visible = self.tab == Tab::Shelf;
        self.shelf.update(cx, |shelf, cx| {
            shelf.set_repository(repository);
            if shelf_visible {
                shelf.refresh(cx);
            }
        });
    }

    /// The Shelf tab lists repository `repo`'s shelves.
    fn select_shelf_repo(&mut self, repo: usize, cx: &mut Context<Self>) {
        self.shelf_repo = repo;
        self.update_shelf(cx);
        cx.notify();
    }

    fn selected_key(&self) -> Option<RowKey> {
        let (repo, row) = self.rows.get(self.selected?)?;
        let repo = self.repos.get(*repo)?;
        let (group, path, dir) = row.key(&repo.status);
        Some((repo.key.clone(), group, path, dir))
    }

    fn find_row(&self, key: &RowKey) -> Option<usize> {
        self.rows.iter().position(|(repo, row)| {
            let repo = &self.repos[*repo];
            repo.key == key.0 && {
                let (group, path, dir) = row.key(&repo.status);
                group == key.1 && path == key.2 && dir == key.3
            }
        })
    }

    /// Shows the Changes or Shelf tab (reading the shelves again).
    pub fn select_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        self.menu = None;
        if tab == Tab::Shelf {
            self.shelf.update(cx, |shelf, cx| shelf.refresh(cx));
        }
        cx.notify();
    }

    /// Files for Shelve/Stash from the Git menu, with their repository:
    /// the selected changes, else the checked ones of the first repository
    /// with any (a shelf or stash belongs to one repository).
    pub fn chosen_entries(&self) -> Option<(usize, Vec<Entry>)> {
        match self.selected_entries() {
            Some((repo, entries)) if !entries.is_empty() => Some((repo, entries)),
            _ => self.checked_entries().into_iter().next(),
        }
    }

    /// Shelve Changes… for `entries` of repository `repo`: the dialog's
    /// name is the commit message's first line, else the file names.
    pub fn request_shelve(&mut self, repo: usize, entries: Vec<Entry>, cx: &mut Context<Self>) {
        let repository = self.repos.get(repo).map(|repo| repo.repository.clone());
        let Some(repository) = repository.filter(|_| !entries.is_empty()) else {
            self.feedback = Some(("Select the changes to shelve".into(), theme::error));
            return cx.notify();
        };
        let message = self.message.read(cx).text();
        let name = match message.lines().next().map(str::trim) {
            Some(line) if !line.is_empty() => line.to_owned(),
            _ => {
                let names: Vec<&str> = entries
                    .iter()
                    .map(|entry| entry.path.rsplit('/').next().unwrap_or(&entry.path))
                    .collect();
                match names.as_slice() {
                    [_, _, _, _, ..] => {
                        format!("{}, {} and {} more", names[0], names[1], names.len() - 2)
                    }
                    names => names.join(", "),
                }
            }
        };
        let paths = entries.into_iter().map(|entry| entry.path).collect();
        cx.emit(CommitPanelEvent::Shelve {
            repository,
            paths,
            name,
        });
    }

    /// Stash Changes… for `entries` of repository `repo` (untracked files
    /// included when any is untracked).
    pub fn request_stash(&mut self, repo: usize, entries: Vec<Entry>, cx: &mut Context<Self>) {
        let repository = self.repos.get(repo).map(|repo| repo.repository.clone());
        let Some(repository) = repository.filter(|_| !entries.is_empty()) else {
            self.feedback = Some(("Select the changes to stash".into(), theme::error));
            return cx.notify();
        };
        let include_untracked = entries
            .iter()
            .any(|entry| entry.unstaged == Some(Change::Untracked));
        let mut paths = Vec::new();
        for entry in entries {
            paths.extend(entry.orig_path);
            paths.push(entry.path);
        }
        cx.emit(CommitPanelEvent::Stash {
            repository,
            paths,
            include_untracked,
        });
    }

    /// Shows the outcome of a git operation run elsewhere (a popup).
    pub fn report(&mut self, message: String, cx: &mut Context<Self>) {
        self.feedback = Some((message.into(), theme::muted));
        cx.notify();
    }

    /// Shows an error from an operation run elsewhere.
    pub fn report_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.feedback = Some((message.into(), theme::error));
        cx.notify();
    }

    /// Adds the refused repository to git's `safe.directory` where the
    /// project lives, then reads it again.
    fn trust(&mut self, path: String, cx: &mut Context<Self>) {
        let project = self.project.clone();
        self.busy = true;
        cx.notify();
        self._operation = cx.spawn(async move |this, cx| {
            let trusted = cx
                .background_spawn(async move { project.trust_repository(&path) })
                .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match trusted {
                    Ok(()) => this.refresh(cx),
                    Err(err) => {
                        this.feedback = Some((err.to_string().into(), theme::error));
                        cx.notify();
                    }
                }
            })
            .ok();
        });
    }

    /// Rows of every repository: with several, each under its header.
    fn rebuild_rows(&mut self) {
        let several = self.repos.len() > 1;
        self.rows.clear();
        for (index, repo) in self.repos.iter().enumerate() {
            if several {
                self.rows.push((
                    index,
                    Row::Repo {
                        count: repo.status.entries.len(),
                        collapsed: repo.folded,
                    },
                ));
                if repo.folded {
                    continue;
                }
            }
            let rows = build_rows(
                &repo.status,
                self.staging,
                self.by_directory,
                &repo.collapsed,
            );
            self.rows.extend(rows.into_iter().map(|row| (index, row)));
        }
        if self.selected.is_some_and(|row| row >= self.rows.len()) {
            self.selected = None;
        }
    }

    /// Status entries (of the row's repository) of the files at or under
    /// `row`.
    fn entries_under(&self, row: usize) -> Vec<usize> {
        let Some((repo, target)) = self.rows.get(row) else {
            return Vec::new();
        };
        let entries = &self.repos[*repo].status.entries;
        let indices = 0..entries.len();
        match target {
            Row::Repo { .. } => indices.collect(),
            Row::File { entry, .. } => vec![*entry],
            Row::Group { group, .. } => indices
                .filter(|&index| group.change(&entries[index]).is_some())
                .collect(),
            Row::Dir { group, path, .. } => {
                let prefix = format!("{path}/");
                indices
                    .filter(|&index| {
                        let entry = &entries[index];
                        group.change(entry).is_some() && entry.path.starts_with(&prefix)
                    })
                    .collect()
            }
        }
    }

    fn check_state(&self, row: usize) -> Check {
        let Some((repo, _)) = self.rows.get(row) else {
            return Check::Unchecked;
        };
        let repo = &self.repos[*repo];
        repo.inclusion.state(
            self.entries_under(row)
                .into_iter()
                .map(|entry| &repo.status.entries[entry])
                .filter(|entry| checkable(entry))
                .map(|entry| entry.path.as_str()),
        )
    }

    fn toggle_checked(&mut self, row: usize, cx: &mut Context<Self>) {
        let entries = self.entries_under(row);
        let Some(&(repo, _)) = self.rows.get(row) else {
            return;
        };
        let repo = &mut self.repos[repo];
        let paths: Vec<&str> = entries
            .iter()
            .map(|&entry| &repo.status.entries[entry])
            .filter(|entry| checkable(entry))
            .map(|entry| entry.path.as_str())
            .collect();
        repo.inclusion.toggle(&paths);
        cx.notify();
    }

    /// The checked entries Commit takes, per repository with any
    /// (conflicted files count once resolved, i.e. under Changes).
    fn checked_entries(&self) -> Vec<(usize, Vec<Entry>)> {
        self.repos
            .iter()
            .enumerate()
            .map(|(index, repo)| (index, repo.checked_entries()))
            .filter(|(_, entries)| !entries.is_empty())
            .collect()
    }

    /// Entries of the selected row (all files under a repository, group
    /// or folder), with their repository.
    fn selected_entries(&self) -> Option<(usize, Vec<Entry>)> {
        let row = self.selected?;
        let &(repo, _) = self.rows.get(row)?;
        let status = &self.repos[repo].status;
        let entries = self.entries_under(row);
        Some((
            repo,
            entries
                .into_iter()
                .map(|entry| status.entries[entry].clone())
                .collect(),
        ))
    }

    /// Runs git operations off the main thread, one after another, then
    /// refreshes. `Ok(Some)` messages are shown as confirmations (each
    /// prefixed with its repository's name when several ran);
    /// `on_success` gets the repositories once every operation succeeded.
    fn operate(
        &mut self,
        jobs: Vec<(usize, Job)>,
        on_success: impl FnOnce(&mut Self, Vec<Repository>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let jobs: Vec<(Repository, Job)> = jobs
            .into_iter()
            .filter_map(|(repo, job)| Some((self.repos.get(repo)?.repository.clone(), job)))
            .collect();
        if jobs.is_empty() {
            return;
        }
        self.busy = true;
        self.feedback = None;
        cx.notify();
        self._operation = cx.spawn(async move |this, cx| {
            let results: Vec<(Repository, io::Result<Option<String>>)> = cx
                .background_spawn(async move {
                    jobs.into_iter()
                        .map(|(repository, job)| {
                            let result = job(&repository);
                            (repository, result)
                        })
                        .collect()
                })
                .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                let several = results.len() > 1;
                let mut failed = false;
                let mut lines = Vec::new();
                for (repository, result) in &results {
                    let text = match result {
                        Ok(Some(message)) => message.clone(),
                        Ok(None) => continue,
                        Err(err) => {
                            failed = true;
                            err.to_string()
                        }
                    };
                    lines.push(if several {
                        format!("{}: {text}", repository_name(repository))
                    } else {
                        text
                    });
                }
                let color: fn() -> Hsla = if failed { theme::error } else { theme::muted };
                this.feedback = (!lines.is_empty()).then(|| (lines.join("; ").into(), color));
                if !failed {
                    let repositories = results.into_iter().map(|(r, _)| r).collect();
                    on_success(this, repositories, cx);
                }
                this.refresh(cx);
            })
            .ok();
        });
    }

    fn stage(&mut self, repo: usize, entries: Vec<Entry>, cx: &mut Context<Self>) {
        let paths: Vec<String> = entries.into_iter().map(|entry| entry.path).collect();
        let stage = job(move |repo| repo.stage(&paths).map(|()| None));
        self.operate(vec![(repo, stage)], |_, _, _| {}, cx);
    }

    fn unstage(&mut self, repo: usize, entries: Vec<Entry>, cx: &mut Context<Self>) {
        let paths: Vec<String> = entries.into_iter().map(|entry| entry.path).collect();
        let unstage = job(move |repo| repo.unstage(&paths).map(|()| None));
        self.operate(vec![(repo, unstage)], |_, _, _| {}, cx);
    }

    /// PyCharm's Add to VCS for untracked files.
    fn add_to_vcs(&mut self, repo: usize, entries: Vec<Entry>, cx: &mut Context<Self>) {
        let paths: Vec<String> = entries
            .into_iter()
            .filter(|entry| entry.unstaged == Some(Change::Untracked))
            .map(|entry| entry.path)
            .collect();
        let add = job(move |repo| repo.add(&paths).map(|()| None));
        self.operate(vec![(repo, add)], |_, _, _| {}, cx);
    }

    /// Asks, then reverts the entries (per repository) to HEAD (see
    /// [`Repository::rollback`]).
    pub fn rollback(
        &mut self,
        groups: Vec<(usize, Vec<Entry>)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let groups: Vec<(usize, Vec<Entry>)> = groups
            .into_iter()
            .filter(|(_, entries)| !entries.is_empty())
            .collect();
        if groups.is_empty() {
            self.feedback = Some(("Select the changes to roll back".into(), theme::error));
            return cx.notify();
        }
        let all: Vec<&Entry> = groups.iter().flat_map(|(_, entries)| entries).collect();
        let untracked = all
            .iter()
            .filter(|entry| entry.unstaged == Some(Change::Untracked))
            .count();
        let what = match all.as_slice() {
            [entry] => entry.path.clone(),
            _ => format!("{} files", all.len()),
        };
        let detail = if untracked > 0 {
            format!(
                "Changes go back to the last commit; added files leave version control. \
                 {untracked} unversioned file(s) will be deleted from disk. This cannot be undone."
            )
        } else {
            "Changes go back to the last commit; added files leave version control (they stay \
             on disk). This cannot be undone."
                .to_owned()
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Rollback {what}?"),
            Some(&detail),
            &["Rollback", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update(cx, |this, cx| {
                let jobs = groups
                    .into_iter()
                    .map(|(repo, entries)| {
                        let count = entries.len();
                        let rollback = job(move |repo| {
                            repo.rollback(&entries)
                                .map(|()| Some(format!("Rolled back {count} file(s)")))
                        });
                        (repo, rollback)
                    })
                    .collect();
                this.operate(jobs, |_, _, _| {}, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Rollback from the toolbar or the Git menu: the selected changes,
    /// else the checked ones (of every repository).
    pub fn rollback_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let groups = match self.selected_entries() {
            Some((repo, entries)) if !entries.is_empty() => vec![(repo, entries)],
            _ => self.checked_entries(),
        };
        self.rollback(groups, window, cx);
    }

    /// Deletes files from disk (asks first).
    fn delete(
        &mut self,
        repo: usize,
        entries: Vec<Entry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repos.get(repo).map(|repo| repo.repository.clone()) else {
            return;
        };
        let what = match entries.as_slice() {
            [entry] => entry.path.clone(),
            _ => format!("{} files", entries.len()),
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete {what}?"),
            Some("The files are deleted from disk."),
            &["Delete", "Cancel"],
            cx,
        );
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update(cx, |this, cx| {
                let delete = job(move |_| {
                    for entry in &entries {
                        let path = project.repository_path(&repository, &entry.path);
                        project.remove(&path, false)?;
                    }
                    Ok(Some(format!("Deleted {what}")))
                });
                this.operate(vec![(repo, delete)], |_, _, _| {}, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Adds root-anchored patterns for unversioned files to the
    /// repository's `.gitignore`.
    fn ignore(&mut self, repo: usize, entries: Vec<Entry>, cx: &mut Context<Self>) {
        let Some(repository) = self.repos.get(repo).map(|repo| repo.repository.clone()) else {
            return;
        };
        let paths: Vec<String> = entries
            .into_iter()
            .filter(|entry| entry.unstaged == Some(Change::Untracked))
            .map(|entry| entry.path)
            .collect();
        let project = self.project.clone();
        let ignore = job(move |_| {
            let file = project.repository_path(&repository, ".gitignore");
            let existing = match project.read_text(&file) {
                Ok(text) => text,
                Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
                Err(err) => return Err(err),
            };
            let text = git::ignore_entries(&existing, &paths);
            project.write(&file, text.into_bytes())?;
            Ok(Some(format!(
                "Added {} pattern(s) to .gitignore",
                paths.len()
            )))
        });
        self.operate(vec![(repo, ignore)], |_, _, _| {}, cx);
    }

    fn copy_paths(&self, repo: usize, entries: &[Entry], cx: &mut Context<Self>) {
        let Some(repo) = self.repos.get(repo) else {
            return;
        };
        let text = entries
            .iter()
            .map(|entry| {
                self.project
                    .repository_path(&repo.repository, &entry.path)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// The selected row's repository.
    fn selected_repo(&self) -> Option<usize> {
        let row = self.selected?;
        self.rows.get(row).map(|(repo, _)| *repo)
    }

    /// Commits the checked files (or, in staging mode, what is staged) of
    /// every repository having some, with the same message; with `push`,
    /// asks to push each afterwards.
    pub fn commit(&mut self, push: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let message = self.message.read(cx).text();
        let amend = self.amend;
        if message.trim().is_empty() {
            self.feedback = Some(("Enter a commit message".into(), theme::error));
            return cx.notify();
        }
        let staging = self.staging;
        let mut chosen: Vec<(usize, Vec<Entry>)> = if staging {
            self.repos
                .iter()
                .enumerate()
                .filter(|(_, repo)| repo.status.entries.iter().any(|e| e.staged.is_some()))
                .map(|(index, _)| (index, Vec::new()))
                .collect()
        } else {
            self.checked_entries()
        };
        if chosen.is_empty() && amend {
            // Only the message changes: the repository is the only one,
            // else the selected row's.
            let repo = match self.repos.len() {
                1 => Some(0),
                _ => self.selected_repo(),
            };
            let Some(repo) = repo else {
                self.feedback = Some((
                    "Select a row of the repository whose last commit to amend".into(),
                    theme::error,
                ));
                return cx.notify();
            };
            chosen.push((repo, Vec::new()));
        }
        if chosen.is_empty() {
            let error = if staging {
                "Nothing is staged; stage changes first"
            } else {
                "No changes are checked for the commit"
            };
            self.feedback = Some((error.into(), theme::error));
            return cx.notify();
        }
        let jobs = chosen
            .into_iter()
            .map(|(repo, entries)| {
                let message = message.clone();
                let commit = job(move |repo| {
                    let summary = if staging {
                        repo.commit(&message, amend)?
                    } else {
                        repo.commit_entries(&message, &entries, amend)?
                    };
                    let verb = if amend { "Amended" } else { "Committed" };
                    Ok(Some(format!("{verb} {summary}")))
                });
                (repo, commit)
            })
            .collect();
        self.operate(
            jobs,
            move |this, committed, cx| {
                this.amend = false;
                this.message
                    .update(cx, |editor, cx| editor.set_text("", cx));
                if push {
                    this.pushes = committed.into();
                    if let Some(repository) = this.pushes.pop_front() {
                        cx.emit(CommitPanelEvent::Push(repository));
                    }
                }
            },
            cx,
        );
    }

    fn commit_action(&mut self, _: &CommitChanges, _: &mut Window, cx: &mut Context<Self>) {
        self.commit(false, cx);
    }

    /// Amend toggled on with an empty message: start from the last one (of
    /// the first repository with checked files, else the selected row's).
    fn toggle_amend(&mut self, cx: &mut Context<Self>) {
        self.amend = !self.amend;
        cx.notify();
        if !self.amend || !self.message.read(cx).text().trim().is_empty() {
            return;
        }
        let repo = self
            .checked_entries()
            .first()
            .map(|(repo, _)| *repo)
            .or_else(|| self.selected_repo())
            .unwrap_or(0);
        let Some(repository) = self.repos.get(repo).map(|repo| repo.repository.clone()) else {
            return;
        };
        let message = self.message.clone();
        cx.spawn(async move |_, cx| {
            let last = cx
                .background_spawn(async move { repository.head_message() })
                .await;
            if let Ok(Some(last)) = last {
                message
                    .update(cx, |editor, cx| {
                        if editor.text().is_empty() {
                            editor.set_text(&last, cx);
                        }
                    })
                    .ok();
            }
        })
        .detach();
    }

    /// Fetch or fast-forward pull of every repository, off the main
    /// thread.
    pub fn sync(&mut self, action: SyncAction, cx: &mut Context<Self>) {
        let jobs = (0..self.repos.len())
            .map(|repo| {
                let sync = job(move |repo| {
                    Ok(Some(match action {
                        SyncAction::Fetch => {
                            repo.fetch()?;
                            "Fetched".into()
                        }
                        SyncAction::Pull => {
                            repo.pull()?;
                            "Pulled".into()
                        }
                    }))
                });
                (repo, sync)
            })
            .collect();
        self.operate(jobs, |_, _, _| {}, cx);
    }

    /// Abort or continue the operation in progress in repository `repo`.
    fn finish_operation(&mut self, repo: usize, abort: bool, cx: &mut Context<Self>) {
        let Some(state) = self.repos.get(repo).map(|repo| repo.operation) else {
            return;
        };
        let finish = job(move |repo| {
            if abort {
                repo.abort(state)?;
                return Ok(Some("Aborted".into()));
            }
            Ok(Some(match repo.continue_rebase()? {
                git::Outcome::Done => "Rebase finished".into(),
                git::Outcome::Conflicts => "The next commit conflicts: resolve it below".into(),
            }))
        });
        self.operate(vec![(repo, finish)], |_, _, _| {}, cx);
    }

    /// Opens a file's diff: HEAD ↔ working tree without a staging area,
    /// else the index side of its group. Conflicted files open in the
    /// editor to resolve.
    fn show_diff(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some((
            repo,
            Row::File {
                group,
                entry,
                change,
                ..
            },
        )) = self.rows.get(row)
        else {
            return;
        };
        let Repo {
            repository, status, ..
        } = &self.repos[*repo];
        let entry = status.entries[*entry].clone();
        let event = match group {
            Group::Conflicts => {
                CommitPanelEvent::OpenFile(self.project.repository_path(repository, &entry.path))
            }
            Group::Staged | Group::Unstaged => CommitPanelEvent::OpenDiff {
                repository: repository.clone(),
                source: DiffSource::Index {
                    entry,
                    staged: *group == Group::Staged,
                },
            },
            Group::Changes | Group::Unversioned => {
                let old = if status.unborn || matches!(change, Change::Added | Change::Untracked) {
                    Side::Empty
                } else {
                    Side::Revision("HEAD".into())
                };
                CommitPanelEvent::OpenDiff {
                    repository: repository.clone(),
                    source: DiffSource::Revisions {
                        old,
                        new: Side::Worktree,
                        change: FileChange {
                            path: entry.path,
                            orig_path: entry.orig_path,
                            change: *change,
                        },
                    },
                }
            }
        };
        cx.emit(event);
    }

    fn jump_to_source(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some((repo, Row::File { entry, change, .. })) = self.rows.get(row) else {
            return;
        };
        if *change != Change::Deleted {
            let repo = &self.repos[*repo];
            let path = &repo.status.entries[*entry].path;
            cx.emit(CommitPanelEvent::OpenFile(
                self.project.repository_path(&repo.repository, path),
            ));
        }
    }

    fn toggle_collapsed(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some((repo, row)) = self.rows.get(row) else {
            return;
        };
        let repo = *repo;
        let key = match row {
            Row::Repo { .. } => None,
            Row::Group { group, .. } => Some((*group, String::new())),
            Row::Dir { group, path, .. } => Some((*group, path.clone())),
            Row::File { .. } => return,
        };
        let repo = &mut self.repos[repo];
        match key {
            None => repo.folded = !repo.folded,
            Some(key) => {
                if !repo.collapsed.remove(&key) {
                    repo.collapsed.insert(key);
                }
            }
        }
        self.rebuild_rows();
        cx.notify();
    }

    fn expand_all(&mut self, cx: &mut Context<Self>) {
        for repo in &mut self.repos {
            repo.collapsed.clear();
            repo.folded = false;
        }
        self.rebuild_rows();
        cx.notify();
    }

    /// Collapses every group and folder (repository headers stay open, so
    /// their groups show).
    fn collapse_all(&mut self, cx: &mut Context<Self>) {
        for repo in &mut self.repos {
            repo.collapsed = all_folders(&repo.status, self.staging);
        }
        self.selected = None;
        self.rebuild_rows();
        cx.notify();
    }

    fn click_row(
        &mut self,
        row: usize,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu = None;
        self.selected = Some(row);
        window.focus(&self.focus_handle);
        if event.click_count() >= 2 {
            match self.rows.get(row) {
                Some((_, Row::File { .. })) => self.show_diff(row, cx),
                Some(_) => self.toggle_collapsed(row, cx),
                None => {}
            }
        }
        cx.notify();
    }

    fn open_menu(
        &mut self,
        row: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected = Some(row);
        window.focus(&self.focus_handle);
        let entries = self.entries_under(row);
        if let Some(&(repo, _)) = self.rows.get(row)
            && !entries.is_empty()
        {
            self.menu = Some(ContextMenu {
                position: event.position,
                repo,
                entries,
            });
        }
        cx.notify();
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(self.selected.map_or(0, |row| row.saturating_sub(1)));
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if !self.rows.is_empty() {
            self.selected = Some(
                self.selected
                    .map_or(0, |row| (row + 1).min(self.rows.len() - 1)),
            );
            cx.notify();
        }
    }

    fn toggle_selected(&mut self, _: &ToggleChecked, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected
            && self
                .rows
                .get(row)
                .is_some_and(|(_, row)| row.group().is_none_or(Group::checkable))
            && !self.staging
        {
            self.toggle_checked(row, cx);
        }
    }

    fn open_selected(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        match self.selected.and_then(|row| self.rows.get(row)) {
            Some((_, Row::File { .. })) => self.show_diff(self.selected.unwrap_or_default(), cx),
            Some(_) => self.toggle_collapsed(self.selected.unwrap_or_default(), cx),
            None => {}
        }
    }

    fn show_diff_action(&mut self, _: &ShowDiff, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected {
            self.show_diff(row, cx);
        }
    }

    fn jump_to_source_action(&mut self, _: &JumpToSource, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected {
            self.jump_to_source(row, cx);
        }
    }

    /// Changes | Shelf, as PyCharm's Commit window tabs.
    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = |id: &'static str, label: &'static str, tab: Tab, active: bool| {
            div()
                .id(id)
                .h_full()
                .px_2()
                .flex()
                .items_center()
                .text_sm()
                .cursor_pointer()
                .border_b_2()
                .border_color(if active {
                    theme::accent()
                } else {
                    gpui::transparent_black()
                })
                .text_color(if active {
                    theme::text()
                } else {
                    theme::muted()
                })
                .hover(|style| style.text_color(theme::text()))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.select_tab(tab, cx)))
        };
        div()
            .h(px(26.))
            .px_1()
            .flex()
            .flex_none()
            .items_end()
            .border_b_1()
            .border_color(theme::border())
            .child(tab(
                "commit-tab-changes",
                "Changes",
                Tab::Changes,
                self.tab == Tab::Changes,
            ))
            .child(tab(
                "commit-tab-shelf",
                "Shelf",
                Tab::Shelf,
                self.tab == Tab::Shelf,
            ))
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let button = |id: &'static str, glyph: &'static str, title: &'static str, active: bool| {
            div()
                .id(id)
                .flex_none()
                .w(px(24.))
                .h(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .text_sm()
                .text_color(theme::text())
                .when(active, |button| button.bg(theme::active_row()))
                .cursor_pointer()
                .hover(|style| style.bg(theme::hover_row()))
                .tooltip(move |window, cx| Tooltip::view(title, window, cx))
                .child(glyph)
        };
        let selected_file = self
            .selected
            .is_some_and(|row| matches!(self.rows.get(row), Some((_, Row::File { .. }))));
        div()
            .h(px(28.))
            .px_1()
            .flex()
            .flex_none()
            .items_center()
            .gap_0p5()
            .border_b_1()
            .border_color(theme::border())
            .child(
                button("commit-refresh", "⟳", "Refresh", false)
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
            .child(
                button("commit-rollback", "↶", "Rollback…", false).on_click(
                    cx.listener(|this, _, window, cx| this.rollback_selection(window, cx)),
                ),
            )
            .child(
                button("commit-show-diff", "⇄", "Show Diff (Ctrl+D)", false)
                    .when(!selected_file, |button| button.text_color(theme::muted()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(row) = this.selected {
                            this.show_diff(row, cx)
                        }
                    })),
            )
            .child(div().w(px(1.)).h(px(16.)).mx_1().bg(theme::border()))
            .child(
                button("commit-expand-all", "⊞", "Expand All", false)
                    .on_click(cx.listener(|this, _, _, cx| this.expand_all(cx))),
            )
            .child(
                button("commit-collapse-all", "⊟", "Collapse All", false)
                    .on_click(cx.listener(|this, _, _, cx| this.collapse_all(cx))),
            )
            .child(
                button(
                    "commit-group-by-directory",
                    "▤",
                    "Group By Directory",
                    self.by_directory,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.by_directory = !this.by_directory;
                    this.rebuild_rows();
                    cx.notify();
                })),
            )
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some((repo_index, row)) = self.rows.get(index) else {
            return div().into_any_element();
        };
        let repo = &self.repos[*repo_index];
        let status = &repo.status;
        let group = row.group();
        // Under a repository header with several repositories.
        let nested = usize::from(self.repos.len() > 1);
        let depth = match row {
            Row::Repo { .. } => 0,
            Row::Group { .. } => nested,
            Row::Dir { depth, .. } | Row::File { depth, .. } => *depth + nested,
        };
        let selected = self.selected == Some(index);
        let actions = group.and_then(|group| self.row_actions(index, group, cx));
        let checkbox = (!self.staging && group.is_none_or(Group::checkable)).then(|| {
            checkbox(self.check_state(index))
                .id(("commit-check", index))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_checked(index, cx)
                }))
        });
        let files = |count: usize| match count {
            1 => "1 file".to_owned(),
            n => format!("{n} files"),
        };
        let chevron = |collapsed: bool| {
            div()
                .id(("commit-chevron", index))
                .w(px(12.))
                .flex_none()
                .text_xs()
                .text_color(theme::muted())
                .child(if collapsed { "▸" } else { "▾" })
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_collapsed(index, cx)
                }))
        };
        let base = div()
            .id(("commit-row", index))
            .w_full()
            .h(px(ROW_HEIGHT))
            .pl(px(6. + INDENT * depth as f32))
            .pr_2()
            .flex()
            .items_center()
            .gap_1p5()
            .text_sm()
            .when(selected, |row| row.bg(theme::active_row()))
            .when(!selected, |row| {
                row.hover(|style| style.bg(theme::hover_row()))
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.click_row(index, event, window, cx)
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.open_menu(index, event, window, cx)
                }),
            );
        let Some(group) = group else {
            let Row::Repo { count, collapsed } = row else {
                return base.into_any_element();
            };
            return base
                .child(chevron(*collapsed))
                .children(checkbox)
                .child(div().flex_none().text_color(theme::muted()).child("⎇"))
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme::text())
                        .child(row_text("repo", repo.name().to_owned())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(row_text(
                            "repo-detail",
                            format!("{} · {}", branch_label(status), files(*count)),
                        )),
                )
                .into_any_element();
        };
        match row {
            Row::Repo { .. } => base.into_any_element(),
            Row::Group {
                count, collapsed, ..
            } => base
                .child(chevron(*collapsed))
                .children(checkbox)
                .child(
                    div()
                        .flex_none()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(if group == Group::Conflicts {
                            theme::git_conflicted()
                        } else {
                            theme::text()
                        })
                        .child(row_text("title", group.title())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(row_text("count", files(*count))),
                )
                .children(actions)
                .into_any_element(),
            Row::Dir {
                label, collapsed, ..
            } => base
                .child(chevron(*collapsed))
                .children(checkbox)
                .child(div().flex_none().text_color(theme::muted()).child("▱"))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(theme::text())
                        .child(row_text("label", label.clone())),
                )
                .children(actions)
                .into_any_element(),
            Row::File {
                entry, change, dir, ..
            } => {
                let entry = &status.entries[*entry];
                let name = entry.path.rsplit('/').next().unwrap_or(&entry.path);
                let mut detail = dir.clone();
                if let Some(orig) = &entry.orig_path {
                    if !detail.is_empty() {
                        detail.push_str("  ");
                    }
                    detail.push_str(&format!("← {orig}"));
                }
                base.child(div().w(px(12.)).flex_none())
                    .children(checkbox)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .gap_2()
                            .overflow_hidden()
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(change_color(*change))
                                    .when(*change == Change::Deleted, |name| name.line_through())
                                    .child(row_text("name", name.to_owned())),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(row_text("detail", detail)),
                            ),
                    )
                    .children(actions)
                    .into_any_element()
            }
        }
    }

    /// Hover buttons of a row: stage/unstage in the staging-area groups,
    /// Mark Resolved for conflicts.
    fn row_actions(
        &self,
        index: usize,
        group: Group,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let (glyph, title, stage): (&'static str, &'static str, bool) = match group {
            Group::Staged => ("−", "Unstage", false),
            Group::Unstaged => ("+", "Stage", true),
            Group::Unversioned if self.staging => ("+", "Stage", true),
            Group::Conflicts => ("✓", "Mark Resolved", true),
            _ => return None,
        };
        Some(
            div()
                .id(("commit-row-action", index))
                .px_1()
                .flex_none()
                .text_color(theme::muted())
                .hover(|style| style.text_color(theme::text()))
                .cursor_pointer()
                .tooltip(move |window, cx| Tooltip::view(title, window, cx))
                .child(glyph)
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    let Some(&(repo, _)) = this.rows.get(index) else {
                        return;
                    };
                    let status = &this.repos[repo].status;
                    let entries: Vec<Entry> = this
                        .entries_under(index)
                        .into_iter()
                        .map(|entry| status.entries[entry].clone())
                        .collect();
                    if stage {
                        this.stage(repo, entries, cx)
                    } else {
                        this.unstage(repo, entries, cx)
                    }
                })),
        )
    }

    fn render_menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let menu = self.menu.as_ref()?;
        let repo = menu.repo;
        let status = &self.repos.get(repo)?.status;
        let entries: Vec<Entry> = menu
            .entries
            .iter()
            .map(|&entry| status.entries[entry].clone())
            .collect();
        let untracked = entries
            .iter()
            .all(|entry| entry.unstaged == Some(Change::Untracked));
        let single_file = matches!(
            self.selected.and_then(|row| self.rows.get(row)),
            Some((_, Row::File { .. }))
        );
        type Handler = Box<
            dyn Fn(&mut CommitPanel, usize, Vec<Entry>, &mut Window, &mut Context<CommitPanel>),
        >;
        let mut items: Vec<(&'static str, Handler)> = Vec::new();
        if !self.staging {
            items.push((
                "Commit File…",
                Box::new(|this, repo, entries, window, cx| {
                    // Exactly these files: other repositories' are unchecked.
                    for (index, other) in this.repos.iter_mut().enumerate() {
                        let paths: Vec<&str> = if index == repo {
                            entries.iter().map(|e| e.path.as_str()).collect()
                        } else {
                            Vec::new()
                        };
                        let all = other.status.entries.iter().map(|e| e.path.as_str());
                        other.inclusion.only(all, &paths);
                    }
                    this.focus(window, cx);
                }),
            ));
        }
        items.push((
            "Rollback…",
            Box::new(|this, repo, entries, window, cx| {
                this.rollback(vec![(repo, entries)], window, cx)
            }),
        ));
        items.push((
            "Shelve Changes…",
            Box::new(|this, repo, entries, _, cx| this.request_shelve(repo, entries, cx)),
        ));
        items.push((
            "Stash Changes…",
            Box::new(|this, repo, entries, _, cx| this.request_stash(repo, entries, cx)),
        ));
        if single_file {
            items.push((
                "Show Diff",
                Box::new(|this, _, _, _, cx| {
                    if let Some(row) = this.selected {
                        this.show_diff(row, cx)
                    }
                }),
            ));
            items.push((
                "Jump to Source",
                Box::new(|this, _, _, _, cx| {
                    if let Some(row) = this.selected {
                        this.jump_to_source(row, cx)
                    }
                }),
            ));
        }
        if self.staging {
            items.push((
                "Stage",
                Box::new(|this, repo, entries, _, cx| this.stage(repo, entries, cx)),
            ));
            items.push((
                "Unstage",
                Box::new(|this, repo, entries, _, cx| this.unstage(repo, entries, cx)),
            ));
        }
        if untracked {
            items.push((
                "Add to VCS",
                Box::new(|this, repo, entries, _, cx| this.add_to_vcs(repo, entries, cx)),
            ));
            items.push((
                "Ignore (add to .gitignore)",
                Box::new(|this, repo, entries, _, cx| this.ignore(repo, entries, cx)),
            ));
        }
        items.push((
            "Delete…",
            Box::new(|this, repo, entries, window, cx| this.delete(repo, entries, window, cx)),
        ));
        items.push((
            "Copy Path",
            Box::new(|this, repo, entries, _, cx| this.copy_paths(repo, &entries, cx)),
        ));
        let rows = items
            .into_iter()
            .enumerate()
            .map(|(index, (label, handler))| {
                let entries = entries.clone();
                div()
                    .id(("commit-menu-item", index))
                    .px_3()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::active_row()))
                    .child(label)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.menu = None;
                        handler(this, repo, entries.clone(), window, cx);
                        cx.notify();
                    }))
            });
        Some(deferred(
            anchored().position(menu.position).child(
                div()
                    .id("commit-menu")
                    .occlude()
                    .min_w(px(220.))
                    .p_1()
                    .flex()
                    .flex_col()
                    .text_sm()
                    .text_color(theme::text())
                    .bg(theme::panel())
                    .border_1()
                    .border_color(theme::border())
                    .rounded_md()
                    .shadow_lg()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.menu = None;
                        cx.notify();
                    }))
                    .children(rows),
            ),
        ))
    }

    /// Merge/rebase banners, one per repository with an operation in
    /// progress: what it is and how to finish it.
    fn render_operations(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let several = self.repos.len() > 1;
        let button = |id: (&'static str, usize), label: &'static str| {
            div()
                .id(id)
                .px_2()
                .rounded_sm()
                .border_1()
                .border_color(theme::border())
                .cursor_pointer()
                .hover(|style| style.bg(theme::hover_row()))
                .child(label)
        };
        let mut banners = Vec::new();
        for (index, repo) in self.repos.iter().enumerate() {
            let text = match repo.operation {
                OperationState::None => continue,
                OperationState::Merging => "Merging: resolve conflicts, then Commit",
                OperationState::Rebasing => "Rebasing: resolve conflicts, then Continue",
                OperationState::CherryPicking => "Cherry-picking: resolve conflicts, then Commit",
                OperationState::Reverting => "Reverting: resolve conflicts, then Commit",
            };
            let text = if several {
                format!("{}: {text}", repo.name())
            } else {
                text.to_owned()
            };
            let rebasing = repo.operation == OperationState::Rebasing;
            banners.push(
                banner()
                    .child(
                        div()
                            .flex_1()
                            .text_color(theme::warning())
                            .child(selectable(("operation", index), text)),
                    )
                    .when(rebasing, |banner| {
                        banner.child(button(("operation-continue", index), "Continue").on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.finish_operation(index, false, cx)
                            }),
                        ))
                    })
                    .child(button(("operation-abort", index), "Abort").on_click(
                        cx.listener(move |this, _, _, cx| this.finish_operation(index, true, cx)),
                    ))
                    .into_any_element(),
            );
        }
        banners
    }

    /// Beside the repositories that could be read: the ones git refuses
    /// (with Trust) and the folders that failed.
    fn render_problems(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        if self.repos.is_empty() {
            return Vec::new();
        }
        let mut banners: Vec<gpui::AnyElement> = Vec::new();
        for (index, path) in self.untrusted.iter().enumerate() {
            let trusted = path.clone();
            banners.push(
                banner()
                    .child(
                        div()
                            .flex_1()
                            .text_color(theme::warning())
                            .child(selectable(
                                ("untrusted", index),
                                format!("Git refuses {path}: its folder belongs to another user."),
                            )),
                    )
                    .child(
                        div()
                            .id(("trust-repository", index))
                            .px_2()
                            .rounded_sm()
                            .border_1()
                            .border_color(theme::border())
                            .cursor_pointer()
                            .hover(|style| style.bg(theme::hover_row()))
                            .child("Trust This Folder")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.trust(trusted.to_string(), cx)
                            })),
                    )
                    .into_any_element(),
            );
        }
        for (index, error) in self.errors.iter().enumerate() {
            banners.push(
                banner()
                    .child(
                        div()
                            .flex_1()
                            .text_color(theme::error())
                            .child(selectable(("repository-error", index), error.clone())),
                    )
                    .into_any_element(),
            );
        }
        banners
    }

    /// Several repositories: which one's shelves the Shelf tab lists.
    fn render_shelf_repos(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        if self.repos.len() < 2 {
            return None;
        }
        Some(
            div()
                .h(px(28.))
                .px_1()
                .flex()
                .flex_none()
                .items_center()
                .gap_0p5()
                .border_b_1()
                .border_color(theme::border())
                .text_sm()
                .children(self.repos.iter().enumerate().map(|(index, repo)| {
                    let active = index == self.shelf_repo;
                    div()
                        .id(("shelf-repo", index))
                        .px_2()
                        .h(px(22.))
                        .flex()
                        .items_center()
                        .rounded_sm()
                        .cursor_pointer()
                        .text_color(if active {
                            theme::text()
                        } else {
                            theme::muted()
                        })
                        .when(active, |button| button.bg(theme::active_row()))
                        .hover(|style| style.bg(theme::hover_row()))
                        .child(repo.name().to_owned())
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.select_shelf_repo(index, cx)),
                        )
                })),
        )
    }

    fn render_commit_area(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let empty_message = self.message.read(cx).text().is_empty();
        let lines = self.message.read(cx).buffer().line_count().clamp(4, 10);
        let busy = self.busy;
        let commit_label = match (busy, self.amend) {
            (true, _) => "Committing…",
            (false, true) => "Amend Commit",
            (false, false) => "Commit",
        };
        let push_label = if self.amend {
            "Amend Commit and Push…"
        } else {
            "Commit and Push…"
        };
        div()
            .flex_none()
            .px_2()
            .py_2()
            .flex()
            .flex_col()
            .gap_1p5()
            .border_t_1()
            .border_color(theme::border())
            .child(
                div()
                    .id("commit-amend")
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .text_sm()
                    .cursor_pointer()
                    .child(checkbox(if self.amend {
                        Check::Checked
                    } else {
                        Check::Unchecked
                    }))
                    .child(selectable("amend", "Amend"))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_amend(cx))),
            )
            .child(
                div()
                    .key_context("CommitMessage")
                    .relative()
                    .h(px(theme::EDITOR_LINE_HEIGHT * lines as f32 + 10.))
                    .px_1()
                    .py_1()
                    .border_1()
                    .border_color(theme::border())
                    .rounded_sm()
                    .bg(theme::background())
                    .child(self.message.clone())
                    .when(empty_message, |area| {
                        area.child(
                            div()
                                .absolute()
                                .top(px(5.))
                                .left(px(6.))
                                .text_sm()
                                .text_color(theme::muted())
                                .child("Commit Message"),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("commit-button")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .text_sm()
                            .bg(if busy {
                                theme::border()
                            } else {
                                theme::accent()
                            })
                            .text_color(theme::on_accent())
                            .cursor_pointer()
                            .tooltip(|window, cx| Tooltip::view("Commit (Ctrl+Enter)", window, cx))
                            .child(commit_label)
                            .on_click(cx.listener(|this, _, _, cx| this.commit(false, cx))),
                    )
                    .child(
                        div()
                            .id("commit-and-push-button")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .text_sm()
                            .border_1()
                            .border_color(theme::border())
                            .text_color(if busy { theme::muted() } else { theme::text() })
                            .cursor_pointer()
                            .hover(|style| style.bg(theme::hover_row()))
                            .child(push_label)
                            .on_click(cx.listener(|this, _, _, cx| this.commit(true, cx))),
                    ),
            )
            .children(self.feedback.clone().map(|(text, color)| {
                div()
                    .text_xs()
                    .text_color(color())
                    .child(selectable("feedback", text))
            }))
    }
}

impl Focusable for CommitPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CommitPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // `git.staging_area` changed in settings.json: switch modes.
        let staging = settings::current().git.staging_area;
        if staging != self.staging {
            self.staging = staging;
            for repo in &mut self.repos {
                repo.collapsed.clear();
            }
            self.rebuild_rows();
        }
        let message = |text: SharedString, color: Hsla| {
            div()
                .p_3()
                .text_sm()
                .text_color(color)
                .child(selectable("message", text))
                .into_any_element()
        };
        let ready = !self.repos.is_empty();
        let clean = self.repos.iter().all(|repo| repo.status.entries.is_empty());
        let body = if !self.loaded {
            div().flex_1().into_any_element()
        } else if ready && clean {
            let unborn = self.repos.iter().all(|repo| repo.status.unborn);
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .pt_6()
                .gap_1()
                .text_sm()
                .text_color(theme::muted())
                .child(selectable("empty", "No local changes"))
                .child(div().text_xs().child(selectable(
                    "empty-detail",
                    if unborn {
                        "No commits yet"
                    } else {
                        "Everything is committed"
                    },
                )))
                .into_any_element()
        } else if ready {
            uniform_list(
                "commit-rows",
                self.rows.len(),
                cx.processor(|this, range: Range<usize>, _, cx| {
                    range.map(|index| this.render_row(index, cx)).collect()
                }),
            )
            .flex_1()
            .w_full()
            .into_any_element()
        } else if !self.untrusted.is_empty() {
            div()
                .flex()
                .flex_col()
                .children(self.untrusted.iter().enumerate().map(|(index, path)| {
                    let trusted = path.clone();
                    div()
                        .p_3()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .text_sm()
                        .child(selectable(
                            ("untrusted", index),
                            format!(
                                "Git refuses this repository because its folder ({path}) \
                                 belongs to another user than the one running git. This is \
                                 common for folders on another drive, shared folders, or \
                                 folders mounted into WSL."
                            ),
                        ))
                        .child(div().text_color(theme::muted()).child(selectable(
                            ("untrusted-hint", index),
                            "Trusting it adds the folder to git's safe.directory list.",
                        )))
                        .child(
                            div()
                                .id(("trust-repository", index))
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .border_1()
                                .border_color(theme::border())
                                .hover(|style| style.bg(theme::hover_row()))
                                .cursor_pointer()
                                .child("Trust This Folder")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.trust(trusted.to_string(), cx)
                                })),
                        )
                }))
                .into_any_element()
        } else if !self.errors.is_empty() {
            let errors: Vec<&str> = self.errors.iter().map(SharedString::as_ref).collect();
            message(errors.join("\n").into(), theme::error())
        } else if self.project.is_multi_root() {
            message(
                "No folder of this workspace is in a git repository.".into(),
                theme::muted(),
            )
        } else {
            message(
                "This project is not in a git repository.".into(),
                theme::muted(),
            )
        };
        let changes = !ready || self.tab == Tab::Changes;
        // Below the shelf (or the error), where there is no commit area.
        let feedback = (!changes || !ready)
            .then(|| self.feedback.clone())
            .flatten()
            .map(|(text, color)| {
                div()
                    .px_2()
                    .pb_1()
                    .text_xs()
                    .text_color(color())
                    .child(selectable("feedback", text))
            });
        div()
            .key_context("CommitPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::commit_action))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::toggle_selected))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::show_diff_action))
            .on_action(cx.listener(Self::jump_to_source_action))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::panel())
            .when(ready, |panel| panel.child(self.render_tabs(cx)))
            .when(ready && changes, |panel| {
                panel.child(self.render_toolbar(cx))
            })
            .children(self.render_operations(cx))
            .children(self.render_problems(cx))
            .when(changes, |panel| {
                panel.child(div().flex_1().min_h_0().flex().flex_col().child(body))
            })
            .when(!changes, |panel| {
                panel.children(self.render_shelf_repos(cx)).child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .child(self.shelf.clone()),
                )
            })
            .when(ready && changes, |panel| {
                panel.child(self.render_commit_area(cx))
            })
            .children(feedback)
            .children(self.render_menu(cx))
    }
}

/// Re-reading status after file events is cheap but not free; batches
/// closer together than this share one refresh.
pub const REFRESH_DEBOUNCE: Duration = Duration::from_millis(300);

/// Repositories on disk for tests (also the workspace's).
#[cfg(test)]
pub(crate) mod test_support {
    use std::{
        path::{Path, PathBuf},
        process::Command,
    };

    use crate::{
        project::{Location, Project},
        workspace_file::Folder,
    };

    /// Runs git in `dir` (it must succeed); returns its output.
    pub fn git(dir: &Path, args: &[&str]) -> String {
        let mut command = Command::new("git");
        remote::launch::no_window(&mut command);
        let output = command
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// A repository at `dir` with `a.txt` committed on `branch`.
    pub fn repository(dir: &Path, branch: &str) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", branch]);
        for (key, value) in [
            ("user.name", "Test"),
            ("user.email", "test@example.com"),
            ("commit.gpgsign", "false"),
            ("core.autocrlf", "false"),
        ] {
            git(dir, &["config", key, value]);
        }
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", "first"]);
    }

    /// A fresh temp dir holding repositories `api` (branch `main`) and
    /// `web` (branch `dev`) and a folder `docs` outside any repository;
    /// the project's folders are api, web, docs.
    pub fn three_folders(name: &str) -> (PathBuf, Project) {
        let dir = std::env::temp_dir().join(format!("next_code_{name}_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        repository(&dir.join("api"), "main");
        repository(&dir.join("web"), "dev");
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(dir.join("docs/notes.txt"), "notes\n").unwrap();
        let folders = ["api", "web", "docs"]
            .map(|folder| Folder {
                path: dir.join(folder).to_string_lossy().into_owned(),
                name: None,
            })
            .to_vec();
        let location = Location::Workspace {
            distro: None,
            file: None,
            folders,
        };
        let project = Project::open(&location, &mut |_| {}).unwrap();
        (dir, project)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn file_events_do_not_cancel_an_in_flight_status_read(cx: &mut gpui::TestAppContext) {
        let (sender, receiver) = futures::channel::oneshot::channel();
        let panel =
            cx.new(|cx| CommitPanel::new(Project::local(std::env::temp_dir()), Some(receiver), cx));
        cx.run_until_parked();

        // The startup status is slow; file events and explicit refreshes
        // must leave its receiver alive and request just one later read.
        for _ in 0..5 {
            panel.update(cx, |panel, cx| {
                panel.schedule_refresh(cx);
                panel.refresh(cx);
            });
            cx.executor().advance_clock(REFRESH_DEBOUNCE);
            cx.run_until_parked();
        }
        assert!(sender.send(Ok(None)).is_ok(), "status read was cancelled");
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(panel.loaded && panel.repos.is_empty(), "no repository");
            assert!(!panel.refresh_running);
            assert!(!panel.refresh_requested);
            assert!(panel.refresh_scheduled, "missing follow-up status read");
        });
    }

    #[gpui::test]
    fn continuous_file_events_keep_the_first_refresh_deadline(cx: &mut gpui::TestAppContext) {
        let (sender, receiver) = futures::channel::oneshot::channel();
        let panel =
            cx.new(|cx| CommitPanel::new(Project::local(std::env::temp_dir()), Some(receiver), cx));
        assert!(sender.send(Ok(None)).is_ok());
        cx.run_until_parked();
        panel.update(cx, |panel, cx| panel.schedule_refresh(cx));
        cx.run_until_parked();
        cx.executor().advance_clock(REFRESH_DEBOUNCE / 2);
        panel.update(cx, |panel, cx| panel.schedule_refresh(cx));
        cx.executor().advance_clock(REFRESH_DEBOUNCE / 2);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert!(!panel.refresh_scheduled, "file activity postponed the read");
        });
    }

    fn entry(path: &str, staged: Option<Change>, unstaged: Option<Change>) -> Entry {
        Entry {
            path: path.into(),
            orig_path: None,
            staged,
            unstaged,
        }
    }

    fn status() -> Status {
        Status {
            branch: Some("main".into()),
            entries: vec![
                entry("README.md", None, Some(Change::Modified)),
                entry("conflict.txt", None, Some(Change::Conflicted)),
                entry("new/one.txt", None, Some(Change::Untracked)),
                entry(
                    "src/app/main.rs",
                    Some(Change::Modified),
                    Some(Change::Modified),
                ),
                entry("src/app/old.rs", None, Some(Change::Deleted)),
                entry("src/lib.rs", Some(Change::Added), None),
            ],
            ..Status::default()
        }
    }

    fn summary(rows: &[Row], status: &Status) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                Row::Repo { count, .. } => format!("repo ({count})"),
                Row::Group { group, count, .. } => format!("{} ({count})", group.title()),
                Row::Dir { label, depth, .. } => format!("{}{label}/", "  ".repeat(*depth)),
                Row::File {
                    entry,
                    change,
                    depth,
                    ..
                } => format!(
                    "{}{} {}",
                    "  ".repeat(*depth),
                    status.entries[*entry].path.rsplit('/').next().unwrap(),
                    change.letter()
                ),
            })
            .collect()
    }

    #[test]
    fn without_a_staging_area_changes_are_one_group_beside_conflicts_and_unversioned() {
        let status = status();
        let rows = build_rows(&status, false, true, &Collapsed::new());
        assert_eq!(
            summary(&rows, &status),
            [
                "Merge Conflicts (1)",
                "  conflict.txt !",
                "Changes (4)",
                "  src/",
                "    app/",
                "      main.rs M",
                "      old.rs D",
                "    lib.rs A",
                "  README.md M",
                "Unversioned Files (1)",
                "  new/",
                "    one.txt U",
            ]
        );
        // Flat, a folder and a whole group collapsed.
        let rows = build_rows(&status, false, false, &Collapsed::new());
        assert!(summary(&rows, &status).contains(&"  main.rs M".to_owned()));
        let collapsed: Collapsed = [
            (Group::Changes, "src/app".to_owned()),
            (Group::Unversioned, String::new()),
        ]
        .into();
        let rows = build_rows(&status, false, true, &collapsed);
        assert_eq!(
            summary(&rows, &status)[2..],
            [
                "Changes (4)",
                "  src/",
                "    app/",
                "    lib.rs A",
                "  README.md M",
                "Unversioned Files (1)",
            ]
        );
    }

    #[test]
    fn the_staging_area_mode_splits_staged_and_unstaged() {
        let status = status();
        let rows = build_rows(&status, true, false, &Collapsed::new());
        assert_eq!(
            summary(&rows, &status),
            [
                "Merge Conflicts (1)",
                "  conflict.txt !",
                "Staged (2)",
                "  main.rs M",
                "  lib.rs A",
                "Unstaged (3)",
                "  README.md M",
                "  main.rs M",
                "  old.rs D",
                "Unversioned Files (1)",
                "  one.txt U",
            ]
        );
        // An empty repository still shows the Changes group (PyCharm's
        // default changelist), but no empty staging groups.
        let clean = Status::default();
        assert_eq!(
            summary(&build_rows(&clean, false, true, &Collapsed::new()), &clean),
            ["Changes (0)"]
        );
        assert!(build_rows(&clean, true, true, &Collapsed::new()).is_empty());
    }

    #[test]
    fn checkboxes_are_tri_state_and_survive_refreshes() {
        let mut inclusion = Inclusion::default();
        let changes = ["src/a.rs", "src/b.rs", "README.md"];
        assert_eq!(
            inclusion.state(changes),
            Check::Checked,
            "new files start checked"
        );
        // Unchecking one file makes its folder and group partial.
        inclusion.toggle(&["src/a.rs"]);
        assert_eq!(inclusion.state(["src/a.rs"]), Check::Unchecked);
        assert_eq!(inclusion.state(["src/a.rs", "src/b.rs"]), Check::Partial);
        assert_eq!(inclusion.state(changes), Check::Partial);
        // A partial node checks everything; a checked one unchecks all.
        inclusion.toggle(&changes);
        assert_eq!(inclusion.state(changes), Check::Checked);
        inclusion.toggle(&["src/a.rs", "src/b.rs"]);
        assert_eq!(inclusion.state(changes), Check::Partial);
        assert!(inclusion.includes("README.md"));

        // A refresh keeps choices for files still changed, forgets the
        // others; files appearing later start checked.
        let present: HashSet<&str> = ["src/a.rs", "README.md", "new.txt"].into();
        inclusion.retain(&present);
        assert!(!inclusion.includes("src/a.rs"));
        assert!(inclusion.includes("src/b.rs"), "gone, so forgotten");
        assert!(inclusion.includes("new.txt"));

        // Commit File… checks exactly the chosen files.
        inclusion.only(changes, &["README.md"]);
        assert_eq!(inclusion.state(["src/a.rs", "src/b.rs"]), Check::Unchecked);
        assert!(inclusion.includes("README.md"));
    }

    #[test]
    fn combined_changes_show_the_file_against_head() {
        assert_eq!(
            entry("a", Some(Change::Added), Some(Change::Modified)).combined_change(),
            Change::Added
        );
        assert_eq!(
            entry("a", Some(Change::Modified), Some(Change::Deleted)).combined_change(),
            Change::Deleted
        );
        assert_eq!(
            entry("a", None, Some(Change::Modified)).combined_change(),
            Change::Modified
        );
    }

    #[gpui::test]
    fn shelve_and_stash_requests_carry_the_files_and_a_default_name(cx: &mut gpui::TestAppContext) {
        use std::{cell::RefCell, process::Command, rc::Rc};
        let dir = std::env::temp_dir().join(format!("next_code_requests_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let git = |args: &[&str]| {
            let mut command = Command::new("git");
            remote::launch::no_window(&mut command);
            let output = command.arg("-C").arg(&dir).args(args).output().unwrap();
            assert!(output.status.success(), "git {args:?}: {output:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "Test"]);
        git(&["config", "user.email", "test@example.com"]);
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "first"]);
        std::fs::write(dir.join("a.txt"), "a2\n").unwrap();
        for name in ["b.txt", "c.txt", "d.txt"] {
            std::fs::write(dir.join(name), "new\n").unwrap();
        }

        let panel = cx.new(|cx| CommitPanel::new(Project::local(dir.clone()), None, cx));
        cx.run_until_parked();
        let requests = Rc::new(RefCell::new(Vec::new()));
        cx.update(|cx| {
            let requests = requests.clone();
            cx.subscribe(&panel, move |_, event, _| match event {
                CommitPanelEvent::Shelve { paths, name, .. } => {
                    requests.borrow_mut().push((paths.clone(), name.clone()))
                }
                CommitPanelEvent::Stash {
                    paths,
                    include_untracked,
                    ..
                } => requests
                    .borrow_mut()
                    .push((paths.clone(), format!("untracked: {include_untracked}"))),
                _ => {}
            })
            .detach();
        });
        panel.update(cx, |panel, cx| {
            let entries = panel.repos[0].status.entries.clone();
            assert_eq!(entries.len(), 4);
            panel.request_shelve(0, entries[..2].to_vec(), cx);
            panel.request_shelve(0, entries.clone(), cx);
            panel.message.update(cx, |message, cx| {
                message.set_text("Fix login\n\nDetails", cx)
            });
            panel.request_shelve(0, entries[..1].to_vec(), cx);
            panel.request_stash(0, entries[..1].to_vec(), cx);
            panel.request_stash(0, entries[..2].to_vec(), cx);
            panel.request_shelve(0, Vec::new(), cx);
        });
        cx.run_until_parked();
        let strings = |paths: &[&str]| paths.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        assert_eq!(
            *requests.borrow(),
            [
                (strings(&["a.txt", "b.txt"]), "a.txt, b.txt".to_owned()),
                (
                    strings(&["a.txt", "b.txt", "c.txt", "d.txt"]),
                    "a.txt, b.txt and 2 more".to_owned()
                ),
                (strings(&["a.txt"]), "Fix login".to_owned()),
                (strings(&["a.txt"]), "untracked: false".to_owned()),
                (strings(&["a.txt", "b.txt"]), "untracked: true".to_owned()),
            ]
        );
        panel.read_with(cx, |panel, _| {
            assert!(panel.feedback.is_some(), "an empty selection is reported");
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn folders_group_by_repository_case_insensitively_on_windows_drives() {
        assert_eq!(
            repository_key("C:/Work/App/"),
            repository_key("c:/work/app")
        );
        assert_ne!(
            repository_key("/home/me/App"),
            repository_key("/home/me/app")
        );
        let keys = [
            Some(repository_key("C:/Work/App")),
            None,
            Some(repository_key("/srv/web")),
            Some(repository_key("c:/work/app")),
        ];
        assert_eq!(
            group_folders(&keys),
            (
                vec![vec![0, 3], vec![2]],
                vec![Some(0), None, Some(1), Some(0)]
            )
        );
    }

    /// `(repository, file)` of each file row, in order.
    fn file_rows(panel: &CommitPanel) -> Vec<(String, String)> {
        panel
            .rows
            .iter()
            .filter_map(|(repo, row)| match row {
                Row::File { entry, .. } => {
                    let repo = &panel.repos[*repo];
                    Some((
                        repo.name().to_owned(),
                        repo.status.entries[*entry].path.clone(),
                    ))
                }
                _ => None,
            })
            .collect()
    }

    #[gpui::test]
    fn each_repository_groups_its_changes_and_commits_on_its_own(cx: &mut gpui::TestAppContext) {
        use test_support::git;
        let (dir, project) = test_support::three_folders("commit_repos");
        let (api, web) = (dir.join("api"), dir.join("web"));
        std::fs::write(api.join("a.txt"), "a2\n").unwrap();
        std::fs::write(web.join("w.txt"), "new\n").unwrap();
        std::fs::write(web.join("a.txt"), "a3\n").unwrap();
        let panel = cx.new(|cx| CommitPanel::new(project, None, cx));
        cx.run_until_parked();

        panel.update(cx, |panel, cx| {
            // docs has no repository: skipped, not an error.
            let names: Vec<&str> = panel.repos.iter().map(Repo::name).collect();
            assert_eq!(names, ["api", "web"]);
            assert_eq!(panel.root_repos, [Some(0), Some(1), None]);
            assert!(panel.errors.is_empty() && panel.untrusted.is_empty());
            let headers: Vec<usize> = panel
                .rows
                .iter()
                .enumerate()
                .filter(|(_, (_, row))| matches!(row, Row::Repo { .. }))
                .map(|(index, _)| index)
                .collect();
            assert_eq!(headers.len(), 2);
            assert_eq!(
                file_rows(panel),
                [
                    ("api".to_owned(), "a.txt".to_owned()),
                    ("web".to_owned(), "a.txt".to_owned()),
                    ("web".to_owned(), "w.txt".to_owned()),
                ]
            );

            // Files map to their own repository.
            let (repository, path) = panel.file_in_repository(&web.join("w.txt")).unwrap();
            assert!(panel.repos[1].is(&repository));
            assert_eq!(path, "w.txt");
            assert_eq!(panel.repo_index_for(&dir.join("docs/notes.txt")), None);
            let fallback = panel
                .active_repo(Some(&dir.join("docs/notes.txt")))
                .unwrap();
            assert_eq!(fallback.name(), "api");
            assert_eq!(
                panel.active_repo(Some(&web.join("a.txt"))).unwrap().name(),
                "web"
            );

            // A repository's header checks or unchecks its files only.
            panel.toggle_checked(headers[1], cx);
            let checked: Vec<usize> = panel.checked_entries().iter().map(|(r, _)| *r).collect();
            assert_eq!(checked, [0]);
            assert_eq!(panel.check_state(headers[0]), Check::Checked);
            assert_eq!(panel.check_state(headers[1]), Check::Unchecked);
            panel.toggle_checked(headers[1], cx);

            panel
                .message
                .update(cx, |message, cx| message.set_text("Both sides", cx));
            panel.commit(false, cx);
        });
        cx.run_until_parked();

        for repo in [&api, &web] {
            assert_eq!(git(repo, &["log", "-1", "--format=%s"]), "Both sides");
            assert_eq!(git(repo, &["rev-list", "--count", "HEAD"]), "2");
            assert_eq!(git(repo, &["status", "--porcelain"]), "");
        }
        panel.read_with(cx, |panel, _| {
            let (feedback, _) = panel.feedback.clone().unwrap();
            assert!(feedback.contains("api: Committed"), "{feedback}");
            assert!(feedback.contains("web: Committed"), "{feedback}");
            assert!(
                panel
                    .repos
                    .iter()
                    .all(|repo| repo.status.entries.is_empty())
            );
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[gpui::test]
    fn a_single_repository_has_no_repository_header(cx: &mut gpui::TestAppContext) {
        let dir = std::env::temp_dir().join(format!("next_code_single_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        test_support::repository(&dir, "main");
        let dir = dir.canonicalize().unwrap();
        std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
        let panel = cx.new(|cx| CommitPanel::new(Project::local(dir.clone()), None, cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let status = &panel.repos[0].status;
            let rows: Vec<Row> = panel.rows.iter().map(|(_, row)| row.clone()).collect();
            assert_eq!(summary(&rows, status), ["Changes (1)", "  a.txt M"]);
        });
        std::fs::remove_dir_all(&dir).ok();
    }
}
