//! Git panel: branch, changed files in sections, staging, discarding and
//! committing. Git runs where the project lives (locally or in WSL).

use std::{collections::HashSet, io, ops::Range, time::Duration};

use git::{Change, Entry, Repository, Status};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla, PromptLevel, SharedString,
    Subscription, Task, Window, div, prelude::*, px, uniform_list,
};

use crate::{
    change_tree::{self, Node},
    diff_view::DiffSource,
    editor::{self, Editor},
    project::Project,
    theme,
};

const ROW_HEIGHT: f32 = 24.;

/// [`Project::open_repository`] started before the window (see
/// `workspace::Preload`).
pub type PreloadedRepository =
    futures::channel::oneshot::Receiver<io::Result<Option<(Repository, Status)>>>;

pub enum GitPanelEvent {
    OpenDiff {
        repository: Repository,
        source: DiffSource,
    },
    /// Open a conflicted file to resolve it.
    OpenFile(std::path::PathBuf),
    OpenHistory(Repository),
    /// Pick a revision to compare the working tree with.
    Compare(Repository),
    /// Open the branch picker.
    Branches(Repository),
    /// Open the worktree picker.
    Worktrees(Repository),
    /// Status was re-read; open diffs may be stale.
    Refreshed,
}

impl EventEmitter<GitPanelEvent> for GitPanel {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Conflicts,
    Staged,
    Changes,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Self::Conflicts => "Merge Conflicts",
            Self::Staged => "Staged Changes",
            Self::Changes => "Changes",
        }
    }

    /// The change an entry shows in this section, if it belongs here.
    fn change(self, entry: &Entry) -> Option<Change> {
        match self {
            Self::Conflicts => entry.unstaged.filter(|&c| c == Change::Conflicted),
            Self::Staged => entry.staged,
            Self::Changes => entry.unstaged.filter(|&c| c != Change::Conflicted),
        }
    }
}

enum Row {
    Header(Section, usize),
    Dir {
        section: Section,
        path: String,
        label: String,
        depth: usize,
        collapsed: bool,
    },
    File {
        section: Section,
        /// Index into the status entries.
        entry: usize,
        change: Change,
        depth: usize,
        /// Containing folder, shown in list mode.
        dir: String,
    },
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

pub struct GitPanel {
    project: Project,
    state: State,
    rows: Vec<Row>,
    message: Entity<Editor>,
    /// Result of the last operation, with its color (errors in red).
    feedback: Option<(SharedString, fn() -> Hsla)>,
    busy: bool,
    /// The next commit replaces the last one.
    amend: bool,
    /// Files as a compact folder tree (else a flat list).
    as_tree: bool,
    collapsed: HashSet<(SectionKey, String)>,
    focus_handle: FocusHandle,
    _refresh: Task<()>,
    _operation: Task<()>,
    _subscription: Subscription,
}

/// `Section` as a hashable key for collapsed folders.
type SectionKey = u8;

#[derive(Debug, Clone, Copy)]
pub enum SyncAction {
    Fetch,
    Pull,
    Push,
}

impl SyncAction {
    fn done(self) -> &'static str {
        match self {
            Self::Fetch => "Fetched",
            Self::Pull => "Pulled",
            Self::Push => "Pushed",
        }
    }
}

pub fn change_color(change: Change) -> Hsla {
    match change {
        Change::Added | Change::Untracked => theme::git_added(),
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

impl GitPanel {
    pub fn new(
        project: Project,
        preloaded: Option<PreloadedRepository>,
        cx: &mut Context<Self>,
    ) -> Self {
        let message = cx.new(Editor::single_line);
        let subscription = cx.subscribe(&message, |_, _, _: &editor::EditorEvent, cx| cx.notify());
        let mut panel = Self {
            project,
            state: State::Loading,
            rows: Vec::new(),
            message,
            feedback: None,
            busy: false,
            amend: false,
            as_tree: true,
            collapsed: HashSet::new(),
            focus_handle: cx.focus_handle(),
            _refresh: Task::ready(()),
            _operation: Task::ready(()),
            _subscription: subscription,
        };
        match preloaded {
            Some(preloaded) => panel.show_preloaded(preloaded, cx),
            None => panel.refresh(cx),
        }
        panel
    }

    pub fn focus(&self, window: &mut Window, cx: &App) {
        window.focus(&self.message.focus_handle(cx));
    }

    /// The repository, once its status has been read.
    pub fn repository(&self) -> Option<Repository> {
        match &self.state {
            State::Ready { repository, .. } => Some(repository.clone()),
            _ => None,
        }
    }

    /// Refreshes after [`REFRESH_DEBOUNCE`], coalescing bursts of changes.
    pub fn schedule_refresh(&mut self, cx: &mut Context<Self>) {
        self._refresh = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REFRESH_DEBOUNCE).await;
            this.update(cx, |this, cx| this.refresh(cx)).ok();
        });
    }

    /// Re-reads the repository status (discovering the repository first).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let project = self.project.clone();
        let known = match &self.state {
            State::Ready { repository, .. } => Some(repository.clone()),
            _ => None,
        };
        self._refresh = cx.spawn(async move |this, cx| {
            let opened = cx
                .background_spawn(async move {
                    match known {
                        Some(repository) => {
                            repository.status().map(|status| Some((repository, status)))
                        }
                        None => project.open_repository(),
                    }
                })
                .await;
            this.update(cx, |this, cx| this.show(opened, cx)).ok();
        });
    }

    /// Shows a repository read before the panel existed (at startup); reads
    /// it now if that read was abandoned.
    fn show_preloaded(&mut self, preloaded: PreloadedRepository, cx: &mut Context<Self>) {
        self._refresh = cx.spawn(async move |this, cx| {
            let opened = preloaded.await;
            this.update(cx, |this, cx| match opened {
                Ok(opened) => this.show(opened, cx),
                Err(_) => this.refresh(cx),
            })
            .ok();
        });
    }

    fn show(&mut self, opened: io::Result<Option<(Repository, Status)>>, cx: &mut Context<Self>) {
        crate::startup::mark_once("git status shown");
        self.state = match opened {
            Ok(Some((repository, status))) => State::Ready { repository, status },
            Ok(None) => State::NoRepository,
            Err(err) => match git::unsafe_repository(&err) {
                Some(refusal) => State::Untrusted(refusal.path.clone().into()),
                None => State::Failed(err.to_string().into()),
            },
        };
        self.rebuild_rows();
        cx.emit(GitPanelEvent::Refreshed);
        cx.notify();
    }

    /// Shows the outcome of a git operation run elsewhere (a picker).
    pub fn report(&mut self, message: String, cx: &mut Context<Self>) {
        self.feedback = Some((message.into(), theme::muted));
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
        self.rows.clear();
        let State::Ready { status, .. } = &self.state else {
            return;
        };
        for section in [Section::Conflicts, Section::Staged, Section::Changes] {
            let files: Vec<(usize, Change)> = status
                .entries
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| Some((index, section.change(entry)?)))
                .collect();
            if files.is_empty() {
                continue;
            }
            self.rows.push(Row::Header(section, files.len()));
            let paths: Vec<&str> = files
                .iter()
                .map(|&(entry, _)| status.entries[entry].path.as_str())
                .collect();
            let key = section as SectionKey;
            let nodes = if self.as_tree {
                let collapsed: HashSet<String> = self
                    .collapsed
                    .iter()
                    .filter(|(s, _)| *s == key)
                    .map(|(_, path)| path.clone())
                    .collect();
                change_tree::tree(&paths, &collapsed)
            } else {
                change_tree::list(&paths)
            };
            self.rows.extend(nodes.into_iter().map(|node| match node {
                Node::Dir {
                    path,
                    label,
                    depth,
                    collapsed,
                } => Row::Dir {
                    section,
                    path,
                    label,
                    depth,
                    collapsed,
                },
                Node::File {
                    item, depth, dir, ..
                } => Row::File {
                    section,
                    entry: files[item].0,
                    change: files[item].1,
                    depth,
                    dir,
                },
            }));
        }
    }

    fn entries(&self, section: Section) -> Vec<Entry> {
        match &self.state {
            State::Ready { status, .. } => status
                .entries
                .iter()
                .filter(|entry| section.change(entry).is_some())
                .cloned()
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Runs a git operation off the main thread, then refreshes. `Ok(Some)`
    /// is shown as a confirmation.
    fn operate(
        &mut self,
        cx: &mut Context<Self>,
        operation: impl FnOnce(&Repository) -> io::Result<Option<String>> + Send + 'static,
        on_success: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
    ) {
        let State::Ready { repository, .. } = &self.state else {
            return;
        };
        let repository = repository.clone();
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
        let paths = entries
            .into_iter()
            .map(|entry| entry.path)
            .collect::<Vec<_>>();
        self.operate(cx, move |repo| repo.stage(&paths).map(|()| None), |_, _| {});
    }

    fn unstage(&mut self, entries: Vec<Entry>, cx: &mut Context<Self>) {
        let paths = entries
            .into_iter()
            .map(|entry| entry.path)
            .collect::<Vec<_>>();
        self.operate(
            cx,
            move |repo| repo.unstage(&paths).map(|()| None),
            |_, _| {},
        );
    }

    fn discard(&mut self, entries: Vec<Entry>, window: &mut Window, cx: &mut Context<Self>) {
        let untracked = entries
            .iter()
            .filter(|entry| entry.unstaged == Some(Change::Untracked))
            .count();
        let what = match entries.as_slice() {
            [entry] => format!("changes to {}", entry.path),
            _ => format!("changes to {} files", entries.len()),
        };
        let detail = if untracked > 0 {
            "Untracked files will be deleted. This cannot be undone."
        } else {
            "This cannot be undone."
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Discard {what}?"),
            Some(detail),
            &["Discard", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update(cx, |this, cx| {
                this.operate(
                    cx,
                    move |repo| repo.discard(&entries).map(|()| None),
                    |_, _| {},
                );
            })
            .ok();
        })
        .detach();
    }

    fn commit(&mut self, cx: &mut Context<Self>) {
        let message = self.message.read(cx).text();
        let staged = !self.entries(Section::Staged).is_empty();
        let amend = self.amend;
        if message.trim().is_empty() {
            self.feedback = Some(("Enter a commit message".into(), theme::error));
            return cx.notify();
        }
        if !staged && !amend {
            self.feedback = Some((
                "Nothing is staged; stage changes first".into(),
                theme::error,
            ));
            return cx.notify();
        }
        self.operate(
            cx,
            move |repo| {
                repo.commit(&message, amend).map(|summary| {
                    let verb = if amend { "Amended" } else { "Committed" };
                    Some(format!("{verb} {summary}"))
                })
            },
            |this, cx| {
                this.amend = false;
                this.message
                    .update(cx, |editor, cx| editor.set_text("", cx));
            },
        );
    }

    /// Fetch, pull (fast-forward only) or push, off the main thread.
    pub fn sync(&mut self, action: SyncAction, cx: &mut Context<Self>) {
        let State::Ready { status, .. } = &self.state else {
            return;
        };
        let status = status.clone();
        self.operate(
            cx,
            move |repo| {
                match action {
                    SyncAction::Fetch => repo.fetch()?,
                    SyncAction::Pull => repo.pull()?,
                    SyncAction::Push => repo.push(&status)?,
                }
                Ok(Some(action.done().into()))
            },
            |_, _| {},
        );
    }

    fn commit_action(&mut self, _: &editor::Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.commit(cx);
    }

    fn open(&mut self, row: usize, cx: &mut Context<Self>) {
        if let Some(Row::Dir { section, path, .. }) = self.rows.get(row) {
            let key = (*section as SectionKey, path.clone());
            if !self.collapsed.remove(&key) {
                self.collapsed.insert(key);
            }
            self.rebuild_rows();
            return cx.notify();
        }
        let (Some(Row::File { section, entry, .. }), State::Ready { repository, status }) =
            (self.rows.get(row), &self.state)
        else {
            return;
        };
        let entry = status.entries[*entry].clone();
        let event = match section {
            Section::Conflicts => {
                GitPanelEvent::OpenFile(self.project.repository_path(repository, &entry.path))
            }
            Section::Staged | Section::Changes => GitPanelEvent::OpenDiff {
                repository: repository.clone(),
                source: DiffSource::Index {
                    entry,
                    staged: *section == Section::Staged,
                },
            },
        };
        cx.emit(event);
    }

    fn render_header(&self, section: Section, count: usize, cx: &mut Context<Self>) -> gpui::Div {
        let action = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px_1()
                .text_color(theme::muted())
                .hover(|style| style.text_color(theme::text()))
                .cursor_pointer()
                .child(label)
        };
        let mut actions = div().flex().gap_1();
        match section {
            Section::Staged => {
                actions = actions.child(action("unstage-all", "−").on_click(
                    cx.listener(|this, _, _, cx| this.unstage(this.entries(Section::Staged), cx)),
                ));
            }
            Section::Changes => {
                actions = actions
                    .child(action("discard-all", "↺").on_click(cx.listener(
                        |this, _, window, cx| {
                            this.discard(this.entries(Section::Changes), window, cx)
                        },
                    )))
                    .child(
                        action("stage-all", "+").on_click(cx.listener(|this, _, _, cx| {
                            this.stage(this.entries(Section::Changes), cx)
                        })),
                    );
            }
            Section::Conflicts => {}
        }
        div()
            .h(px(ROW_HEIGHT))
            .px_2()
            .flex()
            .items_center()
            .justify_between()
            .text_xs()
            .text_color(theme::muted())
            .child(format!("{} ({count})", section.title().to_uppercase()))
            .child(actions)
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let State::Ready { status, .. } = &self.state else {
            return div().into_any_element();
        };
        let (section, entry, change, depth, dir) = match &self.rows[index] {
            &Row::Header(section, count) => {
                return self.render_header(section, count, cx).into_any_element();
            }
            Row::Dir {
                label,
                depth,
                collapsed,
                ..
            } => {
                return div()
                    .id(("git-dir", index))
                    .h(px(ROW_HEIGHT))
                    .pl(px(16. + 14. * *depth as f32))
                    .pr_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .text_color(theme::muted())
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::hover_row()))
                    .on_click(cx.listener(move |this, _, _, cx| this.open(index, cx)))
                    .child(if *collapsed { "▸" } else { "▾" })
                    .child(label.clone())
                    .into_any_element();
            }
            Row::File {
                section,
                entry,
                change,
                depth,
                dir,
            } => (
                *section,
                &status.entries[*entry],
                *change,
                *depth,
                dir.clone(),
            ),
        };
        let name = entry.path.rsplit('/').next().unwrap_or(&entry.path);
        let button = |id: &'static str, label: &'static str| {
            div()
                .id((id, index))
                .px_1()
                .text_color(theme::muted())
                .hover(|style| style.text_color(theme::text()))
                .cursor_pointer()
                .child(label)
        };
        let one = vec![entry.clone()];
        let actions = match section {
            Section::Staged => div()
                .flex()
                .child(
                    button("unstage", "−").on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.unstage(one.clone(), cx)
                    })),
                ),
            Section::Changes => {
                let discard = one.clone();
                div()
                    .flex()
                    .child(button("discard", "↺").on_click(cx.listener(
                        move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.discard(discard.clone(), window, cx)
                        },
                    )))
                    .child(
                        button("stage", "+").on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.stage(one.clone(), cx)
                        })),
                    )
            }
            // Staging a conflicted file marks it resolved.
            Section::Conflicts => div()
                .flex()
                .child(
                    button("resolve", "+").on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.stage(one.clone(), cx)
                    })),
                ),
        };
        div()
            .id(("git-row", index))
            .w_full()
            .h(px(ROW_HEIGHT))
            .pl(px(30. + 14. * depth as f32))
            .pr_2()
            .flex()
            .items_center()
            .gap_2()
            .text_sm()
            .cursor_pointer()
            .hover(|style| style.bg(theme::hover_row()))
            .on_click(cx.listener(move |this, _, _, cx| this.open(index, cx)))
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
                            .text_color(theme::text())
                            .child(name.to_owned()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(dir),
                    ),
            )
            .child(actions)
            .child(
                div()
                    .w(px(12.))
                    .flex_none()
                    .text_color(change_color(change))
                    .child(change.letter().to_string()),
            )
            .into_any_element()
    }

    fn render_branch(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (text, ahead, behind) = match &self.state {
            State::Ready { status, .. } => {
                let mut text = status
                    .branch
                    .clone()
                    .unwrap_or_else(|| "detached HEAD".into());
                if status.unborn {
                    text.push_str(" (no commits yet)");
                }
                (text, status.ahead, status.behind)
            }
            State::Loading => ("Reading repository…".into(), 0, 0),
            State::NoRepository => ("No git repository".into(), 0, 0),
            State::Untrusted(_) => ("Untrusted repository".into(), 0, 0),
            State::Failed(_) => ("Git unavailable".into(), 0, 0),
        };
        let repository = match &self.state {
            State::Ready { repository, .. } => Some(repository.clone()),
            _ => None,
        };
        let tool = |id: &'static str, label: SharedString| {
            div()
                .id(id)
                .px_1()
                .text_xs()
                .text_color(theme::muted())
                .hover(|style| style.text_color(theme::text()))
                .cursor_pointer()
                .child(label)
        };
        let busy = self.busy;
        let tools =
            repository.clone().map(|repository| {
                let (compare, worktrees) = (repository.clone(), repository.clone());
                div()
                    .px_1()
                    .pb_1()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_1()
                    .child(tool("git-history", "History".into()).on_click(cx.listener(
                        move |_, _, _, cx| cx.emit(GitPanelEvent::OpenHistory(repository.clone())),
                    )))
                    .child(tool("git-compare", "Compare…".into()).on_click(cx.listener(
                        move |_, _, _, cx| cx.emit(GitPanelEvent::Compare(compare.clone())),
                    )))
                    .child(
                        tool("git-worktrees", "Worktrees…".into()).on_click(cx.listener(
                            move |_, _, _, cx| cx.emit(GitPanelEvent::Worktrees(worktrees.clone())),
                        )),
                    )
                    .child(
                        tool(
                            "git-layout",
                            if self.as_tree { "List" } else { "Tree" }.into(),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.as_tree = !this.as_tree;
                            this.rebuild_rows();
                            cx.notify();
                        })),
                    )
            });
        let branch = div()
            .id("git-branch")
            .min_w_0()
            .truncate()
            .px_1()
            .rounded_sm()
            .text_color(theme::text())
            .child(format!("⎇ {text} ▾"))
            .when_some(repository.clone(), |branch, repository| {
                branch
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::hover_row()))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(GitPanelEvent::Branches(repository.clone()))
                    }))
            });
        let count = |label: &str, n: u32, arrow: &str| {
            if n > 0 {
                format!("{label} {arrow}{n}")
            } else {
                label.to_owned()
            }
        };
        let sync = repository.is_some().then(|| {
            div().px_2().pb_1().flex().gap_2().children(
                [
                    (SyncAction::Fetch, "git-fetch", "Fetch".to_owned()),
                    (SyncAction::Pull, "git-pull", count("Pull", behind, "↓")),
                    (SyncAction::Push, "git-push", count("Push", ahead, "↑")),
                ]
                .map(|(action, id, label)| {
                    div()
                        .id(id)
                        .px_2()
                        .rounded_sm()
                        .border_1()
                        .border_color(theme::border())
                        .text_xs()
                        .text_color(if busy { theme::muted() } else { theme::text() })
                        .cursor_pointer()
                        .hover(|style| style.bg(theme::hover_row()))
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.busy {
                                this.sync(action, cx)
                            }
                        }))
                }),
            )
        });
        // Branch on its own row (never squeezed by the tools); the tools wrap
        // in a narrow dock.
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .px_2()
                    .pt_1()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .child(branch.flex_1())
                    .child(
                        tool("git-refresh", "⟳".into())
                            .flex_none()
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            )
            .children(tools)
            .children(sync)
    }
}

impl Focusable for GitPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GitPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.state {
            State::Ready { status, .. } if status.entries.is_empty() => div()
                .p_3()
                .text_sm()
                .text_color(theme::muted())
                .child("No changes.")
                .into_any_element(),
            State::Ready { .. } => uniform_list(
                "git-rows",
                self.rows.len(),
                cx.processor(|this, range: Range<usize>, _, cx| {
                    range.map(|index| this.render_row(index, cx)).collect()
                }),
            )
            .flex_1()
            .w_full()
            .into_any_element(),
            State::NoRepository => div()
                .p_3()
                .text_sm()
                .text_color(theme::muted())
                .child("This project is not in a git repository.")
                .into_any_element(),
            State::Untrusted(path) => {
                let trusted = path.clone();
                div()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .text_sm()
                    .child(format!(
                        "Git refuses this repository because its folder ({path}) belongs \
                         to another user than the one running git. This is common for \
                         folders on another drive, shared folders, or folders mounted \
                         into WSL."
                    ))
                    .child(
                        div()
                            .text_color(theme::muted())
                            .child("Trusting it adds the folder to git's safe.directory list."),
                    )
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
            State::Failed(error) => div()
                .p_3()
                .text_sm()
                .text_color(theme::error())
                .child(error.clone())
                .into_any_element(),
            State::Loading => div().into_any_element(),
        };
        let ready = matches!(self.state, State::Ready { .. });
        div()
            .key_context("GitPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::commit_action))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::panel())
            .child(self.render_branch(cx))
            .when(ready, |panel| {
                panel.child(
                    div()
                        .px_2()
                        .pb_2()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .h(px(28.))
                                .px_2()
                                .py_1()
                                .border_1()
                                .border_color(theme::border())
                                .rounded_sm()
                                .bg(theme::background())
                                .child(self.message.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .id("git-commit")
                                        .flex_1()
                                        .flex()
                                        .justify_center()
                                        .py_1()
                                        .rounded_sm()
                                        .text_sm()
                                        .bg(if self.busy {
                                            theme::border()
                                        } else {
                                            theme::accent()
                                        })
                                        .text_color(theme::on_accent())
                                        .cursor_pointer()
                                        .child(match (self.busy, self.amend) {
                                            (true, _) => "…",
                                            (false, true) => "Amend",
                                            (false, false) => "Commit",
                                        })
                                        .on_click(cx.listener(|this, _, _, cx| this.commit(cx))),
                                )
                                .child(
                                    div()
                                        .id("git-amend")
                                        .flex_none()
                                        .px_1()
                                        .text_xs()
                                        .text_color(if self.amend {
                                            theme::text()
                                        } else {
                                            theme::muted()
                                        })
                                        .cursor_pointer()
                                        .child(if self.amend { "☑ Amend" } else { "☐ Amend" })
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.amend = !this.amend;
                                            cx.notify();
                                        })),
                                ),
                        ),
                )
            })
            .children(self.feedback.clone().map(|(text, color)| {
                div()
                    .px_2()
                    .pb_1()
                    .text_xs()
                    .text_color(color())
                    .child(text)
            }))
            .child(body)
    }
}

/// Re-reading status after file events is cheap but not free; batches
/// closer together than this share one refresh.
pub const REFRESH_DEBOUNCE: Duration = Duration::from_millis(300);
