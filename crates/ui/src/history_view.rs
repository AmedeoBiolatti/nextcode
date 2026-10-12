//! A tab with the commit graph: commits of every branch (or just HEAD) with
//! their lanes, refs, authors and dates; the selected commit's details and
//! changed files below, each opening its diff.

use std::{
    collections::HashSet,
    ops::Range,
    time::{SystemTime, UNIX_EPOCH},
};

use git::{Commit, FileChange, GraphRow, Repository, Side};
use gpui::{
    App, Bounds, Context, EventEmitter, FocusHandle, Focusable, Hsla, PathBuilder, SharedString,
    Task, UniformListScrollHandle, Window, canvas, div, fill, point, prelude::*, px, rgb, size,
    uniform_list,
};

use crate::{
    change_tree::{self, Node},
    compare_view::render_change_node,
    diff_view::DiffSource,
    theme,
};

const ROW_HEIGHT: f32 = 24.;
const LANE_WIDTH: f32 = 14.;
/// Lanes beyond this are drawn clipped; real histories rarely need more.
const MAX_LANES: usize = 16;
const LIMIT: usize = 2000;
const DETAILS_HEIGHT: f32 = 280.;
const LANE_COLORS: [u32; 6] = [0x56a8f5, 0x6aab73, 0xc77dbb, 0xe0a458, 0x2aacb8, 0xf75464];

pub enum HistoryEvent {
    OpenDiff {
        repository: Repository,
        source: DiffSource,
    },
    Compare {
        repository: Repository,
        base: String,
        target: Side,
    },
}

impl EventEmitter<HistoryEvent> for HistoryView {}

struct Selected {
    commit: usize,
    changes: Option<Vec<FileChange>>,
    nodes: Vec<Node>,
    collapsed: HashSet<String>,
    _load: Task<()>,
}

pub struct HistoryView {
    repository: Repository,
    all_branches: bool,
    commits: Vec<Commit>,
    graph: Vec<GraphRow>,
    error: Option<SharedString>,
    loading: bool,
    selected: Option<Selected>,
    scroll: UniformListScrollHandle,
    focus_handle: FocusHandle,
    _load: Task<()>,
}

fn lane_color(lane: usize) -> Hsla {
    rgb(LANE_COLORS[lane % LANE_COLORS.len()]).into()
}

/// "3 hours ago", "2 days ago", …
fn relative_time(time: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let seconds = (now - time).max(0);
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

/// The lanes and node of one commit.
fn graph_cell(row: &GraphRow) -> impl IntoElement {
    let row = row.clone();
    let lanes = row.width.min(MAX_LANES);
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let x = |lane: usize| bounds.origin.x + px(LANE_WIDTH * lane as f32 + LANE_WIDTH / 2.);
            let top = bounds.origin.y;
            let middle = top + bounds.size.height / 2.;
            let bottom = top + bounds.size.height;
            let mut line = |from: (usize, gpui::Pixels), to: (usize, gpui::Pixels), color: Hsla| {
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
                line((from, top), (to, middle), lane_color(from));
            }
            for &(from, to) in &row.bottom {
                line((from, middle), (to, bottom), lane_color(to));
            }
            if row.column < MAX_LANES {
                let node = Bounds::centered_at(point(x(row.column), middle), size(px(8.), px(8.)));
                window.paint_quad(fill(node, lane_color(row.column)).corner_radii(px(4.)));
            }
        },
    )
    .flex_none()
    .w(px(LANE_WIDTH * lanes.max(1) as f32))
    .h(px(ROW_HEIGHT))
}

impl HistoryView {
    pub fn new(repository: Repository, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            repository,
            all_branches: true,
            commits: Vec::new(),
            graph: Vec::new(),
            error: None,
            loading: true,
            selected: None,
            scroll: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            _load: Task::ready(()),
        };
        view.reload(cx);
        view
    }

    /// Re-reads the log, keeping the selected commit if it still exists.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let (repository, all) = (self.repository.clone(), self.all_branches);
        self.loading = true;
        self._load = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let commits = repository.log(LIMIT, all)?;
                    let graph = git::graph(&commits);
                    Ok::<_, std::io::Error>((commits, graph))
                })
                .await;
            this.update(cx, |this, cx| {
                this.loading = false;
                let selected_id = this
                    .selected
                    .as_ref()
                    .and_then(|s| this.commits.get(s.commit))
                    .map(|c| c.id.clone());
                match result {
                    Ok((commits, graph)) => {
                        this.commits = commits;
                        this.graph = graph;
                        this.error = None;
                    }
                    Err(err) => this.error = Some(err.to_string().into()),
                }
                let index = selected_id
                    .and_then(|id| this.commits.iter().position(|c| c.id == id))
                    .or((!this.commits.is_empty()).then_some(0));
                match index {
                    Some(index) => this.select(index, cx),
                    None => this.selected = None,
                }
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if self
            .selected
            .as_ref()
            .is_some_and(|s| s.commit == index && s.changes.is_some())
        {
            return;
        }
        let Some(commit) = self.commits.get(index).cloned() else {
            return;
        };
        let repository = self.repository.clone();
        let load = cx.spawn(async move |this, cx| {
            let changes = cx
                .background_spawn(async move { repository.commit_changes(&commit) })
                .await;
            this.update(cx, |this, cx| {
                if let Some(selected) = &mut this.selected {
                    match changes {
                        Ok(changes) => selected.changes = Some(changes),
                        Err(err) => this.error = Some(err.to_string().into()),
                    }
                    Self::rebuild(selected);
                }
                cx.notify();
            })
            .ok();
        });
        self.selected = Some(Selected {
            commit: index,
            changes: None,
            nodes: Vec::new(),
            collapsed: HashSet::new(),
            _load: load,
        });
        cx.notify();
    }

    fn rebuild(selected: &mut Selected) {
        let paths: Vec<&str> = selected
            .changes
            .iter()
            .flatten()
            .map(|change| change.path.as_str())
            .collect();
        selected.nodes = change_tree::tree(&paths, &selected.collapsed);
    }

    fn open_node(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(selected) = &mut self.selected else {
            return;
        };
        match selected.nodes.get(index) {
            Some(Node::Dir { path, .. }) => {
                if !selected.collapsed.remove(path) {
                    selected.collapsed.insert(path.clone());
                }
                Self::rebuild(selected);
                cx.notify();
            }
            Some(&Node::File { item, .. }) => {
                let commit = &self.commits[selected.commit];
                let Some(change) = selected.changes.as_ref().and_then(|c| c.get(item)) else {
                    return;
                };
                let old = commit
                    .parents
                    .first()
                    .map_or(Side::Empty, |parent| Side::Revision(parent.clone()));
                cx.emit(HistoryEvent::OpenDiff {
                    repository: self.repository.clone(),
                    source: DiffSource::Revisions {
                        old,
                        new: Side::Revision(commit.id.clone()),
                        change: change.clone(),
                    },
                });
            }
            None => {}
        }
    }

    fn render_commit(&self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let commit = &self.commits[index];
        let selected = self.selected.as_ref().is_some_and(|s| s.commit == index);
        let refs = commit.refs.iter().map(|name| {
            let (label, color) = if let Some(branch) = name.strip_prefix("HEAD -> ") {
                (branch.to_owned(), theme::ACCENT)
            } else if let Some(tag) = name.strip_prefix("tag: ") {
                (tag.to_owned(), 0x8a6d1f)
            } else {
                (name.clone(), theme::BORDER)
            };
            div()
                .flex_none()
                .px_1()
                .rounded_sm()
                .text_xs()
                .bg(rgb(color))
                .text_color(rgb(0xffffff))
                .child(label)
        });
        div()
            .id(("commit", index))
            .h(px(ROW_HEIGHT))
            .pl_2()
            .pr_3()
            .flex()
            .items_center()
            .gap_2()
            .text_sm()
            .cursor_pointer()
            .when(selected, |row| row.bg(rgb(theme::ACTIVE_ROW)))
            .when(!selected, |row| {
                row.hover(|style| style.bg(rgb(theme::HOVER_ROW)))
            })
            .on_click(cx.listener(move |this, _, _, cx| this.select(index, cx)))
            .child(graph_cell(&self.graph[index]))
            .children(refs)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(rgb(theme::TEXT))
                    .child(commit.subject.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(140.))
                    .truncate()
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(commit.author.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(96.))
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(relative_time(commit.time)),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(64.))
                    .font_family(theme::MONO_FONT)
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(commit.short_id().to_owned()),
            )
            .into_any_element()
    }

    fn render_details(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let selected = self.selected.as_ref()?;
        let commit = self.commits.get(selected.commit)?;
        let parents = commit
            .parents
            .iter()
            .map(|p| &p[..p.len().min(8)])
            .collect::<Vec<_>>()
            .join(", ");
        let compare_id = commit.id.clone();
        let files: gpui::AnyElement = match &selected.changes {
            None => div()
                .p_2()
                .text_sm()
                .text_color(rgb(theme::MUTED))
                .child("Loading…")
                .into_any_element(),
            Some(changes) => {
                let changes = changes.clone();
                uniform_list(
                    "commit-files",
                    selected.nodes.len(),
                    cx.processor(move |this, range: Range<usize>, _, cx| {
                        let Some(selected) = &this.selected else {
                            return Vec::new();
                        };
                        range
                            .map(|index| {
                                render_change_node(
                                    &selected.nodes[index],
                                    &changes,
                                    index,
                                    cx.listener(move |this, _, _, cx| this.open_node(index, cx)),
                                )
                            })
                            .collect()
                    }),
                )
                .flex_1()
                .into_any_element()
            }
        };
        Some(
            div()
                .h(px(DETAILS_HEIGHT))
                .flex_none()
                .flex()
                .border_t_1()
                .border_color(rgb(theme::BORDER))
                .child(
                    div()
                        .w(px(360.))
                        .flex_none()
                        .p_3()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .border_r_1()
                        .border_color(rgb(theme::BORDER))
                        .text_sm()
                        .child(
                            div()
                                .text_color(rgb(theme::TEXT))
                                .child(commit.subject.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::MUTED))
                                .child(format!("{} <{}>", commit.author, commit.email)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::MUTED))
                                .child(relative_time(commit.time)),
                        )
                        .child(
                            div()
                                .font_family(theme::MONO_FONT)
                                .text_xs()
                                .text_color(rgb(theme::MUTED))
                                .child(commit.id.clone()),
                        )
                        .when(!parents.is_empty(), |details| {
                            details.child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme::MUTED))
                                    .child(format!("parents: {parents}")),
                            )
                        })
                        .child(
                            div()
                                .id("compare-worktree")
                                .mt_2()
                                .px_2()
                                .py_px()
                                .w_auto()
                                .rounded_sm()
                                .border_1()
                                .border_color(rgb(theme::BORDER))
                                .cursor_pointer()
                                .hover(|style| style.bg(rgb(theme::HOVER_ROW)))
                                .child("Compare with working tree")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.emit(HistoryEvent::Compare {
                                        repository: this.repository.clone(),
                                        base: compare_id.clone(),
                                        target: Side::Worktree,
                                    })
                                })),
                        ),
                )
                .child(div().flex_1().min_w_0().flex().flex_col().child(files)),
        )
    }
}

impl Focusable for HistoryView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for HistoryView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = if self.loading {
            "Loading…".to_owned()
        } else if self.commits.len() >= LIMIT {
            format!("latest {LIMIT} commits")
        } else {
            format!("{} commits", self.commits.len())
        };
        let toggle = div()
            .id("all-branches")
            .px_2()
            .py_px()
            .rounded_sm()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(theme::HOVER_ROW)))
            .child(if self.all_branches {
                "All branches"
            } else {
                "Current branch"
            })
            .on_click(cx.listener(|this, _, _, cx| {
                this.all_branches = !this.all_branches;
                this.reload(cx);
            }));
        let list: gpui::AnyElement = match &self.error {
            Some(error) => div()
                .p_3()
                .text_sm()
                .text_color(rgb(theme::ERROR))
                .child(error.clone())
                .into_any_element(),
            None if !self.loading && self.commits.is_empty() => div()
                .p_3()
                .text_sm()
                .text_color(rgb(theme::MUTED))
                .child("No commits yet.")
                .into_any_element(),
            None => uniform_list(
                "commits",
                self.commits.len(),
                cx.processor(|this, range: Range<usize>, _, cx| {
                    range.map(|index| this.render_commit(index, cx)).collect()
                }),
            )
            .track_scroll(self.scroll.clone())
            .flex_1()
            .into_any_element(),
        };
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::BACKGROUND))
            .text_color(rgb(theme::TEXT))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(rgb(theme::BORDER))
                    .text_sm()
                    .child(toggle)
                    .child(
                        div()
                            .id("history-refresh")
                            .px_2()
                            .cursor_pointer()
                            .text_color(rgb(theme::MUTED))
                            .hover(|style| style.text_color(rgb(theme::TEXT)))
                            .child("⟳")
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    )
                    .child(div().flex_1())
                    .child(div().text_xs().text_color(rgb(theme::MUTED)).child(status)),
            )
            .child(div().flex_1().min_h_0().flex().flex_col().child(list))
            .children(self.render_details(cx))
    }
}
