//! A notebook open in the editor (or headless mode): its cells, the
//! kernel running them, the run queue, marimo's reactive re-runs, saving
//! and reloading. Views (`view.rs`) show it; several views may share one.
//!
//! The kernel is a bridge process ([`Kernel`]) started on the first run.
//! One `execute` runs at a time; its outputs are folded into the running
//! cell. Jupyter stops the queue at the first error; marimo runs a cell's
//! stale ancestors first and its descendants after it, skipping the
//! descendants of a cell that failed.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use futures::{
    StreamExt,
    channel::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
};
use gpui::{App, AppContext, AsyncApp, Context, Entity, EntityId, EventEmitter, Task, WeakEntity};
use notebook::{
    Cell, CellKind, Format, Kind, MarimoCell, Notebook, Output, ansi, ipynb, marimo,
    outputs::{self, Applied, OutputState},
    protocol::{self, Event, KernelState, MarimoRun, Op},
    widgets::{self, CommMessage, WidgetStore},
};
use serde_json::Value;

use super::kernel::{self, Kernel, KernelLaunch};
use crate::project::Project;

/// Stable id of a cell while the document is open (not saved).
pub type CellId = u64;

#[derive(Debug, Clone, PartialEq)]
pub enum RunState {
    Idle,
    Queued,
    Running { started: Instant },
    Finished { elapsed: Duration, ok: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub enum KernelStatus {
    NotStarted,
    Starting,
    Idle,
    Busy,
    Restarting,
    /// The kernel or its bridge ended (why).
    Dead(String),
    /// `modules` (ipykernel, jupyter_client) cannot be imported by `python`.
    Missing {
        python: String,
        modules: Vec<String>,
    },
    /// The interpreter could not be started (why).
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct DocCell {
    pub id: CellId,
    pub cell: Cell,
    pub run: RunState,
    /// marimo: the analysis of the cell's code.
    pub analysis: Option<marimo::Analysis>,
    /// marimo: why the cell cannot run (multiple definitions, cycle,
    /// syntax), as marimo words it.
    pub error: Option<String>,
    /// marimo: edited since it last ran.
    pub stale: bool,
}

/// A kernel completion: `matches` replace `start..end` (byte offsets in the
/// cell's source).
#[derive(Debug, Clone, PartialEq)]
pub struct KernelCompletion {
    pub matches: Vec<String>,
    pub start: usize,
    pub end: usize,
}

/// A kernelspec the bridge listed.
#[derive(Debug, Clone, PartialEq)]
pub struct KernelSpecInfo {
    pub name: String,
    pub display_name: String,
    pub language: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NotebookEvent {
    /// Cells were inserted, deleted, moved, split or changed kind.
    Structure,
    /// A cell's source changed other than through view `origin`'s editor:
    /// views showing it update their editor.
    Source {
        cell: CellId,
        origin: Option<EntityId>,
    },
    /// A cell's outputs, execution count or run state changed.
    Outputs(CellId),
    /// Kernel status or interpreter changed.
    Kernel,
    /// Dirty state changed (edit or save).
    Dirty,
    /// Replaced by the file's contents on disk.
    Reloaded,
}

/// Structural undo entries kept.
const UNDO_LIMIT: usize = 100;
/// How long `shutdown_and_wait` waits for the kernel to exit before killing.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// marimo's error name for cells that cannot run.
const MARIMO_ERROR: &str = "MarimoError";

pub(crate) type Bridge = (Kernel, UnboundedReceiver<Event>);
/// Starts a bridge (blocks): [`Kernel::spawn`], or a fake one in tests.
pub(crate) type Spawner = Arc<dyn Fn(&Project, &KernelLaunch) -> io::Result<Bridge> + Send + Sync>;
type Reply = (u64, Result<Value, String>);

/// One structural change; applying it returns its inverse.
enum Step {
    Insert {
        index: usize,
        cell: Box<DocCell>,
    },
    Remove {
        index: usize,
    },
    /// Swaps the cell at `index` (same id) for `cell`.
    Replace {
        index: usize,
        cell: Box<DocCell>,
    },
    Move {
        from: usize,
        to: usize,
    },
}

/// The `execute` in flight.
struct Running {
    cell: CellId,
    request: u64,
    started: Instant,
    state: OutputState,
}

pub struct NotebookDocument {
    project: Project,
    path: PathBuf,
    format: Format,
    cells: Vec<DocCell>,
    next_id: CellId,
    /// Bumped by every change; equal to `saved_version` when clean.
    version: u64,
    saved_version: u64,
    /// The file's text as last read or written.
    disk_text: String,
    /// Each entry's steps are applied last to first to undo (or redo).
    undo: VecDeque<Vec<Step>>,
    redo: Vec<Vec<Step>>,
    /// marimo: dependencies between cells (by index).
    graph: marimo::Graph,
    status: KernelStatus,
    launch: Option<KernelLaunch>,
    spawner: Spawner,
    kernel: Option<Kernel>,
    /// Bumped whenever a kernel starts or goes: tasks of an older kernel
    /// leave the document alone.
    generation: u64,
    replies: Option<UnboundedSender<Reply>>,
    pump: Option<Task<()>>,
    starting: Option<Task<()>>,
    restarting: Option<Task<()>>,
    queue: VecDeque<CellId>,
    running: Option<Running>,
    /// Cells that ran in the current kernel.
    ran: HashSet<CellId>,
    /// marimo: cells that failed since the queue was last empty.
    failed: HashSet<CellId>,
    /// Counts finished cells; `finished` holds each cell's last count.
    run_serial: u64,
    finished: HashMap<CellId, u64>,
    idle_waiters: Vec<oneshot::Sender<()>>,
    /// ipywidgets models of the kernel (or saved in the file).
    widgets: WidgetStore,
}

impl EventEmitter<NotebookEvent> for NotebookDocument {}

impl NotebookDocument {
    /// The notebook at `path` with file contents `text` (`.ipynb`, or a
    /// marimo `.py`; empty text: a new notebook) as an entity.
    pub fn create(
        project: Project,
        path: PathBuf,
        text: &str,
        cx: &mut App,
    ) -> Result<Entity<Self>, String> {
        let notebook = Self::parse(&path, text)?;
        let text = text.to_owned();
        Ok(cx.new(|cx| Self::from_notebook(project, path, notebook, text, cx)))
    }

    /// Reads `text` as the notebook at `path` (`.ipynb`, else marimo; empty
    /// text: a new notebook). Pure, for parsing off the UI thread.
    pub fn parse(path: &Path, text: &str) -> Result<Notebook, String> {
        let ipynb = path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("ipynb"));
        let kind = if ipynb { Kind::Ipynb } else { Kind::Marimo };
        if text.trim().is_empty() {
            return Ok(match kind {
                Kind::Ipynb => ipynb::new_notebook(),
                Kind::Marimo => marimo::new_notebook(),
            });
        }
        if kind == Kind::Marimo && !marimo::is_marimo(text) {
            return Err(format!("{} is not a marimo notebook", path.display()));
        }
        Notebook::parse(kind, text)
    }

    /// A document for an already parsed `notebook`; `text` is the file as
    /// read (for [`Self::changed_on_disk`]).
    pub fn from_notebook(
        project: Project,
        path: PathBuf,
        notebook: Notebook,
        text: String,
        _cx: &mut Context<Self>,
    ) -> Self {
        let Notebook { cells, format } = notebook;
        let spawner: Spawner = Arc::new(Kernel::spawn);
        let mut this = Self {
            project,
            path,
            format,
            cells: Vec::with_capacity(cells.len()),
            next_id: 1,
            version: 0,
            saved_version: 0,
            disk_text: text,
            undo: VecDeque::new(),
            redo: Vec::new(),
            graph: marimo::Graph::default(),
            status: KernelStatus::NotStarted,
            launch: None,
            spawner,
            kernel: None,
            generation: 0,
            replies: None,
            pump: None,
            starting: None,
            restarting: None,
            queue: VecDeque::new(),
            running: None,
            ran: HashSet::new(),
            failed: HashSet::new(),
            run_serial: 0,
            finished: HashMap::new(),
            idle_waiters: Vec::new(),
            widgets: WidgetStore::new(),
        };
        this.load_widget_state();
        this.cells = cells.into_iter().map(|cell| this.doc_cell(cell)).collect();
        this.ensure_a_cell();
        this.refresh_graph();
        this
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn project(&self) -> &Project {
        &self.project
    }

    pub fn is_marimo(&self) -> bool {
        matches!(self.format, Format::Marimo(_))
    }

    /// The language the notebook names: Jupyter's kernelspec (or
    /// `language_info`) language, Python for marimo; `None` when unnamed.
    pub fn language(&self) -> Option<&str> {
        match &self.format {
            Format::Ipynb(info) => info
                .metadata
                .get("kernelspec")
                .and_then(|spec| spec.get("language"))
                .or_else(|| info.metadata.get("language_info")?.get("name"))
                .and_then(Value::as_str),
            Format::Marimo(_) => Some("python"),
        }
    }

    pub fn cells(&self) -> &[DocCell] {
        &self.cells
    }

    pub fn cell(&self, id: CellId) -> Option<&DocCell> {
        self.cells.iter().find(|cell| cell.id == id)
    }

    pub fn index_of(&self, id: CellId) -> Option<usize> {
        self.cells.iter().position(|cell| cell.id == id)
    }

    pub fn is_dirty(&self) -> bool {
        self.version != self.saved_version
    }

    /// Sets a cell's text (typing in a view's editor: `origin` is that
    /// editor's entity, so the view does not echo it back).
    pub fn set_source(
        &mut self,
        id: CellId,
        source: String,
        origin: Option<EntityId>,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.index_of(id) else {
            return;
        };
        if self.cells[index].cell.source == source {
            return;
        }
        self.cells[index].cell.source = source;
        let mut errors = Vec::new();
        let mut stale = false;
        if self.is_marimo() {
            let analysis = self.analysis_of(&self.cells[index].cell);
            let ran = self.ran.contains(&id);
            let cell = &mut self.cells[index];
            if !cell.stale && (ran || matches!(cell.run, RunState::Finished { .. })) {
                cell.stale = true;
                stale = true;
            }
            if cell.analysis != analysis {
                cell.analysis = analysis;
                errors = self.refresh_graph();
            }
        }
        self.touch(cx);
        cx.emit(NotebookEvent::Source { cell: id, origin });
        if stale && !errors.contains(&id) {
            errors.push(id);
        }
        for id in errors {
            cx.emit(NotebookEvent::Outputs(id));
        }
        cx.notify();
    }

    /// Inserts a new cell at `index` (clamped); undoable.
    pub fn insert(
        &mut self,
        index: usize,
        kind: CellKind,
        source: String,
        cx: &mut Context<Self>,
    ) -> CellId {
        let index = index.min(self.cells.len());
        let cell = self.new_cell(kind, source);
        let cell = self.doc_cell(cell);
        let id = cell.id;
        self.commit(
            vec![Step::Insert {
                index,
                cell: Box::new(cell),
            }],
            cx,
        );
        id
    }

    /// Deletes cells (a notebook keeps at least one cell: deleting all
    /// leaves one empty code cell); undoable.
    pub fn delete(&mut self, ids: &[CellId], cx: &mut Context<Self>) {
        let mut indices: Vec<usize> = ids.iter().filter_map(|&id| self.index_of(id)).collect();
        indices.sort_unstable_by(|a, b| b.cmp(a));
        indices.dedup();
        if indices.is_empty() {
            return;
        }
        let all = indices.len() == self.cells.len();
        let mut steps: Vec<Step> = indices
            .into_iter()
            .map(|index| Step::Remove { index })
            .collect();
        if all {
            let cell = self.new_cell(CellKind::Code, String::new());
            steps.push(Step::Insert {
                index: 0,
                cell: Box::new(self.doc_cell(cell)),
            });
        }
        self.commit(steps, cx);
    }

    /// Moves the cells `ids` (contiguous) one place up (`-1`) or down
    /// (`1`); false at the edge. Undoable.
    pub fn move_cells(&mut self, ids: &[CellId], delta: isize, cx: &mut Context<Self>) -> bool {
        let mut indices: Vec<usize> = ids.iter().filter_map(|&id| self.index_of(id)).collect();
        indices.sort_unstable();
        indices.dedup();
        let (Some(&first), Some(&last)) = (indices.first(), indices.last()) else {
            return false;
        };
        if last - first + 1 != indices.len() {
            return false;
        }
        let (mut first, mut last) = (first, last);
        let mut steps = Vec::new();
        for _ in 0..delta.unsigned_abs() {
            if delta < 0 {
                if first == 0 {
                    break;
                }
                steps.push(Step::Move {
                    from: first - 1,
                    to: last,
                });
                first -= 1;
                last -= 1;
            } else {
                if last + 1 >= self.cells.len() {
                    break;
                }
                steps.push(Step::Move {
                    from: last + 1,
                    to: first,
                });
                first += 1;
                last += 1;
            }
        }
        if steps.is_empty() {
            return false;
        }
        self.commit(steps, cx);
        true
    }

    /// Code ↔ Markdown ↔ Raw (outputs dropped); undoable.
    pub fn set_kind(&mut self, id: CellId, kind: CellKind, cx: &mut Context<Self>) {
        let Some(index) = self.index_of(id) else {
            return;
        };
        let old = &self.cells[index].cell;
        if old.kind == kind {
            return;
        }
        let mut cell = without_outputs(old);
        if let Some(marimo) = &mut cell.marimo {
            // marimo's Markdown cells are `mo.md(...)` code.
            if old.kind == CellKind::Markdown && kind == CellKind::Code {
                cell.source = marimo::cell_code(old).into_owned();
            }
            marimo.original = None;
        }
        cell.kind = kind;
        let analysis = self.analysis_of(&cell);
        let replacement = DocCell {
            id,
            cell,
            run: RunState::Idle,
            analysis,
            error: None,
            stale: false,
        };
        self.commit(
            vec![Step::Replace {
                index,
                cell: Box::new(replacement),
            }],
            cx,
        );
    }

    /// Splits a cell at byte offset `at` (the second part becomes a new
    /// cell below, whitespace at the cut trimmed); undoable.
    pub fn split(&mut self, id: CellId, at: usize, cx: &mut Context<Self>) -> Option<CellId> {
        let index = self.index_of(id)?;
        let old = &self.cells[index].cell;
        let source = &old.source;
        let mut at = at.min(source.len());
        while !source.is_char_boundary(at) {
            at -= 1;
        }
        let head_source = source[..at].trim_end().to_owned();
        let tail_source = without_leading_blank_lines(&source[at..]).to_owned();
        let kind = old.kind;
        let mut head = without_outputs(old);
        head.source = head_source;
        if let Some(marimo) = &mut head.marimo {
            marimo.original = None;
        }
        let head = DocCell {
            id,
            analysis: self.analysis_of(&head),
            cell: head,
            run: RunState::Idle,
            error: None,
            stale: false,
        };
        let tail = self.new_cell(kind, tail_source);
        let tail = self.doc_cell(tail);
        let tail_id = tail.id;
        self.commit(
            vec![
                Step::Replace {
                    index,
                    cell: Box::new(head),
                },
                Step::Insert {
                    index: index + 1,
                    cell: Box::new(tail),
                },
            ],
            cx,
        );
        Some(tail_id)
    }

    /// Copies of the cells (for C/X, then V).
    pub fn copy(&self, ids: &[CellId]) -> Vec<Cell> {
        self.cells
            .iter()
            .filter(|cell| ids.contains(&cell.id))
            .map(|cell| cell.cell.clone())
            .collect()
    }

    /// Inserts copies of `cells` at `index` (new ids); undoable.
    pub fn paste(&mut self, index: usize, cells: Vec<Cell>, cx: &mut Context<Self>) -> Vec<CellId> {
        let index = index.min(self.cells.len());
        let mut ids = Vec::with_capacity(cells.len());
        let mut steps = Vec::with_capacity(cells.len());
        for (offset, mut cell) in cells.into_iter().enumerate() {
            // nbformat cell ids must stay unique.
            if cell.id.is_some() || self.wants_cell_ids() {
                cell.id = Some(ipynb::new_cell_id());
            }
            match (&self.format, &mut cell.marimo) {
                (Format::Marimo(_), None) => cell.marimo = Some(new_marimo_cell()),
                (Format::Ipynb(_), marimo) => *marimo = None,
                _ => {}
            }
            let cell = self.doc_cell(cell);
            ids.push(cell.id);
            steps.push(Step::Insert {
                index: index + offset,
                cell: Box::new(cell),
            });
        }
        if !steps.is_empty() {
            self.commit(steps, cx);
        }
        ids
    }

    /// Undoes the last structural change (insert, delete, move, kind,
    /// split, paste); false if none.
    pub fn undo_structure(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(entry) = self.undo.pop_back() else {
            return false;
        };
        let (redo, replaced) = self.apply_steps(entry.into_iter().rev());
        self.redo.push(redo);
        self.after_structure(replaced, cx);
        true
    }

    /// Redoes the last undone structural change; false if none.
    pub fn redo_structure(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(entry) = self.redo.pop() else {
            return false;
        };
        let (undo, replaced) = self.apply_steps(entry.into_iter().rev());
        self.push_undo(undo);
        self.after_structure(replaced, cx);
        true
    }

    /// Clears outputs and execution counts of `ids` (`None`: all cells).
    pub fn clear_outputs(&mut self, ids: Option<&[CellId]>, cx: &mut Context<Self>) {
        let mut changed = Vec::new();
        for cell in &mut self.cells {
            if ids.is_some_and(|ids| !ids.contains(&cell.id)) {
                continue;
            }
            let finished = matches!(cell.run, RunState::Finished { .. });
            if cell.cell.outputs.is_empty() && cell.cell.execution_count.is_none() && !finished {
                continue;
            }
            cell.cell.outputs.clear();
            cell.cell.execution_count = None;
            if finished {
                cell.run = RunState::Idle;
            }
            changed.push(cell.id);
        }
        for id in changed {
            self.outputs_changed(id, cx);
        }
    }

    /// Collapses or expands a cell's outputs. An `.ipynb` saves it
    /// (`metadata.jupyter.outputs_hidden` and `collapsed`, see
    /// [`Cell::set_outputs_hidden`]); a marimo file has no place for it, so
    /// there it lasts while the notebook is open (and across reloads).
    pub fn set_outputs_hidden(&mut self, id: CellId, hidden: bool, cx: &mut Context<Self>) {
        self.set_hidden(id, cx, |cell| cell.set_outputs_hidden(hidden));
    }

    /// Collapses or expands a cell's source (`metadata.jupyter.
    /// source_hidden`; in memory only for marimo, like outputs).
    pub fn set_source_hidden(&mut self, id: CellId, hidden: bool, cx: &mut Context<Self>) {
        self.set_hidden(id, cx, |cell| cell.set_source_hidden(hidden));
    }

    fn set_hidden(&mut self, id: CellId, cx: &mut Context<Self>, change: impl FnOnce(&mut Cell)) {
        let Some(index) = self.index_of(id) else {
            return;
        };
        let cell = &mut self.cells[index].cell;
        let before = (cell.outputs_hidden(), cell.source_hidden());
        change(cell);
        if (cell.outputs_hidden(), cell.source_hidden()) == before {
            return;
        }
        if !self.is_marimo() {
            self.touch(cx);
        }
        cx.emit(NotebookEvent::Outputs(id));
        cx.notify();
    }

    /// marimo: sets UI element `object_id` (shown in cell `cell`'s output)
    /// to `value`, in the form marimo's front end sends, as marimo does:
    /// the kernel converts it (`UIElement._update`, `on_change`), then the
    /// cells that use a global bound to the element run (with their
    /// descendants; not the cell defining it). Resolves once they are
    /// queued.
    pub fn set_ui_value(
        &mut self,
        cell: CellId,
        object_id: &str,
        value: Value,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        self.show_ui_value(cell, object_id, value.clone(), cx);
        let Some(kernel) = self.ready_kernel() else {
            return Task::ready(Err("the kernel is not running".into()));
        };
        let (_, reply) = kernel.request(Op::MarimoSetUi {
            object_id: object_id.to_owned(),
            value,
        });
        cx.spawn(async move |this, cx| {
            let result = reply.await.unwrap_or_else(|_| Err(bridge_ended()))?;
            let names: HashSet<&str> = result["names"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            this.update(cx, |this, cx| {
                let users: Vec<CellId> = this
                    .cells
                    .iter()
                    .filter(|cell| {
                        cell.analysis.as_ref().is_some_and(|analysis| {
                            analysis
                                .refs
                                .iter()
                                .any(|name| names.contains(name.as_str()))
                        })
                    })
                    .map(|cell| cell.id)
                    .collect();
                if !users.is_empty() {
                    this.run(&users, cx);
                }
            })
            .map_err(|err| err.to_string())
        })
    }

    /// marimo: shows `value` in element `object_id` of cell `cell`'s output
    /// without telling the kernel (a slider being dragged).
    pub fn show_ui_value(
        &mut self,
        cell: CellId,
        object_id: &str,
        value: Value,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.index_of(cell) else {
            return;
        };
        let mut changed = false;
        for output in &mut self.cells[index].cell.outputs {
            changed |= outputs::set_marimo_ui_value(output, object_id, &value);
        }
        if changed {
            cx.emit(NotebookEvent::Outputs(cell));
            cx.notify();
        }
    }

    /// marimo: the value element `object_id` of cell `cell`'s output shows.
    pub fn ui_value(&self, cell: CellId, object_id: &str) -> Option<Value> {
        self.cell(cell)?
            .cell
            .outputs
            .iter()
            .find_map(|output| match output {
                Output::DisplayData { data, .. } | Output::ExecuteResult { data, .. } => {
                    outputs::marimo_ui_parts(data)?
                        .iter()
                        .filter_map(|part| part.get("element"))
                        .find(|element| element["id"].as_str() == Some(object_id))
                        .map(|element| element["value"].clone())
                }
                _ => None,
            })
    }

    /// Queues code cells to run (starting the kernel if needed). marimo:
    /// also their stale/never-run ancestors and their descendants, in
    /// dependency order; cells in error get the error as output.
    pub fn run(&mut self, ids: &[CellId], cx: &mut Context<Self>) {
        let order = if self.is_marimo() {
            self.marimo_order(ids)
        } else {
            let mut seen = HashSet::new();
            ids.iter()
                .copied()
                .filter(|&id| {
                    self.cell(id)
                        .is_some_and(|cell| cell.cell.kind == CellKind::Code)
                        && seen.insert(id)
                })
                .collect()
        };
        for id in order {
            if self.queue.contains(&id) {
                continue;
            }
            if let Some(index) = self.index_of(id) {
                self.cells[index].run = RunState::Queued;
                self.queue.push_back(id);
                cx.emit(NotebookEvent::Outputs(id));
            }
        }
        cx.notify();
        self.run_next(cx);
    }

    pub fn run_all(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<CellId> = self.cells.iter().map(|cell| cell.id).collect();
        self.run(&ids, cx);
    }

    /// The cells before `id`.
    pub fn run_above(&mut self, id: CellId, cx: &mut Context<Self>) {
        let Some(index) = self.index_of(id) else {
            return;
        };
        let ids: Vec<CellId> = self.cells[..index].iter().map(|cell| cell.id).collect();
        self.run(&ids, cx);
    }

    /// `id` and the cells after it.
    pub fn run_below(&mut self, id: CellId, cx: &mut Context<Self>) {
        let Some(index) = self.index_of(id) else {
            return;
        };
        let ids: Vec<CellId> = self.cells[index..].iter().map(|cell| cell.id).collect();
        self.run(&ids, cx);
    }

    /// Interrupts the running cell; queued cells are dequeued.
    pub fn interrupt(&mut self, cx: &mut Context<Self>) {
        self.clear_queue(cx);
        if self.running.is_some()
            && let Some(kernel) = &self.kernel
        {
            // The reply only confirms the interrupt was sent.
            let _ = kernel.request(Op::Interrupt);
        }
        self.check_idle();
    }

    /// Restarts the kernel (starts it if none), then optionally runs all.
    pub fn restart(&mut self, then_run_all: bool, cx: &mut Context<Self>) {
        let Some(kernel) = &self.kernel else {
            if then_run_all {
                self.run_all(cx);
            } else {
                self.start_kernel(cx);
            }
            return;
        };
        let (_, reply) = kernel.request(Op::Restart);
        self.widgets.kernel_gone();
        let generation = self.generation;
        self.abort_running(None, cx);
        self.clear_queue(cx);
        self.set_status(KernelStatus::Restarting, cx);
        if let Some(task) = self.restarting.take() {
            task.detach();
        }
        self.restarting = Some(cx.spawn(async move |this, cx| {
            let result = reply
                .await
                .unwrap_or_else(|_| Err("The kernel bridge ended".into()));
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                if let Some(task) = this.restarting.take() {
                    task.detach();
                }
                match result {
                    Ok(_) => {
                        this.ran.clear();
                        this.set_status(KernelStatus::Idle, cx);
                        if then_run_all {
                            this.run_all(cx);
                        } else {
                            this.run_next(cx);
                        }
                    }
                    Err(message) => this.kernel_died(message, cx),
                }
                this.check_idle();
            })
            .ok();
        }));
    }

    /// Stops the kernel (tab closed, headless `notebook_close`).
    pub fn shutdown(&mut self, cx: &mut Context<Self>) {
        drop(self.stop(cx));
    }

    /// [`Self::shutdown`], resolving once the kernel process exited (or was
    /// killed after a timeout).
    pub fn shutdown_and_wait(&mut self, cx: &mut Context<Self>) -> Task<()> {
        match self.stop(cx) {
            Some(kernel) => {
                cx.background_spawn(async move { kernel.shutdown_blocking(SHUTDOWN_TIMEOUT) })
            }
            None => Task::ready(()),
        }
    }

    /// The ipywidgets models outputs show.
    pub fn widgets(&self) -> &WidgetStore {
        &self.widgets
    }

    /// Sets `key` of widget `model` (a control was used); the kernel hears
    /// of it when `send` (drags send now and then, and at their end).
    pub fn widget_update(
        &mut self,
        model: &str,
        key: &str,
        value: Value,
        send: bool,
        cx: &mut Context<Self>,
    ) {
        let version = self.widgets.version();
        let message = self.widgets.update(model, key, value, send);
        self.send_comm(message);
        if self.widgets.version() != version {
            self.widgets_changed(cx);
        }
    }

    /// A `custom` message to widget `model` (a Button's click).
    pub fn widget_custom(&mut self, model: &str, content: Value) {
        let message = self.widgets.custom(model, content);
        self.send_comm(message);
    }

    fn send_comm(&self, message: Option<CommMessage>) {
        if let (Some(message), Some(kernel)) = (message, self.ready_kernel()) {
            drop(kernel.request(Op::CommMsg {
                comm_id: message.comm_id,
                data: message.data,
                buffers: message.buffers,
                msg_id: message.msg_id,
            }));
        }
    }

    /// Cells showing widgets redraw (and re-measure).
    fn widgets_changed(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<CellId> = self
            .cells
            .iter()
            .filter(|cell| {
                cell.cell
                    .outputs
                    .iter()
                    .any(|output| widgets::view_model_id(output).is_some())
            })
            .map(|cell| cell.id)
            .collect();
        for id in ids {
            cx.emit(NotebookEvent::Outputs(id));
        }
        cx.notify();
    }

    /// The widget state saved in an `.ipynb` file's metadata.
    fn load_widget_state(&mut self) {
        if let Format::Ipynb(info) = &self.format {
            self.widgets.load_metadata(info.metadata.get("widgets"));
        }
    }

    pub fn kernel_status(&self) -> &KernelStatus {
        &self.status
    }

    /// How the kernel starts (resolved on first use).
    pub fn launch(&self) -> Option<&KernelLaunch> {
        self.launch.as_ref()
    }

    /// Another interpreter/kernelspec: restarts a running kernel with it.
    pub fn set_launch(&mut self, launch: KernelLaunch, cx: &mut Context<Self>) {
        if self.launch.as_ref() == Some(&launch) {
            return;
        }
        self.launch = Some(launch);
        cx.emit(NotebookEvent::Kernel);
        if self.kernel.is_some() || self.starting.is_some() {
            self.shutdown(cx);
            self.start_kernel(cx);
        } else if !matches!(self.status, KernelStatus::NotStarted) {
            // A banner about the previous interpreter no longer applies.
            self.set_status(KernelStatus::NotStarted, cx);
        }
        cx.notify();
    }

    /// Kernelspecs the bridge lists (starts a bridge if none runs).
    pub fn kernel_specs(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<KernelSpecInfo>, String>> {
        if let Some(kernel) = &self.kernel {
            let (_, reply) = kernel.request(Op::KernelSpecs);
            return cx.background_spawn(async move {
                kernel_specs_from(reply.await.unwrap_or_else(|_| Err(bridge_ended())))
            });
        }
        let (project, path) = (self.project.clone(), self.path.clone());
        let launch = self.launch.clone();
        let spawner = self.spawner.clone();
        cx.background_spawn(async move {
            let launch = launch.unwrap_or_else(|| kernel::default_launch(&project, &path));
            let (kernel, mut events) = spawner(&project, &launch)
                .map_err(|err| format!("Cannot start {}: {err}", launch.label()))?;
            wait_for_hello(&mut events).await?;
            let (_, reply) = kernel.request(Op::KernelSpecs);
            let result = reply.await.unwrap_or_else(|_| Err(bridge_ended()));
            drop(kernel);
            kernel_specs_from(result)
        })
    }

    /// Kernel completions at byte `offset` of `code` (`None` without a
    /// running kernel).
    pub fn complete(
        &mut self,
        code: String,
        offset: usize,
        cx: &mut Context<Self>,
    ) -> Task<Option<KernelCompletion>> {
        let Some(kernel) = self.ready_kernel() else {
            return Task::ready(None);
        };
        let cursor_pos = protocol::code_points(&code, offset);
        let (_, reply) = kernel.request(Op::Complete {
            code: code.clone(),
            cursor_pos,
        });
        cx.background_spawn(async move {
            let value = reply.await.ok()?.ok()?;
            let matches = value["matches"]
                .as_array()?
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect();
            let position = |key: &str| {
                let position = value[key].as_u64().map_or(cursor_pos, |p| p as usize);
                protocol::byte_offset(&code, position)
            };
            Some(KernelCompletion {
                matches,
                start: position("cursor_start"),
                end: position("cursor_end"),
            })
        })
    }

    /// The kernel's help (`text/plain`, ANSI stripped) for the name at
    /// `offset`.
    pub fn inspect(
        &mut self,
        code: String,
        offset: usize,
        cx: &mut Context<Self>,
    ) -> Task<Option<String>> {
        let Some(kernel) = self.ready_kernel() else {
            return Task::ready(None);
        };
        let cursor_pos = protocol::code_points(&code, offset);
        let (_, reply) = kernel.request(Op::Inspect {
            code,
            cursor_pos,
            detail_level: 0,
        });
        cx.background_spawn(async move {
            let value = reply.await.ok()?.ok()?;
            if value["found"].as_bool() != Some(true) {
                return None;
            }
            let text = match &value["data"]["text/plain"] {
                Value::String(text) => text.clone(),
                Value::Array(lines) => lines.iter().filter_map(Value::as_str).collect(),
                _ => return None,
            };
            Some(ansi::strip(&text))
        })
    }

    /// Writes the file in its format (blocks on the project write).
    pub fn save(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        // Widget state goes where Jupyter keeps it, once widgets were used.
        if self.widgets.touched()
            && let Format::Ipynb(info) = &mut self.format
        {
            let roots = self
                .cells
                .iter()
                .flat_map(|cell| cell.cell.outputs.iter().filter_map(widgets::view_model_id));
            match self.widgets.to_metadata(roots) {
                Some(state) => {
                    info.metadata.insert("widgets".into(), state);
                }
                None => {
                    info.metadata.remove("widgets");
                }
            }
        }
        let text = match &self.format {
            Format::Ipynb(info) => {
                let cells: Vec<Cell> = self.cells.iter().map(|cell| cell.cell.clone()).collect();
                ipynb::to_text(info, &cells)
            }
            // marimo files keep no outputs.
            Format::Marimo(info) => {
                let cells: Vec<Cell> = self
                    .cells
                    .iter()
                    .map(|cell| without_outputs(&cell.cell))
                    .collect();
                marimo::to_text(info, &cells)
            }
        };
        self.project
            .write(&self.path, text.clone().into_bytes())
            .map_err(|err| format!("Cannot save {}: {err}", self.path.display()))?;
        self.disk_text = text;
        let was_dirty = self.is_dirty();
        self.saved_version = self.version;
        if was_dirty {
            cx.emit(NotebookEvent::Dirty);
            cx.notify();
        }
        Ok(())
    }

    /// Whether `text` (the file on disk now) differs from what was last
    /// read or written (a watcher event for our own save is not a change).
    pub fn changed_on_disk(&self, text: &str) -> bool {
        text != self.disk_text
    }

    /// Replaces the cells with `text`. Cells are matched to the open ones
    /// (see [`match_reloaded`]); matched cells keep their id (views keep
    /// their editors), run state and, for marimo (which saves no outputs),
    /// their outputs and collapsed state. In a marimo notebook a matched
    /// cell whose code changed is stale, like after a local edit, and so
    /// are the descendants of changed and new cells that ran. A Jupyter
    /// cell keeps its outputs when its code is unchanged and the file has
    /// none for it (a tool that strips outputs or rewrites only sources).
    pub fn reload(&mut self, text: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let Notebook { cells, format } = Self::parse(&self.path, text)?;
        let marimo = matches!(format, Format::Marimo(_));
        let matched = match_reloaded(&self.cells, &cells);
        let mut previous: Vec<Option<DocCell>> = std::mem::take(&mut self.cells)
            .into_iter()
            .map(Some)
            .collect();
        self.format = format;
        self.load_widget_state();
        let mut reloaded = Vec::with_capacity(cells.len());
        // Indices (in `reloaded`) of cells whose code changed or that are new.
        let mut changed = Vec::new();
        for (mut cell, old) in cells.into_iter().zip(matched) {
            let Some(old) = old.and_then(|index| previous[index].take()) else {
                changed.push(reloaded.len());
                reloaded.push(self.doc_cell(cell));
                continue;
            };
            let same = old.cell.source == cell.source;
            let ran = self.ran.contains(&old.id) || matches!(old.run, RunState::Finished { .. });
            let keep_outputs = if marimo {
                true
            } else {
                same && cell.outputs.is_empty() && cell.execution_count.is_none()
            };
            if marimo {
                cell.set_outputs_hidden(old.cell.outputs_hidden());
                cell.set_source_hidden(old.cell.source_hidden());
            }
            if keep_outputs {
                cell.outputs = old.cell.outputs;
                cell.execution_count = old.cell.execution_count;
            }
            if !same {
                changed.push(reloaded.len());
            }
            reloaded.push(DocCell {
                id: old.id,
                analysis: self.analysis_of(&cell),
                cell,
                run: old.run,
                error: None,
                stale: marimo && (old.stale || (!same && ran)),
            });
        }
        self.cells = reloaded;
        self.ensure_a_cell();
        let present: HashSet<CellId> = self.cells.iter().map(|cell| cell.id).collect();
        self.queue.retain(|id| present.contains(id));
        self.undo.clear();
        self.redo.clear();
        self.refresh_graph();
        if marimo {
            for index in self.graph.run_order(&changed) {
                let cell = &mut self.cells[index];
                if self.ran.contains(&cell.id) || matches!(cell.run, RunState::Finished { .. }) {
                    cell.stale = true;
                }
            }
        }
        self.disk_text = text.to_owned();
        let was_dirty = self.is_dirty();
        self.version += 1;
        self.saved_version = self.version;
        cx.emit(NotebookEvent::Reloaded);
        if was_dirty {
            cx.emit(NotebookEvent::Dirty);
        }
        cx.notify();
        self.check_idle();
        Ok(())
    }

    /// Resolves once nothing runs or is queued.
    pub fn when_idle(&mut self) -> oneshot::Receiver<()> {
        let (sender, receiver) = oneshot::channel();
        if self.is_busy() {
            self.idle_waiters.push(sender);
        } else {
            sender.send(()).ok();
        }
        receiver
    }

    /// A mark for [`Self::finished_since`].
    pub fn run_mark(&self) -> u64 {
        self.run_serial
    }

    /// Cells (with their index) that finished running, failed or were
    /// skipped (marimo) after `mark` was taken.
    pub fn finished_since(&self, mark: u64) -> impl Iterator<Item = (usize, &DocCell)> {
        self.cells
            .iter()
            .enumerate()
            .filter(move |(_, cell)| self.finished.get(&cell.id).is_some_and(|&at| at > mark))
    }

    #[cfg(test)]
    pub(crate) fn set_spawner(&mut self, spawner: Spawner) {
        self.spawner = spawner;
    }

    // Cells and structure.

    fn doc_cell(&mut self, cell: Cell) -> DocCell {
        let id = self.next_id;
        self.next_id += 1;
        DocCell {
            id,
            analysis: self.analysis_of(&cell),
            cell,
            run: RunState::Idle,
            error: None,
            stale: false,
        }
    }

    fn analysis_of(&self, cell: &Cell) -> Option<marimo::Analysis> {
        (self.is_marimo() && cell.kind != CellKind::Raw)
            .then(|| marimo::analyze(&marimo::cell_code(cell)))
    }

    fn wants_cell_ids(&self) -> bool {
        matches!(&self.format, Format::Ipynb(info) if info.nbformat_minor >= 5)
    }

    /// A new cell as this notebook's format writes it.
    fn new_cell(&self, kind: CellKind, source: String) -> Cell {
        let mut cell = Cell::new(kind, source);
        if self.wants_cell_ids() {
            cell.id = Some(ipynb::new_cell_id());
        }
        if self.is_marimo() {
            cell.marimo = Some(new_marimo_cell());
        }
        cell
    }

    fn ensure_a_cell(&mut self) {
        if self.cells.is_empty() {
            let cell = self.new_cell(CellKind::Code, String::new());
            let cell = self.doc_cell(cell);
            self.cells.push(cell);
        }
    }

    /// Rebuilds marimo's graph and the cells' errors; returns the cells
    /// whose error changed.
    fn refresh_graph(&mut self) -> Vec<CellId> {
        if !self.is_marimo() {
            return Vec::new();
        }
        let analyses: Vec<marimo::Analysis> = self
            .cells
            .iter()
            .map(|cell| cell.analysis.clone().unwrap_or_default())
            .collect();
        self.graph = marimo::Graph::new(&analyses);
        let mut changed = Vec::new();
        for (index, cell) in self.cells.iter_mut().enumerate() {
            let error = self
                .graph
                .error(index)
                .map(ToString::to_string)
                .or_else(|| {
                    analyses[index]
                        .syntax_error
                        .clone()
                        .map(|error| marimo::CellError::Syntax(error).to_string())
                });
            if cell.error != error {
                cell.error = error;
                changed.push(cell.id);
            }
        }
        changed
    }

    fn touch(&mut self, cx: &mut Context<Self>) {
        let was_dirty = self.is_dirty();
        self.version += 1;
        if !was_dirty {
            cx.emit(NotebookEvent::Dirty);
        }
    }

    fn commit(&mut self, steps: Vec<Step>, cx: &mut Context<Self>) {
        let (inverse, replaced) = self.apply_steps(steps.into_iter());
        self.push_undo(inverse);
        self.redo.clear();
        self.after_structure(replaced, cx);
    }

    fn push_undo(&mut self, entry: Vec<Step>) {
        if self.undo.len() == UNDO_LIMIT {
            self.undo.pop_front();
        }
        self.undo.push_back(entry);
    }

    /// Applies `steps` in order; returns their inverses (in the order
    /// applied) and the replaced cells.
    fn apply_steps(&mut self, steps: impl Iterator<Item = Step>) -> (Vec<Step>, Vec<CellId>) {
        let mut inverse = Vec::new();
        let mut replaced = Vec::new();
        for step in steps {
            inverse.push(match step {
                Step::Insert { index, cell } => {
                    let index = index.min(self.cells.len());
                    self.cells.insert(index, *cell);
                    Step::Remove { index }
                }
                Step::Remove { index } => {
                    let mut cell = self.cells.remove(index);
                    self.queue.retain(|&id| id != cell.id);
                    if cell.run == RunState::Queued {
                        cell.run = RunState::Idle;
                    }
                    Step::Insert {
                        index,
                        cell: Box::new(cell),
                    }
                }
                Step::Replace { index, cell } => {
                    let mut cell = *cell;
                    // The run state is not history.
                    cell.run = self.cells[index].run.clone();
                    replaced.push(cell.id);
                    let old = std::mem::replace(&mut self.cells[index], cell);
                    Step::Replace {
                        index,
                        cell: Box::new(old),
                    }
                }
                Step::Move { from, to } => {
                    let cell = self.cells.remove(from);
                    self.cells.insert(to, cell);
                    Step::Move { from: to, to: from }
                }
            });
        }
        (inverse, replaced)
    }

    fn after_structure(&mut self, replaced: Vec<CellId>, cx: &mut Context<Self>) {
        let errors = self.refresh_graph();
        self.touch(cx);
        cx.emit(NotebookEvent::Structure);
        for id in replaced {
            cx.emit(NotebookEvent::Source {
                cell: id,
                origin: None,
            });
        }
        for id in errors {
            cx.emit(NotebookEvent::Outputs(id));
        }
        cx.notify();
        self.check_idle();
    }

    fn outputs_changed(&mut self, id: CellId, cx: &mut Context<Self>) {
        // ipynb files keep outputs; marimo files do not.
        if !self.is_marimo() {
            self.touch(cx);
        }
        cx.emit(NotebookEvent::Outputs(id));
        cx.notify();
    }

    // Running.

    /// marimo: `ids`' stale or never-run ancestors, `ids` and their
    /// descendants, in dependency order.
    fn marimo_order(&self, ids: &[CellId]) -> Vec<CellId> {
        let requested: Vec<usize> = ids
            .iter()
            .filter_map(|&id| self.index_of(id))
            .filter(|&index| self.cells[index].cell.kind != CellKind::Raw)
            .collect();
        let mut wanted: HashSet<usize> = self.graph.run_order(&requested).into_iter().collect();
        for &index in &requested {
            for ancestor in self.graph.ancestors(index) {
                let cell = &self.cells[ancestor];
                if cell.stale || !self.ran.contains(&cell.id) {
                    wanted.insert(ancestor);
                }
            }
        }
        let mut order: Vec<usize> = self
            .graph
            .topological()
            .into_iter()
            .filter(|index| wanted.remove(index))
            .collect();
        // Anything the topological order left out, in notebook order.
        let mut rest: Vec<usize> = wanted.into_iter().collect();
        rest.sort_unstable();
        order.extend(rest);
        order
            .into_iter()
            .filter(|&index| self.cells[index].cell.kind != CellKind::Raw)
            .map(|index| self.cells[index].id)
            .collect()
    }

    /// Something runs or is queued, or the kernel is (re)starting.
    pub(crate) fn is_busy(&self) -> bool {
        self.running.is_some()
            || !self.queue.is_empty()
            || self.starting.is_some()
            || self.restarting.is_some()
    }

    fn check_idle(&mut self) {
        if self.is_busy() {
            return;
        }
        self.failed.clear();
        for waiter in self.idle_waiters.drain(..) {
            waiter.send(()).ok();
        }
    }

    fn set_status(&mut self, status: KernelStatus, cx: &mut Context<Self>) {
        if self.status != status {
            self.status = status;
            cx.emit(NotebookEvent::Kernel);
            cx.notify();
        }
    }

    fn ready_kernel(&self) -> Option<&Kernel> {
        self.kernel.as_ref().filter(|_| self.restarting.is_none())
    }

    /// Queued cells back to idle.
    fn clear_queue(&mut self, cx: &mut Context<Self>) {
        for id in std::mem::take(&mut self.queue) {
            if let Some(index) = self.index_of(id)
                && self.cells[index].run == RunState::Queued
            {
                self.cells[index].run = RunState::Idle;
                cx.emit(NotebookEvent::Outputs(id));
            }
        }
        cx.notify();
    }

    fn mark_finished(&mut self, index: usize, elapsed: Duration, ok: bool) {
        let cell = &mut self.cells[index];
        cell.run = RunState::Finished { elapsed, ok };
        self.run_serial += 1;
        self.finished.insert(cell.id, self.run_serial);
        if !ok {
            self.failed.insert(cell.id);
        }
    }

    /// Ends the running cell without its reply (restart, dead kernel,
    /// shutdown), with `note` as an error output.
    fn abort_running(&mut self, note: Option<Output>, cx: &mut Context<Self>) {
        let Some(running) = self.running.take() else {
            return;
        };
        if let Some(index) = self.index_of(running.cell) {
            self.cells[index].cell.outputs.extend(note);
            self.mark_finished(index, running.started.elapsed(), false);
            self.outputs_changed(running.cell, cx);
        }
    }

    /// Runs the next queued cell, starting the kernel first if needed.
    fn run_next(&mut self, cx: &mut Context<Self>) {
        if self.running.is_some() || self.restarting.is_some() {
            return;
        }
        if self.kernel.is_none() {
            if !self.queue.is_empty() {
                self.start_kernel(cx);
            }
            self.check_idle();
            return;
        }
        let marimo = self.is_marimo();
        while let Some(id) = self.queue.pop_front() {
            let Some(index) = self.index_of(id) else {
                continue;
            };
            let kind = self.cells[index].cell.kind;
            if kind == CellKind::Raw || (!marimo && kind != CellKind::Code) {
                self.cells[index].run = RunState::Idle;
                cx.emit(NotebookEvent::Outputs(id));
                continue;
            }
            if marimo {
                if let Some(error) = self.cells[index].error.clone() {
                    self.skip(
                        index,
                        Output::Error {
                            ename: MARIMO_ERROR.into(),
                            evalue: error,
                            traceback: Vec::new(),
                        },
                        cx,
                    );
                    continue;
                }
                if let Some(ename) = self.failed_ancestor(index) {
                    self.skip(
                        index,
                        Output::Stream {
                            name: "stderr".into(),
                            text: format!("An ancestor raised an exception ({ename})\n"),
                        },
                        cx,
                    );
                    continue;
                }
            }
            let op = self.execute_op(index);
            let Some(kernel) = &self.kernel else {
                return;
            };
            let (request, reply) = kernel.request(op);
            if let Some(replies) = self.replies.clone() {
                cx.background_spawn(async move {
                    let result = reply.await.unwrap_or_else(|_| Err(bridge_ended()));
                    replies.unbounded_send((request, result)).ok();
                })
                .detach();
            }
            let started = Instant::now();
            let cell = &mut self.cells[index];
            cell.cell.outputs.clear();
            cell.cell.execution_count = None;
            cell.run = RunState::Running { started };
            cell.stale = false;
            self.ran.insert(id);
            self.running = Some(Running {
                cell: id,
                request,
                started,
                state: OutputState::default(),
            });
            self.outputs_changed(id, cx);
            return;
        }
        self.check_idle();
    }

    /// marimo: a cell that does not run this round, with `output` as why.
    fn skip(&mut self, index: usize, output: Output, cx: &mut Context<Self>) {
        let cell = &mut self.cells[index];
        cell.cell.outputs = vec![output];
        cell.cell.execution_count = None;
        let id = cell.id;
        self.mark_finished(index, Duration::ZERO, false);
        self.outputs_changed(id, cx);
    }

    /// marimo: the error name of an ancestor of the cell at `index` that
    /// failed this round.
    fn failed_ancestor(&self, index: usize) -> Option<String> {
        if self.failed.is_empty() {
            return None;
        }
        let failed: Vec<&DocCell> = self
            .graph
            .ancestors(index)
            .into_iter()
            .map(|ancestor| &self.cells[ancestor])
            .filter(|cell| self.failed.contains(&cell.id))
            .collect();
        if failed.is_empty() {
            return None;
        }
        let ename = failed.iter().find_map(|cell| {
            cell.cell.outputs.iter().find_map(|output| match output {
                Output::Error { ename, .. } => Some(ename.clone()),
                _ => None,
            })
        });
        Some(ename.unwrap_or_else(|| "Exception".into()))
    }

    fn execute_op(&self, index: usize) -> Op {
        let cell = &self.cells[index];
        if !self.is_marimo() {
            return Op::Execute {
                code: cell.cell.source.clone(),
                marimo: None,
            };
        }
        let analysis = cell.analysis.clone().unwrap_or_default();
        let defined: HashSet<&str> = self
            .cells
            .iter()
            .filter_map(|cell| cell.analysis.as_ref())
            .flat_map(|analysis| analysis.public_defs())
            .collect();
        let refs = analysis
            .refs
            .iter()
            .filter(|name| !marimo::is_builtin(name) || defined.contains(name.as_str()))
            .cloned()
            .collect();
        Op::Execute {
            code: marimo::cell_code(&cell.cell).into_owned(),
            marimo: Some(MarimoRun {
                cell: cell.id.to_string(),
                defs: analysis.public_defs().map(str::to_owned).collect(),
                refs,
            }),
        }
    }

    // The kernel.

    fn start_kernel(&mut self, cx: &mut Context<Self>) {
        if self.kernel.is_some() || self.starting.is_some() {
            return;
        }
        self.set_status(KernelStatus::Starting, cx);
        let project = self.project.clone();
        let path = self.path.clone();
        let launch = self.launch.clone();
        let spawner = self.spawner.clone();
        let cwd = self
            .path
            .parent()
            .unwrap_or(self.project.root())
            .to_string_lossy()
            .into_owned();
        self.starting = Some(cx.spawn(async move |this, cx| {
            let started = start_bridge(project, path, launch, spawner, cwd, &this, cx).await;
            this.update(cx, |this, cx| this.kernel_started(started, cx))
                .ok();
        }));
    }

    fn kernel_started(&mut self, started: Result<Bridge, KernelStatus>, cx: &mut Context<Self>) {
        if let Some(task) = self.starting.take() {
            task.detach();
        }
        match started {
            Ok((kernel, events)) => {
                self.generation += 1;
                let generation = self.generation;
                let (replies, replies_rx) = mpsc::unbounded();
                self.replies = Some(replies);
                self.kernel = Some(kernel);
                self.ran.clear();
                self.pump = Some(cx.spawn(async move |this, cx| {
                    pump(this, generation, events, replies_rx, cx).await
                }));
                self.set_status(KernelStatus::Idle, cx);
                self.run_next(cx);
            }
            Err(status) => {
                self.clear_queue(cx);
                self.set_status(status, cx);
                self.check_idle();
            }
        }
    }

    /// Forgets the kernel (its tasks stop acting on the document); returns
    /// it for the caller to drop or shut down.
    fn take_kernel(&mut self) -> Option<Kernel> {
        self.generation += 1;
        if let Some(task) = self.pump.take() {
            task.detach();
        }
        if let Some(task) = self.restarting.take() {
            task.detach();
        }
        self.replies = None;
        self.widgets.kernel_gone();
        self.kernel.take()
    }

    fn stop(&mut self, cx: &mut Context<Self>) -> Option<Kernel> {
        // Dropping the start cancels it (and drops its bridge).
        self.starting = None;
        let kernel = self.take_kernel();
        self.abort_running(None, cx);
        self.clear_queue(cx);
        self.set_status(KernelStatus::NotStarted, cx);
        self.check_idle();
        kernel
    }

    fn kernel_died(&mut self, message: String, cx: &mut Context<Self>) {
        drop(self.take_kernel());
        self.abort_running(
            Some(Output::Error {
                ename: "DeadKernelError".into(),
                evalue: message.clone(),
                traceback: Vec::new(),
            }),
            cx,
        );
        self.clear_queue(cx);
        self.set_status(KernelStatus::Dead(message), cx);
        self.check_idle();
    }

    fn on_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Hello { .. } | Event::Log { .. } => {}
            Event::Status { state, .. } => match state {
                KernelState::Starting => self.set_status(KernelStatus::Starting, cx),
                KernelState::Idle => self.set_status(KernelStatus::Idle, cx),
                KernelState::Busy => self.set_status(KernelStatus::Busy, cx),
                KernelState::Restarting => self.set_status(KernelStatus::Restarting, cx),
                KernelState::Dead => self.kernel_died("The kernel died".into(), cx),
            },
            Event::Output {
                request,
                parent,
                msg_type,
                content,
            } => self.on_output(request, parent.as_deref(), &msg_type, &content, cx),
            Event::Comm {
                parent,
                msg_type,
                content,
                buffers,
                ..
            } => {
                if self
                    .widgets
                    .apply(&msg_type, &content, &buffers, parent.as_deref())
                {
                    self.widgets_changed(cx);
                }
            }
            Event::ExecuteInput {
                request,
                execution_count,
            } => {
                let Some(running) = self.running.as_ref().filter(|r| r.request == request) else {
                    return;
                };
                let id = running.cell;
                if let Some(index) = self.index_of(id) {
                    self.cells[index].cell.execution_count = Some(execution_count);
                    self.outputs_changed(id, cx);
                }
            }
            Event::Dead { message } => self.kernel_died(message, cx),
        }
    }

    fn on_output(
        &mut self,
        request: Option<u64>,
        parent: Option<&str>,
        msg_type: &str,
        content: &Value,
        cx: &mut Context<Self>,
    ) {
        // ipywidgets' Output widget captures the outputs of its `msg_id`.
        if let Some(model) = parent.and_then(|parent| self.widgets.capturing(parent)) {
            let model = model.to_owned();
            if self.widgets.capture(&model, msg_type, content) {
                self.widgets_changed(cx);
            }
            return;
        }
        // Outputs of widget callbacks (our comm messages) belong to no cell.
        let callback = parent.is_some_and(|parent| self.widgets.is_own(parent));
        let target = self
            .running
            .as_ref()
            .filter(|_| !callback)
            .filter(|running| request.is_none_or(|request| request == running.request))
            .and_then(|running| Some((running.cell, self.index_of(running.cell)?)));
        let applied = match (target, self.running.as_mut()) {
            (Some((id, index)), Some(running)) => {
                let applied = outputs::apply(
                    &mut self.cells[index].cell.outputs,
                    &mut running.state,
                    msg_type,
                    content,
                );
                if applied == Applied::Changed {
                    self.outputs_changed(id, cx);
                }
                applied
            }
            // Only display updates matter for cells that are done.
            _ => outputs::apply(
                &mut Vec::new(),
                &mut OutputState::default(),
                msg_type,
                content,
            ),
        };
        if let Applied::UpdateDisplay {
            display_id,
            data,
            metadata,
        } = applied
        {
            let changed: Vec<CellId> = self
                .cells
                .iter_mut()
                .filter_map(|cell| {
                    outputs::update_display(&mut cell.cell.outputs, &display_id, &data, &metadata)
                        .then_some(cell.id)
                })
                .collect();
            for id in changed {
                self.outputs_changed(id, cx);
            }
        }
    }

    fn on_reply(&mut self, request: u64, result: Result<Value, String>, cx: &mut Context<Self>) {
        let Some(running) = self.running.take_if(|running| running.request == request) else {
            return;
        };
        let ok = matches!(&result, Ok(reply) if reply["status"] == "ok");
        if let Some(index) = self.index_of(running.cell) {
            let cell = &mut self.cells[index].cell;
            match &result {
                Ok(reply) => {
                    if cell.execution_count.is_none() {
                        cell.execution_count = reply["execution_count"].as_u64();
                    }
                }
                Err(message) => cell.outputs.push(Output::Stream {
                    name: "stderr".into(),
                    text: format!("{message}\n"),
                }),
            }
            self.mark_finished(index, running.started.elapsed(), ok);
            self.outputs_changed(running.cell, cx);
        }
        // Jupyter stops at the first error; marimo only skips the failed
        // cell's descendants.
        if !ok && !self.is_marimo() {
            self.clear_queue(cx);
        }
        self.run_next(cx);
    }
}

enum Incoming {
    Event(Option<Event>),
    Reply(Reply),
}

/// Feeds a kernel's events and execute replies to the document, in the
/// order the bridge sent them (events are drained first: an execute's
/// outputs precede its reply).
async fn pump(
    this: WeakEntity<NotebookDocument>,
    generation: u64,
    mut events: UnboundedReceiver<Event>,
    mut replies: UnboundedReceiver<Reply>,
    cx: &mut AsyncApp,
) {
    loop {
        let incoming = futures::select_biased! {
            event = events.next() => Incoming::Event(event),
            reply = replies.next() => match reply {
                Some(reply) => Incoming::Reply(reply),
                None => return,
            },
        };
        let alive = this
            .update(cx, |this, cx| {
                if this.generation != generation {
                    return false;
                }
                match incoming {
                    Incoming::Event(Some(event)) => this.on_event(event, cx),
                    Incoming::Event(None) => {
                        this.kernel_died("The kernel bridge ended".into(), cx);
                        return false;
                    }
                    Incoming::Reply((request, result)) => this.on_reply(request, result, cx),
                }
                this.generation == generation
            })
            .unwrap_or(false);
        if !alive {
            return;
        }
    }
}

/// Resolves the launch, starts the bridge, waits for its `hello` and
/// starts the kernel in `cwd`.
async fn start_bridge(
    project: Project,
    path: PathBuf,
    launch: Option<KernelLaunch>,
    spawner: Spawner,
    cwd: String,
    this: &WeakEntity<NotebookDocument>,
    cx: &mut AsyncApp,
) -> Result<Bridge, KernelStatus> {
    let launch = match launch {
        Some(launch) => launch,
        None => {
            let (project, path) = (project.clone(), path.clone());
            let launch = cx
                .background_spawn(async move { kernel::default_launch(&project, &path) })
                .await;
            this.update(cx, |this, cx| {
                this.launch = Some(launch.clone());
                cx.emit(NotebookEvent::Kernel);
                cx.notify();
            })
            .map_err(|_| KernelStatus::NotStarted)?;
            launch
        }
    };
    let spawn_launch = launch.clone();
    let (kernel, mut events) = cx
        .background_spawn(async move { spawner(&project, &spawn_launch) })
        .await
        .map_err(|err| KernelStatus::Failed(format!("Cannot start {}: {err}", launch.label())))?;
    let (python, missing) = wait_for_hello(&mut events)
        .await
        .map_err(KernelStatus::Failed)?;
    if !missing.is_empty() {
        return Err(KernelStatus::Missing {
            python,
            modules: missing,
        });
    }
    let (_, reply) = kernel.request(Op::Start {
        kernel_name: launch.kernel_name.clone(),
        cwd,
    });
    match reply.await {
        Ok(Ok(_)) => Ok((kernel, events)),
        Ok(Err(message)) => Err(KernelStatus::Failed(message)),
        Err(_) => Err(KernelStatus::Failed(bridge_ended())),
    }
}

/// The bridge's `hello`: its interpreter and the modules it misses.
async fn wait_for_hello(
    events: &mut UnboundedReceiver<Event>,
) -> Result<(String, Vec<String>), String> {
    loop {
        match events.next().await {
            Some(Event::Hello {
                python, missing, ..
            }) => return Ok((python, missing)),
            Some(Event::Dead { message }) => return Err(message),
            Some(_) => {}
            None => return Err("The kernel bridge ended before it started".into()),
        }
    }
}

fn bridge_ended() -> String {
    "The kernel bridge ended".into()
}

/// `{name: {display_name, language, …}}` sorted by name.
fn kernel_specs_from(result: Result<Value, String>) -> Result<Vec<KernelSpecInfo>, String> {
    let value = result?;
    let specs = value
        .as_object()
        .ok_or_else(|| format!("unexpected kernelspecs reply: {value}"))?;
    let mut list: Vec<KernelSpecInfo> = specs
        .iter()
        .map(|(name, spec)| KernelSpecInfo {
            name: name.clone(),
            display_name: spec["display_name"].as_str().unwrap_or(name).to_owned(),
            language: spec["language"].as_str().unwrap_or_default().to_owned(),
        })
        .collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(list)
}

fn new_marimo_cell() -> MarimoCell {
    MarimoCell {
        name: "_".into(),
        ..MarimoCell::default()
    }
}

/// A copy of `cell` without outputs or execution count.
fn without_outputs(cell: &Cell) -> Cell {
    Cell {
        kind: cell.kind,
        source: cell.source.clone(),
        id: cell.id.clone(),
        execution_count: None,
        outputs: Vec::new(),
        metadata: cell.metadata.clone(),
        attachments: cell.attachments.clone(),
        extra: cell.extra.clone(),
        marimo: cell.marimo.clone(),
    }
}

/// For each cell of a reloaded file, the open cell (index in `old`) it
/// continues: first one of the same kind and text (in order), then a
/// marimo cell with the same name, then by position: the first unmatched
/// open cell of the same kind between the open cells its neighbours
/// matched (a cell edited in place). Unmatched: a new cell.
fn match_reloaded(old: &[DocCell], new: &[Cell]) -> Vec<Option<usize>> {
    let mut matched = vec![None; new.len()];
    let mut taken = vec![false; old.len()];
    let mut by_text: HashMap<(CellKind, &str), VecDeque<usize>> = HashMap::new();
    for (index, cell) in old.iter().enumerate() {
        by_text
            .entry((cell.cell.kind, cell.cell.source.as_str()))
            .or_default()
            .push_back(index);
    }
    for (slot, cell) in matched.iter_mut().zip(new) {
        if let Some(index) = by_text
            .get_mut(&(cell.kind, cell.source.as_str()))
            .and_then(VecDeque::pop_front)
        {
            *slot = Some(index);
            taken[index] = true;
        }
    }
    fn name(cell: &Cell) -> Option<&str> {
        cell.marimo
            .as_ref()
            .map(|marimo| marimo.name.as_str())
            .filter(|name| !name.is_empty() && *name != "_")
    }
    for (slot, cell) in matched.iter_mut().zip(new) {
        let Some(wanted) = name(cell).filter(|_| slot.is_none()) else {
            continue;
        };
        if let Some(index) = (0..old.len()).find(|&index| {
            !taken[index]
                && old[index].cell.kind == cell.kind
                && name(&old[index].cell) == Some(wanted)
        }) {
            *slot = Some(index);
            taken[index] = true;
        }
    }
    // The open cell matched by the next matched reloaded cell.
    let mut next_anchor = vec![old.len(); new.len()];
    let mut anchor = old.len();
    for index in (0..new.len()).rev() {
        next_anchor[index] = anchor;
        if let Some(matched) = matched[index] {
            anchor = matched;
        }
    }
    let mut low = 0;
    for (index, cell) in new.iter().enumerate() {
        if let Some(matched) = matched[index] {
            low = matched + 1;
            continue;
        }
        let found = (low..next_anchor[index])
            .find(|&candidate| !taken[candidate] && old[candidate].cell.kind == cell.kind);
        if let Some(found) = found {
            matched[index] = Some(found);
            taken[found] = true;
            low = found + 1;
        }
    }
    matched
}

/// `text` without its leading whitespace-only lines.
fn without_leading_blank_lines(mut text: &str) -> &str {
    while let Some(end) = text.find('\n') {
        if !text[..end].trim().is_empty() {
            break;
        }
        text = &text[end + 1..];
    }
    if text.trim().is_empty() { "" } else { text }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, PipeReader, PipeWriter, Write},
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc as std_mpsc,
        },
        thread,
    };

    use gpui::TestAppContext;
    use serde_json::json;

    use super::*;

    /// A scripted bridge: `hello` (with `missing`), then each `execute`
    /// prints its code; code `raise` fails, `die` kills the kernel and
    /// `sleep` runs until interrupted.
    fn fake_bridge(
        stdin: PipeReader,
        mut stdout: PipeWriter,
        missing: Vec<String>,
        requests: std_mpsc::Sender<Value>,
    ) {
        let mut send = |line: Value| {
            writeln!(stdout, "{line}").ok();
            stdout.flush().ok();
        };
        let mut lines = BufReader::new(stdin).lines();
        lines.next();
        send(
            json!({ "event": "hello", "python": "/fake/python", "version": "3.13", "missing": missing }),
        );
        let mut count = 0;
        let mut sleeping: Option<(Value, u64)> = None;
        let mut clicks = 0;
        for line in lines.map_while(Result::ok) {
            let request: Value = serde_json::from_str(&line).unwrap();
            let id = request["id"].clone();
            requests.send(request.clone()).ok();
            let output = |msg_type: &str, content: Value| json!({ "event": "output", "request": id, "msg_type": msg_type, "content": content });
            match request["op"].as_str().unwrap() {
                "execute" => {
                    let code = request["code"].as_str().unwrap();
                    if code == "die" {
                        send(json!({ "event": "dead", "message": "the kernel crashed" }));
                        return;
                    }
                    count += 1;
                    send(json!({ "event": "status", "state": "busy", "request": id }));
                    send(
                        json!({ "event": "execute_input", "request": id, "execution_count": count }),
                    );
                    if code == "sleep" {
                        sleeping = Some((id, count));
                        continue;
                    }
                    if code == "interact" {
                        fake_interact(&mut send, &id);
                        send(
                            json!({ "reply": id, "ok": true, "result": { "status": "ok", "execution_count": count } }),
                        );
                        send(json!({ "event": "status", "state": "idle", "request": id }));
                        continue;
                    }
                    let status = if code == "raise" {
                        send(output(
                            "error",
                            json!({ "ename": "ValueError", "evalue": "bad", "traceback": ["\u{1b}[31mValueError\u{1b}[0m: bad"] }),
                        ));
                        "error"
                    } else {
                        send(output(
                            "stream",
                            json!({ "name": "stdout", "text": format!("{code}\n") }),
                        ));
                        "ok"
                    };
                    send(
                        json!({ "reply": id, "ok": true, "result": { "status": status, "execution_count": count } }),
                    );
                    send(json!({ "event": "status", "state": "idle", "request": id }));
                }
                "comm_msg" => {
                    send(json!({ "reply": id, "ok": true, "result": null }));
                    clicks += fake_comm_msg(&mut send, &request, clicks);
                }
                "interrupt" => {
                    send(json!({ "reply": id, "ok": true, "result": null }));
                    if let Some((sleeper, count)) = sleeping.take() {
                        send(json!({
                            "event": "output", "request": sleeper, "msg_type": "error",
                            "content": { "ename": "KeyboardInterrupt", "evalue": "", "traceback": [] },
                        }));
                        send(
                            json!({ "reply": sleeper, "ok": true, "result": { "status": "error", "execution_count": count } }),
                        );
                    }
                }
                "restart" => {
                    count = 0;
                    send(json!({ "reply": id, "ok": true, "result": {} }));
                }
                "shutdown" => {
                    send(json!({ "reply": id, "ok": true, "result": null }));
                    return;
                }
                _ => send(json!({ "reply": id, "ok": true, "result": {} })),
            }
        }
    }

    /// What `interact(f, x=10)` beside a button and a label sends, in
    /// ipywidgets' order: the comms open, the first call's output is
    /// captured by the Output widget, then the box's view is displayed.
    fn fake_interact(send: &mut impl FnMut(Value), request: &Value) {
        let parent = format!("exec-{request}");
        let open = |id: &str, name: &str, mut state: Value| {
            state["_model_name"] = name.into();
            state["_model_module"] = "@jupyter-widgets/controls".into();
            state["_model_module_version"] = "2.0.0".into();
            json!({
                "event": "comm", "request": request, "parent": parent, "msg_type": "comm_open",
                "content": {"comm_id": id, "target_name": "jupyter.widget",
                            "data": {"state": state, "buffer_paths": []}},
            })
        };
        send(open(
            "slider",
            "IntSliderModel",
            json!({"value": 10, "min": 0, "max": 20}),
        ));
        send(open(
            "out",
            "OutputModel",
            json!({"msg_id": "", "outputs": []}),
        ));
        send(open(
            "button",
            "ButtonModel",
            json!({"description": "Click"}),
        ));
        send(open("label", "LabelModel", json!({"value": "clicked 0"})));
        send(open(
            "box",
            "VBoxModel",
            json!({"children": ["IPY_MODEL_slider", "IPY_MODEL_out", "IPY_MODEL_button", "IPY_MODEL_label"]}),
        ));
        fake_capture(send, request, &parent, "x=10\n");
        send(json!({
            "event": "output", "request": request, "parent": parent, "msg_type": "display_data",
            "content": {"metadata": {}, "data": {
                "text/plain": "VBox(children=(IntSlider(value=10), Output(), Button(), Label()))",
                "application/vnd.jupyter.widget-view+json": {"model_id": "box", "version_major": 2, "version_minor": 0},
            }},
        }));
    }

    /// An Output widget capturing `text` printed under `parent` after
    /// `clear_output(wait=True)`, as `interact` does.
    fn fake_capture(send: &mut impl FnMut(Value), request: &Value, parent: &str, text: &str) {
        let msg_id = |value: &str| {
            json!({
                "event": "comm", "request": request, "parent": parent, "msg_type": "comm_msg",
                "content": {"comm_id": "out", "data": {"method": "update", "state": {"msg_id": value}, "buffer_paths": []}},
            })
        };
        send(msg_id(parent));
        send(json!({
            "event": "output", "request": request, "parent": parent, "msg_type": "clear_output",
            "content": {"wait": true},
        }));
        send(json!({
            "event": "output", "request": request, "parent": parent, "msg_type": "stream",
            "content": {"name": "stdout", "text": text},
        }));
        send(msg_id(""));
    }

    /// The kernel's side of a frontend comm message: a slider update is
    /// echoed and re-runs the interact function; a button click updates
    /// the label and prints from its callback. Returns the clicks seen.
    fn fake_comm_msg(send: &mut impl FnMut(Value), request: &Value, clicks: usize) -> usize {
        let msg_id = request["msg_id"].as_str().unwrap();
        let data = &request["data"];
        let message = |comm_id: &str, method: &str, state: Value| {
            json!({
                "event": "comm", "parent": msg_id, "msg_type": "comm_msg",
                "content": {"comm_id": comm_id, "data": {"method": method, "state": state, "buffer_paths": []}},
            })
        };
        match data["method"].as_str() {
            Some("update") => {
                let comm_id = request["comm_id"].as_str().unwrap();
                send(message(comm_id, "echo_update", data["state"].clone()));
                let value = &data["state"]["value"];
                fake_capture(send, &Value::Null, msg_id, &format!("x={value}\n"));
                0
            }
            Some("custom") => {
                let value = format!("clicked {}", clicks + 1);
                send(message("label", "update", json!({ "value": value })));
                send(json!({
                    "event": "output", "parent": msg_id, "msg_type": "stream",
                    "content": {"name": "stdout", "text": "callback\n"},
                }));
                1
            }
            _ => 0,
        }
    }

    struct Harness {
        document: Entity<NotebookDocument>,
        requests: std_mpsc::Receiver<Value>,
        spawned: Arc<AtomicUsize>,
        events: std::rc::Rc<std::cell::RefCell<Vec<NotebookEvent>>>,
    }

    impl Harness {
        fn new(name: &str, text: &str, missing: &[&str], cx: &mut TestAppContext) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "next_code_notebook_document_{}_{name}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            let (sender, requests) = std_mpsc::channel();
            let spawned = Arc::new(AtomicUsize::new(0));
            let missing: Vec<String> = missing.iter().map(|m| m.to_string()).collect();
            let counter = spawned.clone();
            let spawner: Spawner = Arc::new(move |_, _| {
                let (bridge_in, kernel_out) = io::pipe()?;
                let (kernel_in, bridge_out) = io::pipe()?;
                let (missing, sender) = (missing.clone(), sender.clone());
                thread::spawn(move || fake_bridge(bridge_in, bridge_out, missing, sender));
                counter.fetch_add(1, Ordering::SeqCst);
                Kernel::from_pipes(Box::new(kernel_out), Box::new(kernel_in), Box::new(()))
            });
            let text = text.to_owned();
            let document = cx.new(|cx| {
                let notebook = NotebookDocument::parse(&path, &text).unwrap();
                let mut document = NotebookDocument::from_notebook(
                    Project::local(dir.clone()),
                    path,
                    notebook,
                    text.clone(),
                    cx,
                );
                document.set_spawner(spawner);
                document.set_launch(
                    KernelLaunch {
                        python: vec!["python".into()],
                        kernel_name: None,
                        cwd: dir,
                    },
                    cx,
                );
                document
            });
            let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let recorded = events.clone();
            cx.update(|cx| {
                cx.subscribe(&document, move |_, event: &NotebookEvent, _| {
                    recorded.borrow_mut().push(event.clone())
                })
                .detach()
            });
            Self {
                document,
                requests,
                spawned,
                events,
            }
        }

        /// Lets the fake bridge (a real thread) answer until `done`.
        fn wait(&self, cx: &mut TestAppContext, done: impl Fn(&NotebookDocument) -> bool) {
            for _ in 0..1000 {
                cx.run_until_parked();
                if self.document.read_with(cx, |document, _| done(document)) {
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
            panic!("the notebook did not get there");
        }

        fn settle(&self, cx: &mut TestAppContext) {
            self.wait(cx, |document| !document.is_busy());
        }

        fn ids(&self, cx: &mut TestAppContext) -> Vec<CellId> {
            self.document.read_with(cx, |document, _| {
                document.cells().iter().map(|c| c.id).collect()
            })
        }

        fn sources(&self, cx: &mut TestAppContext) -> Vec<String> {
            self.document.read_with(cx, |document, _| {
                document
                    .cells()
                    .iter()
                    .map(|c| c.cell.source.clone())
                    .collect()
            })
        }

        /// The code of each `execute` so far.
        fn executed(&self) -> Vec<String> {
            self.requests
                .try_iter()
                .filter(|request| request["op"] == "execute")
                .map(|request| request["code"].as_str().unwrap().to_owned())
                .collect()
        }

        fn cell(&self, index: usize, cx: &mut TestAppContext) -> DocCell {
            self.document
                .read_with(cx, |document, _| document.cells()[index].clone())
        }
    }

    fn ok(cell: &DocCell) -> Option<bool> {
        match cell.run {
            RunState::Finished { ok, .. } => Some(ok),
            _ => None,
        }
    }

    fn text(cell: &DocCell) -> String {
        cell.cell.outputs.iter().map(outputs::plain_text).collect()
    }

    fn ipynb(sources: &[(&str, &str)]) -> String {
        let cells: Vec<Value> = sources
            .iter()
            .map(|(kind, source)| match *kind {
                "code" => json!({
                    "cell_type": "code", "execution_count": null, "metadata": {},
                    "outputs": [], "source": source,
                }),
                kind => json!({ "cell_type": kind, "metadata": {}, "source": source }),
            })
            .collect();
        serde_json::to_string_pretty(&json!({
            "cells": cells, "metadata": {}, "nbformat": 4, "nbformat_minor": 4,
        }))
        .unwrap()
    }

    const MARIMO: &str = "import marimo\n\n__generated_with = \"0.25.1\"\napp = marimo.App()\n\n\n\
        @app.cell\ndef _():\n    a = 1\n    return (a,)\n\n\n\
        @app.cell\ndef _(a):\n    b = a + 1\n    return (b,)\n\n\n\
        @app.cell\ndef _(b):\n    print(b)\n    return\n\n\n\
        @app.cell\ndef _():\n    z = 5\n    return (z,)\n\n\n\
        if __name__ == \"__main__\":\n    app.run()\n";

    #[gpui::test]
    fn jupyter_runs_code_cells_in_order_and_stops_at_the_first_error(cx: &mut TestAppContext) {
        let h = Harness::new(
            "stop.ipynb",
            &ipynb(&[
                ("markdown", "# Title"),
                ("code", "first"),
                ("code", "raise"),
                ("code", "never"),
            ]),
            &[],
            cx,
        );
        h.document.update(cx, |document, cx| document.run_all(cx));
        h.settle(cx);
        assert_eq!(h.executed(), ["first", "raise"]);
        let (markdown, first, error, never) =
            (h.cell(0, cx), h.cell(1, cx), h.cell(2, cx), h.cell(3, cx));
        assert_eq!(markdown.run, RunState::Idle);
        assert_eq!(ok(&first), Some(true));
        assert_eq!(first.cell.execution_count, Some(1));
        assert_eq!(text(&first), "first\n");
        assert_eq!(ok(&error), Some(false));
        assert_eq!(error.cell.execution_count, Some(2));
        assert!(text(&error).contains("ValueError: bad"), "{}", text(&error));
        assert_eq!(never.run, RunState::Idle, "an error dequeues the rest");
        assert!(never.cell.outputs.is_empty());
        // The bridge reports idle after the reply.
        h.wait(cx, |document| {
            document.kernel_status() == &KernelStatus::Idle
        });
        assert!(
            h.document.read_with(cx, |document, _| document.is_dirty()),
            "ipynb files keep outputs"
        );

        // Running again replaces the outputs; counts go on.
        let first_id = first.id;
        h.document
            .update(cx, |document, cx| document.run(&[first_id], cx));
        h.settle(cx);
        let first = h.cell(1, cx);
        assert_eq!(first.cell.execution_count, Some(3));
        assert_eq!(text(&first), "first\n");
        assert_eq!(h.spawned.load(Ordering::SeqCst), 1, "one kernel");
    }

    #[gpui::test]
    fn interrupt_dequeues_and_stops_the_running_cell(cx: &mut TestAppContext) {
        let h = Harness::new(
            "interrupt.ipynb",
            &ipynb(&[("code", "sleep"), ("code", "after")]),
            &[],
            cx,
        );
        h.document.update(cx, |document, cx| document.run_all(cx));
        h.wait(cx, |document| {
            matches!(document.cells()[0].run, RunState::Running { .. })
                && document.cells()[0].cell.execution_count.is_some()
        });
        assert_eq!(h.cell(1, cx).run, RunState::Queued);
        h.document.update(cx, |document, cx| document.interrupt(cx));
        h.settle(cx);
        let (sleeper, after) = (h.cell(0, cx), h.cell(1, cx));
        assert_eq!(ok(&sleeper), Some(false));
        assert!(text(&sleeper).contains("KeyboardInterrupt"));
        assert_eq!(after.run, RunState::Idle);
        assert_eq!(h.executed(), ["sleep"]);
    }

    #[gpui::test]
    fn a_dead_kernel_fails_the_running_cell_and_the_next_run_starts_another(
        cx: &mut TestAppContext,
    ) {
        let h = Harness::new(
            "dead.ipynb",
            &ipynb(&[("code", "die"), ("code", "after")]),
            &[],
            cx,
        );
        h.document.update(cx, |document, cx| document.run_all(cx));
        h.settle(cx);
        let (died, after) = (h.cell(0, cx), h.cell(1, cx));
        assert_eq!(ok(&died), Some(false));
        assert!(text(&died).contains("DeadKernelError"), "{}", text(&died));
        assert_eq!(after.run, RunState::Idle);
        h.document.read_with(cx, |document, _| {
            assert!(matches!(document.kernel_status(), KernelStatus::Dead(_)));
        });

        h.document
            .update(cx, |document, cx| document.run(&[after.id], cx));
        h.settle(cx);
        assert_eq!(h.spawned.load(Ordering::SeqCst), 2);
        let after = h.cell(1, cx);
        assert_eq!(ok(&after), Some(true));
        assert_eq!(after.cell.execution_count, Some(1), "a fresh kernel");
    }

    #[gpui::test]
    fn missing_modules_leave_the_cells_idle(cx: &mut TestAppContext) {
        let h = Harness::new(
            "missing.ipynb",
            &ipynb(&[("code", "a"), ("code", "b")]),
            &["ipykernel"],
            cx,
        );
        h.document.update(cx, |document, cx| document.run_all(cx));
        h.settle(cx);
        h.document.read_with(cx, |document, _| {
            assert_eq!(
                document.kernel_status(),
                &KernelStatus::Missing {
                    python: "/fake/python".into(),
                    modules: vec!["ipykernel".into()],
                }
            );
            assert!(document.cells().iter().all(|c| c.run == RunState::Idle));
        });
        assert!(h.executed().is_empty());
    }

    #[gpui::test]
    fn structural_changes_undo_and_redo(cx: &mut TestAppContext) {
        let h = Harness::new(
            "undo.ipynb",
            &ipynb(&[("code", "one"), ("code", "two\n\nthree")]),
            &[],
            cx,
        );
        let ids = h.ids(cx);
        let d = &h.document;
        let inserted = d.update(cx, |d, cx| d.insert(1, CellKind::Markdown, "md".into(), cx));
        assert_eq!(h.sources(cx), ["one", "md", "two\n\nthree"]);
        assert!(d.update(cx, |d, cx| d.move_cells(&[inserted], -1, cx)));
        assert!(
            !d.update(cx, |d, cx| d.move_cells(&[inserted], -1, cx)),
            "at the top"
        );
        assert_eq!(h.sources(cx), ["md", "one", "two\n\nthree"]);
        let tail = d.update(cx, |d, cx| d.split(ids[1], 4, cx)).expect("split");
        assert_eq!(h.sources(cx), ["md", "one", "two", "three"]);
        d.update(cx, |d, cx| d.set_kind(tail, CellKind::Raw, cx));
        let copied = d.read_with(cx, |d, _| d.copy(&[ids[0], inserted]));
        let pasted = d.update(cx, |d, cx| d.paste(4, copied, cx));
        assert_eq!(pasted.len(), 2);
        assert_eq!(h.sources(cx), ["md", "one", "two", "three", "md", "one"]);
        let all = h.ids(cx);
        d.update(cx, |d, cx| d.delete(&all, cx));
        assert_eq!(h.sources(cx), [""], "a notebook keeps one cell");

        let mut expected: [&[&str]; 6] = [
            &["md", "one", "two", "three", "md", "one"],
            &["md", "one", "two", "three"],
            &["md", "one", "two", "three"],
            &["md", "one", "two\n\nthree"],
            &["one", "md", "two\n\nthree"],
            &["one", "two\n\nthree"],
        ];
        let kinds = |cx: &mut TestAppContext| {
            d.read_with(cx, |d, _| {
                d.cells().iter().map(|c| c.cell.kind).collect::<Vec<_>>()
            })
        };
        for (step, sources) in expected.iter().enumerate() {
            assert!(d.update(cx, |d, cx| d.undo_structure(cx)));
            assert_eq!(h.sources(cx), *sources, "undo {step}");
            if step == 1 {
                assert_eq!(kinds(cx)[3], CellKind::Raw);
            }
            if step == 2 {
                assert_eq!(kinds(cx)[3], CellKind::Code, "the kind change undone");
            }
        }
        assert!(!d.update(cx, |d, cx| d.undo_structure(cx)));
        assert_eq!(h.ids(cx), ids, "cells keep their ids");
        expected.reverse();
        for sources in expected.iter().skip(1) {
            assert!(d.update(cx, |d, cx| d.redo_structure(cx)));
            assert_eq!(h.sources(cx), *sources);
        }
        assert!(d.update(cx, |d, cx| d.redo_structure(cx)));
        assert_eq!(h.sources(cx), [""]);
        assert!(!d.update(cx, |d, cx| d.redo_structure(cx)));
        assert!(h.events.borrow().contains(&NotebookEvent::Structure));
        assert!(h.events.borrow().contains(&NotebookEvent::Dirty));
    }

    #[gpui::test]
    fn marimo_runs_stale_ancestors_and_descendants_in_dependency_order(cx: &mut TestAppContext) {
        let h = Harness::new("reactive.py", MARIMO, &[], cx);
        let ids = h.ids(cx);
        assert_eq!(ids.len(), 4);
        let d = &h.document;

        // A fresh kernel: the cell's ancestors run first; `z` is unrelated.
        d.update(cx, |d, cx| d.run(&[ids[2]], cx));
        h.settle(cx);
        assert_eq!(h.executed(), ["a = 1", "b = a + 1", "print(b)"]);
        assert_eq!(h.cell(3, cx).run, RunState::Idle);

        // Editing a cell marks it stale; running it re-runs its descendants.
        d.update(cx, |d, cx| d.set_source(ids[0], "a = 10".into(), None, cx));
        assert!(h.cell(0, cx).stale);
        let requests_before: Vec<Value> = h.requests.try_iter().collect();
        assert!(requests_before.is_empty());
        d.update(cx, |d, cx| d.run(&[ids[0]], cx));
        h.settle(cx);
        let executed: Vec<Value> = h
            .requests
            .try_iter()
            .filter(|r| r["op"] == "execute")
            .collect();
        let codes: Vec<&str> = executed
            .iter()
            .map(|r| r["code"].as_str().unwrap())
            .collect();
        assert_eq!(codes, ["a = 10", "b = a + 1", "print(b)"]);
        assert_eq!(executed[1]["marimo"]["defs"], json!(["b"]));
        assert_eq!(executed[1]["marimo"]["refs"], json!(["a"]));
        assert_eq!(
            executed[2]["marimo"]["refs"],
            json!(["b"]),
            "builtins are not refs"
        );
        assert_eq!(executed[1]["marimo"]["cell"], ids[1].to_string());
        assert!(!h.cell(0, cx).stale);

        // A second definition of `a`: both cells are in error and their
        // descendants are skipped; unrelated cells still run.
        let duplicate = d.update(cx, |d, cx| d.insert(4, CellKind::Code, "a = 2".into(), cx));
        let error = h.cell(0, cx).error.expect("multiple definitions");
        assert!(error.contains('a'), "{error}");
        assert!(h.cell(4, cx).error.is_some());
        d.update(cx, |d, cx| d.run_all(cx));
        h.settle(cx);
        assert_eq!(h.executed(), ["z = 5"]);
        for index in [0, 4] {
            let cell = h.cell(index, cx);
            assert_eq!(ok(&cell), Some(false));
            assert!(text(&cell).contains(MARIMO_ERROR), "{}", text(&cell));
        }
        for index in [1, 2] {
            let cell = h.cell(index, cx);
            assert_eq!(ok(&cell), Some(false));
            assert!(
                text(&cell).contains("An ancestor raised an exception (MarimoError)"),
                "{}",
                text(&cell)
            );
        }
        assert_eq!(ok(&h.cell(3, cx)), Some(true));

        // Removing it clears the errors.
        d.update(cx, |d, cx| d.delete(&[duplicate], cx));
        assert!(h.cell(0, cx).error.is_none());
    }

    #[gpui::test]
    fn saving_and_reloading_a_marimo_notebook_keeps_unchanged_outputs(cx: &mut TestAppContext) {
        let h = Harness::new("reload.py", MARIMO, &[], cx);
        let d = &h.document;
        d.update(cx, |d, cx| d.run_all(cx));
        h.settle(cx);
        assert!(
            !d.read_with(cx, |d, _| d.is_dirty()),
            "marimo files keep no outputs"
        );
        let z = h.ids(cx)[3];
        d.update(cx, |d, cx| d.set_source(z, "z = 6".into(), None, cx));
        assert!(d.read_with(cx, |d, _| d.is_dirty()));
        d.update(cx, |d, cx| d.save(cx)).unwrap();
        let path = d.read_with(cx, |d, _| d.path().to_owned());
        let saved = std::fs::read_to_string(&path).unwrap();
        // marimo returns only the definitions other cells use.
        assert!(saved.contains("    z = 6\n    return\n"), "{saved}");
        d.read_with(cx, |d, _| {
            assert!(!d.is_dirty());
            assert!(!d.changed_on_disk(&saved), "our own save");
        });

        // An external edit of `a`: the cell keeps its place, id and output
        // and turns stale with its descendants, as after a local edit.
        let edited = saved.replace("a = 1\n", "a = 3\n");
        assert!(d.read_with(cx, |d, _| d.changed_on_disk(&edited)));
        let ids = h.ids(cx);
        d.update(cx, |d, cx| d.reload(&edited, cx)).unwrap();
        assert!(h.events.borrow().contains(&NotebookEvent::Reloaded));
        assert_eq!(h.ids(cx), ids, "cells keep their ids");
        assert_eq!(h.sources(cx)[0], "a = 3");
        assert_eq!(text(&h.cell(0, cx)), "a = 1\n", "the old output stays");
        assert_eq!(text(&h.cell(2, cx)), "print(b)\n", "outputs kept");
        let stale: Vec<bool> = (0..4).map(|index| h.cell(index, cx).stale).collect();
        assert_eq!(
            stale,
            [true, true, true, true],
            "a, its descendants, and z (edited before)"
        );
        assert!(!d.read_with(cx, |d, _| d.is_dirty()));
        h.executed();
        d.update(cx, |d, cx| d.run(&[ids[0]], cx));
        h.settle(cx);
        assert_eq!(h.executed(), ["a = 3", "b = a + 1", "print(b)"]);
        assert!((0..3).all(|index| !h.cell(index, cx).stale));
    }

    #[gpui::test]
    fn reloading_marimo_matches_cells_by_text_name_and_position(cx: &mut TestAppContext) {
        let h = Harness::new("diff.py", MARIMO, &[], cx);
        let d = &h.document;
        d.update(cx, |d, cx| d.run_all(cx));
        h.settle(cx);
        let ids = h.ids(cx);
        let z = ids[3];
        d.update(cx, |d, cx| d.set_outputs_hidden(z, true, cx));
        assert!(
            !d.read_with(cx, |d, _| d.is_dirty()),
            "marimo files keep no collapsed state"
        );
        // Externally: a new unrelated cell, `b`'s cell edited, `z` moved to
        // the top as a named cell (`def zed`).
        let marimo_file = |cells: &[&str]| {
            let mut file =
                "import marimo\n\n__generated_with = \"0.25.1\"\napp = marimo.App()\n\n\n"
                    .to_owned();
            for cell in cells {
                file.push_str("@app.cell\n");
                file.push_str(cell);
                file.push_str("\n\n\n");
            }
            file.push_str("if __name__ == \"__main__\":\n    app.run()\n");
            file
        };
        let a = "def _():\n    a = 1\n    return (a,)";
        let c = "def _():\n    c = 1\n    return";
        let print = "def _(b):\n    print(b)\n    return";
        let b = "def _(a):\n    b = a * 100\n    return (b,)";
        let file = marimo_file(&["def zed():\n    z = 5\n    return", c, a, b, print]);
        d.update(cx, |d, cx| d.reload(&file, cx)).unwrap();
        let reloaded = h.ids(cx);
        assert_eq!(reloaded[0], z, "same text, moved");
        assert!(!ids.contains(&reloaded[1]), "a new cell");
        assert_eq!(reloaded[2..], ids[..3], "unchanged and edited in place");
        let stale: Vec<bool> = (0..5).map(|index| h.cell(index, cx).stale).collect();
        assert_eq!(stale, [false, false, false, true, true]);
        assert!(h.cell(0, cx).cell.outputs_hidden(), "kept in memory");
        assert_eq!(h.sources(cx)[3], "b = a * 100");
        assert_eq!(text(&h.cell(4, cx)), "print(b)\n", "outputs kept");
        h.executed();
        d.update(cx, |d, cx| d.run(&[reloaded[4]], cx));
        h.settle(cx);
        assert_eq!(
            h.executed(),
            ["b = a * 100", "print(b)"],
            "the stale ancestor runs first"
        );

        // A named cell edited and moved to the end is found by its name.
        let file = marimo_file(&[c, a, b, print, "def zed():\n    z = 7\n    return"]);
        d.update(cx, |d, cx| d.reload(&file, cx)).unwrap();
        assert_eq!(h.ids(cx)[4], z);
        assert!(h.cell(4, cx).stale);
        assert!(h.cell(4, cx).cell.outputs_hidden());
        assert_eq!(text(&h.cell(4, cx)), "z = 5\n");
    }

    /// `ipynb` cells with outputs: `(source, output text or "")`.
    fn ipynb_with_outputs(cells: &[(&str, &str)]) -> String {
        let cells: Vec<Value> = cells
            .iter()
            .map(|(source, output)| {
                let outputs = if output.is_empty() {
                    json!([])
                } else {
                    json!([{ "name": "stdout", "output_type": "stream", "text": output }])
                };
                let count = if output.is_empty() {
                    Value::Null
                } else {
                    json!(1)
                };
                json!({
                    "cell_type": "code", "execution_count": count, "metadata": {},
                    "outputs": outputs, "source": source,
                })
            })
            .collect();
        serde_json::to_string_pretty(&json!({
            "cells": cells, "metadata": {}, "nbformat": 4, "nbformat_minor": 4,
        }))
        .unwrap()
    }

    #[gpui::test]
    fn reloading_ipynb_keeps_outputs_of_unchanged_cells_the_file_has_none_for(
        cx: &mut TestAppContext,
    ) {
        let h = Harness::new(
            "reload.ipynb",
            &ipynb_with_outputs(&[("one", ""), ("two", ""), ("three", "")]),
            &[],
            cx,
        );
        let d = &h.document;
        d.update(cx, |d, cx| d.run_all(cx));
        h.settle(cx);
        d.update(cx, |d, cx| d.save(cx)).unwrap();
        let ids = h.ids(cx);
        // An output-stripping tool rewrote the file, changing `two`; `three`
        // has other outputs in the file.
        let stripped = ipynb_with_outputs(&[("one", ""), ("TWO", ""), ("three", "file\n")]);
        d.update(cx, |d, cx| d.reload(&stripped, cx)).unwrap();
        assert_eq!(h.ids(cx), ids);
        assert_eq!(text(&h.cell(0, cx)), "one\n", "kept");
        assert_eq!(h.cell(0, cx).cell.execution_count, Some(1));
        assert_eq!(text(&h.cell(1, cx)), "", "changed: the file's");
        assert_eq!(text(&h.cell(2, cx)), "file\n", "the file's outputs win");
        assert!(!h.cell(1, cx).stale, "Jupyter has no stale cells");
    }

    #[gpui::test]
    fn collapsed_state_is_saved_in_ipynb_and_read_back(cx: &mut TestAppContext) {
        let h = Harness::new(
            "collapse.ipynb",
            &ipynb_with_outputs(&[("one", "1\n"), ("two", "2\n")]),
            &[],
            cx,
        );
        let d = &h.document;
        let ids = h.ids(cx);
        d.update(cx, |d, cx| {
            d.set_outputs_hidden(ids[0], true, cx);
            d.set_source_hidden(ids[1], true, cx);
        });
        assert!(d.read_with(cx, |d, _| d.is_dirty()));
        assert!(h.events.borrow().contains(&NotebookEvent::Outputs(ids[0])));
        d.update(cx, |d, cx| d.save(cx)).unwrap();
        let path = d.read_with(cx, |d, _| d.path().to_owned());
        let saved = std::fs::read_to_string(&path).unwrap();
        let reopened = NotebookDocument::parse(&path, &saved).unwrap();
        let flags: Vec<(bool, bool)> = reopened
            .cells
            .iter()
            .map(|cell| (cell.outputs_hidden(), cell.source_hidden()))
            .collect();
        assert_eq!(flags, [(true, false), (false, true)]);
        let saved: Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(
            saved["cells"][0]["metadata"],
            json!({ "collapsed": true, "jupyter": { "outputs_hidden": true } })
        );
        // Expanding again leaves no `jupyter` key behind.
        d.update(cx, |d, cx| d.set_source_hidden(ids[1], false, cx));
        d.update(cx, |d, cx| d.save(cx)).unwrap();
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["cells"][1]["metadata"], json!({}));
    }

    #[gpui::test]
    fn widgets_follow_the_comm_stream_and_their_state_is_saved(cx: &mut TestAppContext) {
        let h = Harness::new(
            "widgets.ipynb",
            &ipynb(&[("code", "interact"), ("code", "sleep")]),
            &[],
            cx,
        );
        let d = &h.document;
        let ids = h.ids(cx);
        d.update(cx, |d, cx| d.run(&ids[..1], cx));
        h.settle(cx);
        // The cell shows the box; the first call's print went to the
        // Output widget.
        let cell = h.cell(0, cx);
        assert_eq!(cell.cell.outputs.len(), 1, "{:?}", cell.cell.outputs);
        assert_eq!(widgets::view_model_id(&cell.cell.outputs[0]), Some("box"));
        let captured = |cx: &mut TestAppContext| {
            d.read_with(cx, |d, _| {
                d.widgets()
                    .get("out")
                    .map(|out| {
                        out.outputs
                            .iter()
                            .map(outputs::plain_text)
                            .collect::<String>()
                    })
                    .unwrap_or_default()
            })
        };
        assert_eq!(captured(cx), "x=10\n");
        assert!(h.events.borrow().contains(&NotebookEvent::Outputs(ids[0])));

        // Moving the slider: the model changes at once, the kernel's echo
        // is ignored and the re-run's output replaces nothing in the cell.
        d.update(cx, |d, cx| {
            d.widget_update("slider", "value", json!(13), true, cx)
        });
        h.wait(cx, |d| {
            d.widgets().get("out").is_some_and(|out| {
                out.outputs.len() == 1 && outputs::plain_text(&out.outputs[0]).contains("x=13")
            })
        });
        let sent: Vec<Value> = h
            .requests
            .try_iter()
            .filter(|request| request["op"] == "comm_msg")
            .collect();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["comm_id"], "slider");
        assert_eq!(sent[0]["data"]["state"], json!({"value": 13}));
        assert!(sent[0]["msg_id"].as_str().is_some_and(|id| !id.is_empty()));
        assert_eq!(
            d.read_with(cx, |d, _| d.widgets().get("slider").unwrap().f64("value")),
            Some(13.)
        );

        // A click while another cell runs: the label updates and the
        // callback's print does not land in the running cell.
        d.update(cx, |d, cx| d.run(&ids[1..], cx));
        h.wait(cx, |d| matches!(d.cells()[1].run, RunState::Running { .. }));
        d.update(cx, |d, _| {
            d.widget_custom("button", json!({"event": "click"}))
        });
        h.wait(cx, |d| {
            d.widgets().get("label").unwrap().str("value") == "clicked 1"
        });
        d.update(cx, |d, cx| d.interrupt(cx));
        h.settle(cx);
        assert!(!text(&h.cell(1, cx)).contains("callback"));

        // Saved like Jupyter; a reopened notebook shows the last values
        // without a kernel.
        d.update(cx, |d, cx| d.save(cx)).unwrap();
        let path = d.read_with(cx, |d, _| d.path().to_owned());
        let saved = std::fs::read_to_string(&path).unwrap();
        let json: Value = serde_json::from_str(&saved).unwrap();
        let state = &json["metadata"]["widgets"][widgets::STATE_MIME]["state"];
        assert_eq!(state["slider"]["state"]["value"], 13);
        assert_eq!(state["label"]["state"]["value"], "clicked 1");
        assert_eq!(
            state["out"]["state"]["outputs"][0]["text"],
            json!(["x=13\n"])
        );
        let reopened = cx.new(|cx| {
            let notebook = NotebookDocument::parse(&path, &saved).unwrap();
            NotebookDocument::from_notebook(
                Project::local(path.parent().unwrap().to_owned()),
                path.clone(),
                notebook,
                saved.clone(),
                cx,
            )
        });
        reopened.read_with(cx, |d, _| {
            let slider = d.widgets().get("slider").unwrap();
            assert!(!slider.live);
            assert_eq!(slider.f64("value"), Some(13.));
            assert_eq!(
                d.widgets().get("box").unwrap().references("children").len(),
                4
            );
        });
        // A restart ends the comms: the models stay as static state.
        d.update(cx, |d, cx| d.restart(false, cx));
        h.settle(cx);
        assert!(d.read_with(cx, |d, _| !d.widgets().get("slider").unwrap().live));
    }
}
