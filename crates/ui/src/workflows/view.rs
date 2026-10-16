//! A workflow's tab: the file in an editor on the left, its steps as a
//! graph on the right (Azure DevOps style: columns by dependency depth,
//! cards with state, solid edges from each need to the steps needing it,
//! dashed ones from each step to those running `after` it), and a
//! toolbar with the workflow's state, Run All, Stop All and Retry Failed.
//!
//! The graph follows the buffer, 200 ms after the last edit: a file that
//! does not parse shows its problems in a banner (with line and column;
//! a click goes there) over the last good graph, dimmed. Clicking a card
//! selects it, puts the editor's cursor on its `"id"` and shows its
//! details below the graph (state, wait or failure reason, last output
//! lines, artifact links, Terminal / Continue / Restart or Retry); with a
//! terminal of this machine under them they become a pane of their own,
//! resized by a divider. The card holding the editor's cursor is
//! highlighted. Run All saves the file first.

use std::{
    cell::Cell,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App, Bounds, Context, CursorStyle, Entity, FocusHandle, Focusable, Hsla,
    MouseButton, MouseDownEvent, MouseMoveEvent, PathBuilder, Pixels, Point, ScrollDelta,
    ScrollWheelEvent, SharedString, Subscription, Task, Window, canvas, div, point, prelude::*, px,
    relative,
};
use text::Buffer;
use workflow::{
    ArtifactTarget, GraphLayout, Metrics, Outcome, Ready, Status, Step, StepSpan, StepState,
    View as Placement, Workflow,
};

use super::{
    ActiveRun, RetryFailed, RunAll, Source, StopAll, Workflows, file_stem, location_of,
    outcome_color, outcome_text, workflow_id,
};
use crate::{
    editor::{Editor, EditorEvent},
    project::Project,
    selectable::selectable,
    shell::tab_title,
    theme,
};

pub(super) const CONTEXT: &str = "WorkflowView";
/// Card size and spacing of the graph.
const METRICS: Metrics = Metrics {
    card_width: 200.,
    card_height: 76.,
    column_gap: 64.,
    row_gap: 20.,
    edge_gap: 14.,
    margin: 20.,
};
/// An edge's arrowhead (length, half its width) and the straight run out
/// of the step it leaves, in layout pixels.
const ARROW_LENGTH: f32 = 8.;
const ARROW_HALF_WIDTH: f32 = 4.5;
const EDGE_LEAD: f32 = 8.;
/// The graph follows edits after this pause.
const REPARSE_DELAY: Duration = Duration::from_millis(200);
/// Output lines in the selected step's details.
const DETAIL_LINES: usize = 4;
const HANDLE_WIDTH: f32 = 5.;
/// Narrowest share of the width for either side of the split.
const MIN_SPLIT: f32 = 0.2;
/// While a step's terminal shows: the least height of the graph above it
/// and of the step pane (details and terminal), and the pane's share of
/// the height to start with.
const MIN_GRAPH_HEIGHT: f32 = 90.;
const MIN_STEP_HEIGHT: f32 = 240.;
const STEP_SHARE: f32 = 0.45;

/// A divider being dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Divider {
    /// Between the editor and the graph.
    Editor,
    /// Between the graph and the step pane.
    Steps,
}

/// What a problem in the file looks like in the banner.
struct ShownProblem {
    message: String,
    offset: Option<usize>,
    /// 1-based line and column.
    place: Option<(usize, usize)>,
}

/// The graph as last parsed from the buffer.
#[derive(Default)]
struct Graph {
    /// The last valid workflow, and its layout.
    workflow: Option<Arc<Workflow>>,
    layout: Option<GraphLayout>,
    /// Where each step is in the current text.
    spans: Vec<StepSpan>,
    /// Problems of the current text (the graph is the last good one).
    problems: Vec<ShownProblem>,
}

pub struct WorkflowView {
    workflows: Entity<Workflows>,
    /// Runs are keyed by it (see [`workflow_id`]).
    id: String,
    path: PathBuf,
    editor: Entity<Editor>,
    graph: Graph,
    /// The selected step, by id.
    selected: Option<String>,
    /// The step whose text holds the editor's cursor.
    cursor_step: Option<String>,
    last_cursor: Option<usize>,
    /// The editor's share of the width.
    split: f32,
    /// While a terminal shows under the selected step's details: the step
    /// pane's share of the graph side's height, and whether it is
    /// maximized (the graph at its least height; a double-click on the
    /// divider toggles it). Kept for as long as the tab is open.
    step_share: f32,
    step_maximized: bool,
    resizing: Option<Divider>,
    split_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// The graph side: the graph, and the step pane under it.
    side_bounds: Rc<Cell<Bounds<Pixels>>>,
    graph_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// Where the graph is drawn and at what zoom when the user zoomed or
    /// panned; until then (`auto_fit`) it is fitted to its area.
    placement: Placement,
    auto_fit: bool,
    /// Where the mouse was while the graph is dragged.
    panning: Option<Point<Pixels>>,
    /// A step without a project whose terminal shows under the details.
    host_terminal: Option<String>,
    notice: Option<SharedString>,
    _reparse: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// `12s`, `3m 05s`, `1h 02m`.
fn elapsed_text(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}

/// The last component of a path (`/` or `\` separated).
fn base_name(path: &str) -> &str {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
}

fn status_look(status: &Status) -> (&'static str, Hsla) {
    match status {
        Status::Pending => ("○", theme::muted()),
        Status::Waiting => ("◦", theme::muted()),
        Status::Starting => ("◌", theme::warning()),
        Status::Running => ("▶", theme::accent()),
        Status::Stopping => ("□", theme::warning()),
        Status::Ready => ("✓", theme::git_added()),
        Status::Failed(_) => ("✗", theme::error()),
        Status::Stopped => ("■", theme::muted()),
    }
}

/// What a step does, in a few words: `check · run · port 8000 · after`
/// (`after`: it runs once the steps on its dashed edges finished).
fn kind_hint(step: &Step) -> String {
    let mut parts: Vec<String> = Vec::new();
    if step.check.is_some() {
        parts.push("check".into());
    }
    if step.run.is_some() {
        parts.push("run".into());
    }
    match &step.ready {
        Ready::Exit => {}
        Ready::Output(_) => parts.push("output".into()),
        Ready::Port(port) => parts.push(format!("port {port}")),
        Ready::Manual(_) => parts.push("manual".into()),
        Ready::Delay(ms) => parts.push(format!("delay {ms} ms")),
    }
    if !step.after.is_empty() {
        parts.push("after".into());
    }
    parts.join(" · ")
}

/// Where a step runs: its project tab's name, or this machine.
fn place(step: &Step, active: Option<&ActiveRun>, run_index: Option<usize>) -> String {
    if let Some(location) = active.zip(run_index).and_then(|(run, at)| run.location(at)) {
        return tab_title(location);
    }
    match &step.project {
        Some(spec) => location_of(spec).map_or_else(
            |err| format!("invalid project: {err}"),
            |location| tab_title(&location),
        ),
        None => "this machine".into(),
    }
}

/// The run's state of the step with id `id` (the run may predate edits).
fn state_of<'a>(active: Option<&'a ActiveRun>, id: &str) -> Option<(usize, &'a StepState)> {
    let active = active?;
    let index = active.run.step(id)?;
    Some((index, &active.run.steps()[index]))
}

/// A piece of an edge's stroke: straight, or a cubic curve to its first
/// point with handles at the other two.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Piece<P> {
    Line(P),
    Curve(P, P, P),
}

impl<P> Piece<P> {
    fn map<Q>(self, f: impl Fn(P) -> Q) -> Piece<Q> {
        match self {
            Self::Line(to) => Piece::Line(f(to)),
            Self::Curve(to, a, b) => Piece::Curve(f(to), f(a), f(b)),
        }
    }
}

/// An edge as stroked: where it starts, its pieces, and its arrowhead's
/// base and tip.
#[derive(Debug, Clone, PartialEq)]
struct Course<P> {
    start: P,
    pieces: Vec<Piece<P>>,
    base: P,
    tip: P,
}

impl<P: Copy> Course<P> {
    fn map<Q>(&self, f: impl Fn(P) -> Q + Copy) -> Course<Q> {
        Course {
            start: f(self.start),
            pieces: self.pieces.iter().map(|piece| piece.map(f)).collect(),
            base: f(self.base),
            tip: f(self.tip),
        }
    }
}

/// How the edge through layout `points` is stroked: a straight run of
/// [`EDGE_LEAD`] out of its step, curves with horizontal handles between
/// its points (straight where level, through the columns it passes), and
/// a straight run of [`ARROW_LENGTH`] into the base of its arrowhead,
/// whose tip is the last point: the line meets the head square on and
/// stops there.
fn edge_course(points: &[(f32, f32)]) -> Course<(f32, f32)> {
    let (start, tip) = (points[0], points[points.len() - 1]);
    let base = (tip.0 - ARROW_LENGTH, tip.1);
    let mut course = Vec::with_capacity(points.len() + 2);
    course.push((start.0 + EDGE_LEAD, start.1));
    course.extend_from_slice(&points[1..points.len() - 1]);
    course.push((base.0 - ARROW_LENGTH, base.1));
    course.push(base);
    let mut from = start;
    let pieces = course
        .into_iter()
        .map(|to: (f32, f32)| {
            let piece = if to.1 == from.1 || to.0 <= from.0 {
                Piece::Line(to)
            } else {
                let handle = (to.0 - from.0) / 2.;
                Piece::Curve(to, (from.0 + handle, from.1), (to.0 - handle, to.1))
            };
            from = to;
            piece
        })
        .collect();
    Course {
        start,
        pieces,
        base,
        tip,
    }
}

/// The step pane's share of the graph side's `height` with its top
/// `below` the side's bottom, leaving both it and the graph their least
/// heights where the side has room.
fn step_share(below: f32, height: f32) -> f32 {
    let most = (height - HANDLE_WIDTH - MIN_GRAPH_HEIGHT).max(MIN_STEP_HEIGHT);
    (below.clamp(MIN_STEP_HEIGHT, most) / height).min(1.)
}

fn button(id: impl Into<gpui::ElementId>, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .px_2()
        .py_0p5()
        .rounded_sm()
        .border_1()
        .border_color(theme::border())
        .text_xs()
        .text_color(theme::text())
        .cursor_pointer()
        .hover(|style| style.bg(theme::hover_row()))
        .child(label)
}

impl WorkflowView {
    /// A tab for the workflow file `path` (with contents `text`), read and
    /// written through `project`.
    pub fn new(
        workflows: Entity<Workflows>,
        source: Source,
        path: PathBuf,
        project: Project,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor =
            cx.new(|cx| Editor::for_file(Buffer::with_path(text, path.clone()), project, cx));
        let subscriptions = vec![
            cx.subscribe(&editor, |this, _, event, cx| match event {
                EditorEvent::Edited => this.schedule_reparse(cx),
                EditorEvent::Saved => {
                    // The palette's "Run Workflow:" list reads files.
                    this.workflows
                        .update(cx, |workflows, cx| workflows.refresh(cx).detach());
                    cx.notify();
                }
                EditorEvent::DirtyChanged => cx.notify(),
                _ => {}
            }),
            cx.observe(&editor, |this, _, cx| this.cursor_moved(cx)),
            cx.observe(&workflows, |_, _, cx| cx.notify()),
        ];
        window.focus(&editor.focus_handle(cx));
        let mut view = Self {
            workflows,
            id: workflow_id(&source, &path),
            path,
            editor,
            graph: Graph::default(),
            selected: None,
            cursor_step: None,
            last_cursor: None,
            split: 0.38,
            step_share: STEP_SHARE,
            step_maximized: false,
            resizing: None,
            split_bounds: Rc::default(),
            side_bounds: Rc::default(),
            graph_bounds: Rc::default(),
            placement: Placement {
                scale: 1.,
                offset: (0., 0.),
            },
            auto_fit: true,
            panning: None,
            host_terminal: None,
            notice: None,
            _reparse: None,
            _subscriptions: subscriptions,
        };
        view.reparse(cx);
        view
    }

    /// The workflow's name (as last parsed), else the file's.
    pub fn name(&self) -> String {
        match &self.graph.workflow {
            Some(workflow) => workflow.name.clone(),
            None => file_stem(&self.path),
        }
    }

    pub fn is_dirty(&self, cx: &App) -> bool {
        self.editor.read(cx).buffer().is_dirty()
    }

    /// The state of the workflow's latest run.
    pub fn outcome(&self, cx: &App) -> Outcome {
        self.workflows
            .read(cx)
            .run_of(&self.id)
            .map_or(Outcome::Idle, |active| active.run.outcome())
    }

    fn schedule_reparse(&mut self, cx: &mut Context<Self>) {
        self._reparse = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REPARSE_DELAY).await;
            this.update(cx, |this, cx| this.reparse(cx)).ok();
        }));
    }

    /// Rebuilds the graph from the buffer; a file with problems keeps the
    /// last good graph.
    fn reparse(&mut self, cx: &mut Context<Self>) {
        self._reparse = None;
        let text = self.editor.read(cx).text();
        self.graph.spans = workflow::step_spans(&text);
        match workflow::check(&text, &file_stem(&self.path)) {
            Ok(parsed) => {
                self.graph.layout = workflow::layout_workflow(&parsed, METRICS).ok();
                self.graph.workflow = Some(Arc::new(parsed));
                self.graph.problems.clear();
            }
            Err(problems) => {
                self.graph.problems = problems
                    .into_iter()
                    .map(|problem| ShownProblem {
                        place: problem.line_column(&text),
                        message: problem.message,
                        offset: problem.offset,
                    })
                    .collect();
            }
        }
        self.last_cursor = None;
        self.cursor_moved(cx);
        cx.notify();
    }

    /// Highlights the card of the step the editor's cursor is in.
    fn cursor_moved(&mut self, cx: &mut Context<Self>) {
        let cursor = self.editor.read(cx).cursor();
        if self.last_cursor == Some(cursor) {
            return;
        }
        self.last_cursor = Some(cursor);
        let step = workflow::step_at(&self.graph.spans, cursor).and_then(|span| span.id.clone());
        if step != self.cursor_step {
            self.cursor_step = step;
            cx.notify();
        }
    }

    /// Where step `id`'s `"id"` is in the text.
    fn step_offset(&self, id: &str) -> Option<usize> {
        self.graph
            .spans
            .iter()
            .find(|span| span.id.as_deref() == Some(id))
            .map(|span| span.id_offset.unwrap_or(span.start))
    }

    /// Selects step `id` and puts the editor's cursor on its `"id"`.
    pub(crate) fn select(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(id.to_owned());
        if let Some(offset) = self.step_offset(id) {
            self.editor
                .update(cx, |editor, cx| editor.set_cursor(offset, cx));
            window.focus(&self.editor.focus_handle(cx));
        }
        cx.notify();
    }

    /// Puts the editor's cursor on the first problem.
    fn go_to_problem(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(offset) = self
            .graph
            .problems
            .first()
            .and_then(|problem| problem.offset)
        {
            self.editor
                .update(cx, |editor, cx| editor.set_cursor(offset, cx));
            window.focus(&self.editor.focus_handle(cx));
        }
    }

    /// Run All: saves the file if it has edits, then runs it afresh.
    pub fn run_all(&mut self, cx: &mut Context<Self>) {
        self.notice = None;
        let editor = self.editor.clone();
        if editor.read(cx).buffer().is_dirty() && !editor.update(cx, |editor, cx| editor.save(cx)) {
            self.notice = Some("Could not save the file (see the editor's status bar).".into());
            return cx.notify();
        }
        let text = editor.read(cx).text();
        match workflow::parse(&text, &file_stem(&self.path)) {
            Ok(parsed) => {
                let id = self.id.clone();
                self.workflows.update(cx, |workflows, cx| {
                    workflows.start_parsed(id, Arc::new(parsed), cx);
                });
            }
            Err(error) => {
                let first = error.lines().next().unwrap_or_default();
                self.notice = Some(format!("Cannot run: {first}").into());
            }
        }
        self.reparse(cx);
    }

    fn stop_all(&mut self, cx: &mut Context<Self>) {
        let id = self.id.clone();
        self.workflows
            .update(cx, |workflows, cx| workflows.stop(&id, cx));
    }

    fn retry_failed(&mut self, cx: &mut Context<Self>) {
        let id = self.id.clone();
        self.workflows
            .update(cx, |workflows, cx| workflows.retry_failed(&id, cx));
    }

    fn run_all_action(&mut self, _: &RunAll, _: &mut Window, cx: &mut Context<Self>) {
        self.run_all(cx);
    }

    fn stop_all_action(&mut self, _: &StopAll, _: &mut Window, cx: &mut Context<Self>) {
        self.stop_all(cx);
    }

    fn retry_failed_action(&mut self, _: &RetryFailed, _: &mut Window, cx: &mut Context<Self>) {
        self.retry_failed(cx);
    }

    /// The layout's size, and the graph area's (as last laid out).
    fn sizes(&self) -> Option<((f32, f32), (f32, f32))> {
        let layout = self.graph.layout.as_ref()?;
        let area = self.graph_bounds.get().size;
        Some((
            (layout.width, layout.height),
            (area.width.into(), area.height.into()),
        ))
    }

    /// Where the graph shows now: fitted to its area until the user zooms
    /// or pans (and after Fit).
    fn placement(&self) -> Placement {
        match self.sizes() {
            Some((content, area)) if self.auto_fit => Placement::fit(content, area),
            _ => self.placement,
        }
    }

    /// The user moved or zoomed the graph: it stays where they put it.
    fn place(&mut self, placement: Placement, cx: &mut Context<Self>) {
        self.auto_fit = false;
        self.placement = match self.sizes() {
            Some((content, area)) => placement.clamped(content, area),
            None => placement,
        };
        cx.notify();
    }

    /// Zooms by `factor` around `anchor` (graph area coordinates).
    fn zoom_by(&mut self, factor: f32, anchor: (f32, f32), cx: &mut Context<Self>) {
        let current = self.placement();
        self.place(current.zoom_at(current.scale * factor, anchor), cx);
    }

    /// Zooms by `factor` around the middle of the graph area.
    fn zoom_centered(&mut self, factor: f32, cx: &mut Context<Self>) {
        let area = self.graph_bounds.get().size;
        let middle = (f32::from(area.width) / 2., f32::from(area.height) / 2.);
        self.zoom_by(factor, middle, cx);
    }

    fn fit(&mut self, cx: &mut Context<Self>) {
        self.auto_fit = true;
        cx.notify();
    }

    /// Ctrl+scroll zooms around the pointer; scrolling pans (Shift:
    /// sideways).
    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let mut delta = event.delta.pixel_delta(px(20.));
        if event.modifiers.control || event.modifiers.platform {
            let origin = self.graph_bounds.get().origin;
            let anchor = (
                f32::from(event.position.x - origin.x),
                f32::from(event.position.y - origin.y),
            );
            // About 20 % per wheel notch (3 lines on X11); trackpads by
            // distance.
            let factor = match event.delta {
                ScrollDelta::Lines(lines) => 1.06f32.powf(lines.y.clamp(-3., 3.)),
                ScrollDelta::Pixels(_) => (f32::from(delta.y).clamp(-60., 60.) * 0.002).exp(),
            };
            return self.zoom_by(factor, anchor, cx);
        }
        if event.modifiers.shift && delta.x == px(0.) {
            delta = point(delta.y, px(0.));
        }
        let current = self.placement();
        let offset = (
            current.offset.0 + f32::from(delta.x),
            current.offset.1 + f32::from(delta.y),
        );
        self.place(Placement { offset, ..current }, cx);
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        match self.resizing {
            Some(Divider::Editor) => {
                let bounds = self.split_bounds.get();
                let width = f32::from(bounds.size.width);
                if width > 0. {
                    let offset = f32::from(event.position.x - bounds.origin.x);
                    self.split = (offset / width).clamp(MIN_SPLIT, 1. - MIN_SPLIT);
                    cx.notify();
                }
            }
            Some(Divider::Steps) => {
                let bounds = self.side_bounds.get();
                let height = f32::from(bounds.size.height);
                if height > 0. {
                    let below = f32::from(bounds.bottom() - event.position.y) - HANDLE_WIDTH / 2.;
                    self.step_share = step_share(below, height);
                    self.step_maximized = false;
                    cx.notify();
                }
            }
            None => {
                let Some(last) = self.panning else {
                    return;
                };
                if event.pressed_button != Some(MouseButton::Left) {
                    self.panning = None;
                    return;
                }
                let moved = event.position - last;
                self.panning = Some(event.position);
                let current = self.placement();
                let offset = (
                    current.offset.0 + f32::from(moved.x),
                    current.offset.1 + f32::from(moved.y),
                );
                self.place(Placement { offset, ..current }, cx);
            }
        }
    }

    fn on_mouse_up(&mut self, cx: &mut Context<Self>) {
        if self.resizing.is_some() || self.panning.is_some() {
            self.resizing = None;
            self.panning = None;
            cx.notify();
        }
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let workflows = self.workflows.read(cx);
        let active = workflows.run_of(&self.id);
        let outcome = active.map_or(Outcome::Idle, |active| active.run.outcome());
        let running = active.is_some_and(|active| {
            active.run.is_active() || active.run.steps().iter().any(StepState::command_live)
        });
        let any_failed = active.is_some_and(|active| {
            active
                .run
                .steps()
                .iter()
                .any(|step| matches!(step.status, Status::Failed(_)))
        });
        div()
            .flex_none()
            .h(px(34.))
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(theme::border())
            .bg(theme::panel())
            .child(
                div()
                    .flex_none()
                    .text_color(outcome_color(outcome))
                    .child(super::TAB_ICON),
            )
            .child(
                div()
                    .flex_none()
                    .child(selectable("workflow-name", self.name())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(selectable("workflow-path", self.path.display().to_string())),
            )
            .children(self.notice.clone().map(|notice| {
                div()
                    .flex_none()
                    .max_w(px(360.))
                    .truncate()
                    .text_xs()
                    .text_color(theme::error())
                    .child(selectable("workflow-notice", notice))
            }))
            .child(
                div()
                    .id("workflow-outcome")
                    .flex_none()
                    .px_2()
                    .rounded_sm()
                    .text_xs()
                    .text_color(outcome_color(outcome))
                    .border_1()
                    .border_color(outcome_color(outcome))
                    .child(selectable("text", outcome_text(outcome))),
            )
            .child(
                button("workflow-run-all", "Run All")
                    .on_click(cx.listener(|this, _, _, cx| this.run_all(cx))),
            )
            .when(running, |toolbar| {
                toolbar.child(
                    button("workflow-stop-all", "Stop All")
                        .on_click(cx.listener(|this, _, _, cx| this.stop_all(cx))),
                )
            })
            .when(any_failed, |toolbar| {
                toolbar.child(
                    button("workflow-retry-failed", "Retry Failed")
                        .on_click(cx.listener(|this, _, _, cx| this.retry_failed(cx))),
                )
            })
    }

    fn render_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let first = self.graph.problems.first()?;
        let place = first
            .place
            .map(|(line, column)| format!("Line {line}, column {column}: "))
            .unwrap_or_default();
        let more = match self.graph.problems.len() {
            1 => String::new(),
            count => format!("  (+{} more)", count - 1),
        };
        Some(
            div()
                .id("workflow-problems")
                .absolute()
                .top(px(8.))
                .left(px(8.))
                .right(px(8.))
                .px_3()
                .py_1p5()
                .rounded_md()
                .border_1()
                .border_color(theme::error())
                .bg(theme::panel())
                .shadow_md()
                .text_sm()
                .text_color(theme::error())
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, _, window, cx| this.go_to_problem(window, cx)))
                .child(selectable(
                    "text",
                    format!("{place}{}{more}", first.message),
                ))
                .into_any_element(),
        )
    }

    fn render_card(
        &self,
        index: usize,
        step: &Step,
        active: Option<&ActiveRun>,
        placement: Placement,
        now: Instant,
        cx: &Context<Self>,
    ) -> AnyElement {
        let layout = self
            .graph
            .layout
            .as_ref()
            .expect("cards come from a layout");
        let node = layout.nodes[index];
        let state = state_of(active, &step.id);
        let status = state.map_or(Status::Pending, |(_, state)| state.status.clone());
        let (icon, color) = status_look(&status);
        let elapsed = state
            .and_then(|(_, state)| state.elapsed(now))
            .map(elapsed_text)
            .unwrap_or_default();
        let selected = self.selected.as_deref() == Some(step.id.as_str());
        let under_cursor = self.cursor_step.as_deref() == Some(step.id.as_str());
        let id = step.id.clone();
        let mut detail = step.id.clone();
        let kind = kind_hint(step);
        if !kind.is_empty() {
            detail = format!("{detail} · {kind}");
        }
        // While it stops, the countdown says how long it may still take.
        if let Some((_, state)) = state.filter(|(_, state)| state.status == Status::Stopping) {
            detail = state.detail.clone();
        }
        let artifacts = step.artifacts.len();
        // Sizes, text and spacing follow the zoom; hit testing comes with
        // the elements being where they are drawn.
        let scale = placement.scale;
        let size = |base: f32| px(base * scale);
        div()
            .id(("workflow-card", index))
            .absolute()
            .left(px(placement.offset.0 + node.x * scale))
            .top(px(placement.offset.1 + node.y * scale))
            .w(size(METRICS.card_width))
            .h(size(METRICS.card_height))
            .flex()
            .overflow_hidden()
            .rounded(size(6.))
            .bg(if under_cursor {
                theme::active_row()
            } else {
                theme::panel()
            })
            .border_1()
            .border_color(if selected {
                theme::accent()
            } else if status == Status::Pending {
                theme::border()
            } else {
                color
            })
            .when(selected, |card| card.border_2())
            .shadow_sm()
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.select(&id, window, cx);
                }),
            )
            .child(div().flex_none().w(size(4.)).h_full().bg(color))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .px(size(8.))
                    .py(size(4.))
                    .flex()
                    .flex_col()
                    .justify_between()
                    .text_size(size(12.))
                    .line_height(size(17.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(size(6.))
                            .text_size(size(14.))
                            .line_height(size(20.))
                            .child(div().flex_none().text_color(color).child(icon))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(theme::text())
                                    .child(step.label().to_owned()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(size(12.))
                                    .text_color(theme::muted())
                                    .child(elapsed),
                            ),
                    )
                    .child(div().truncate().text_color(theme::muted()).child(detail))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(size(6.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(theme::accent_text())
                                    .child(place(step, active, state.map(|(at, _)| at))),
                            )
                            .when(artifacts > 0, |line| {
                                line.child(
                                    div()
                                        .flex_none()
                                        .px(size(4.))
                                        .rounded(size(3.))
                                        .border_1()
                                        .border_color(theme::border())
                                        .text_size(size(11.))
                                        .line_height(size(14.))
                                        .text_color(theme::muted())
                                        .child(format!("↗ {artifacts}")),
                                )
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_graph(
        &self,
        workflow: &Workflow,
        now: Instant,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(layout) = &self.graph.layout else {
            return div().into_any_element();
        };
        let workflows = self.workflows.read(cx);
        let active = workflows.run_of(&self.id);
        // Needs edges light up once the need is ready; dashed `after`
        // edges once the step finished, ready or failed.
        let statuses: Vec<Status> = workflow
            .steps
            .iter()
            .map(|step| {
                state_of(active, &step.id)
                    .map_or(Status::Pending, |(_, state)| state.status.clone())
            })
            .collect();
        let placement = self.placement();
        let scale = placement.scale;
        let at = |(x, y): (f32, f32)| {
            point(
                px(placement.offset.0 + x * scale),
                px(placement.offset.1 + y * scale),
            )
        };
        // Edges as drawn (graph area pixels), and their look.
        let edges: Vec<(Course<Point<Pixels>>, bool, bool)> = layout
            .edges
            .iter()
            .map(|edge| {
                let lit = match &statuses[edge.from] {
                    Status::Ready => true,
                    Status::Failed(_) => edge.after,
                    _ => false,
                };
                (edge_course(&edge.points).map(at), lit, edge.after)
            })
            .collect();
        let cards: Vec<AnyElement> = workflow
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| self.render_card(index, step, active, placement, now, cx))
            .collect();
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .when(!self.graph.problems.is_empty(), |graph| graph.opacity(0.45))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        let origin = bounds.origin;
                        for (course, lit, after) in &edges {
                            let color = match (*lit, *after) {
                                (true, false) => theme::git_added(),
                                (true, true) => theme::accent(),
                                (false, _) => theme::muted(),
                            };
                            let width = (if *lit { 2. } else { 1.5 }) * scale.max(0.6);
                            let mut path = PathBuilder::stroke(px(width));
                            if *after {
                                path = path.dash_array(&[px(6. * scale), px(4. * scale)]);
                            }
                            path.move_to(course.start + origin);
                            for piece in &course.pieces {
                                match *piece {
                                    Piece::Line(to) => path.line_to(to + origin),
                                    Piece::Curve(to, a, b) => {
                                        path.cubic_bezier_to(to + origin, a + origin, b + origin)
                                    }
                                }
                            }
                            if let Ok(path) = path.build() {
                                window.paint_path(path, color);
                            }
                            let half = px(ARROW_HALF_WIDTH * scale);
                            let (base, tip) = (course.base + origin, course.tip + origin);
                            let mut arrow = PathBuilder::fill();
                            arrow.add_polygon(
                                &[
                                    tip,
                                    point(base.x, base.y - half),
                                    point(base.x, base.y + half),
                                ],
                                true,
                            );
                            if let Ok(arrow) = arrow.build() {
                                window.paint_path(arrow, color);
                            }
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .children(cards)
            .into_any_element()
    }

    /// The selected step's details, and whether its terminal shows under
    /// them: then they fill the step pane, the terminal taking the height
    /// the details leave.
    fn render_details(
        &self,
        workflow: &Workflow,
        now: Instant,
        cx: &mut Context<Self>,
    ) -> Option<(AnyElement, bool)> {
        let id = self.selected.as_deref()?;
        let step = workflow.steps.iter().find(|step| step.id == id)?;
        let workflows = self.workflows.read(cx);
        let active = workflows.run_of(&self.id);
        let state = state_of(active, id);
        let status = state.map_or(Status::Pending, |(_, state)| state.status.clone());
        let (icon, color) = status_look(&status);
        let elapsed = state
            .and_then(|(_, state)| state.elapsed(now))
            .map(elapsed_text)
            .unwrap_or_default();
        let reason = match (&status, state, &step.ready) {
            (Status::Failed(reason), Some((_, state)), _) => match &state.check_output {
                Some(check) => format!("{reason} — {check}"),
                None => reason.clone(),
            },
            (Status::Running, _, Ready::Manual(prompt)) => prompt.clone(),
            (_, Some((_, state)), _) => state.detail.clone(),
            (_, None, _) => "Not started: Run All runs the workflow.".into(),
        };
        let lines: Vec<String> = state
            .map(|(_, state)| {
                state
                    .last_lines(DETAIL_LINES)
                    .into_iter()
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let artifacts = self.render_artifacts(step, state, cx);
        let run_index = state.map(|(at, _)| at);
        let has_terminal = active
            .zip(run_index)
            .is_some_and(|(active, at)| active.terminal(at).is_some());
        let on_host = active
            .zip(run_index)
            .is_some_and(|(active, at)| active.location(at).is_none());
        let waiting_manual = status == Status::Running && matches!(step.ready, Ready::Manual(_));
        let failed = matches!(status, Status::Failed(_));
        let step_id = step.id.clone();
        let actions = div()
            .flex()
            .flex_none()
            .gap_1()
            .when(has_terminal, |actions| {
                let step_id = step_id.clone();
                actions.child(button("step-terminal", "Terminal").on_click(cx.listener(
                    move |this, _, _, cx| {
                        if on_host {
                            this.host_terminal = match this.host_terminal.take() {
                                Some(shown) if shown == step_id => None,
                                _ => Some(step_id.clone()),
                            };
                            cx.notify();
                        } else if let Some(at) = run_index {
                            let id = this.id.clone();
                            this.workflows
                                .update(cx, |workflows, cx| workflows.show_terminal(&id, at, cx));
                        }
                    },
                )))
            })
            .when_some(run_index.filter(|_| waiting_manual), |actions, at| {
                actions.child(button("step-continue", "Continue").on_click(cx.listener(
                    move |this, _, _, cx| {
                        let id = this.id.clone();
                        this.workflows.update(cx, |workflows, cx| {
                            workflows.continue_step(&id, at, cx);
                        });
                    },
                )))
            })
            .when_some(run_index, |actions, at| {
                let label = if failed { "Retry" } else { "Restart" };
                actions.child(button("step-restart", label).on_click(cx.listener(
                    move |this, _, _, cx| {
                        let id = this.id.clone();
                        this.workflows
                            .update(cx, |workflows, cx| workflows.restart_step(&id, at, cx));
                    },
                )))
            })
            .child(
                button("step-details-close", "×").on_click(cx.listener(|this, _, _, cx| {
                    this.selected = None;
                    cx.notify();
                })),
            );
        let terminal = self
            .host_terminal
            .as_deref()
            .filter(|shown| *shown == id)
            .and(active.zip(run_index))
            .and_then(|(active, at)| active.terminal(at).cloned());
        let shows_terminal = terminal.is_some();
        Some((
            div()
                .when(shows_terminal, |details| {
                    details.flex_1().min_h_0().overflow_hidden()
                })
                .when(!shows_terminal, |details| details.flex_none())
                .flex()
                .flex_col()
                .border_t_1()
                .border_color(theme::border())
                .bg(theme::panel())
                .child(
                    div()
                        .flex_none()
                        .px_3()
                        .py_1p5()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(div().flex_none().text_color(color).child(icon))
                                .child(
                                    div()
                                        .flex_none()
                                        .child(selectable("step-label", step.label().to_owned())),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(color)
                                        .child(selectable("step-status", status.name())),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_xs()
                                        .text_color(theme::muted())
                                        .child(selectable(
                                            "step-place",
                                            format!(
                                                "{elapsed}  {}",
                                                place(step, active, run_index)
                                            ),
                                        )),
                                )
                                .child(actions),
                        )
                        .when(!reason.is_empty(), |details| {
                            details.child(
                                div()
                                    .text_xs()
                                    .text_color(if failed {
                                        theme::error()
                                    } else {
                                        theme::text()
                                    })
                                    .child(selectable("step-reason", reason)),
                            )
                        })
                        .when(!lines.is_empty(), |details| {
                            // One block, so a selection can span the lines.
                            details.child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_sm()
                                    .bg(theme::background())
                                    .font_family(theme::MONO_FONT)
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .child(selectable("step-output", lines.join("\n"))),
                            )
                        })
                        .children(artifacts),
                )
                .children(terminal.map(|view| {
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .border_t_1()
                        .border_color(theme::border())
                        .child(div().flex_1().min_w_0().child(view))
                }))
                .into_any_element(),
            shows_terminal,
        ))
    }

    /// The step's artifacts as links: files open in an editor tab of the
    /// step's project, URLs in the browser; a file not found is dimmed
    /// and marked missing. "Check again" looks them up again.
    fn render_artifacts(
        &self,
        step: &Step,
        state: Option<(usize, &StepState)>,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if step.artifacts.is_empty() {
            return None;
        }
        let checked = state.and_then(|(_, state)| state.artifacts.as_ref());
        let error = checked.and_then(|checked| checked.as_ref().err().cloned());
        let checked = checked.and_then(|checked| checked.as_ref().ok());
        let run_index = state.map(|(at, _)| at);
        let checkable = state.is_some_and(|(_, state)| {
            !matches!(
                state.status,
                Status::Pending | Status::Waiting | Status::Starting
            )
        });
        let rows = step.artifacts.iter().enumerate().map(|(index, artifact)| {
            let found = checked.and_then(|checked| checked.get(index));
            let url = matches!(artifact.target, ArtifactTarget::Url(_));
            let name = match (&artifact.label, found) {
                (Some(label), _) => label.clone(),
                (None, Some(found)) if !url => base_name(&found.resolved).to_owned(),
                (None, Some(found)) => found.resolved.clone(),
                (None, None) => artifact.name().to_owned(),
            };
            let (opens, note) = match found {
                Some(found) if found.exists => (true, ""),
                Some(_) => (false, "missing"),
                None => (false, "not checked"),
            };
            let shown = found.map_or_else(
                || artifact.target.text().to_owned(),
                |found| found.resolved.clone(),
            );
            div()
                .id(("step-artifact", index))
                .flex()
                .items_center()
                .gap_2()
                .rounded_sm()
                .child(
                    div()
                        .flex_none()
                        .text_color(if opens {
                            theme::accent_text()
                        } else {
                            theme::muted()
                        })
                        .child(selectable(
                            "name",
                            if url { format!("{name} ↗") } else { name },
                        )),
                )
                .when(!note.is_empty(), |row| {
                    row.child(
                        div()
                            .flex_none()
                            .text_color(theme::warning())
                            .child(selectable("note", note)),
                    )
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(theme::muted())
                        .child(selectable("path", shown)),
                )
                .when_some(run_index.filter(|_| opens), |row, at| {
                    row.cursor_pointer()
                        .hover(|style| style.bg(theme::hover_row()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let id = this.id.clone();
                            this.workflows.update(cx, |workflows, cx| {
                                workflows.open_artifact(&id, at, index, cx)
                            });
                        }))
                })
        });
        Some(
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .text_xs()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_none()
                                .text_color(theme::muted())
                                .child(selectable("step-artifacts-heading", "Artifacts")),
                        )
                        .when_some(run_index.filter(|_| checkable), |header, at| {
                            header.child(button("step-artifacts-check", "Check again").on_click(
                                cx.listener(move |this, _, _, cx| {
                                    let id = this.id.clone();
                                    this.workflows.update(cx, |workflows, cx| {
                                        workflows.check_artifacts(&id, at, cx);
                                    });
                                }),
                            ))
                        })
                        .children(error.map(|error| {
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(theme::error())
                                .child(selectable("step-artifacts-error", error))
                        })),
                )
                .children(rows)
                .into_any_element(),
        )
    }

    /// The steps' (id, state) as shown (tests).
    #[cfg(test)]
    pub(crate) fn cards(&self, cx: &App) -> Vec<(String, String)> {
        let workflows = self.workflows.read(cx);
        let active = workflows.run_of(&self.id);
        self.graph
            .workflow
            .iter()
            .flat_map(|workflow| &workflow.steps)
            .map(|step| {
                let status = state_of(active, &step.id).map_or("pending", |(_, s)| s.status.name());
                (step.id.clone(), status.to_owned())
            })
            .collect()
    }

    /// The problems shown, with their places (tests).
    #[cfg(test)]
    pub(crate) fn problems(&self) -> Vec<(String, Option<(usize, usize)>)> {
        self.graph
            .problems
            .iter()
            .map(|problem| (problem.message.clone(), problem.place))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn columns(&self) -> Vec<Vec<String>> {
        let (Some(workflow), Some(layout)) = (&self.graph.workflow, &self.graph.layout) else {
            return Vec::new();
        };
        layout
            .columns
            .iter()
            .map(|column| {
                column
                    .iter()
                    .map(|&at| workflow.steps[at].id.clone())
                    .collect()
            })
            .collect()
    }

    /// The graph's edges as (need, step) ids (tests).
    #[cfg(test)]
    pub(crate) fn edges(&self) -> Vec<(String, String)> {
        let (Some(workflow), Some(layout)) = (&self.graph.workflow, &self.graph.layout) else {
            return Vec::new();
        };
        layout
            .edges
            .iter()
            .map(|edge| {
                (
                    workflow.steps[edge.from].id.clone(),
                    workflow.steps[edge.to].id.clone(),
                )
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn editor(&self) -> Entity<Editor> {
        self.editor.clone()
    }

    #[cfg(test)]
    pub(crate) fn cursor_step(&self) -> Option<&str> {
        self.cursor_step.as_deref()
    }

    /// −, the zoom, + and Fit, over the graph's bottom-right corner.
    fn render_zoom(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.graph.layout.as_ref()?;
        let percent = (self.placement().scale * 100.).round();
        Some(
            div()
                .id("workflow-zoom")
                .absolute()
                .bottom(px(8.))
                .right(px(8.))
                .flex()
                .items_center()
                .gap_1()
                .p_1()
                .rounded_md()
                .border_1()
                .border_color(theme::border())
                .bg(theme::panel())
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    button("workflow-zoom-out", "−")
                        .on_click(cx.listener(|this, _, _, cx| this.zoom_centered(1. / 1.2, cx))),
                )
                .child(
                    div()
                        .w(px(40.))
                        .flex()
                        .justify_center()
                        .text_xs()
                        .text_color(theme::text())
                        .child(selectable("percent", format!("{percent}%"))),
                )
                .child(
                    button("workflow-zoom-in", "+")
                        .on_click(cx.listener(|this, _, _, cx| this.zoom_centered(1.2, cx))),
                )
                .child(
                    button("workflow-zoom-fit", "Fit")
                        .when(self.auto_fit, |fit| fit.border_color(theme::accent()))
                        .on_click(cx.listener(|this, _, _, cx| this.fit(cx))),
                )
                .into_any_element(),
        )
    }
}

impl Focusable for WorkflowView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for WorkflowView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = Instant::now();
        let workflow = self.graph.workflow.clone();
        let split_bounds = self.split_bounds.clone();
        let side_bounds = self.side_bounds.clone();
        let graph_bounds = self.graph_bounds.clone();
        let graph = match &workflow {
            Some(workflow) if workflow.steps.is_empty() => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::muted())
                .child(selectable(
                    "graph-empty",
                    "No steps yet: add some to `steps`.",
                ))
                .into_any_element(),
            Some(workflow) => self.render_graph(workflow, now, cx),
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::muted())
                .child(selectable(
                    "graph-empty",
                    "The graph shows once the file is a valid workflow.",
                ))
                .into_any_element(),
        };
        let details = workflow
            .as_ref()
            .and_then(|workflow| self.render_details(workflow, now, cx));
        let shows_terminal = matches!(details, Some((_, true)));
        let editor = div()
            .flex_none()
            .w(relative(self.split))
            .h_full()
            .flex()
            .overflow_hidden()
            .child(self.editor.clone());
        let handle = div()
            .id("workflow-split")
            .flex_none()
            .w(px(HANDLE_WIDTH))
            .h_full()
            .bg(if self.resizing == Some(Divider::Editor) {
                theme::accent()
            } else {
                theme::border()
            })
            .hover(|style| style.bg(theme::accent()))
            .cursor(CursorStyle::ResizeLeftRight)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.resizing = Some(Divider::Editor);
                    cx.stop_propagation();
                    cx.notify();
                }),
            );
        let graph_area = div()
            .id("workflow-graph")
            .relative()
            .flex_1()
            .min_h_0()
            .when(shows_terminal, |graph| graph.min_h(px(MIN_GRAPH_HEIGHT)))
            .overflow_hidden()
            .bg(theme::background())
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    this.panning = Some(event.position);
                    cx.notify();
                }),
            )
            .child(
                // A new size re-fits the graph next frame (while fitted).
                canvas(
                    move |laid_out, window, _| {
                        if graph_bounds.replace(laid_out) != laid_out {
                            window.request_animation_frame();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(graph)
            .children(self.render_banner(cx))
            .children(self.render_zoom(cx));
        // The graph, and under it the selected step's details; with its
        // terminal they form a pane of their own, resized by the divider
        // between them and the graph (a double-click maximizes it).
        let side = div()
            .relative()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .h_full()
            .flex()
            .flex_col()
            .child(
                canvas(
                    move |laid_out, _, _| side_bounds.set(laid_out),
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(graph_area);
        let side = match details {
            Some((details, true)) => {
                let share = if self.step_maximized {
                    1.
                } else {
                    self.step_share
                };
                side.child(
                    div()
                        .id("workflow-steps-split")
                        .flex_none()
                        .h(px(HANDLE_WIDTH))
                        .w_full()
                        .bg(if self.resizing == Some(Divider::Steps) {
                            theme::accent()
                        } else {
                            theme::border()
                        })
                        .hover(|style| style.bg(theme::accent()))
                        .cursor(CursorStyle::ResizeUpDown)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                if event.click_count >= 2 {
                                    this.step_maximized = !this.step_maximized;
                                    this.resizing = None;
                                } else {
                                    this.resizing = Some(Divider::Steps);
                                }
                                cx.notify();
                            }),
                        ),
                )
                .child(
                    div()
                        .id("workflow-step-pane")
                        .h(relative(share))
                        .min_h(px(MIN_STEP_HEIGHT))
                        .flex()
                        .flex_col()
                        .overflow_hidden()
                        .child(details),
                )
            }
            Some((details, false)) => side.child(details),
            None => side,
        };
        div()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::run_all_action))
            .on_action(cx.listener(Self::stop_all_action))
            .on_action(cx.listener(Self::retry_failed_action))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.on_mouse_up(cx)),
            )
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::background())
            .text_color(theme::text())
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(
                        canvas(
                            move |laid_out, _, _| split_bounds.set(laid_out),
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(editor)
                    .child(handle)
                    .child(side),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_times_read_naturally() {
        assert_eq!(elapsed_text(Duration::from_secs(7)), "7s");
        assert_eq!(elapsed_text(Duration::from_secs(65)), "1m 05s");
        assert_eq!(elapsed_text(Duration::from_secs(3720)), "1h 02m");
    }

    #[test]
    fn cards_say_what_a_step_does() {
        let step = workflow::parse(
            r#"{"steps": [{"id": "api", "check": "x", "run": "y", "ready": {"port": 8000}}]}"#,
            "f",
        )
        .unwrap()
        .steps
        .remove(0);
        assert_eq!(kind_hint(&step), "check · run · port 8000");
        assert_eq!(place(&step, None, None), "this machine");
    }

    #[test]
    fn edges_meet_their_arrowheads_square_on() {
        // From a port at (100, 50) through a column at y 120 into (464, 80).
        let points = [(100., 50.), (164., 120.), (364., 120.), (464., 80.)];
        let course = edge_course(&points);
        assert_eq!((course.start, course.tip), (points[0], points[3]));
        assert_eq!(
            course.base,
            (464. - ARROW_LENGTH, 80.),
            "the stroke stops at the head"
        );
        assert_eq!(
            course.pieces,
            [
                Piece::Line((100. + EDGE_LEAD, 50.)),
                Piece::Curve((164., 120.), (136., 50.), (136., 120.)),
                Piece::Line((364., 120.)),
                Piece::Curve((448., 80.), (406., 120.), (406., 80.)),
                Piece::Line((456., 80.)),
            ]
        );
        // The last run into the head is level and an arrowhead long.
        let Piece::Curve(curve_end, ..) = course.pieces[3] else {
            unreachable!()
        };
        assert_eq!(course.base.0 - curve_end.0, ARROW_LENGTH);
        // A level edge is straight all along.
        let level = edge_course(&[(0., 10.), (64., 10.)]);
        assert!(
            level
                .pieces
                .iter()
                .all(|piece| matches!(piece, Piece::Line(_)))
        );
    }

    #[test]
    fn the_step_pane_leaves_the_graph_room() {
        // 600 tall: the pane may span 240 to 600 − 5 − 90.
        assert_eq!(step_share(300., 600.), 0.5);
        assert_eq!(step_share(10., 600.), 0.4);
        assert_eq!(step_share(590., 600.), 505. / 600.);
        // Too short for both: the pane takes it all.
        assert_eq!(step_share(100., 200.), 1.);
    }
}
