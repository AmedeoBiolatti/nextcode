//! The Commit tool window (PyCharm's): local changes as a tree with a
//! checkbox per file, folder and group; Commit commits exactly the checked
//! files (`git commit --only`, untracked ones added first). With the
//! `git.staging_area` setting it shows git's index instead (Staged /
//! Unstaged groups with stage and unstage). Git runs where the project
//! lives (locally, in WSL or in a dev container).

use std::{collections::HashSet, io, ops::Range, time::Duration};

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
    settings, theme,
};

const ROW_HEIGHT: f32 = 22.;
const INDENT: f32 = 16.;

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
    /// Commit and Push: the commit is done, confirm the push.
    Push(Repository),
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
            Self::Changes => (!special).then(|| combined_change(entry)),
            Self::Staged => entry.staged.filter(|_| !special),
            Self::Unstaged => entry.unstaged.filter(|_| !special),
        }
    }
}

/// An entry's change against HEAD, staged and unstaged parts together.
fn combined_change(entry: &Entry) -> Change {
    match (entry.staged, entry.unstaged) {
        (Some(change @ (Change::Added | Change::Renamed | Change::Copied)), _) => change,
        (Some(Change::Deleted), _) | (_, Some(Change::Deleted)) => Change::Deleted,
        (Some(change), _) | (None, Some(change)) => change,
        (None, None) => Change::Modified,
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
    fn group(&self) -> Group {
        match self {
            Self::Group { group, .. } | Self::Dir { group, .. } | Self::File { group, .. } => {
                *group
            }
        }
    }

    /// What identifies the row across rebuilds.
    fn key(&self, status: &Status) -> (Group, String, bool) {
        match self {
            Self::Group { group, .. } => (*group, String::new(), true),
            Self::Dir { group, path, .. } => (*group, path.clone(), true),
            Self::File { group, entry, .. } => (*group, status.entries[*entry].path.clone(), false),
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

enum State {
    Loading,
    /// The project is not inside a git repository.
    NoRepository,
    Ready {
        repository: Repository,
        status: Status,
    },
    /// Git refuses the repository at this path: its folder belongs to
    /// another user.
    Untrusted(SharedString),
    Failed(SharedString),
}

/// The open right-click menu: where, and the files it acts on.
struct ContextMenu {
    position: Point<Pixels>,
    entries: Vec<usize>,
}

#[derive(Debug, Clone, Copy)]
pub enum SyncAction {
    Fetch,
    Pull,
}

pub struct CommitPanel {
    project: Project,
    state: State,
    /// The merge, rebase, cherry-pick or revert in progress.
    operation: OperationState,
    rows: Vec<Row>,
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
    collapsed: Collapsed,
    inclusion: Inclusion,
    selected: Option<usize>,
    menu: Option<ContextMenu>,
    focus_handle: FocusHandle,
    _refresh: Task<()>,
    refresh_running: bool,
    refresh_requested: bool,
    refresh_scheduled: bool,
    _operation: Task<()>,
    _subscription: Subscription,
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
fn row_text(id: &'static str, text: impl Into<SharedString>) -> SelectableText {
    selectable(id, text)
        .double_click_through()
        .right_click_through()
}

/// A checkbox drawn in the theme's colors.
fn checkbox(check: Check) -> gpui::Div {
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
        let message = cx.new(Editor::text_area);
        let subscription = cx.subscribe(&message, |_, _, _: &editor::EditorEvent, cx| cx.notify());
        let mut panel = Self {
            project,
            state: State::Loading,
            operation: OperationState::None,
            rows: Vec::new(),
            staging: settings::current().git.staging_area,
            message,
            feedback: None,
            busy: false,
            amend: false,
            by_directory: true,
            collapsed: Collapsed::new(),
            inclusion: Inclusion::default(),
            selected: None,
            menu: None,
            focus_handle: cx.focus_handle(),
            _refresh: Task::ready(()),
            refresh_running: false,
            refresh_requested: false,
            refresh_scheduled: false,
            _operation: Task::ready(()),
            _subscription: subscription,
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

    /// The repository, once its status has been read.
    pub fn repository(&self) -> Option<Repository> {
        match &self.state {
            State::Ready { repository, .. } => Some(repository.clone()),
            _ => None,
        }
    }

    /// The repository's last status.
    pub fn status(&self) -> Option<&Status> {
        match &self.state {
            State::Ready { status, .. } => Some(status),
            _ => None,
        }
    }

    pub fn operation(&self) -> OperationState {
        self.operation
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

    /// Re-reads the repository status (discovering the repository first)
    /// and the operation in progress.
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
        let known = self.repository();
        self._refresh = cx.spawn(async move |this, cx| {
            let (opened, operation) = cx
                .background_spawn(async move {
                    let opened = match known {
                        Some(repository) => {
                            repository.status().map(|status| Some((repository, status)))
                        }
                        None => project.open_repository(),
                    };
                    let operation = match &opened {
                        Ok(Some((repository, _))) => {
                            repository.operation_state().unwrap_or(OperationState::None)
                        }
                        _ => OperationState::None,
                    };
                    (opened, operation)
                })
                .await;
            this.update(cx, |this, cx| {
                this.operation = operation;
                this.show(opened, cx);
                this.finish_refresh(cx);
            })
            .ok();
        });
    }

    /// Shows a repository read before the panel existed (at startup); reads
    /// it now if that read was abandoned. The operation in progress is read
    /// afterwards, off the startup path.
    fn show_preloaded(&mut self, preloaded: PreloadedRepository, cx: &mut Context<Self>) {
        self.refresh_running = true;
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
            let repository = this
                .update(cx, |this, cx| {
                    this.show(opened, cx);
                    this.repository()
                })
                .ok()
                .flatten();
            if let Some(repository) = repository {
                let operation = cx
                    .background_spawn(async move { repository.operation_state() })
                    .await;
                this.update(cx, |this, cx| {
                    this.operation = operation.unwrap_or(OperationState::None);
                    this.finish_refresh(cx);
                    cx.notify();
                })
                .ok();
            } else {
                this.update(cx, |this, cx| {
                    this.finish_refresh(cx);
                })
                .ok();
            }
        });
    }

    fn finish_refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_running = false;
        if std::mem::take(&mut self.refresh_requested) {
            self.schedule_refresh(cx);
        }
    }

    fn show(&mut self, opened: io::Result<Option<(Repository, Status)>>, cx: &mut Context<Self>) {
        crate::startup::mark_once("git status shown");
        let selected = self.selected_key();
        self.state = match opened {
            Ok(Some((repository, status))) => {
                let present: HashSet<&str> =
                    status.entries.iter().map(|e| e.path.as_str()).collect();
                self.inclusion.retain(&present);
                State::Ready { repository, status }
            }
            Ok(None) => State::NoRepository,
            Err(err) => match git::unsafe_repository(&err) {
                Some(refusal) => State::Untrusted(refusal.path.clone().into()),
                None => State::Failed(err.to_string().into()),
            },
        };
        self.rebuild_rows();
        self.selected = selected.and_then(|key| self.find_row(&key));
        cx.emit(CommitPanelEvent::Refreshed);
        cx.notify();
    }

    fn selected_key(&self) -> Option<(Group, String, bool)> {
        let status = self.status()?;
        Some(self.rows.get(self.selected?)?.key(status))
    }

    fn find_row(&self, key: &(Group, String, bool)) -> Option<usize> {
        let status = self.status()?;
        self.rows.iter().position(|row| row.key(status) == *key)
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

    fn rebuild_rows(&mut self) {
        self.rows = match &self.state {
            State::Ready { status, .. } => {
                build_rows(status, self.staging, self.by_directory, &self.collapsed)
            }
            _ => Vec::new(),
        };
        if self.selected.is_some_and(|row| row >= self.rows.len()) {
            self.selected = None;
        }
    }

    /// Status entries of the files at or under `row`.
    fn entries_under(&self, row: usize) -> Vec<usize> {
        let (Some(status), Some(target)) = (self.status(), self.rows.get(row)) else {
            return Vec::new();
        };
        let group = target.group();
        let in_group = |entry: &&Entry| group.change(entry).is_some();
        match target {
            Row::File { entry, .. } => vec![*entry],
            Row::Group { .. } => status
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| in_group(entry))
                .map(|(index, _)| index)
                .collect(),
            Row::Dir { path, .. } => {
                let prefix = format!("{path}/");
                status
                    .entries
                    .iter()
                    .enumerate()
                    .filter(|(_, entry)| in_group(entry) && entry.path.starts_with(&prefix))
                    .map(|(index, _)| index)
                    .collect()
            }
        }
    }

    fn check_state(&self, row: usize) -> Check {
        let Some(status) = self.status() else {
            return Check::Unchecked;
        };
        self.inclusion.state(
            self.entries_under(row)
                .into_iter()
                .map(|entry| status.entries[entry].path.as_str()),
        )
    }

    fn toggle_checked(&mut self, row: usize, cx: &mut Context<Self>) {
        let entries = self.entries_under(row);
        let Some(status) = self.status() else {
            return;
        };
        let paths: Vec<&str> = entries
            .iter()
            .map(|&entry| status.entries[entry].path.as_str())
            .collect();
        let mut inclusion = self.inclusion.clone();
        inclusion.toggle(&paths);
        self.inclusion = inclusion;
        cx.notify();
    }

    /// The checked entries Commit takes (conflicted files count once
    /// resolved, i.e. when they show under Changes).
    fn checked_entries(&self) -> Vec<Entry> {
        let Some(status) = self.status() else {
            return Vec::new();
        };
        status
            .entries
            .iter()
            .filter(|entry| {
                (Group::Changes.change(entry).is_some()
                    || Group::Unversioned.change(entry).is_some())
                    && self.inclusion.includes(&entry.path)
            })
            .cloned()
            .collect()
    }

    /// Entries of the selected row (all files under a group or folder).
    fn selected_entries(&self) -> Vec<Entry> {
        let (Some(row), Some(status)) = (self.selected, self.status()) else {
            return Vec::new();
        };
        self.entries_under(row)
            .into_iter()
            .map(|entry| status.entries[entry].clone())
            .collect()
    }

    /// Runs a git operation off the main thread, then refreshes. `Ok(Some)`
    /// is shown as a confirmation.
    fn operate(
        &mut self,
        cx: &mut Context<Self>,
        operation: impl FnOnce(&Repository) -> io::Result<Option<String>> + Send + 'static,
        on_success: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        let Some(repository) = self.repository() else {
            return;
        };
        self.busy = true;
        self.feedback = None;
        cx.notify();
        self._operation = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { operation(&repository) })
                .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(message) => {
                        this.feedback = message.map(|m| (m.into(), theme::muted as fn() -> Hsla));
                        on_success(this, cx);
                    }
                    Err(err) => this.feedback = Some((err.to_string().into(), theme::error)),
                }
                this.refresh(cx);
            })
            .ok();
        });
    }

    fn stage(&mut self, entries: Vec<Entry>, cx: &mut Context<Self>) {
        let paths: Vec<String> = entries.into_iter().map(|entry| entry.path).collect();
        self.operate(cx, move |repo| repo.stage(&paths).map(|()| None), |_, _| {});
    }

    fn unstage(&mut self, entries: Vec<Entry>, cx: &mut Context<Self>) {
        let paths: Vec<String> = entries.into_iter().map(|entry| entry.path).collect();
        self.operate(
            cx,
            move |repo| repo.unstage(&paths).map(|()| None),
            |_, _| {},
        );
    }

    /// PyCharm's Add to VCS for untracked files.
    fn add_to_vcs(&mut self, entries: Vec<Entry>, cx: &mut Context<Self>) {
        let paths: Vec<String> = entries
            .into_iter()
            .filter(|entry| entry.unstaged == Some(Change::Untracked))
            .map(|entry| entry.path)
            .collect();
        self.operate(cx, move |repo| repo.add(&paths).map(|()| None), |_, _| {});
    }

    /// Asks, then reverts `entries` to HEAD (see [`Repository::rollback`]).
    pub fn rollback(&mut self, entries: Vec<Entry>, window: &mut Window, cx: &mut Context<Self>) {
        if entries.is_empty() {
            self.feedback = Some(("Select the changes to roll back".into(), theme::error));
            return cx.notify();
        }
        let untracked = entries
            .iter()
            .filter(|entry| entry.unstaged == Some(Change::Untracked))
            .count();
        let what = match entries.as_slice() {
            [entry] => entry.path.clone(),
            _ => format!("{} files", entries.len()),
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
                let count = entries.len();
                this.operate(
                    cx,
                    move |repo| {
                        repo.rollback(&entries)
                            .map(|()| Some(format!("Rolled back {count} file(s)")))
                    },
                    |_, _| {},
                );
            })
            .ok();
        })
        .detach();
    }

    /// Rollback from the toolbar or the Git menu: the selected changes,
    /// else the checked ones.
    pub fn rollback_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut entries = self.selected_entries();
        if entries.is_empty() {
            entries = self.checked_entries();
        }
        self.rollback(entries, window, cx);
    }

    /// Deletes files from disk (asks first).
    fn delete(&mut self, entries: Vec<Entry>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository() else {
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
                this.operate(
                    cx,
                    move |_| {
                        for entry in &entries {
                            let path = project.repository_path(&repository, &entry.path);
                            project.remove(&path, false)?;
                        }
                        Ok(Some(format!("Deleted {what}")))
                    },
                    |_, _| {},
                );
            })
            .ok();
        })
        .detach();
    }

    /// Adds root-anchored patterns for unversioned files to the
    /// repository's `.gitignore`.
    fn ignore(&mut self, entries: Vec<Entry>, cx: &mut Context<Self>) {
        let Some(repository) = self.repository() else {
            return;
        };
        let paths: Vec<String> = entries
            .into_iter()
            .filter(|entry| entry.unstaged == Some(Change::Untracked))
            .map(|entry| entry.path)
            .collect();
        let project = self.project.clone();
        self.operate(
            cx,
            move |_| {
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
            },
            |_, _| {},
        );
    }

    fn copy_paths(&self, entries: &[Entry], cx: &mut Context<Self>) {
        let Some(repository) = self.repository() else {
            return;
        };
        let text = entries
            .iter()
            .map(|entry| {
                self.project
                    .repository_path(&repository, &entry.path)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>()
            .join("\n");
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// Commits the checked files (or, in staging mode, what is staged); with
    /// `push`, asks to push afterwards.
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
        let entries = self.checked_entries();
        let staged = self
            .status()
            .is_some_and(|status| status.entries.iter().any(|entry| entry.staged.is_some()));
        if staging && !staged && !amend {
            self.feedback = Some((
                "Nothing is staged; stage changes first".into(),
                theme::error,
            ));
            return cx.notify();
        }
        if !staging && entries.is_empty() && !amend {
            self.feedback = Some(("No changes are checked for the commit".into(), theme::error));
            return cx.notify();
        }
        self.operate(
            cx,
            move |repo| {
                let summary = if staging {
                    repo.commit(&message, amend)?
                } else {
                    repo.commit_entries(&message, &entries, amend)?
                };
                let verb = if amend { "Amended" } else { "Committed" };
                Ok(Some(format!("{verb} {summary}")))
            },
            move |this, cx| {
                this.amend = false;
                this.message
                    .update(cx, |editor, cx| editor.set_text("", cx));
                if push && let Some(repository) = this.repository() {
                    cx.emit(CommitPanelEvent::Push(repository));
                }
            },
        );
    }

    fn commit_action(&mut self, _: &CommitChanges, _: &mut Window, cx: &mut Context<Self>) {
        self.commit(false, cx);
    }

    /// Amend toggled on with an empty message: start from the last one.
    fn toggle_amend(&mut self, cx: &mut Context<Self>) {
        self.amend = !self.amend;
        cx.notify();
        if !self.amend || !self.message.read(cx).text().trim().is_empty() {
            return;
        }
        let Some(repository) = self.repository() else {
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

    /// Fetch or fast-forward pull, off the main thread.
    pub fn sync(&mut self, action: SyncAction, cx: &mut Context<Self>) {
        self.operate(
            cx,
            move |repo| {
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
            },
            |_, _| {},
        );
    }

    /// Abort or continue the operation in progress.
    fn finish_operation(&mut self, abort: bool, cx: &mut Context<Self>) {
        let state = self.operation;
        self.operate(
            cx,
            move |repo| {
                if abort {
                    repo.abort(state)?;
                    return Ok(Some("Aborted".into()));
                }
                Ok(Some(match repo.continue_rebase()? {
                    git::Outcome::Done => "Rebase finished".into(),
                    git::Outcome::Conflicts => "The next commit conflicts: resolve it below".into(),
                }))
            },
            |_, _| {},
        );
    }

    /// Opens a file's diff: HEAD ↔ working tree without a staging area,
    /// else the index side of its group. Conflicted files open in the
    /// editor to resolve.
    fn show_diff(&mut self, row: usize, cx: &mut Context<Self>) {
        let (
            Some(Row::File {
                group,
                entry,
                change,
                ..
            }),
            State::Ready { repository, status },
        ) = (self.rows.get(row), &self.state)
        else {
            return;
        };
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
        let (Some(Row::File { entry, change, .. }), State::Ready { repository, status }) =
            (self.rows.get(row), &self.state)
        else {
            return;
        };
        if *change != Change::Deleted {
            let path = &status.entries[*entry].path;
            cx.emit(CommitPanelEvent::OpenFile(
                self.project.repository_path(repository, path),
            ));
        }
    }

    fn toggle_collapsed(&mut self, row: usize, cx: &mut Context<Self>) {
        let key = match self.rows.get(row) {
            Some(Row::Group { group, .. }) => (*group, String::new()),
            Some(Row::Dir { group, path, .. }) => (*group, path.clone()),
            _ => return,
        };
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        self.rebuild_rows();
        cx.notify();
    }

    fn expand_all(&mut self, cx: &mut Context<Self>) {
        self.collapsed.clear();
        self.rebuild_rows();
        cx.notify();
    }

    fn collapse_all(&mut self, cx: &mut Context<Self>) {
        if let Some(status) = self.status() {
            self.collapsed = all_folders(status, self.staging);
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
                Some(Row::File { .. }) => self.show_diff(row, cx),
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
        if !entries.is_empty() {
            self.menu = Some(ContextMenu {
                position: event.position,
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
                .is_some_and(|row| row.group().checkable())
            && !self.staging
        {
            self.toggle_checked(row, cx);
        }
    }

    fn open_selected(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        match self.selected.and_then(|row| self.rows.get(row)) {
            Some(Row::File { .. }) => self.show_diff(self.selected.unwrap_or_default(), cx),
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
            .is_some_and(|row| matches!(self.rows.get(row), Some(Row::File { .. })));
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
        let State::Ready { status, .. } = &self.state else {
            return div().into_any_element();
        };
        let row = &self.rows[index];
        let group = row.group();
        let depth = match row {
            Row::Group { .. } => 0,
            Row::Dir { depth, .. } | Row::File { depth, .. } => *depth,
        };
        let selected = self.selected == Some(index);
        let actions = self.row_actions(index, group, cx);
        let checkbox = (!self.staging && group.checkable()).then(|| {
            checkbox(self.check_state(index))
                .id(("commit-check", index))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_checked(index, cx)
                }))
        });
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
        match row {
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
                        .child(row_text(
                            "count",
                            match count {
                                1 => "1 file".to_owned(),
                                n => format!("{n} files"),
                            },
                        )),
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
                    let Some(status) = this.status() else {
                        return;
                    };
                    let entries: Vec<Entry> = this
                        .entries_under(index)
                        .into_iter()
                        .map(|entry| status.entries[entry].clone())
                        .collect();
                    if stage {
                        this.stage(entries, cx)
                    } else {
                        this.unstage(entries, cx)
                    }
                })),
        )
    }

    fn render_menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let menu = self.menu.as_ref()?;
        let status = self.status()?;
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
            Some(Row::File { .. })
        );
        type Handler =
            Box<dyn Fn(&mut CommitPanel, Vec<Entry>, &mut Window, &mut Context<CommitPanel>)>;
        let mut items: Vec<(&'static str, Handler)> = Vec::new();
        if !self.staging {
            items.push((
                "Commit File…",
                Box::new(|this, entries, window, cx| {
                    if let Some(status) = this.status() {
                        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
                        let mut inclusion = this.inclusion.clone();
                        inclusion.only(status.entries.iter().map(|e| e.path.as_str()), &paths);
                        this.inclusion = inclusion;
                    }
                    this.focus(window, cx);
                }),
            ));
        }
        items.push((
            "Rollback…",
            Box::new(|this, entries, window, cx| this.rollback(entries, window, cx)),
        ));
        if single_file {
            items.push((
                "Show Diff",
                Box::new(|this, _, _, cx| {
                    if let Some(row) = this.selected {
                        this.show_diff(row, cx)
                    }
                }),
            ));
            items.push((
                "Jump to Source",
                Box::new(|this, _, _, cx| {
                    if let Some(row) = this.selected {
                        this.jump_to_source(row, cx)
                    }
                }),
            ));
        }
        if self.staging {
            items.push((
                "Stage",
                Box::new(|this, entries, _, cx| this.stage(entries, cx)),
            ));
            items.push((
                "Unstage",
                Box::new(|this, entries, _, cx| this.unstage(entries, cx)),
            ));
        }
        if untracked {
            items.push((
                "Add to VCS",
                Box::new(|this, entries, _, cx| this.add_to_vcs(entries, cx)),
            ));
            items.push((
                "Ignore (add to .gitignore)",
                Box::new(|this, entries, _, cx| this.ignore(entries, cx)),
            ));
        }
        items.push((
            "Delete…",
            Box::new(|this, entries, window, cx| this.delete(entries, window, cx)),
        ));
        items.push((
            "Copy Path",
            Box::new(|this, entries, _, cx| this.copy_paths(&entries, cx)),
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
                        handler(this, entries.clone(), window, cx);
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

    /// Merge/rebase banner: what is in progress and how to finish it.
    fn render_operation(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let text = match self.operation {
            OperationState::None => return None,
            OperationState::Merging => "Merging: resolve conflicts, then Commit",
            OperationState::Rebasing => "Rebasing: resolve conflicts, then Continue",
            OperationState::CherryPicking => "Cherry-picking: resolve conflicts, then Commit",
            OperationState::Reverting => "Reverting: resolve conflicts, then Commit",
        };
        let button = |id: &'static str, label: &'static str| {
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
        let rebasing = self.operation == OperationState::Rebasing;
        Some(
            div()
                .px_2()
                .py_1()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .text_xs()
                .bg(theme::active_row())
                .child(
                    div()
                        .flex_1()
                        .text_color(theme::warning())
                        .child(selectable("operation", text)),
                )
                .when(rebasing, |banner| {
                    banner.child(
                        button("operation-continue", "Continue").on_click(
                            cx.listener(|this, _, _, cx| this.finish_operation(false, cx)),
                        ),
                    )
                })
                .child(
                    button("operation-abort", "Abort")
                        .on_click(cx.listener(|this, _, _, cx| this.finish_operation(true, cx))),
                ),
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
            self.collapsed.clear();
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
        let body = match &self.state {
            State::Ready { status, .. } if status.entries.is_empty() => div()
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
                    if status.unborn {
                        "No commits yet"
                    } else {
                        "Everything is committed"
                    },
                )))
                .into_any_element(),
            State::Ready { .. } => uniform_list(
                "commit-rows",
                self.rows.len(),
                cx.processor(|this, range: Range<usize>, _, cx| {
                    range.map(|index| this.render_row(index, cx)).collect()
                }),
            )
            .flex_1()
            .w_full()
            .into_any_element(),
            State::NoRepository => message(
                "This project is not in a git repository.".into(),
                theme::muted(),
            ),
            State::Untrusted(path) => {
                let trusted = path.clone();
                div()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .text_sm()
                    .child(selectable(
                        "untrusted",
                        format!(
                            "Git refuses this repository because its folder ({path}) belongs \
                             to another user than the one running git. This is common for \
                             folders on another drive, shared folders, or folders mounted \
                             into WSL."
                        ),
                    ))
                    .child(div().text_color(theme::muted()).child(selectable(
                        "untrusted-hint",
                        "Trusting it adds the folder to git's safe.directory list.",
                    )))
                    .child(
                        div()
                            .id("trust-repository")
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
                    .into_any_element()
            }
            State::Failed(error) => message(error.clone(), theme::error()),
            State::Loading => div().flex_1().into_any_element(),
        };
        let ready = matches!(self.state, State::Ready { .. });
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
            .when(ready, |panel| panel.child(self.render_toolbar(cx)))
            .children(self.render_operation(cx))
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .when(ready, |panel| panel.child(self.render_commit_area(cx)))
            .when(!ready, |panel| {
                panel.children(self.feedback.clone().map(|(text, color)| {
                    div()
                        .px_2()
                        .pb_1()
                        .text_xs()
                        .text_color(color())
                        .child(selectable("feedback", text))
                }))
            })
            .children(self.render_menu(cx))
    }
}

/// Re-reading status after file events is cheap but not free; batches
/// closer together than this share one refresh.
pub const REFRESH_DEBOUNCE: Duration = Duration::from_millis(300);

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
            assert!(matches!(panel.state, State::NoRepository));
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
            combined_change(&entry("a", Some(Change::Added), Some(Change::Modified))),
            Change::Added
        );
        assert_eq!(
            combined_change(&entry("a", Some(Change::Modified), Some(Change::Deleted))),
            Change::Deleted
        );
        assert_eq!(
            combined_change(&entry("a", None, Some(Change::Modified))),
            Change::Modified
        );
    }
}
