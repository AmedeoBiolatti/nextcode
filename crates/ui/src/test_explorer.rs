//! The Tests panel (`PanelId::Tests`): pytest and cargo tests found where
//! the project lives (under a node per folder in a multi-root workspace),
//! as a tree with status icons and counts, a filter,
//! run/debug per node, and a details area with the selected test's
//! failure (or the last run's output). Discovery and runs happen on
//! background threads through [`engine`].

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla, SharedString, Subscription,
    Task, UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};
use run::{NodeKind, Status, TestResult, TestTree, Tool};

use crate::{
    editor::{Editor, EditorEvent},
    pinpoint::Pin,
    project::Project,
    selectable::selectable,
    settings, theme,
};

pub mod engine;

const ROW_HEIGHT: f32 = 24.;
const INDENT: f32 = 14.;
/// Output lines shown in the details area.
const MAX_OUTPUT_LINES: usize = 1500;
/// Saving several test files in a row rediscovers once.
const DISCOVER_DEBOUNCE: Duration = Duration::from_millis(1500);

pub enum TestsPanelEvent {
    /// Open `path` at a 0-based line.
    Jump { path: PathBuf, line: usize },
    /// Debug a node (the workspace builds the request and starts it).
    Debug(Box<run::Node>, Arc<HashMap<String, String>>),
    /// Discovery finished or results changed (gutter markers follow).
    Changed,
}

impl EventEmitter<TestsPanelEvent> for TestsPanel {}

/// What to run once discovery is done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunTarget {
    All,
    Failed,
    /// Node ids (pytest node ids or paths, cargo test ids).
    Ids(Vec<String>),
    /// Every pytest test.
    Pytest,
}

pub struct TestsPanel {
    project: Project,
    focus_handle: FocusHandle,
    filter: Entity<Editor>,
    tree: Option<Arc<TestTree>>,
    package_dirs: Arc<HashMap<String, String>>,
    /// Each folder's own tree with several folders (see [`engine`]).
    folders: Arc<Vec<TestTree>>,
    errors: Vec<SharedString>,
    results: HashMap<String, TestResult>,
    running: HashSet<String>,
    collapsed: HashSet<String>,
    selected: Option<String>,
    /// One line above the tree: what is happening or the last outcome.
    status: Option<SharedString>,
    /// Output of the last run, shown when nothing with details is selected.
    last_output: Option<SharedString>,
    discovering: bool,
    /// Runs as soon as discovery ends.
    pending: Option<RunTarget>,
    scroll: UniformListScrollHandle,
    _discover: Option<Task<()>>,
    _debounce: Option<Task<()>>,
    _run: Option<Task<()>>,
    _subscription: Subscription,
}

/// A visible row.
#[derive(Clone)]
struct Row {
    index: usize,
    depth: usize,
    label: SharedString,
    status: Status,
    counts: Option<SharedString>,
    chevron: Option<&'static str>,
    selected: bool,
}

impl TestsPanel {
    pub fn new(project: Project, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(Editor::single_line);
        let subscription = cx.subscribe(&filter, |_, _, event, cx| {
            if let EditorEvent::Edited = event {
                cx.notify();
            }
        });
        Self {
            project,
            focus_handle: cx.focus_handle(),
            filter,
            tree: None,
            package_dirs: Arc::default(),
            folders: Arc::default(),
            errors: Vec::new(),
            results: HashMap::new(),
            running: HashSet::new(),
            collapsed: HashSet::new(),
            selected: None,
            status: None,
            last_output: None,
            discovering: false,
            pending: None,
            scroll: UniformListScrollHandle::new(),
            _discover: None,
            _debounce: None,
            _run: None,
            _subscription: subscription,
        }
    }

    pub fn tree(&self) -> Option<&TestTree> {
        self.tree.as_deref()
    }

    pub fn results(&self) -> &HashMap<String, TestResult> {
        &self.results
    }

    pub fn is_busy(&self) -> bool {
        self.discovering || self._run.is_some()
    }

    /// The line above the tree: what runs, or the last outcome.
    pub(crate) fn status_text(&self) -> Option<&str> {
        self.status.as_ref().map(|status| status.as_ref())
    }

    /// Finds tests again (keeping results of tests that still exist).
    pub fn discover(&mut self, cx: &mut Context<Self>) {
        self._debounce = None;
        self.discovering = true;
        self.status = Some("Discovering tests…".into());
        cx.notify();
        let project = self.project.clone();
        let task = cx.background_spawn(async move { engine::discover(&project) });
        self._discover = Some(cx.spawn(async move |this, cx| {
            let discovery = task.await;
            this.update(cx, |this, cx| this.discovered(discovery, cx))
                .ok();
        }));
    }

    fn discovered(&mut self, discovery: engine::Discovery, cx: &mut Context<Self>) {
        self.discovering = false;
        self._discover = None;
        let tree = discovery.tree;
        // Results of tests that are gone go too; collection errors are new.
        self.results.retain(|id, result| {
            tree.get(id)
                .is_some_and(|index| tree.nodes[index].kind == NodeKind::Test)
                && result.status != Status::Running
        });
        self.results.extend(discovery.results);
        // Parametrized cases start collapsed.
        if self.tree.is_none() {
            self.collapsed.extend(
                tree.nodes
                    .iter()
                    .filter(|node| node.kind == NodeKind::Function)
                    .map(|node| node.id.clone()),
            );
        }
        let count = tree
            .nodes
            .iter()
            .filter(|node| node.kind == NodeKind::Test)
            .count();
        self.status = Some(format!("{count} tests").into());
        self.errors = discovery.errors.into_iter().map(Into::into).collect();
        self.package_dirs = Arc::new(discovery.package_dirs);
        self.folders = Arc::new(discovery.folders);
        self.tree = Some(Arc::new(tree));
        cx.emit(TestsPanelEvent::Changed);
        cx.notify();
        if let Some(target) = self.pending.take() {
            self.run(target, cx);
        }
    }

    /// Rediscovers a little later (after saving a test file).
    pub fn schedule_discover(&mut self, cx: &mut Context<Self>) {
        if !settings::current().tests.discover_on_save || self.tree.is_none() {
            return;
        }
        self._debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DISCOVER_DEBOUNCE).await;
            this.update(cx, |this, cx| {
                if this.is_busy() {
                    this.schedule_discover(cx);
                } else {
                    this.discover(cx);
                }
            })
            .ok();
        }));
    }

    /// Runs tests, discovering them first if needed.
    pub fn run(&mut self, target: RunTarget, cx: &mut Context<Self>) {
        if self._run.is_some() {
            self.status = Some("A test run is already in progress".into());
            return cx.notify();
        }
        let Some(tree) = self.tree.clone().filter(|_| !self.discovering) else {
            self.pending = Some(target);
            if !self.discovering {
                self.discover(cx);
            }
            return;
        };
        let nodes: Vec<usize> = match &target {
            RunTarget::All => tree.roots.clone(),
            RunTarget::Pytest => tree
                .nodes
                .iter()
                .enumerate()
                .filter(|(_, node)| node.kind == NodeKind::Root && node.tool == Tool::Pytest)
                .map(|(index, _)| index)
                .collect(),
            RunTarget::Failed => tree
                .nodes
                .iter()
                .enumerate()
                .filter(|(_, node)| {
                    node.kind == NodeKind::Test
                        && self.results.get(&node.id).is_some_and(|result| {
                            matches!(result.status, Status::Failed | Status::Error)
                        })
                })
                .map(|(index, _)| index)
                .collect(),
            RunTarget::Ids(ids) => {
                let found: Vec<usize> = ids.iter().filter_map(|id| tree.get(id)).collect();
                if found.len() < ids.len() {
                    let missing: Vec<&str> = ids
                        .iter()
                        .filter(|id| tree.get(id).is_none())
                        .map(String::as_str)
                        .collect();
                    self.status =
                        Some(format!("Not a discovered test: {}", missing.join(", ")).into());
                    cx.notify();
                }
                found
            }
        };
        if nodes.is_empty() {
            if target == RunTarget::Failed {
                self.status = Some("No failed tests".into());
            } else if self.status.is_none() {
                self.status = Some("No tests to run".into());
            }
            return cx.notify();
        }
        let folders = self.folders.clone();
        self.running = engine::planned_leaves(&self.project, &tree, &folders, &nodes);
        // The failed file nodes being rerun lose their old error.
        for &index in &nodes {
            self.results.remove(&tree.nodes[index].id);
        }
        self.status = Some(format!("Running {} tests…", self.running.len()).into());
        cx.emit(TestsPanelEvent::Changed);
        cx.notify();
        let project = self.project.clone();
        let task = cx
            .background_spawn(async move { engine::run_nodes(&project, &tree, &folders, &nodes) });
        self._run = Some(cx.spawn(async move |this, cx| {
            let report = task.await;
            this.update(cx, |this, cx| this.finished(report, cx)).ok();
        }));
    }

    fn finished(&mut self, report: engine::RunReport, cx: &mut Context<Self>) {
        self._run = None;
        self.running.clear();
        let (mut passed, mut failed, mut skipped) = (0, 0, 0);
        for result in report.results.values() {
            match result.status {
                Status::Passed => passed += 1,
                Status::Failed | Status::Error => failed += 1,
                Status::Skipped => skipped += 1,
                _ => {}
            }
        }
        self.results.extend(report.results);
        let mut status = format!("{passed} passed, {failed} failed");
        if skipped > 0 {
            status.push_str(&format!(", {skipped} skipped"));
        }
        self.status = Some(status.into());
        self.errors = report.errors.into_iter().map(Into::into).collect();
        self.last_output = Some(report.output.into());
        cx.emit(TestsPanelEvent::Changed);
        cx.notify();
    }

    /// Where a node is defined.
    fn location(&self, index: usize) -> Option<(PathBuf, usize)> {
        let node = &self.tree.as_ref()?.nodes[index];
        Some((
            self.project.path(node.path.as_deref()?),
            node.line.unwrap_or(0),
        ))
    }

    fn click(&mut self, index: usize, count: usize, cx: &mut Context<Self>) {
        let Some(tree) = self.tree.clone() else {
            return;
        };
        let node = &tree.nodes[index];
        self.selected = Some(node.id.clone());
        if count >= 2 {
            self.run(RunTarget::Ids(vec![node.id.clone()]), cx);
            return;
        }
        if let Some((path, line)) = self.location(index) {
            cx.emit(TestsPanelEvent::Jump { path, line });
        }
        cx.notify();
    }

    fn toggle(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(tree) = &self.tree else {
            return;
        };
        let id = tree.nodes[index].id.clone();
        if !self.collapsed.remove(&id) {
            self.collapsed.insert(id);
        }
        cx.notify();
    }

    fn debug(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(tree) = &self.tree {
            self.selected = Some(tree.nodes[index].id.clone());
            cx.emit(TestsPanelEvent::Debug(
                Box::new(tree.nodes[index].clone()),
                self.package_dirs.clone(),
            ));
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &App) {
        window.focus(&self.filter.focus_handle(cx));
    }

    fn rows(&self, cx: &App) -> Vec<Row> {
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        let summaries = tree.summarize(&self.results, &self.running);
        let filter = self.filter.read(cx).text();
        let shown = tree.filtered(&filter);
        let mut rows = Vec::new();
        let mut stack: Vec<usize> = tree.roots.iter().rev().copied().collect();
        while let Some(index) = stack.pop() {
            if shown.as_ref().is_some_and(|shown| !shown.contains(&index)) {
                continue;
            }
            let node = &tree.nodes[index];
            let collapsed = self.collapsed.contains(&node.id) && shown.is_none();
            let summary = summaries[index];
            rows.push(Row {
                index,
                depth: node.depth,
                label: match (node.kind, node.tool) {
                    (NodeKind::Root, Tool::Pytest) => "pytest".into(),
                    (NodeKind::Root, Tool::Cargo) => "cargo test".into(),
                    _ => node.label.clone().into(),
                },
                status: summary.status,
                counts: (node.kind != NodeKind::Test && summary.total > 0).then(|| {
                    if summary.failed > 0 {
                        format!("{} ✗  {}/{}", summary.failed, summary.passed, summary.total)
                    } else {
                        format!("{}/{}", summary.passed, summary.total)
                    }
                    .into()
                }),
                chevron: (!node.children.is_empty()).then_some(if collapsed {
                    "▸"
                } else {
                    "▾"
                }),
                selected: self.selected.as_deref() == Some(node.id.as_str()),
            });
            if !collapsed {
                stack.extend(node.children.iter().rev());
            }
        }
        rows
    }

    fn render_row(&self, row: Row, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let index = row.index;
        let (icon, color) = status_icon(row.status);
        let key = self
            .tree
            .as_ref()
            .and_then(|tree| tree.nodes.get(index))
            .map_or_else(|| index.to_string(), |node| node.id.clone());
        let label = row.label.clone();
        let button = |id: &'static str, label: &'static str, color: Hsla| {
            div()
                .id((id, index))
                .px_1()
                .flex_none()
                .text_xs()
                .text_color(color)
                .cursor_pointer()
                .invisible()
                .group_hover("test-row", |style| style.visible())
                .hover(|style| style.bg(theme::active_row()))
                .child(label)
        };
        div()
            .id(("test-row", index))
            .group("test-row")
            .h(px(ROW_HEIGHT))
            .w_full()
            .pl(px(4. + INDENT * row.depth as f32))
            .pr_1()
            .flex()
            .items_center()
            .gap_1()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_sm()
            .cursor_pointer()
            .when(row.selected, |el| el.bg(theme::active_row()))
            .when(!row.selected, |el| {
                el.hover(|style| style.bg(theme::hover_row()))
            })
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                this.click(index, event.click_count(), cx)
            }))
            .child({
                let chevron = div()
                    .id(("test-chevron", index))
                    .w(px(12.))
                    .flex_none()
                    .text_xs()
                    .text_color(theme::muted())
                    .children(row.chevron);
                match row.chevron {
                    Some(_) => chevron
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle(index, cx);
                        }))
                        .pin("toggle")
                        .label("Expand/Collapse")
                        .into_any_element(),
                    None => chevron.into_any_element(),
                }
            })
            .child(div().w(px(14.)).flex_none().text_color(color).child(icon))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(selectable("label", row.label).double_click_through()),
            )
            .children(row.counts.map(|counts| {
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(selectable("counts", counts).double_click_through())
            }))
            .child(
                button("test-run", "▶", theme::run_marker())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        if let Some(tree) = &this.tree {
                            let id = tree.nodes[index].id.clone();
                            this.selected = Some(id.clone());
                            this.run(RunTarget::Ids(vec![id]), cx);
                        }
                    }))
                    .pin("run")
                    .label("Run"),
            )
            .child(
                button("test-debug", "Debug", theme::accent_text())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.debug(index, cx);
                    }))
                    .pin("debug")
                    .label("Debug"),
            )
            .pin(format!("row:{key}"))
            .label(label)
            .interactive(true)
    }

    /// The selected node's result, else the last run's output.
    fn details(&self) -> Option<(SharedString, Option<SharedString>, SharedString)> {
        let tree = self.tree.as_ref()?;
        if let Some(id) = &self.selected
            && let Some(result) = self.results.get(id)
            && let Some(index) = tree.get(id)
        {
            let node = &tree.nodes[index];
            let mut title = format!("{} — {}", node.label, status_word(result.status));
            if let Some(duration) = result.duration {
                title.push_str(&format!(" ({duration:.2} s)"));
            }
            return Some((
                title.into(),
                result.message.clone().map(Into::into),
                result.output.clone().unwrap_or_default().into(),
            ));
        }
        let output = self.last_output.clone()?;
        Some(("Last run output".into(), None, output))
    }

    fn toolbar_button(
        id: &'static str,
        label: &'static str,
        enabled: bool,
        cx: &mut Context<Self>,
        action: fn(&mut Self, &mut Context<Self>),
    ) -> impl IntoElement + use<> {
        div()
            .id(id)
            .px_2()
            .rounded_sm()
            .text_xs()
            .border_1()
            .border_color(theme::border())
            .when(enabled, |button| {
                button
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::hover_row()))
                    .on_click(cx.listener(move |this, _, _, cx| action(this, cx)))
            })
            .when(!enabled, |button| button.text_color(theme::muted()))
            .child(label)
    }
}

fn status_icon(status: Status) -> (&'static str, Hsla) {
    match status {
        Status::NotRun => ("○", theme::muted()),
        Status::Running => ("◌", theme::accent_text()),
        Status::Passed => ("✓", theme::run_marker()),
        Status::Failed => ("✗", theme::error()),
        Status::Error => ("!", theme::error()),
        Status::Skipped => ("⊘", theme::warning()),
    }
}

fn status_word(status: Status) -> &'static str {
    match status {
        Status::NotRun => "not run",
        Status::Running => "running",
        Status::Passed => "passed",
        Status::Failed => "failed",
        Status::Error => "error",
        Status::Skipped => "skipped",
    }
}

impl Focusable for TestsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TestsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Opened for the first time: find the tests.
        if self.tree.is_none() && !self.discovering && self._discover.is_none() {
            cx.defer_in(window, |this, _, cx| {
                if this.tree.is_none() && !this.discovering {
                    this.discover(cx);
                }
            });
        }
        let busy = self.is_busy();
        let rows = self.rows(cx);
        let row_count = rows.len();
        let details = self.details();
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_none()
                    .px_2()
                    .py_1()
                    .flex()
                    .items_center()
                    .gap_1()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(
                        Self::toolbar_button(
                            "tests-run-all",
                            "▶ Run All",
                            !busy,
                            cx,
                            |this, cx| this.run(RunTarget::All, cx),
                        )
                        .pin("run-all")
                        .label("Run All"),
                    )
                    .child(
                        Self::toolbar_button(
                            "tests-run-failed",
                            "Run Failed",
                            !busy,
                            cx,
                            |this, cx| this.run(RunTarget::Failed, cx),
                        )
                        .pin("run-failed")
                        .label("Run Failed"),
                    )
                    .child(
                        Self::toolbar_button("tests-discover", "⟳", !busy, cx, |this, cx| {
                            this.discover(cx)
                        })
                        .pin("discover")
                        .label("Discover Tests"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(theme::muted())
                            .children(
                                self.status
                                    .clone()
                                    .map(|status| selectable("status", status)),
                            ),
                    )
                    .pin("toolbar"),
            )
            .child(
                div()
                    .flex_none()
                    .h(px(30.))
                    .mx_2()
                    .my_1()
                    .px_2()
                    .py_1()
                    .border_1()
                    .border_color(theme::border())
                    .rounded_sm()
                    .child(self.filter.clone())
                    .pin("filter")
                    .label("Filter Tests"),
            )
            .children(self.errors.iter().enumerate().map(|(index, error)| {
                div()
                    .flex_none()
                    .px_2()
                    .text_xs()
                    .text_color(theme::error())
                    .max_h(px(120.))
                    .overflow_hidden()
                    .child(selectable(("error", index), error.clone()))
            }))
            .child(
                uniform_list(
                    "tests",
                    row_count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .filter_map(|index| rows.get(index).cloned())
                            .map(|row| this.render_row(row, cx).into_any_element())
                            .collect()
                    }),
                )
                .track_scroll(self.scroll.clone())
                .flex_1()
                .min_h_0()
                .pin("tree"),
            )
            .children(details.map(|(title, message, output)| {
                div()
                    .id("test-details")
                    .flex_none()
                    .h(px(200.))
                    .border_t_1()
                    .border_color(theme::border())
                    .p_2()
                    .overflow_y_scroll()
                    .text_xs()
                    .child(div().text_sm().child(selectable("title", title)))
                    .children(message.map(|message| {
                        div()
                            .py_1()
                            .text_color(theme::error())
                            .child(selectable("message", message))
                    }))
                    .child(
                        div()
                            .font_family(theme::MONO_FONT)
                            .text_color(theme::text())
                            .whitespace_normal()
                            // The end of long outputs (where failures are),
                            // one block so a selection can span lines.
                            .child(selectable(
                                "output",
                                output
                                    .lines()
                                    .rev()
                                    .take(MAX_OUTPUT_LINES)
                                    .collect::<Vec<_>>()
                                    .into_iter()
                                    .rev()
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            )),
                    )
                    .pin("details")
            }))
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;

    fn discovery() -> engine::Discovery {
        let mut tree = TestTree::default();
        let ids: Vec<String> = [
            "tests/test_a.py::test_pass",
            "tests/test_a.py::test_fail",
            "tests/test_a.py::test_double[1-2]",
            "tests/test_a.py::test_double[2-4]",
            "tests/test_a.py::TestGroup::test_inside",
        ]
        .map(str::to_owned)
        .into();
        tree.add_pytest(&ids, &["tests/test_bad.py".into()], |_, _, function| {
            Some(function.len())
        });
        let mut results = HashMap::new();
        results.insert(
            "tests/test_bad.py".to_owned(),
            TestResult {
                status: Status::Error,
                message: Some("collection failed".into()),
                output: Some("ImportError".into()),
                duration: None,
            },
        );
        engine::Discovery {
            tree,
            results,
            errors: Vec::new(),
            package_dirs: HashMap::new(),
            folders: Vec::new(),
        }
    }

    fn result(status: Status, message: Option<&str>) -> TestResult {
        TestResult {
            status,
            message: message.map(str::to_owned),
            output: message.map(|message| format!("Traceback\n{message}")),
            duration: Some(0.5),
        }
    }

    /// Rows as `depth icon label counts`.
    fn rows(panel: &TestsPanel, cx: &App) -> Vec<String> {
        panel
            .rows(cx)
            .into_iter()
            .map(|row| {
                format!(
                    "{}{} {}{}",
                    "  ".repeat(row.depth),
                    status_icon(row.status).0,
                    row.label,
                    row.counts
                        .map(|counts| format!(" [{counts}]"))
                        .unwrap_or_default()
                )
            })
            .collect()
    }

    #[gpui::test]
    fn results_show_as_icons_counts_and_details(cx: &mut TestAppContext) {
        let root = std::env::temp_dir();
        let panel = cx.new(|cx| TestsPanel::new(Project::local(root), cx));
        panel.update(cx, |panel, cx| {
            panel.discovered(discovery(), cx);
            // Parametrized cases start collapsed.
            assert_eq!(
                rows(panel, cx),
                [
                    "✗ pytest [1 ✗  0/5]",
                    "  ✗ tests [1 ✗  0/5]",
                    "    ○ test_a.py [0/5]",
                    "      ○ test_pass",
                    "      ○ test_fail",
                    "      ○ test_double [0/2]",
                    "      ○ TestGroup [0/1]",
                    "        ○ test_inside",
                    "    ! test_bad.py",
                ]
            );
            let mut report = engine::RunReport::default();
            for (id, status, message) in [
                ("tests/test_a.py::test_pass", Status::Passed, None),
                (
                    "tests/test_a.py::test_fail",
                    Status::Failed,
                    Some("assert 2 == 3"),
                ),
                ("tests/test_a.py::test_double[1-2]", Status::Passed, None),
                ("tests/test_a.py::test_double[2-4]", Status::Skipped, None),
                (
                    "tests/test_a.py::TestGroup::test_inside",
                    Status::Passed,
                    None,
                ),
            ] {
                report.results.insert(id.into(), result(status, message));
            }
            report.output = "$ uv run pytest\n4 passed".into();
            panel.finished(report, cx);
            assert_eq!(
                panel.status.as_ref().map(|status| status.as_ref()),
                Some("3 passed, 1 failed, 1 skipped")
            );
            assert_eq!(
                rows(panel, cx)[..6],
                [
                    "✗ pytest [2 ✗  3/5]",
                    "  ✗ tests [2 ✗  3/5]",
                    "    ✗ test_a.py [1 ✗  3/5]",
                    "      ✓ test_pass",
                    "      ✗ test_fail",
                    "      ✓ test_double [1/2]",
                ]
            );
            // Nothing selected: the run's output.
            assert_eq!(panel.details().unwrap().0.as_ref(), "Last run output");
            let fail = panel
                .tree()
                .unwrap()
                .get("tests/test_a.py::test_fail")
                .unwrap();
            panel.click(fail, 1, cx);
            let (title, message, output) = panel.details().unwrap();
            assert_eq!(title.as_ref(), "test_fail — failed (0.50 s)");
            assert_eq!(
                message.as_ref().map(|message| message.as_ref()),
                Some("assert 2 == 3")
            );
            assert_eq!(output.as_ref(), "Traceback\nassert 2 == 3");

            // Expanding the parametrized function; filtering.
            let group = panel
                .tree()
                .unwrap()
                .get("tests/test_a.py::test_double")
                .unwrap();
            panel.toggle(group, cx);
            assert!(rows(panel, cx).contains(&"        ⊘ [2-4]".to_owned()));
        });
        panel.update(cx, |panel, cx| {
            panel
                .filter
                .update(cx, |filter, cx| filter.set_text("inside", cx));
        });
        panel.read_with(cx, |panel, cx| {
            assert_eq!(
                rows(panel, cx),
                [
                    "✗ pytest [2 ✗  3/5]",
                    "  ✗ tests [2 ✗  3/5]",
                    "    ✗ test_a.py [1 ✗  3/5]",
                    "      ✓ TestGroup [1/1]",
                    "        ✓ test_inside",
                ]
            );
        });
    }

    #[gpui::test]
    fn rediscovery_keeps_results_of_tests_that_still_exist(cx: &mut TestAppContext) {
        let panel = cx.new(|cx| TestsPanel::new(Project::local(std::env::temp_dir()), cx));
        panel.update(cx, |panel, cx| {
            panel.discovered(discovery(), cx);
            let mut report = engine::RunReport::default();
            report.results.insert(
                "tests/test_a.py::test_pass".into(),
                result(Status::Passed, None),
            );
            report.results.insert(
                "tests/test_gone.py::test_x".into(),
                result(Status::Passed, None),
            );
            panel.finished(report, cx);
            panel.discovered(discovery(), cx);
            assert!(panel.results.contains_key("tests/test_a.py::test_pass"));
            assert!(!panel.results.contains_key("tests/test_gone.py::test_x"));
            assert_eq!(panel.results["tests/test_bad.py"].status, Status::Error);
        });
    }
}
