//! The Git tool window (PyCharm's, in the bottom dock, `Alt+9`) and its Log
//! tab: a branches tree on the left (double-click filters the log to a
//! branch), a filter bar (text, branch, user, date, paths) above the commit
//! graph (paged `git log`, more pages load while scrolling), and on the
//! right the selected commits' changed files (double-click opens a diff)
//! above their details. Commit operations live in `actions.rs`. With
//! several repositories (folders of a multi-root project) a Repository
//! selector leads the filter bar; the log follows the active editor's
//! repository until one is picked there.

mod actions;
mod branches;

use std::{
    collections::HashSet,
    ops::Range,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use git::{Commit, GraphRow, LogFilter, Ref, Repository, SelectionChanges};
use gpui::{
    App, Bounds, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    KeyBinding, Modifiers, MouseButton, MouseDownEvent, PathBuilder, Pixels, Point, SharedString,
    Subscription, Task, UniformListScrollHandle, Window, actions, anchored, canvas, deferred, div,
    fill, point, prelude::*, px, size, uniform_list,
};

pub use actions::{ResetDialog, ResetDialogEvent};

use crate::{
    change_tree::{self, Node},
    commit_panel::{group_folders, repository_key, repository_name},
    compare_view::render_change_node,
    diff_view::DiffSource,
    editor::{self, Editor, EditorEvent},
    project::Project,
    selectable::selectable,
    theme,
};

use branches::BranchRow;

const ROW_HEIGHT: f32 = 24.;
const LANE_WIDTH: f32 = 14.;
/// Lanes beyond this are drawn clipped; real histories rarely need more.
const MAX_LANES: usize = 16;
/// Commits per `git log` run; the next page loads when the list scrolls
/// within [`PREFETCH`] rows of the end.
const PAGE: usize = 1000;
const PREFETCH: usize = 200;
/// Pages loaded at most to find a commit to reveal.
const REVEAL_PAGES: usize = 20;
const BRANCHES_WIDTH: f32 = 220.;
const DETAILS_WIDTH: f32 = 380.;
/// Typing in the text filter reloads the log after this pause.
const TEXT_DEBOUNCE: Duration = Duration::from_millis(300);

const CONTEXT: &str = "GitLog";

actions!(
    git_log,
    [
        SelectPrev,
        SelectNext,
        ExtendPrev,
        ExtendNext,
        CopyRevision,
        CloseMenu
    ]
);

pub fn bind_keys(cx: &mut App) {
    let ctx = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", SelectPrev, ctx),
        KeyBinding::new("down", SelectNext, ctx),
        KeyBinding::new("shift-up", ExtendPrev, ctx),
        KeyBinding::new("shift-down", ExtendNext, ctx),
        KeyBinding::new("secondary-c", CopyRevision, ctx),
        KeyBinding::new("escape", CloseMenu, ctx),
    ]);
    actions::bind_keys(cx);
}

pub enum GitLogEvent {
    OpenDiff {
        repository: Repository,
        source: DiffSource,
    },
    /// Compare a commit with the working tree.
    CompareWithLocal {
        repository: Repository,
        base: String,
    },
    /// Ask for a branch name, then create it at `start`.
    NewBranch {
        repository: Repository,
        start: String,
    },
    /// Ask for a tag name, then call [`GitLogPanel::create_tag`].
    NewTag { at: String },
    /// Ask for HEAD's new message (starting from `message`), then call
    /// [`GitLogPanel::reword_head`].
    EditMessage { message: String },
    /// Ask for a reset mode, then call [`GitLogPanel::reset`].
    Reset {
        id: String,
        subject: String,
        branch: Option<String>,
    },
    /// An operation changed the repository; `message` reports it.
    Changed(String),
}

impl EventEmitter<GitLogEvent> for GitLogPanel {}

enum RepoState {
    /// Not looked for yet: the panel has not been shown.
    Unknown,
    Discovering,
    Missing,
    Failed(SharedString),
    Ready(Repository),
}

/// HEAD as the log's actions need it.
#[derive(Clone)]
struct Head {
    id: String,
    /// The checked-out branch; `None` when detached.
    branch: Option<String>,
    /// A remote branch contains HEAD (it cannot be undone).
    pushed: bool,
}

/// The selected commits' changed files and details.
struct Details {
    /// Selected commit ids, newest first.
    ids: Vec<String>,
    changes: Option<Result<SelectionChanges, SharedString>>,
    nodes: Vec<Node>,
    collapsed: HashSet<String>,
    /// Full message and branches containing the commit (single selection).
    message: Option<String>,
    branches: Option<Vec<String>>,
    _load: Task<()>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FilterKind {
    /// Which repository the log shows (several repositories only).
    Repository,
    Branch,
    User,
    Date,
    Paths,
}

struct FilterPopup {
    kind: FilterKind,
    position: Point<Pixels>,
}

/// The date filter's choices: label and age in seconds.
const DATE_RANGES: [(&str, i64); 4] = [
    ("Last 24 hours", 86_400),
    ("Last 7 days", 7 * 86_400),
    ("Last 30 days", 30 * 86_400),
    ("Last 12 months", 365 * 86_400),
];

pub struct GitLogPanel {
    project: Project,
    /// The repository shown.
    repo: RepoState,
    /// Every repository of the project's folders (one per repository).
    repositories: Vec<Repository>,
    /// Show this repository (by key) once the repositories are found.
    preferred: Option<String>,
    /// The user picked the repository: showing the log no longer follows
    /// the active editor.
    chosen: bool,
    focus_handle: FocusHandle,

    refs: Vec<Ref>,
    head: Option<Head>,
    me: Option<String>,
    branch_rows: Vec<BranchRow>,
    collapsed_groups: HashSet<String>,
    selected_branch: Option<usize>,

    filter: LogFilter,
    /// Label of the chosen date range.
    date_label: Option<&'static str>,
    text_input: Entity<Editor>,
    path_input: Entity<Editor>,
    popup: Option<FilterPopup>,

    commits: Vec<Commit>,
    graph: Vec<GraphRow>,
    /// Where the next page starts; `None` once everything is loaded.
    next: Option<usize>,
    loading: bool,
    error: Option<SharedString>,
    /// Bumped on every reload so stale pages are dropped.
    generation: u64,
    scroll: UniformListScrollHandle,

    /// Selected rows, ascending (newest first).
    selected: Vec<usize>,
    anchor: Option<usize>,
    details: Option<Details>,

    menu: Option<actions::CommitMenu>,
    /// The last operation's outcome; `conflict` offers Abort.
    report: Option<(SharedString, bool)>,
    conflict: bool,
    /// Select this commit once it is loaded (Show in Git Log from blame),
    /// loading up to `REVEAL_PAGES` pages for it.
    reveal: Option<String>,
    reveal_pages: usize,

    _load: Task<()>,
    _refs: Task<()>,
    _debounce: Task<()>,
    _operation: Task<()>,
    _subscriptions: Vec<Subscription>,
}

/// "3 hours ago", "2 days ago", …
pub(crate) fn relative_time(time: i64) -> String {
    let seconds = (now() - time).max(0);
    let (value, unit) = match seconds {
        0..60 => return "just now".into(),
        60..3600 => (seconds / 60, "minute"),
        3600..86_400 => (seconds / 3600, "hour"),
        86_400..2_592_000 => (seconds / 86_400, "day"),
        2_592_000..31_536_000 => (seconds / 2_592_000, "month"),
        _ => (seconds / 31_536_000, "year"),
    };
    format!("{value} {unit}{} ago", if value == 1 { "" } else { "s" })
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// `2026-10-16 14:03 UTC`.
pub(crate) fn absolute_time(time: i64) -> String {
    let (days, seconds) = (time.div_euclid(86_400), time.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        seconds / 3600,
        seconds % 3600 / 60
    )
}

/// The lanes and node of one commit.
fn graph_cell(row: &GraphRow, head: bool) -> impl IntoElement {
    let row = row.clone();
    let lanes = row.width.min(MAX_LANES);
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let x = |lane: usize| bounds.origin.x + px(LANE_WIDTH * lane as f32 + LANE_WIDTH / 2.);
            let top = bounds.origin.y;
            let middle = top + bounds.size.height / 2.;
            let bottom = top + bounds.size.height;
            let mut line = |from: (usize, Pixels), to: (usize, Pixels), color: Hsla| {
                if from.0 >= MAX_LANES || to.0 >= MAX_LANES {
                    return;
                }
                let mut path = PathBuilder::stroke(px(1.5));
                path.move_to(point(x(from.0), from.1));
                path.line_to(point(x(to.0), to.1));
                if let Ok(path) = path.build() {
                    window.paint_path(path, color);
                }
            };
            for &(from, to) in &row.top {
                line((from, top), (to, middle), theme::graph_lane(from));
            }
            for &(from, to) in &row.bottom {
                line((from, middle), (to, bottom), theme::graph_lane(to));
            }
            if row.column < MAX_LANES {
                let color = theme::graph_lane(row.column);
                let center = point(x(row.column), middle);
                if head {
                    // HEAD: a ring around the node.
                    let ring = Bounds::centered_at(center, size(px(12.), px(12.)));
                    window.paint_quad(fill(ring, color).corner_radii(px(6.)));
                    let hole = Bounds::centered_at(center, size(px(8.), px(8.)));
                    window.paint_quad(fill(hole, theme::background()).corner_radii(px(4.)));
                }
                let node = Bounds::centered_at(center, size(px(6.), px(6.)));
                window.paint_quad(fill(node, color).corner_radii(px(3.)));
            }
        },
    )
    .flex_none()
    .w(px(LANE_WIDTH * lanes.max(1) as f32))
    .h(px(ROW_HEIGHT))
}

/// A branch or tag label in a commit row; `remote` outlines remote branches.
fn ref_label(index: usize, name: &str, remote: bool) -> impl IntoElement {
    let (label, background, border, text) = if let Some(branch) = name.strip_prefix("HEAD -> ") {
        (
            format!("HEAD → {branch}"),
            theme::accent(),
            theme::accent(),
            theme::on_accent(),
        )
    } else if name == "HEAD" {
        (
            "HEAD".to_owned(),
            theme::accent(),
            theme::accent(),
            theme::on_accent(),
        )
    } else if let Some(tag) = name.strip_prefix("tag: ") {
        (
            format!("◆ {tag}"),
            theme::tag(),
            theme::tag(),
            theme::on_accent(),
        )
    } else if remote {
        (
            name.to_owned(),
            gpui::transparent_black(),
            theme::branch(),
            theme::muted(),
        )
    } else {
        (
            name.to_owned(),
            theme::branch(),
            theme::branch(),
            theme::on_accent(),
        )
    };
    div()
        .flex_none()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(border)
        .bg(background)
        .text_xs()
        .text_color(text)
        .child(selectable(("ref", index), label).right_click_through())
}

impl GitLogPanel {
    pub fn new(project: Project, cx: &mut Context<Self>) -> Self {
        let text_input = cx.new(Editor::single_line);
        let path_input = cx.new(Editor::single_line);
        let subscriptions = vec![cx.subscribe(&text_input, |this, input, event, cx| {
            if let EditorEvent::Edited = event {
                let text = input.read(cx).text();
                this._debounce = cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(TEXT_DEBOUNCE).await;
                    this.update(cx, |this, cx| {
                        this.filter.text = text;
                        this.reload(cx);
                    })
                    .ok();
                });
            }
        })];
        Self {
            project,
            repo: RepoState::Unknown,
            repositories: Vec::new(),
            preferred: None,
            chosen: false,
            focus_handle: cx.focus_handle(),
            refs: Vec::new(),
            head: None,
            me: None,
            branch_rows: Vec::new(),
            collapsed_groups: HashSet::from([branches::TAGS.to_owned()]),
            selected_branch: None,
            filter: LogFilter::default(),
            date_label: None,
            text_input,
            path_input,
            popup: None,
            commits: Vec::new(),
            graph: Vec::new(),
            next: None,
            loading: false,
            error: None,
            generation: 0,
            scroll: UniformListScrollHandle::new(),
            selected: Vec::new(),
            anchor: None,
            details: None,
            menu: None,
            report: None,
            conflict: false,
            _load: Task::ready(()),
            _refs: Task::ready(()),
            _debounce: Task::ready(()),
            _operation: Task::ready(()),
            _subscriptions: subscriptions,
            reveal: None,
            reveal_pages: 0,
        }
    }

    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus_handle);
    }

    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx)
    }

    fn repository(&self) -> Option<Repository> {
        match &self.repo {
            RepoState::Ready(repository) => Some(repository.clone()),
            _ => None,
        }
    }

    /// Finds the repositories and loads the log, the first time the panel
    /// is shown.
    fn start(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.repo, RepoState::Unknown) {
            return;
        }
        self.repo = RepoState::Discovering;
        let project = self.project.clone();
        self._load = cx.spawn(async move |this, cx| {
            let found = cx.background_spawn(async move { discover(&project) }).await;
            this.update(cx, |this, cx| {
                this.repo = match found {
                    Ok(repositories) => {
                        let preferred = this.preferred.take();
                        let index = repositories
                            .iter()
                            .position(|r| Some(repository_key(r.root())) == preferred)
                            .unwrap_or(0);
                        let shown = repositories.get(index).cloned();
                        this.repositories = repositories;
                        shown.map_or(RepoState::Missing, RepoState::Ready)
                    }
                    Err(err) => RepoState::Failed(err.to_string().into()),
                };
                this.reload(cx);
            })
            .ok();
        });
    }

    /// Shows `repository`'s log (once found, if discovery is running).
    pub fn select_repository(&mut self, repository: &Repository, cx: &mut Context<Self>) {
        let key = repository_key(repository.root());
        match self
            .repositories
            .iter()
            .position(|r| repository_key(r.root()) == key)
        {
            Some(index) => self.switch_repository(index, cx),
            None => self.preferred = Some(key),
        }
    }

    /// The log follows the active editor's repository until the user
    /// picks one.
    pub fn prefer_repository(&mut self, repository: &Repository, cx: &mut Context<Self>) {
        if !self.chosen {
            self.select_repository(repository, cx);
        }
    }

    /// Shows repository `index`: another repository's branches, commits
    /// and paths, so those filters and the selection start over.
    fn switch_repository(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(repository) = self.repositories.get(index).cloned() else {
            return;
        };
        let key = repository_key(repository.root());
        if matches!(&self.repo, RepoState::Ready(shown) if repository_key(shown.root()) == key) {
            return;
        }
        self.repo = RepoState::Ready(repository);
        self.popup = None;
        self.menu = None;
        self.refs.clear();
        self.head = None;
        self.selected_branch = None;
        self.rebuild_branches();
        self.commits.clear();
        self.graph.clear();
        self.selected.clear();
        self.anchor = None;
        self.details = None;
        self.filter.revisions.clear();
        self.filter.paths.clear();
        self.path_input
            .update(cx, |input, cx| input.set_text("", cx));
        self.reload(cx);
    }

    /// The repository changed (commits, refs, checkouts): reloads what is
    /// shown, or nothing before the panel has been shown.
    pub fn repository_changed(&mut self, cx: &mut Context<Self>) {
        match self.repo {
            RepoState::Ready(_) => self.reload(cx),
            // A repository may have been created since.
            RepoState::Missing | RepoState::Failed(_) => {
                self.repo = RepoState::Unknown;
                cx.notify();
            }
            RepoState::Unknown | RepoState::Discovering => {}
        }
    }

    /// Shows the history of `path` (repository-relative): the Paths filter.
    pub fn show_path_history(&mut self, path: String, cx: &mut Context<Self>) {
        self.path_input
            .update(cx, |input, cx| input.set_text(&path, cx));
        self.filter.paths = vec![path];
        self.reload(cx);
    }

    /// Selects commit `id` (full hash) with the filters cleared, loading
    /// pages until it shows.
    pub fn reveal_commit(&mut self, id: String, cx: &mut Context<Self>) {
        self.reveal = Some(id);
        self.reveal_pages = 0;
        self.popup = None;
        self.date_label = None;
        self.selected_branch = None;
        self.filter = LogFilter::default();
        for input in [&self.text_input, &self.path_input] {
            input.update(cx, |input, cx| input.set_text("", cx));
        }
        self.reload(cx);
    }

    fn finish_reveal(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.reveal.clone() else {
            return;
        };
        if let Some(index) = self.commits.iter().position(|commit| commit.id == id) {
            self.reveal = None;
            self.anchor = Some(index);
            self.set_selection(vec![index], cx);
            self.scroll
                .scroll_to_item(index, gpui::ScrollStrategy::Center);
        } else if self.next.is_some() && self.reveal_pages < REVEAL_PAGES {
            self.reveal_pages += 1;
            self.load_more(cx);
        } else {
            self.reveal = None;
            let short = crate::commit_panel::short_revision(&id).to_owned();
            self.report = Some((format!("Commit {short} is not in the log").into(), false));
            cx.notify();
        }
    }

    /// Re-reads refs and the first page of the log, keeping the selection.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(repository) = self.repository() else {
            cx.notify();
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        self.loading = true;
        let refs_repository = repository.clone();
        self._refs = cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move { load_refs(&refs_repository) })
                .await;
            this.update(cx, |this, cx| {
                match loaded {
                    Ok((refs, head, me)) => {
                        this.refs = refs;
                        this.head = head;
                        this.me = me;
                        this.rebuild_branches();
                    }
                    Err(err) => this.error = Some(err.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        });
        let filter = self.filter.clone();
        self._load = cx.spawn(async move |this, cx| {
            let page = cx
                .background_spawn(async move { repository.log_page(&filter, 0, PAGE) })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                let selected_ids: Vec<String> = this
                    .selected
                    .iter()
                    .filter_map(|&index| this.commits.get(index))
                    .map(|commit| commit.id.clone())
                    .collect();
                match page {
                    Ok(page) => {
                        this.commits = page.commits;
                        this.next = page.next;
                        this.error = None;
                    }
                    Err(err) => {
                        this.commits.clear();
                        this.next = None;
                        this.error = Some(err.to_string().into());
                    }
                }
                this.graph = this.filter.graph(&this.commits);
                let mut selected: Vec<usize> = selected_ids
                    .iter()
                    .filter_map(|id| this.commits.iter().position(|c| &c.id == id))
                    .collect();
                selected.sort_unstable();
                if selected.is_empty() && !this.commits.is_empty() {
                    selected.push(0);
                }
                this.anchor = selected.first().copied();
                this.set_selection(selected, cx);
                this.finish_reveal(cx);
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    /// Loads the page after the loaded commits.
    fn load_more(&mut self, cx: &mut Context<Self>) {
        let (Some(skip), false) = (self.next, self.loading) else {
            return;
        };
        let Some(repository) = self.repository() else {
            return;
        };
        self.loading = true;
        let generation = self.generation;
        let filter = self.filter.clone();
        self._load = cx.spawn(async move |this, cx| {
            let page = cx
                .background_spawn(async move { repository.log_page(&filter, skip, PAGE) })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                match page {
                    Ok(page) => {
                        this.commits.extend(page.commits);
                        this.next = page.next;
                        this.graph = this.filter.graph(&this.commits);
                        this.finish_reveal(cx);
                    }
                    Err(err) => {
                        this.next = None;
                        this.error = Some(err.to_string().into());
                    }
                }
                cx.notify();
            })
            .ok();
        });
    }

    fn set_filter(&mut self, change: impl FnOnce(&mut LogFilter), cx: &mut Context<Self>) {
        self.popup = None;
        change(&mut self.filter);
        self.reload(cx);
    }

    fn set_selection(&mut self, selected: Vec<usize>, cx: &mut Context<Self>) {
        let commits: Vec<Commit> = selected
            .iter()
            .filter_map(|&index| self.commits.get(index).cloned())
            .collect();
        self.selected = selected;
        let ids: Vec<String> = commits.iter().map(|c| c.id.clone()).collect();
        if self.details.as_ref().is_some_and(|d| d.ids == ids) {
            return;
        }
        let (Some(newest), Some(oldest), Some(repository)) =
            (commits.first(), commits.last(), self.repository())
        else {
            self.details = None;
            cx.notify();
            return;
        };
        let (newest, oldest) = (newest.clone(), oldest.clone());
        let single = commits.len() == 1;
        let load = cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    let changes = repository.selection_changes(&newest, &oldest);
                    let (message, branches) = if single {
                        (
                            repository.commit_message(&newest.id).ok(),
                            repository.branches_containing(&newest.id).ok(),
                        )
                    } else {
                        (None, None)
                    };
                    (changes, message, branches)
                })
                .await;
            this.update(cx, |this, cx| {
                let Some(details) = &mut this.details else {
                    return;
                };
                let (changes, message, branches) = loaded;
                details.changes = Some(changes.map_err(|err| err.to_string().into()));
                details.message = message;
                details.branches = branches;
                Self::rebuild_files(details);
                cx.notify();
            })
            .ok();
        });
        self.details = Some(Details {
            ids,
            changes: None,
            nodes: Vec::new(),
            collapsed: HashSet::new(),
            message: None,
            branches: None,
            _load: load,
        });
        cx.notify();
    }

    fn rebuild_files(details: &mut Details) {
        let paths: Vec<&str> = match &details.changes {
            Some(Ok(span)) => span.changes.iter().map(|c| c.path.as_str()).collect(),
            _ => Vec::new(),
        };
        details.nodes = change_tree::tree(&paths, &details.collapsed);
    }

    fn click_commit(&mut self, index: usize, modifiers: Modifiers, cx: &mut Context<Self>) {
        self.menu = None;
        self.popup = None;
        let selected = if modifiers.shift {
            let anchor = self.anchor.unwrap_or(index);
            (anchor.min(index)..=anchor.max(index)).collect()
        } else if modifiers.secondary() {
            self.anchor = Some(index);
            let mut selected = self.selected.clone();
            match selected.binary_search(&index) {
                Ok(at) => {
                    selected.remove(at);
                }
                Err(at) => selected.insert(at, index),
            }
            selected
        } else {
            self.anchor = Some(index);
            vec![index]
        };
        self.set_selection(selected, cx);
    }

    /// Up/Down: moves a single selection; with Shift, extends it.
    fn step(&mut self, down: bool, extend: bool, cx: &mut Context<Self>) {
        if self.commits.is_empty() {
            return;
        }
        let last = self.commits.len() - 1;
        // The moving end: the selection end away from the anchor.
        let cursor = match (self.anchor, extend) {
            (Some(anchor), true) => {
                let first = self.selected.first().copied().unwrap_or(anchor);
                let end = self.selected.last().copied().unwrap_or(anchor);
                if first < anchor { first } else { end }
            }
            _ if down => self.selected.last().copied().unwrap_or(0),
            _ => self.selected.first().copied().unwrap_or(0),
        };
        let target = if down {
            (cursor + 1).min(last)
        } else {
            cursor.saturating_sub(1)
        };
        let selected = if extend {
            let anchor = self.anchor.unwrap_or(cursor);
            (anchor.min(target)..=anchor.max(target)).collect()
        } else {
            self.anchor = Some(target);
            vec![target]
        };
        self.set_selection(selected, cx);
        self.scroll
            .scroll_to_item(target, gpui::ScrollStrategy::Center);
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.step(false, false, cx);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step(true, false, cx);
    }

    fn extend_prev(&mut self, _: &ExtendPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.step(false, true, cx);
    }

    fn extend_next(&mut self, _: &ExtendNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step(true, true, cx);
    }

    fn close_menu(&mut self, _: &CloseMenu, _: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        self.popup = None;
        cx.notify();
    }

    /// The selected commits, newest first.
    fn selected_commits(&self) -> Vec<&Commit> {
        self.selected
            .iter()
            .filter_map(|&index| self.commits.get(index))
            .collect()
    }

    fn open_node(&mut self, index: usize, event: &ClickEvent, cx: &mut Context<Self>) {
        let Some(repository) = self.repository() else {
            return;
        };
        let Some(details) = &mut self.details else {
            return;
        };
        match details.nodes.get(index) {
            Some(Node::Dir { path, .. }) => {
                if !details.collapsed.remove(path) {
                    details.collapsed.insert(path.clone());
                }
                Self::rebuild_files(details);
                cx.notify();
            }
            Some(&Node::File { item, .. }) if event.click_count() >= 2 => {
                let Some(Ok(span)) = &details.changes else {
                    return;
                };
                let Some(change) = span.changes.get(item) else {
                    return;
                };
                cx.emit(GitLogEvent::OpenDiff {
                    repository,
                    source: DiffSource::Revisions {
                        old: span.base.clone(),
                        new: span.target.clone(),
                        change: change.clone(),
                    },
                });
            }
            _ => {}
        }
    }

    fn open_popup(&mut self, kind: FilterKind, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.menu = None;
        if self.popup.as_ref().is_some_and(|p| p.kind == kind) {
            self.popup = None;
        } else {
            self.popup = Some(FilterPopup { kind, position });
        }
        cx.notify();
    }

    fn apply_paths(&mut self, _: &editor::Newline, _: &mut Window, cx: &mut Context<Self>) {
        let text = self.path_input.read(cx).text();
        let paths = text
            .split(',')
            .map(|path| path.trim().trim_matches('/').replace('\\', "/"))
            .filter(|path| !path.is_empty())
            .collect();
        self.set_filter(|filter| filter.paths = paths, cx);
    }

    fn render_commit(&self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let commit = &self.commits[index];
        let selected = self.selected.binary_search(&index).is_ok();
        let head = self.head.as_ref().is_some_and(|head| head.id == commit.id);
        div()
            .id(("commit", index))
            .w_full()
            .h(px(ROW_HEIGHT))
            .pl_2()
            .pr_3()
            .flex()
            .items_center()
            .gap_2()
            .text_sm()
            .cursor_pointer()
            .when(selected, |row| row.bg(theme::active_row()))
            .when(!selected, |row| {
                row.hover(|style| style.bg(theme::hover_row()))
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                window.focus(&this.focus_handle);
                this.click_commit(index, event.modifiers(), cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus_handle);
                    this.open_menu(index, event.position, cx);
                }),
            )
            .child(graph_cell(&self.graph[index], head))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(theme::text())
                            .when(head, |subject| subject.font_weight(gpui::FontWeight::BOLD))
                            .child(
                                selectable("subject", commit.subject.clone()).right_click_through(),
                            ),
                    )
                    .child(div().flex_1())
                    .children(commit.refs.iter().enumerate().map(|(i, name)| {
                        let remote = self
                            .refs
                            .iter()
                            .any(|r| r.kind == git::RefKind::Remote && r.name == *name);
                        ref_label(i, name, remote)
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(130.))
                    .truncate()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(selectable("author", commit.author.clone()).right_click_through()),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(100.))
                    .truncate()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(selectable("time", relative_time(commit.time)).right_click_through()),
            )
            .into_any_element()
    }

    fn filter_button(
        &self,
        kind: FilterKind,
        label: &'static str,
        value: Option<String>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = value.is_some();
        let text = match value {
            Some(value) => format!("{label}: {value}"),
            None => label.to_owned(),
        };
        div()
            .id(label)
            .flex_none()
            .max_w(px(180.))
            .px_2()
            .py_px()
            .rounded_sm()
            .flex()
            .items_center()
            .gap_1()
            .cursor_pointer()
            .text_color(if active {
                theme::accent_text()
            } else {
                theme::text()
            })
            .hover(|style| style.bg(theme::hover_row()))
            .child(div().min_w_0().truncate().child(text))
            .child(div().text_xs().text_color(theme::muted()).child("▾"))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    let position = event.position + point(px(0.), px(12.));
                    this.open_popup(kind, position, cx);
                }),
            )
    }

    /// "199 commits", "1000+ commits" while more pages remain, "Loading…".
    fn status_text(&self) -> String {
        if self.loading && self.commits.is_empty() {
            "Loading…".to_owned()
        } else if self.next.is_some() {
            format!("{}+ commits", self.commits.len())
        } else {
            format!("{} commits", self.commits.len())
        }
    }

    fn render_filter_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let branch = (!self.filter.revisions.is_empty()).then(|| self.filter.revisions.join(", "));
        let user = (!self.filter.authors.is_empty()).then(|| self.filter.authors.join(", "));
        let date = self.date_label.map(str::to_owned);
        let paths = (!self.filter.paths.is_empty()).then(|| self.filter.paths.join(", "));
        let shown = self
            .repository()
            .map(|repository| repository_name(&repository).to_owned());
        let repository = (self.repositories.len() > 1)
            .then(|| self.filter_button(FilterKind::Repository, "Repository", shown, cx));
        div()
            .flex_none()
            .h(px(32.))
            .px_2()
            .flex()
            .items_center()
            .gap_1()
            .border_b_1()
            .border_color(theme::border())
            .text_sm()
            .children(repository)
            .child(
                div()
                    .w(px(180.))
                    .flex_none()
                    .h(px(24.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(theme::border())
                    .bg(theme::background())
                    .child(div().text_color(theme::muted()).child("⌕"))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h(px(20.))
                            .child(self.text_input.clone()),
                    ),
            )
            .child(self.filter_button(FilterKind::Branch, "Branch", branch, cx))
            .child(self.filter_button(FilterKind::User, "User", user, cx))
            .child(self.filter_button(FilterKind::Date, "Date", date, cx))
            .child(self.filter_button(FilterKind::Paths, "Paths", paths, cx))
            .when(
                !self.filter.is_empty() || self.date_label.is_some(),
                |bar| {
                    bar.child(
                        div()
                            .id("clear-filters")
                            .px_1()
                            .cursor_pointer()
                            .text_color(theme::muted())
                            .hover(|style| style.text_color(theme::text()))
                            .child("✕")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.text_input
                                    .update(cx, |input, cx| input.set_text("", cx));
                                this.path_input
                                    .update(cx, |input, cx| input.set_text("", cx));
                                this.date_label = None;
                                this._debounce = Task::ready(());
                                this.set_filter(|filter| *filter = LogFilter::default(), cx);
                            })),
                    )
                },
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("log-refresh")
                    .px_2()
                    .cursor_pointer()
                    .text_color(theme::muted())
                    .hover(|style| style.text_color(theme::text()))
                    .child("⟳")
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            )
    }

    fn popup_item(
        id: impl Into<gpui::ElementId>,
        label: impl Into<SharedString>,
        checked: bool,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id.into())
            .px_2()
            .py_px()
            .rounded_sm()
            .flex()
            .gap_2()
            .cursor_pointer()
            .hover(|style| style.bg(theme::active_row()))
            .child(
                div()
                    .w(px(12.))
                    .flex_none()
                    .child(if checked { "✓" } else { "" }),
            )
            .child(label.into())
    }

    fn render_popup(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let popup = self.popup.as_ref()?;
        let mut items: Vec<gpui::AnyElement> = Vec::new();
        match popup.kind {
            FilterKind::Repository => {
                let shown = self.repository().map(|r| repository_key(r.root()));
                for (index, repository) in self.repositories.iter().enumerate() {
                    let checked = shown.as_deref() == Some(&*repository_key(repository.root()));
                    items.push(
                        Self::popup_item(
                            ("repository", index),
                            repository_name(repository).to_owned(),
                            checked,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.chosen = true;
                            this.popup = None;
                            this.switch_repository(index, cx);
                            cx.notify();
                        }))
                        .into_any_element(),
                    );
                }
            }
            FilterKind::Branch => {
                items.push(
                    Self::popup_item("branch-all", "All", self.filter.revisions.is_empty())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.set_filter(|filter| filter.revisions.clear(), cx)
                        }))
                        .into_any_element(),
                );
                let names = std::iter::once("HEAD".to_owned()).chain(
                    branches::sorted_refs(&self.refs)
                        .into_iter()
                        .map(|r| r.name.clone()),
                );
                for (index, name) in names.enumerate() {
                    let checked = self.filter.revisions == [name.clone()];
                    items.push(
                        Self::popup_item(("branch", index), name.clone(), checked)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let name = name.clone();
                                this.set_filter(|filter| filter.revisions = vec![name], cx)
                            }))
                            .into_any_element(),
                    );
                }
            }
            FilterKind::User => {
                items.push(
                    Self::popup_item("user-all", "All", self.filter.authors.is_empty())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.set_filter(|filter| filter.authors.clear(), cx)
                        }))
                        .into_any_element(),
                );
                let mut authors: Vec<String> = Vec::new();
                if let Some(me) = &self.me {
                    authors.push(me.clone());
                }
                let mut seen: Vec<&str> = self.commits.iter().map(|c| c.author.as_str()).collect();
                seen.sort_unstable();
                seen.dedup();
                authors.extend(
                    seen.into_iter()
                        .filter(|name| Some(*name) != self.me.as_deref())
                        .map(str::to_owned),
                );
                for (index, author) in authors.into_iter().enumerate() {
                    let checked = self.filter.authors == [author.clone()];
                    let label = if index == 0 && self.me.as_ref() == Some(&author) {
                        format!("me ({author})")
                    } else {
                        author.clone()
                    };
                    items.push(
                        Self::popup_item(("user", index), label, checked)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let author = author.clone();
                                this.set_filter(|filter| filter.authors = vec![author], cx)
                            }))
                            .into_any_element(),
                    );
                }
            }
            FilterKind::Date => {
                items.push(
                    Self::popup_item("date-all", "All", self.date_label.is_none())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.date_label = None;
                            this.set_filter(|filter| filter.since = None, cx)
                        }))
                        .into_any_element(),
                );
                for (index, (label, age)) in DATE_RANGES.into_iter().enumerate() {
                    items.push(
                        Self::popup_item(("date", index), label, self.date_label == Some(label))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.date_label = Some(label);
                                this.set_filter(|filter| filter.since = Some(now() - age), cx)
                            }))
                            .into_any_element(),
                    );
                }
            }
            FilterKind::Paths => {
                items.push(
                    Self::popup_item("paths-all", "All", self.filter.paths.is_empty())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.path_input
                                .update(cx, |input, cx| input.set_text("", cx));
                            this.set_filter(|filter| filter.paths.clear(), cx)
                        }))
                        .into_any_element(),
                );
                items.push(
                    div()
                        .px_2()
                        .pt_1()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(selectable(
                            "paths-label",
                            "Paths (repository-relative, comma-separated), Enter applies:",
                        ))
                        .into_any_element(),
                );
                items.push(
                    div()
                        .key_context("GitLogPaths")
                        .on_action(cx.listener(Self::apply_paths))
                        .m_1()
                        .h(px(24.))
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded_sm()
                        .border_1()
                        .border_color(theme::border())
                        .bg(theme::background())
                        .child(div().flex_1().h(px(20.)).child(self.path_input.clone()))
                        .into_any_element(),
                );
            }
        }
        Some(deferred(
            anchored().position(popup.position).child(
                div()
                    .id("log-filter-popup")
                    .occlude()
                    .min_w(px(220.))
                    .max_h(px(360.))
                    .overflow_y_scroll()
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
                        this.popup = None;
                        cx.notify();
                    }))
                    .children(items),
            ),
        ))
    }

    fn render_details(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(details) = &self.details else {
            return div()
                .p_3()
                .text_sm()
                .text_color(theme::muted())
                .child(selectable("empty", "Select a commit to see its changes"))
                .into_any_element();
        };
        let commits = self.selected_commits();
        let files: gpui::AnyElement = match &details.changes {
            None => div()
                .p_2()
                .text_sm()
                .text_color(theme::muted())
                .child(selectable("loading", "Loading…"))
                .into_any_element(),
            Some(Err(err)) => div()
                .p_2()
                .text_sm()
                .text_color(theme::error())
                .child(selectable("error", err.clone()))
                .into_any_element(),
            Some(Ok(span)) => {
                let changes = span.changes.clone();
                uniform_list(
                    "log-files",
                    details.nodes.len(),
                    cx.processor(move |this, range: Range<usize>, _, cx| {
                        let Some(details) = &this.details else {
                            return Vec::new();
                        };
                        range
                            .map(|index| {
                                // Full width, so the whole row is the target.
                                div()
                                    .w_full()
                                    .child(render_change_node(
                                        &details.nodes[index],
                                        &changes,
                                        index,
                                        cx.listener(move |this, event: &ClickEvent, _, cx| {
                                            this.open_node(index, event, cx)
                                        }),
                                    ))
                                    .into_any_element()
                            })
                            .collect()
                    }),
                )
                .flex_1()
                .into_any_element()
            }
        };
        let file_count = match &details.changes {
            Some(Ok(span)) => format!(
                "{} file{} changed",
                span.changes.len(),
                if span.changes.len() == 1 { "" } else { "s" }
            ),
            _ => String::new(),
        };
        let header = if commits.len() > 1 {
            format!("{} commits selected · {file_count}", commits.len())
        } else {
            file_count
        };
        let info: gpui::AnyElement = match commits.as_slice() {
            [commit] => {
                let message = details
                    .message
                    .clone()
                    .unwrap_or_else(|| commit.subject.clone());
                let branches = details.branches.as_ref().map(|branches| {
                    let shown: Vec<&str> = branches.iter().take(8).map(String::as_str).collect();
                    let more = branches.len().saturating_sub(shown.len());
                    format!(
                        "In {} branch{}: {}{}",
                        branches.len(),
                        if branches.len() == 1 { "" } else { "es" },
                        shown.join(", "),
                        if more > 0 {
                            format!(" and {more} more")
                        } else {
                            String::new()
                        }
                    )
                });
                div()
                    .id("log-commit-info")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .text_sm()
                    .child(
                        div()
                            .text_color(theme::text())
                            .child(selectable("message", message)),
                    )
                    .child(
                        div()
                            .mt_2()
                            .font_family(theme::MONO_FONT)
                            .text_xs()
                            .text_color(theme::muted())
                            .child(selectable("hash", commit.id.clone())),
                    )
                    .child(div().text_xs().text_color(theme::muted()).child(selectable(
                        "author",
                        format!(
                            "{} <{}> on {}",
                            commit.author,
                            commit.email,
                            absolute_time(commit.time)
                        ),
                    )))
                    .children(branches.map(|branches| {
                        div()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(selectable("branches", branches))
                    }))
                    .into_any_element()
            }
            commits => div()
                .id("log-commit-info")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p_3()
                .flex()
                .flex_col()
                .gap_1()
                .text_sm()
                .children(commits.iter().enumerate().map(|(index, commit)| {
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_none()
                                .font_family(theme::MONO_FONT)
                                .text_xs()
                                .text_color(theme::muted())
                                .child(selectable(("hash", index), commit.short_id().to_owned())),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(theme::text())
                                .child(selectable(("subject", index), commit.subject.clone())),
                        )
                }))
                .into_any_element(),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(theme::muted())
                    .border_b_1()
                    .border_color(theme::border())
                    .child(selectable("header", header)),
            )
            .child(div().flex_1().min_h_0().flex().flex_col().child(files))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .border_t_1()
                    .border_color(theme::border())
                    .child(info),
            )
            .into_any_element()
    }

    fn render_report(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let (message, error) = self.report.clone()?;
        Some(
            div()
                .flex_none()
                .px_3()
                .py_1()
                .flex()
                .items_center()
                .gap_2()
                .border_t_1()
                .border_color(theme::border())
                .text_xs()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(if error {
                            theme::error()
                        } else {
                            theme::muted()
                        })
                        .child(selectable("report", message)),
                )
                .when(self.conflict, |row| {
                    row.child(
                        div()
                            .id("log-abort")
                            .px_2()
                            .rounded_sm()
                            .border_1()
                            .border_color(theme::border())
                            .cursor_pointer()
                            .hover(|style| style.bg(theme::hover_row()))
                            .child("Abort")
                            .on_click(cx.listener(|this, _, _, cx| this.abort(cx))),
                    )
                })
                .child(
                    div()
                        .id("log-report-close")
                        .cursor_pointer()
                        .text_color(theme::muted())
                        .child("✕")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.report = None;
                            cx.notify();
                        })),
                ),
        )
    }

    fn render_body(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let message = match &self.repo {
            RepoState::Unknown | RepoState::Discovering => Some(("Loading…".into(), false)),
            RepoState::Missing => Some(("The project is not in a git repository.".into(), false)),
            RepoState::Failed(err) => Some((err.clone(), true)),
            RepoState::Ready(_) => None,
        };
        if let Some((message, error)) = message {
            return div()
                .p_3()
                .text_sm()
                .text_color(if error {
                    theme::error()
                } else {
                    theme::muted()
                })
                .child(selectable("message", message))
                .into_any_element();
        }
        let list: gpui::AnyElement = match &self.error {
            Some(error) if self.commits.is_empty() => div()
                .p_3()
                .text_sm()
                .text_color(theme::error())
                .child(selectable("error", error.clone()))
                .into_any_element(),
            _ if !self.loading && self.commits.is_empty() => div()
                .p_3()
                .text_sm()
                .text_color(theme::muted())
                .child(selectable(
                    "empty",
                    if self.filter.is_empty() {
                        "No commits yet"
                    } else {
                        "No commits matching the filters"
                    },
                ))
                .into_any_element(),
            _ => uniform_list(
                "log-commits",
                self.commits.len(),
                cx.processor(|this, range: Range<usize>, _, cx| {
                    if range.end + PREFETCH >= this.commits.len() {
                        this.load_more(cx);
                    }
                    range
                        .filter(|&index| index < this.commits.len())
                        .map(|index| this.render_commit(index, cx))
                        .collect()
                }),
            )
            .track_scroll(self.scroll.clone())
            .flex_1()
            .into_any_element(),
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .child(
                div()
                    .w(px(BRANCHES_WIDTH))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .border_r_1()
                    .border_color(theme::border())
                    .child(self.render_branches(cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(self.render_filter_bar(cx))
                    .child(div().flex_1().min_h_0().flex().flex_col().child(list))
                    .children(self.render_report(cx)),
            )
            .child(
                div()
                    .w(px(DETAILS_WIDTH))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .border_l_1()
                    .border_color(theme::border())
                    .child(self.render_details(cx)),
            )
            .into_any_element()
    }
}

/// Every folder's repository, looked for in all folders at once, one per
/// repository in folder order (folders without one are skipped). Fails
/// only when no folder has one and a folder failed. Blocks.
fn discover(project: &Project) -> std::io::Result<Vec<Repository>> {
    let results: Vec<std::io::Result<Option<Repository>>> = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..project.roots().len())
            .map(|root| scope.spawn(move || project.repository_in(root)))
            .collect();
        threads
            .into_iter()
            .map(|thread| {
                thread
                    .join()
                    .unwrap_or_else(|_| Err(std::io::Error::other("git panicked")))
            })
            .collect()
    });
    let mut error = None;
    let found: Vec<Option<Repository>> = results
        .into_iter()
        .map(|result| {
            result.unwrap_or_else(|err| {
                error.get_or_insert(err);
                None
            })
        })
        .collect();
    let keys: Vec<Option<String>> = found
        .iter()
        .map(|found| found.as_ref().map(|r| repository_key(r.root())))
        .collect();
    let (groups, _) = group_folders(&keys);
    let repositories: Vec<Repository> = groups
        .iter()
        .filter_map(|folders| found[folders[0]].clone())
        .collect();
    match error {
        Some(err) if repositories.is_empty() => Err(err),
        _ => Ok(repositories),
    }
}

/// Refs, HEAD and the configured user, read together off the UI thread.
fn load_refs(repository: &Repository) -> std::io::Result<(Vec<Ref>, Option<Head>, Option<String>)> {
    let refs = repository.refs()?;
    let head = match repository.resolve("HEAD") {
        Ok(id) => Some(Head {
            pushed: repository.is_pushed(&id)?,
            branch: refs.iter().find(|r| r.head).map(|r| r.name.clone()),
            id,
        }),
        // No commit yet.
        Err(_) => None,
    };
    Ok((refs, head, repository.user_name()?))
}

impl Focusable for GitLogPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GitLogPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.start(cx);
        let body = self.render_body(cx);
        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::extend_prev))
            .on_action(cx.listener(Self::extend_next))
            .on_action(cx.listener(Self::copy_revision))
            .on_action(cx.listener(Self::close_menu))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::background())
            .text_color(theme::text())
            .child(
                // PyCharm's tool window tabs; Log is the only one.
                div()
                    .flex_none()
                    .h(px(28.))
                    .px_2()
                    .flex()
                    .items_end()
                    .border_b_1()
                    .border_color(theme::border())
                    .text_sm()
                    .child(
                        div()
                            .px_2()
                            .pb_1()
                            .border_b_2()
                            .border_color(theme::accent())
                            .child("Log"),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .pb_1()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(self.status_text()),
                    ),
            )
            .child(body)
            .children(self.render_popup(cx))
            .children(self.render_menu(cx))
    }
}

#[cfg(test)]
mod tests;
