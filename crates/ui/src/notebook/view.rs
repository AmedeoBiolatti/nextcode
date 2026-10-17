//! A notebook tab: a toolbar (run, interrupt, restart, kernel status and
//! interpreter), banners for kernel problems, and the cells in a
//! virtualized list. Code cells are `Editor::notebook_cell` editors,
//! created when a cell is first rendered; Markdown cells render until
//! edited. Jupyter's modes: command mode (focus on the view, keys act on
//! the selected cells) and edit mode (focus in a cell's editor). Several
//! views (split panes) can show one [`NotebookDocument`].

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::PathBuf,
    rc::Rc,
    time::Duration,
};

use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, ElementId, Entity, EventEmitter,
    FocusHandle, Focusable, Global, Hsla, KeyBinding, ListAlignment, ListState, MouseButton,
    SharedString, Stateful, Subscription, Task, Window, actions, div, list, prelude::*, px,
};
use notebook::{Cell, CellKind};
use syntax::Language;

use super::{
    CellId, KernelStatus, NotebookDocument, NotebookEvent, RunState,
    markdown::{MarkdownImages, markdown},
    output::{OutputAction, OutputCache, OutputOptions, open_html, render_output},
    picker::{InterpreterPicker, InterpreterPickerEvent},
    widgets::{self, WidgetInputs, WidgetRender, WidgetSink, apply_event, input_specs},
};
use crate::{
    editor::{CompletionProvider, Editor, EditorEvent},
    selectable::selectable,
    theme,
};

actions!(
    notebook,
    [
        RunCell,
        RunCellAndSelectNext,
        RunCellAndInsertBelow,
        RunAll,
        RunAbove,
        RunBelow,
        Interrupt,
        Restart,
        RestartAndRunAll,
        ClearOutputs,
        Save,
        CommandMode,
        EditMode,
        InsertAbove,
        InsertBelow,
        DeleteCells,
        UndoCells,
        RedoCells,
        ToCode,
        ToMarkdown,
        ToRaw,
        SelectPrevious,
        SelectNext,
        ExtendSelectionUp,
        ExtendSelectionDown,
        SelectFirst,
        SelectLast,
        CopyCells,
        CutCells,
        PasteBelow,
        PasteAbove,
        SplitCell,
        MoveCellsUp,
        MoveCellsDown,
        ToggleOutputs,
        ToggleSource,
        ChooseInterpreter,
        OpenInMarimo,
        ShowHelp,
        OpenAsText,
        OpenAsNotebook
    ]
);

#[cfg(test)]
mod tests;

mod language_server;

const CONTEXT: &str = "Notebook";
/// Width of the column left of each cell (execution count, run button,
/// elapsed time).
const GUTTER_WIDTH: f32 = 72.;
/// From a cell's top to its editor's text: the cell's and the editor box's
/// padding and border.
const CELL_TEXT_TOP: f32 = 9.;
/// How often a running cell's elapsed time redraws.
const TICK: Duration = Duration::from_millis(100);

/// Bound after `editor::bind_keys`: the cell editors' bindings at the same
/// depth win over the editor's own.
pub fn bind_keys(cx: &mut App) {
    let command = Some("Notebook && command");
    let cell = Some("Editor && notebook_cell");
    cx.bind_keys([
        // Edit mode (in a cell's editor).
        KeyBinding::new("shift-enter", RunCellAndSelectNext, cell),
        KeyBinding::new("secondary-enter", RunCell, cell),
        KeyBinding::new("alt-enter", RunCellAndInsertBelow, cell),
        KeyBinding::new("escape", CommandMode, Some("Editor && cell_escape")),
        KeyBinding::new("secondary-shift--", SplitCell, cell),
        KeyBinding::new("secondary-_", SplitCell, cell),
        // VS Code's Show Hover: the kernel's help for the name at the cursor.
        KeyBinding::new("secondary-k secondary-i", ShowHelp, cell),
        // Command mode (focus on the notebook).
        KeyBinding::new("shift-enter", RunCellAndSelectNext, command),
        KeyBinding::new("secondary-enter", RunCell, command),
        KeyBinding::new("alt-enter", RunCellAndInsertBelow, command),
        KeyBinding::new("enter", EditMode, command),
        KeyBinding::new("secondary-s", Save, command),
        KeyBinding::new("a", InsertAbove, command),
        KeyBinding::new("b", InsertBelow, command),
        KeyBinding::new("d d", DeleteCells, command),
        KeyBinding::new("z", UndoCells, command),
        KeyBinding::new("shift-z", RedoCells, command),
        KeyBinding::new("secondary-shift-z", RedoCells, command),
        KeyBinding::new("y", ToCode, command),
        KeyBinding::new("m", ToMarkdown, command),
        KeyBinding::new("r", ToRaw, command),
        KeyBinding::new("up", SelectPrevious, command),
        KeyBinding::new("k", SelectPrevious, command),
        KeyBinding::new("down", SelectNext, command),
        KeyBinding::new("j", SelectNext, command),
        KeyBinding::new("shift-up", ExtendSelectionUp, command),
        KeyBinding::new("shift-k", ExtendSelectionUp, command),
        KeyBinding::new("shift-down", ExtendSelectionDown, command),
        KeyBinding::new("shift-j", ExtendSelectionDown, command),
        KeyBinding::new("home", SelectFirst, command),
        KeyBinding::new("end", SelectLast, command),
        KeyBinding::new("c", CopyCells, command),
        KeyBinding::new("x", CutCells, command),
        KeyBinding::new("v", PasteBelow, command),
        KeyBinding::new("shift-v", PasteAbove, command),
        KeyBinding::new("secondary-shift-up", MoveCellsUp, command),
        KeyBinding::new("secondary-shift-down", MoveCellsDown, command),
        KeyBinding::new("o", ToggleOutputs, command),
        KeyBinding::new("i i", Interrupt, command),
        KeyBinding::new("0 0", Restart, command),
    ]);
    language_server::bind_keys(cx);
    widgets::bind_keys(cx);
}

/// Cells copied with C/X, shared by every notebook of the app.
#[derive(Default)]
struct CellClipboard(Vec<Cell>);

impl Global for CellClipboard {}

pub enum NotebookViewEvent {
    /// The dirty state or the kernel changed: the tab redraws.
    Changed,
    /// Show the file in a text editor instead (marimo).
    OpenAsText,
    /// Run `marimo edit` on the file and open it in the browser.
    OpenInMarimo,
    /// Run the command installing ipykernel in a terminal.
    InstallKernel,
    /// Vim's `:q` in a cell (`discard`: `:q!`).
    Close { discard: bool },
    /// Go to (or pick among) a cell editor's language-server locations
    /// (in the notebook's virtual document or other files).
    Navigate {
        locations: Vec<language::Location>,
        title: SharedString,
    },
    /// A cell editor's rename or code action edit (see
    /// `EditorEvent::ApplyEdit`).
    ApplyEdit {
        edit: lsp::types::WorkspaceEdit,
        server: std::sync::Arc<crate::lsp_store::RunningServer>,
        command: Option<lsp::types::Command>,
        label: SharedString,
    },
}

impl EventEmitter<NotebookViewEvent> for NotebookView {}

/// What the view keeps per cell.
struct CellState {
    kind: CellKind,
    editor: Option<Entity<Editor>>,
    _subscriptions: Vec<Subscription>,
    /// A Markdown cell shows its editor instead of the rendered text.
    editing: bool,
    /// "Show more" was clicked: long text outputs show in full.
    expanded: bool,
    /// The kernel's help for the name at the cursor (Ctrl+K Ctrl+I).
    help: Option<SharedString>,
}

impl CellState {
    fn new(kind: CellKind) -> Self {
        Self {
            kind,
            editor: None,
            _subscriptions: Vec::new(),
            editing: false,
            expanded: false,
            help: None,
        }
    }
}

/// A message under the toolbar.
struct Notice {
    text: SharedString,
    error: bool,
}

pub struct NotebookView {
    document: Entity<NotebookDocument>,
    focus_handle: FocusHandle,
    list: ListState,
    /// Cell ids in the order `list` knows them.
    order: Vec<CellId>,
    cells: HashMap<CellId, CellState>,
    /// The selected cell (where keys act); `anchor` is the other end of a
    /// range selection.
    cursor: Option<CellId>,
    anchor: Option<CellId>,
    /// Scroll the cursor's cell into view on the next frame.
    reveal: bool,
    /// Set while the view writes a document change into an editor, so the
    /// editor's `Edited` is not sent back.
    syncing: bool,
    output_cache: OutputCache,
    /// Text fields of ipywidgets shown in this view.
    widget_inputs: WidgetInputs,
    /// Text/number fields of marimo UI elements shown in this view.
    marimo_inputs: WidgetInputs,
    /// `notebook.output_max_lines`, read once per frame.
    output_max_lines: usize,
    notice: Option<Notice>,
    /// Progress of Open in marimo / Install ipykernel.
    task_status: Option<Notice>,
    picker: Option<(Entity<InterpreterPicker>, Subscription)>,
    /// Redraws while a cell runs (live elapsed time).
    ticker: Option<Task<()>>,
    /// The language-server document of the cells (`set_language_server`).
    lsp: Option<(Entity<super::lsp::NotebookLsp>, Subscription)>,
    _subscriptions: Vec<Subscription>,
}

impl NotebookView {
    pub fn new(
        document: Entity<NotebookDocument>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe_in(&document, window, |this, _, event, window, cx| {
            this.on_document_event(event, window, cx)
        });
        let order: Vec<CellId> = document.read(cx).cells().iter().map(|c| c.id).collect();
        let cells = document
            .read(cx)
            .cells()
            .iter()
            .map(|cell| (cell.id, CellState::new(cell.cell.kind)))
            .collect();
        let list = ListState::new(order.len(), ListAlignment::Top, px(600.));
        let cursor = order.first().copied();
        let mut view = Self {
            document,
            focus_handle: cx.focus_handle(),
            list,
            order,
            cells,
            cursor,
            anchor: cursor,
            reveal: false,
            syncing: false,
            output_cache: OutputCache::default(),
            widget_inputs: WidgetInputs::default(),
            marimo_inputs: WidgetInputs::default(),
            output_max_lines: 200,
            notice: None,
            task_status: None,
            picker: None,
            ticker: None,
            lsp: None,
            _subscriptions: vec![subscription],
        };
        view.update_ticker(cx);
        view
    }

    pub fn document(&self) -> &Entity<NotebookDocument> {
        &self.document
    }

    pub fn path(&self, cx: &App) -> PathBuf {
        self.document.read(cx).path().to_path_buf()
    }

    pub fn is_dirty(&self, cx: &App) -> bool {
        self.document.read(cx).is_dirty()
    }

    /// Shows the progress or outcome of a long task (Open in marimo,
    /// Install ipykernel); `None` clears it.
    pub fn set_task_status(&mut self, status: Option<(String, bool)>, cx: &mut Context<Self>) {
        self.task_status = status.map(|(text, error)| Notice {
            text: text.into(),
            error,
        });
        cx.notify();
    }

    /// Saves the document; false (with a notice) when writing failed.
    pub fn save_document(&mut self, cx: &mut Context<Self>) -> bool {
        match self.document.update(cx, |document, cx| document.save(cx)) {
            Ok(()) => {
                self.notice = None;
                cx.notify();
                true
            }
            Err(err) => {
                self.notice = Some(Notice {
                    text: format!("Could not save: {err}").into(),
                    error: true,
                });
                cx.notify();
                false
            }
        }
    }

    /// The selected cell's index and the selection's range (tests).
    #[cfg(test)]
    pub(crate) fn selection(&self, cx: &App) -> (Option<usize>, Range<usize>) {
        let document = self.document.read(cx);
        (
            self.cursor.and_then(|id| document.index_of(id)),
            self.selected_range(document),
        )
    }

    /// Cells with an editor (tests: editors are created lazily).
    #[cfg(test)]
    pub(crate) fn editor_count(&self) -> usize {
        self.cells
            .values()
            .filter(|state| state.editor.is_some())
            .count()
    }

    #[cfg(test)]
    pub(crate) fn cell_editor(&self, id: CellId) -> Option<Entity<Editor>> {
        self.cells.get(&id)?.editor.clone()
    }

    #[cfg(test)]
    pub(crate) fn is_editing_markdown(&self, id: CellId) -> bool {
        self.cells.get(&id).is_some_and(|state| state.editing)
    }

    fn on_document_event(&mut self, event: &NotebookEvent, _: &mut Window, cx: &mut Context<Self>) {
        match event {
            NotebookEvent::Structure => self.sync_cells(cx),
            NotebookEvent::Reloaded => {
                self.sync_cells(cx);
                let ids: Vec<CellId> = self.order.clone();
                for id in ids {
                    self.sync_editor(id, cx);
                }
                self.list.splice(0..self.order.len(), self.order.len());
                cx.emit(NotebookViewEvent::Changed);
            }
            NotebookEvent::Source { cell, origin } => {
                let ours = self
                    .cells
                    .get(cell)
                    .and_then(|state| state.editor.as_ref())
                    .map(|editor| editor.entity_id());
                if ours.is_none() || ours != *origin {
                    self.sync_editor(*cell, cx);
                }
                self.splice_cell(*cell);
            }
            NotebookEvent::Outputs(cell) => {
                self.splice_cell(*cell);
                self.update_ticker(cx);
            }
            NotebookEvent::Kernel => {
                cx.emit(NotebookViewEvent::Changed);
                self.update_ticker(cx);
            }
            NotebookEvent::Dirty => cx.emit(NotebookViewEvent::Changed),
        }
        cx.notify();
    }

    /// Brings the list and per-cell state up to date with the document's
    /// cells (inserted, deleted, moved, kinds changed).
    fn sync_cells(&mut self, cx: &mut Context<Self>) {
        let document = self.document.read(cx);
        let new: Vec<CellId> = document.cells().iter().map(|cell| cell.id).collect();
        let prefix = self
            .order
            .iter()
            .zip(&new)
            .take_while(|(old, new)| old == new)
            .count();
        let room = self.order.len().min(new.len()) - prefix;
        let suffix = self
            .order
            .iter()
            .rev()
            .zip(new.iter().rev())
            .take(room)
            .take_while(|(old, new)| old == new)
            .count();
        self.list.splice(
            prefix..self.order.len() - suffix,
            new.len() - prefix - suffix,
        );
        for (index, cell) in document.cells().iter().enumerate() {
            let state = self
                .cells
                .entry(cell.id)
                .or_insert_with(|| CellState::new(cell.cell.kind));
            if state.kind != cell.cell.kind {
                // Another language (or none): a new editor when shown.
                *state = CellState::new(cell.cell.kind);
                self.list.splice(index..index + 1, 1);
            }
        }
        let live: HashSet<CellId> = new.iter().copied().collect();
        self.cells.retain(|id, _| live.contains(id));
        // A deleted selection moves to the cell now where it was.
        let fallback = |id: Option<CellId>, order: &[CellId]| -> Option<CellId> {
            match id {
                Some(id) if live.contains(&id) => Some(id),
                Some(id) => {
                    let old = order.iter().position(|other| *other == id).unwrap_or(0);
                    new.get(old.min(new.len().saturating_sub(1))).copied()
                }
                None => new.first().copied(),
            }
        };
        let cursor = fallback(self.cursor, &self.order);
        if self.anchor.is_none_or(|anchor| !live.contains(&anchor)) {
            self.anchor = cursor;
        }
        if cursor != self.cursor {
            self.reveal = true;
        }
        self.cursor = cursor;
        self.order = new;
    }

    /// Writes the cell's source into its editor if they differ.
    fn sync_editor(&mut self, id: CellId, cx: &mut Context<Self>) {
        let Some(editor) = self.cells.get(&id).and_then(|state| state.editor.clone()) else {
            return;
        };
        let Some(source) = self
            .document
            .read(cx)
            .cell(id)
            .map(|cell| cell.cell.source.clone())
        else {
            return;
        };
        if editor.read(cx).text() == source {
            return;
        }
        self.syncing = true;
        editor.update(cx, |editor, cx| {
            let cursor = editor.cursor();
            editor.set_text(&source, cx);
            editor.set_cursor(cursor, cx);
        });
        self.syncing = false;
    }

    /// Scrolls cell `index` into view: exactly when it was drawn (or is in
    /// the overdraw), else (a jump, e.g. End) it goes to the top.
    fn reveal_cell(&self, index: usize) {
        let top = self.list.logical_scroll_top().item_ix;
        if index > top && self.list.bounds_for_item(index).is_some() {
            self.list.scroll_to_reveal_item(index);
        } else {
            self.list.scroll_to(gpui::ListOffset {
                item_ix: index,
                offset_in_item: px(0.),
            });
        }
    }

    /// The list measures the cell again (its height may have changed).
    fn splice_cell(&mut self, id: CellId) {
        if let Some(index) = self.order.iter().position(|other| *other == id) {
            self.list.splice(index..index + 1, 1);
        }
    }

    /// Redraws every [`TICK`] while a cell runs.
    fn update_ticker(&mut self, cx: &mut Context<Self>) {
        let running = self
            .document
            .read(cx)
            .cells()
            .iter()
            .any(|cell| matches!(cell.run, RunState::Running { .. }));
        if !running {
            self.ticker = None;
        } else if self.ticker.is_none() {
            self.ticker = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(TICK).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        }
    }

    /// The cell's editor, created on first use.
    fn ensure_editor(
        &mut self,
        id: CellId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Editor>> {
        if let Some(editor) = self.cells.get(&id).and_then(|state| state.editor.clone()) {
            return Some(editor);
        }
        let (source, kind) = {
            let cell = self.document.read(cx).cell(id)?;
            (cell.cell.source.clone(), cell.cell.kind)
        };
        let language = match kind {
            CellKind::Code => Some(Language::Python),
            CellKind::Markdown => Some(Language::Markdown),
            CellKind::Raw => None,
        };
        let provider = (kind == CellKind::Code).then(|| self.completion_provider());
        let editor = cx.new(|cx| {
            let mut editor = Editor::notebook_cell(&source, language, cx);
            editor.set_completion_provider(provider);
            editor
        });
        let events = cx.subscribe_in(&editor, window, move |this, editor, event, window, cx| {
            this.on_editor_event(id, editor, event, window, cx)
        });
        let focus = editor.focus_handle(cx);
        let focused = cx.on_focus_in(&focus, window, move |this, _, cx| {
            this.editor_focused(id, cx);
        });
        let state = self.cells.entry(id).or_insert_with(|| CellState::new(kind));
        state.editor = Some(editor.clone());
        state._subscriptions = vec![events, focused];
        self.link_editor(id, cx);
        Some(editor)
    }

    /// Kernel completions for code cells.
    fn completion_provider(&self) -> CompletionProvider {
        let document = self.document.downgrade();
        Rc::new(move |code: String, offset: usize, cx: &mut App| {
            let Some(document) = document.upgrade() else {
                return Task::ready(None);
            };
            let task = document.update(cx, |document, cx| document.complete(code, offset, cx));
            cx.foreground_executor().spawn(async move {
                let completion = task.await?;
                Some((completion.matches, completion.start..completion.end))
            })
        })
    }

    fn on_editor_event(
        &mut self,
        id: CellId,
        editor: &Entity<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::Edited if !self.syncing => {
                let text = editor.read(cx).text();
                let origin = Some(editor.entity_id());
                self.document
                    .update(cx, |document, cx| document.set_source(id, text, origin, cx));
                self.follow_cursor(id, window, cx);
                if let Some(state) = self.cells.get_mut(&id) {
                    state.help = None;
                }
            }
            EditorEvent::CellBoundary { up } => self.cross_boundary(id, *up, window, cx),
            EditorEvent::Close { discard } => {
                cx.emit(NotebookViewEvent::Close { discard: *discard })
            }
            EditorEvent::Navigate { locations, title } => cx.emit(NotebookViewEvent::Navigate {
                locations: locations.clone(),
                title: title.clone(),
            }),
            EditorEvent::ApplyEdit {
                edit,
                server,
                command,
                label,
            } => cx.emit(NotebookViewEvent::ApplyEdit {
                edit: edit.clone(),
                server: server.clone(),
                command: command.clone(),
                label: label.clone(),
            }),
            _ => {}
        }
    }

    /// After the next frame (the editor laid out its edit), scrolls the
    /// list so the cursor of cell `id`'s editor stays visible, also in a
    /// cell taller than the view.
    fn follow_cursor(&mut self, id: CellId, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        window.on_next_frame(move |_, cx| {
            this.update(cx, |this, cx| {
                let Some(editor) = this.cells.get(&id).and_then(|state| state.editor.clone())
                else {
                    return;
                };
                let Some(index) = this.order.iter().position(|other| *other == id) else {
                    return;
                };
                let Some(item) = this.list.bounds_for_item(index) else {
                    return;
                };
                let viewport = this.list.viewport_bounds();
                let (top, height) = editor.read(cx).cursor_row_bounds();
                let top = item.top() + px(CELL_TEXT_TOP) + top;
                let delta = if top < viewport.top() {
                    top - viewport.top()
                } else if top + height > viewport.bottom() {
                    top + height - viewport.bottom()
                } else {
                    return;
                };
                this.list.scroll_by(delta);
                cx.notify();
            })
            .ok();
        });
    }

    /// Focus entered a cell's editor (a click, or the view moved it).
    fn editor_focused(&mut self, id: CellId, cx: &mut Context<Self>) {
        if self.cursor != Some(id) {
            self.cursor = Some(id);
            self.anchor = Some(id);
            cx.notify();
        }
    }

    /// Up on a cell's first row / Down on its last: the neighbouring cell
    /// in edit mode.
    fn cross_boundary(
        &mut self,
        id: CellId,
        up: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let document = self.document.read(cx);
        let Some(index) = document.index_of(id) else {
            return;
        };
        let target = if up {
            index.checked_sub(1)
        } else {
            Some(index + 1)
        };
        let Some(target) = target.and_then(|index| document.cells().get(index)) else {
            return;
        };
        let target = target.id;
        if let Some(state) = self.cells.get_mut(&id)
            && state.kind == CellKind::Markdown
        {
            state.editing = false;
        }
        if let Some(editor) = self.enter_edit(target, window, cx) {
            editor.update(cx, |editor, cx| {
                let offset = if up { editor.buffer().len() } else { 0 };
                editor.set_cursor(offset, cx);
            });
            self.follow_cursor(target, window, cx);
        }
    }

    // Selection.

    /// Indices of the selected cells.
    fn selected_range(&self, document: &NotebookDocument) -> Range<usize> {
        let Some(cursor) = self.cursor.and_then(|id| document.index_of(id)) else {
            return 0..0;
        };
        let anchor = self
            .anchor
            .and_then(|id| document.index_of(id))
            .unwrap_or(cursor);
        cursor.min(anchor)..cursor.max(anchor) + 1
    }

    fn selected_ids(&self, cx: &App) -> Vec<CellId> {
        let document = self.document.read(cx);
        document.cells()[self.selected_range(document)]
            .iter()
            .map(|cell| cell.id)
            .collect()
    }

    fn select(&mut self, id: CellId, extend: bool, cx: &mut Context<Self>) {
        self.cursor = Some(id);
        if !extend || self.anchor.is_none() {
            self.anchor = Some(id);
        }
        self.reveal = true;
        cx.notify();
    }

    fn select_index(&mut self, index: usize, extend: bool, cx: &mut Context<Self>) {
        let id = self
            .document
            .read(cx)
            .cells()
            .get(index)
            .map(|cell| cell.id);
        if let Some(id) = id {
            self.select(id, extend, cx);
        }
    }

    fn cursor_index(&self, cx: &App) -> Option<usize> {
        self.document.read(cx).index_of(self.cursor?)
    }

    /// Edit mode in cell `id`: its editor (a Markdown cell's instead of
    /// the rendered text) takes focus.
    fn enter_edit(
        &mut self,
        id: CellId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Editor>> {
        self.select(id, false, cx);
        if let Some(state) = self.cells.get_mut(&id) {
            state.editing = state.kind == CellKind::Markdown;
        }
        // A collapsed source shows again to be edited.
        self.document
            .update(cx, |document, cx| document.set_source_hidden(id, false, cx));
        let editor = self.ensure_editor(id, window, cx)?;
        window.focus(&editor.focus_handle(cx));
        Some(editor)
    }

    /// Command mode: focus back on the view; a Markdown cell being edited
    /// renders again.
    fn enter_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.cursor.and_then(|id| self.cells.get_mut(&id)) {
            state.editing = false;
        }
        window.focus(&self.focus_handle);
        cx.notify();
    }

    // Running.

    /// Runs the selected code cells and renders the selected Markdown;
    /// the indices run.
    fn run_selection(&mut self, cx: &mut Context<Self>) -> Range<usize> {
        let document = self.document.read(cx);
        let range = self.selected_range(document);
        let mut code = Vec::new();
        for cell in &document.cells()[range.clone()] {
            match cell.cell.kind {
                CellKind::Code => code.push(cell.id),
                _ => {
                    if let Some(state) = self.cells.get_mut(&cell.id) {
                        state.editing = false;
                    }
                }
            }
        }
        if !code.is_empty() {
            self.document
                .update(cx, |document, cx| document.run(&code, cx));
        }
        range
    }

    fn run_cell(&mut self, _: &RunCell, window: &mut Window, cx: &mut Context<Self>) {
        self.run_selection(cx);
        self.enter_command(window, cx);
    }

    /// Shift+Enter: run, then select the next cell; past the last one a
    /// new code cell is inserted and edited (as Jupyter does).
    fn run_and_select_next(
        &mut self,
        _: &RunCellAndSelectNext,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.run_selection(cx);
        let count = self.document.read(cx).cells().len();
        if range.end < count {
            self.enter_command(window, cx);
            self.select_index(range.end, false, cx);
        } else {
            let id = self.document.update(cx, |document, cx| {
                document.insert(count, CellKind::Code, String::new(), cx)
            });
            self.enter_edit(id, window, cx);
        }
    }

    /// Alt+Enter: run, then edit a new code cell below.
    fn run_and_insert_below(
        &mut self,
        _: &RunCellAndInsertBelow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = self.run_selection(cx);
        let id = self.document.update(cx, |document, cx| {
            document.insert(range.end, CellKind::Code, String::new(), cx)
        });
        self.enter_edit(id, window, cx);
    }

    fn run_all(&mut self, _: &RunAll, _: &mut Window, cx: &mut Context<Self>) {
        self.document
            .update(cx, |document, cx| document.run_all(cx));
    }

    fn run_above(&mut self, _: &RunAbove, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.cursor {
            self.document
                .update(cx, |document, cx| document.run_above(id, cx));
        }
    }

    fn run_below(&mut self, _: &RunBelow, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.cursor {
            self.document
                .update(cx, |document, cx| document.run_below(id, cx));
        }
    }

    fn interrupt(&mut self, _: &Interrupt, _: &mut Window, cx: &mut Context<Self>) {
        self.document
            .update(cx, |document, cx| document.interrupt(cx));
    }

    fn restart(&mut self, _: &Restart, _: &mut Window, cx: &mut Context<Self>) {
        self.document
            .update(cx, |document, cx| document.restart(false, cx));
    }

    fn restart_and_run_all(
        &mut self,
        _: &RestartAndRunAll,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.document
            .update(cx, |document, cx| document.restart(true, cx));
    }

    fn clear_outputs(&mut self, _: &ClearOutputs, _: &mut Window, cx: &mut Context<Self>) {
        self.document
            .update(cx, |document, cx| document.clear_outputs(None, cx));
    }

    fn save(&mut self, _: &Save, _: &mut Window, cx: &mut Context<Self>) {
        self.save_document(cx);
    }

    fn save_from_editor(
        &mut self,
        _: &crate::editor::Save,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_document(cx);
    }

    // Modes.

    fn command_mode(&mut self, _: &CommandMode, window: &mut Window, cx: &mut Context<Self>) {
        self.enter_command(window, cx);
    }

    fn edit_mode(&mut self, _: &EditMode, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.cursor {
            self.enter_edit(id, window, cx);
        }
    }

    // Structure.

    fn insert(&mut self, below: bool, cx: &mut Context<Self>) {
        let document = self.document.read(cx);
        let range = self.selected_range(document);
        let index = if below { range.end } else { range.start };
        let id = self.document.update(cx, |document, cx| {
            document.insert(index, CellKind::Code, String::new(), cx)
        });
        self.select(id, false, cx);
    }

    fn insert_above(&mut self, _: &InsertAbove, _: &mut Window, cx: &mut Context<Self>) {
        self.insert(false, cx);
    }

    fn insert_below(&mut self, _: &InsertBelow, _: &mut Window, cx: &mut Context<Self>) {
        self.insert(true, cx);
    }

    fn delete_cells(&mut self, _: &DeleteCells, _: &mut Window, cx: &mut Context<Self>) {
        let ids = self.selected_ids(cx);
        // The cell after the deleted ones takes the selection.
        self.anchor = None;
        self.document
            .update(cx, |document, cx| document.delete(&ids, cx));
    }

    fn undo_cells(&mut self, _: &UndoCells, _: &mut Window, cx: &mut Context<Self>) {
        self.document
            .update(cx, |document, cx| document.undo_structure(cx));
    }

    fn redo_cells(&mut self, _: &RedoCells, _: &mut Window, cx: &mut Context<Self>) {
        self.document
            .update(cx, |document, cx| document.redo_structure(cx));
    }

    fn set_kind(&mut self, kind: CellKind, cx: &mut Context<Self>) {
        let ids = self.selected_ids(cx);
        self.document.update(cx, |document, cx| {
            for id in ids {
                if document.cell(id).is_some_and(|cell| cell.cell.kind != kind) {
                    document.set_kind(id, kind, cx);
                }
            }
        });
    }

    fn make_code(&mut self, _: &ToCode, _: &mut Window, cx: &mut Context<Self>) {
        self.set_kind(CellKind::Code, cx);
    }

    fn make_markdown(&mut self, _: &ToMarkdown, _: &mut Window, cx: &mut Context<Self>) {
        self.set_kind(CellKind::Markdown, cx);
    }

    fn make_raw(&mut self, _: &ToRaw, _: &mut Window, cx: &mut Context<Self>) {
        self.set_kind(CellKind::Raw, cx);
    }

    fn move_selection(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        let count = self.document.read(cx).cells().len();
        let Some(index) = self.cursor_index(cx) else {
            return self.select_index(0, false, cx);
        };
        let target = index
            .saturating_add_signed(delta)
            .min(count.saturating_sub(1));
        self.select_index(target, extend, cx);
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, false, cx);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, false, cx);
    }

    fn extend_up(&mut self, _: &ExtendSelectionUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, true, cx);
    }

    fn extend_down(&mut self, _: &ExtendSelectionDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, true, cx);
    }

    fn select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.select_index(0, false, cx);
    }

    fn select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        let count = self.document.read(cx).cells().len();
        self.select_index(count.saturating_sub(1), false, cx);
    }

    /// C: the selected cells for V (in any notebook), and their text on
    /// the system clipboard.
    fn copy_cells(&mut self, _: &CopyCells, _: &mut Window, cx: &mut Context<Self>) {
        let ids = self.selected_ids(cx);
        let cells = self.document.read(cx).copy(&ids);
        let text = cells
            .iter()
            .map(|cell| cell.source.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        cx.set_global(CellClipboard(cells));
    }

    fn cut_cells(&mut self, _: &CutCells, window: &mut Window, cx: &mut Context<Self>) {
        self.copy_cells(&CopyCells, window, cx);
        self.delete_cells(&DeleteCells, window, cx);
    }

    fn paste(&mut self, below: bool, cx: &mut Context<Self>) {
        let cells = match cx.try_global::<CellClipboard>() {
            Some(clipboard) if !clipboard.0.is_empty() => clipboard.0.clone(),
            _ => return,
        };
        let document = self.document.read(cx);
        let range = self.selected_range(document);
        let index = if below { range.end } else { range.start };
        let ids = self
            .document
            .update(cx, |document, cx| document.paste(index, cells, cx));
        if let (Some(first), Some(last)) = (ids.first(), ids.last()) {
            self.cursor = Some(*last);
            self.anchor = Some(*first);
            self.reveal = true;
            cx.notify();
        }
    }

    fn paste_below(&mut self, _: &PasteBelow, _: &mut Window, cx: &mut Context<Self>) {
        self.paste(true, cx);
    }

    fn paste_above(&mut self, _: &PasteAbove, _: &mut Window, cx: &mut Context<Self>) {
        self.paste(false, cx);
    }

    /// Ctrl+Shift+-: the text after the cursor becomes a new cell below,
    /// edited from its start.
    fn split_cell(&mut self, _: &SplitCell, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.cursor else {
            return;
        };
        let Some(editor) = self.cells.get(&id).and_then(|state| state.editor.clone()) else {
            return;
        };
        let (text, at) = {
            let editor = editor.read(cx);
            (editor.text(), editor.cursor())
        };
        let origin = Some(editor.entity_id());
        let new = self.document.update(cx, |document, cx| {
            // The editor's text is the source already; make sure.
            if document
                .cell(id)
                .is_some_and(|cell| cell.cell.source != text)
            {
                document.set_source(id, text, origin, cx);
            }
            document.split(id, at, cx)
        });
        if let Some(new) = new
            && let Some(editor) = self.enter_edit(new, window, cx)
        {
            editor.update(cx, |editor, cx| editor.set_cursor(0, cx));
        }
    }

    /// Ctrl+K Ctrl+I: the kernel's help (`inspect_request`) for the name at
    /// the cursor, shown under the cell until it is edited or closed.
    fn show_help(&mut self, _: &ShowHelp, _: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.cursor else {
            return;
        };
        let Some(editor) = self.cells.get(&id).and_then(|state| state.editor.clone()) else {
            return;
        };
        let (code, offset) = {
            let editor = editor.read(cx);
            (editor.text(), editor.cursor())
        };
        let task = self
            .document
            .update(cx, |document, cx| document.inspect(code, offset, cx));
        cx.spawn(async move |this, cx| {
            let help = task.await;
            this.update(cx, |this, cx| {
                let text = help.unwrap_or_else(|| {
                    "No help for the name at the cursor (the kernel answers once a cell has run)"
                        .to_owned()
                });
                if let Some(state) = this.cells.get_mut(&id) {
                    state.help = Some(text.into());
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn move_cells(&mut self, delta: isize, cx: &mut Context<Self>) {
        let ids = self.selected_ids(cx);
        self.document
            .update(cx, |document, cx| document.move_cells(&ids, delta, cx));
        self.reveal = true;
    }

    fn move_cells_up(&mut self, _: &MoveCellsUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cells(-1, cx);
    }

    fn move_cells_down(&mut self, _: &MoveCellsDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_cells(1, cx);
    }

    fn toggle_outputs(&mut self, _: &ToggleOutputs, _: &mut Window, cx: &mut Context<Self>) {
        for id in self.selected_ids(cx) {
            self.toggle_collapsed(id, cx);
        }
    }

    fn toggle_source(&mut self, _: &ToggleSource, _: &mut Window, cx: &mut Context<Self>) {
        for id in self.selected_ids(cx) {
            self.document.update(cx, |document, cx| {
                let hidden = document
                    .cell(id)
                    .is_some_and(|cell| cell.cell.source_hidden());
                document.set_source_hidden(id, !hidden, cx);
            });
        }
    }

    /// The document saves it (`.ipynb`) and tells every view.
    fn toggle_collapsed(&mut self, id: CellId, cx: &mut Context<Self>) {
        self.document.update(cx, |document, cx| {
            let hidden = document
                .cell(id)
                .is_some_and(|cell| cell.cell.outputs_hidden());
            document.set_outputs_hidden(id, !hidden, cx);
        });
    }

    fn show_more(&mut self, id: CellId, cx: &mut Context<Self>) {
        if let Some(state) = self.cells.get_mut(&id) {
            state.expanded = true;
        }
        self.splice_cell(id);
        cx.notify();
    }

    // Kernel, interpreter, marimo.

    fn choose_interpreter(
        &mut self,
        _: &ChooseInterpreter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.picker.take().is_some() {
            return cx.notify();
        }
        let document = self.document.clone();
        let picker = cx.new(|cx| InterpreterPicker::new(document, cx));
        let subscription = cx.subscribe_in(&picker, window, |this, _, event, window, cx| {
            this.picker = None;
            if let InterpreterPickerEvent::Picked(launch) = event {
                let launch = launch.clone();
                this.document
                    .update(cx, |document, cx| document.set_launch(launch, cx));
            }
            window.focus(&this.focus_handle);
            cx.notify();
        });
        window.focus(&picker.focus_handle(cx));
        self.picker = Some((picker, subscription));
        cx.notify();
    }

    fn open_in_marimo(&mut self, _: &OpenInMarimo, _: &mut Window, cx: &mut Context<Self>) {
        if self.document.read(cx).is_marimo() {
            cx.emit(NotebookViewEvent::OpenInMarimo);
        }
    }

    fn open_as_text(&mut self, _: &OpenAsText, _: &mut Window, cx: &mut Context<Self>) {
        if self.document.read(cx).is_marimo() {
            cx.emit(NotebookViewEvent::OpenAsText);
        }
    }

    // Rendering.

    /// Creates the editors of the text widgets cell `id`'s outputs show,
    /// and shows values the kernel changed in them.
    fn sync_widget_inputs(&mut self, id: CellId, window: &mut Window, cx: &mut Context<Self>) {
        let document = self.document.read(cx);
        let Some(cell) = document.cell(id) else {
            return;
        };
        let roots: Vec<&str> = cell
            .cell
            .outputs
            .iter()
            .filter_map(notebook::widgets::view_model_id)
            .collect();
        if roots.is_empty() {
            return;
        }
        self.widget_inputs.prune(document.widgets());
        let specs = input_specs(document.widgets(), &roots);
        if specs.is_empty() {
            return;
        }
        let handle = self.document.downgrade();
        let sink: WidgetSink = Rc::new(move |event, cx| apply_event(&handle, event, cx));
        self.widget_inputs.ensure(specs, &sink, window, cx);
    }

    /// Creates the editors of the number/text marimo elements cell `id`'s
    /// outputs show (Enter sends their value; see `marimo_ui`).
    fn sync_marimo_inputs(&mut self, id: CellId, window: &mut Window, cx: &mut Context<Self>) {
        let document = self.document.read(cx);
        if !document.is_marimo() {
            return;
        }
        let Some(cell) = document.cell(id) else {
            return;
        };
        let specs: Vec<_> = cell
            .cell
            .outputs
            .iter()
            .filter_map(|output| match output {
                notebook::Output::DisplayData { data, .. }
                | notebook::Output::ExecuteResult { data, .. } => {
                    notebook::outputs::marimo_ui_parts(data)
                }
                _ => None,
            })
            .flat_map(super::marimo_ui::input_specs)
            .collect();
        if specs.is_empty() {
            return;
        }
        // Fields send on Enter only (not continuous): nothing comes here.
        let sink: WidgetSink = Rc::new(|_, _| {});
        self.marimo_inputs.ensure(specs, &sink, window, cx);
    }

    fn render_cell(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(&id) = self.order.get(index) else {
            return div().into_any_element();
        };
        let Some((kind, source_hidden)) = self
            .document
            .read(cx)
            .cell(id)
            .map(|cell| (cell.cell.kind, cell.cell.source_hidden()))
        else {
            return div().into_any_element();
        };
        let editing = self.cells.get(&id).is_some_and(|state| state.editing);
        let editor = if !source_hidden && (kind != CellKind::Markdown || editing) {
            self.ensure_editor(id, window, cx)
        } else {
            None
        };
        let in_view = self.focus_handle.is_focused(window);
        let editor_focused = editor
            .as_ref()
            .is_some_and(|editor| editor.focus_handle(cx).is_focused(window));
        let expanded = self.cells.get(&id).is_some_and(|state| state.expanded);
        let max_lines = self.output_max_lines;
        self.sync_widget_inputs(id, window, cx);
        self.sync_marimo_inputs(id, window, cx);
        let this = cx.entity().downgrade();
        let document_handle = self.document.downgrade();

        let document = self.document.clone();
        let document = document.read(cx);
        let Some(cell) = document.cell(id) else {
            return div().into_any_element();
        };
        let selected = self.selected_range(document).contains(&index);
        let collapsed = cell.cell.outputs_hidden();
        let is_cursor = self.cursor == Some(id);
        let bar = if is_cursor && editor_focused {
            Some(theme::git_added())
        } else if selected && (in_view || is_cursor) {
            Some(theme::accent())
        } else if selected {
            Some(theme::border())
        } else {
            None
        };

        // The gutter: [n] / [*] / [ ], run button, elapsed time.
        let label = match (kind, &cell.run) {
            (CellKind::Code, RunState::Running { .. } | RunState::Queued) => "[*]".to_owned(),
            (CellKind::Code, _) => match cell.cell.execution_count {
                Some(count) => format!("[{count}]"),
                None => "[ ]".to_owned(),
            },
            (CellKind::Raw, _) => "raw".to_owned(),
            (CellKind::Markdown, _) => String::new(),
        };
        let elapsed = match &cell.run {
            RunState::Running { started } => {
                Some((format_elapsed(started.elapsed()), theme::warning()))
            }
            RunState::Queued => Some(("queued".to_owned(), theme::muted())),
            RunState::Finished { elapsed, ok } => Some((
                format_elapsed(*elapsed),
                if *ok { theme::muted() } else { theme::error() },
            )),
            RunState::Idle => None,
        };
        let marker = if cell.error.is_some() {
            Some(("error", theme::error()))
        } else if cell.stale {
            Some(("stale", theme::warning()))
        } else {
            None
        };
        let gutter = div()
            .id(("gutter", id))
            .flex_none()
            .w(px(GUTTER_WIDTH))
            .pt_1()
            .pr_2()
            .flex()
            .flex_col()
            .items_end()
            .gap_0p5()
            .text_xs()
            .font_family("monospace")
            .text_color(theme::muted())
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    this.enter_command(window, cx);
                    this.select(id, event.modifiers.shift, cx);
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .when(kind == CellKind::Code, |row| {
                        row.child(
                            div()
                                .id(("run", id))
                                .px_1()
                                .rounded_sm()
                                .text_color(theme::accent())
                                .hover(|style| style.bg(theme::hover_row()))
                                .child("▶")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.select(id, false, cx);
                                    this.run_cell(&RunCell, window, cx);
                                })),
                        )
                    })
                    .child(label),
            )
            .children(elapsed.map(|(text, color)| div().text_color(color).child(text)))
            .children(marker.map(|(text, color)| div().text_color(color).child(text)));

        // The cell's text: its editor, or rendered Markdown (or a line
        // standing for a collapsed source).
        let content = match editor {
            _ if source_hidden => div()
                .id(("source-hidden", id))
                .w_full()
                .px_2()
                .py_1()
                .text_xs()
                .text_color(theme::muted())
                .cursor_pointer()
                .child(format!(
                    "{} hidden ({} line{}): click to show",
                    if kind == CellKind::Markdown {
                        "Markdown"
                    } else {
                        "Code"
                    },
                    cell.cell.source.lines().count(),
                    if cell.cell.source.lines().count() == 1 {
                        ""
                    } else {
                        "s"
                    }
                ))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.document
                        .update(cx, |document, cx| document.set_source_hidden(id, false, cx));
                }))
                .into_any_element(),
            Some(editor) => div()
                .w_full()
                .px_2()
                .py_1()
                .rounded_sm()
                .border_1()
                .border_color(if editor_focused {
                    theme::accent()
                } else {
                    theme::border()
                })
                .bg(if kind == CellKind::Code {
                    theme::panel()
                } else {
                    theme::background()
                })
                .child(editor)
                .into_any_element(),
            None => {
                let base_dir = document.path().parent();
                let text = cell.cell.source.trim();
                div()
                    .id(("markdown", id))
                    .w_full()
                    .px_2()
                    .py_1()
                    .min_h(px(24.))
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        if event.click_count() >= 2 {
                            this.enter_edit(id, window, cx);
                        } else {
                            this.enter_command(window, cx);
                            this.select(id, event.modifiers().shift, cx);
                        }
                    }))
                    .child(if text.is_empty() {
                        div()
                            .text_sm()
                            .text_color(theme::muted())
                            .child("Empty Markdown cell: double-click or press Enter to edit")
                            .into_any_element()
                    } else {
                        markdown(
                            ("md", id),
                            &cell.cell.source,
                            MarkdownImages {
                                attachments: cell.cell.attachments.as_ref(),
                                base_dir,
                            },
                            cx,
                        )
                    })
                    .into_any_element()
            }
        };

        let error = cell.error.clone().map(|error| {
            div()
                .px_2()
                .py_1()
                .text_sm()
                .text_color(theme::error())
                .child(selectable(("cell-error", id), error))
        });

        let help = self
            .cells
            .get(&id)
            .and_then(|state| state.help.clone())
            .map(|help| {
                div()
                    .mt_1()
                    .px_2()
                    .py_1()
                    .flex()
                    .gap_2()
                    .rounded_sm()
                    .border_1()
                    .border_color(theme::border())
                    .bg(theme::panel())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .font_family(theme::MONO_FONT)
                            .child(selectable(("cell-help", id), help)),
                    )
                    .child(
                        div()
                            .id(("cell-help-close", id))
                            .flex_none()
                            .px_1()
                            .text_color(theme::muted())
                            .cursor_pointer()
                            .hover(|style| style.bg(theme::hover_row()))
                            .child("×")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(state) = this.cells.get_mut(&id) {
                                    state.help = None;
                                }
                                cx.notify();
                            })),
                    )
            });

        let outputs = &cell.cell.outputs;
        let outputs = (kind == CellKind::Code && !outputs.is_empty()).then(|| {
            let strip = div()
                .id(("output-strip", id))
                .flex_none()
                .w(px(6.))
                .rounded_sm()
                .cursor_pointer()
                .hover(|style| style.bg(theme::accent()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_collapsed(id, cx);
                }));
            let body = if collapsed {
                div()
                    .id(("outputs-hidden", id))
                    .px_2()
                    .text_xs()
                    .text_color(theme::muted())
                    .cursor_pointer()
                    .child(format!(
                        "{} output{} hidden: click to show",
                        outputs.len(),
                        if outputs.len() == 1 { "" } else { "s" }
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_collapsed(id, cx)))
                    .into_any_element()
            } else {
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_2()
                    .children(outputs.iter().enumerate().map(|(position, output)| {
                        let this = this.clone();
                        let document_handle = document_handle.clone();
                        render_output(
                            ElementId::NamedInteger(format!("output-{id}").into(), position as u64),
                            output,
                            &mut self.output_cache,
                            OutputOptions {
                                max_lines,
                                expanded,
                                widgets: Some(WidgetRender {
                                    store: document.widgets(),
                                    inputs: &self.widget_inputs,
                                }),
                                marimo_inputs: Some(&self.marimo_inputs),
                            },
                            move |action, _, cx| match action {
                                OutputAction::ShowMore => {
                                    this.update(cx, |this, cx| this.show_more(id, cx)).ok();
                                }
                                OutputAction::OpenHtml(html) => {
                                    if let Err(err) = open_html(&html, cx) {
                                        this.update(cx, |this, cx| {
                                            this.notice = Some(Notice {
                                                text: format!("Could not open the HTML: {err}")
                                                    .into(),
                                                error: true,
                                            });
                                            cx.notify();
                                        })
                                        .ok();
                                    }
                                }
                                OutputAction::Widget(event) => {
                                    apply_event(&document_handle, event, cx);
                                }
                                OutputAction::MarimoUi {
                                    object_id,
                                    value,
                                    send,
                                } => {
                                    document_handle
                                        .update(cx, |document, cx| {
                                            let value = match value {
                                                Some(value) => value,
                                                None => match document.ui_value(id, &object_id) {
                                                    Some(value) => value,
                                                    None => return,
                                                },
                                            };
                                            if send {
                                                document
                                                    .set_ui_value(id, &object_id, value, cx)
                                                    .detach();
                                            } else {
                                                document.show_ui_value(id, &object_id, value, cx);
                                            }
                                        })
                                        .ok();
                                }
                            },
                            cx,
                        )
                    }))
                    .into_any_element()
            };
            div().w_full().mt_1().flex().child(strip).child(body)
        });

        div()
            .id(("cell", id))
            .w_full()
            .flex()
            .py_1()
            .pr_4()
            .when(selected, |row| row.bg(theme::active_row()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    if this.cursor != Some(id) {
                        this.cursor = Some(id);
                        this.anchor = Some(id);
                        cx.notify();
                    }
                }),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(3.))
                    .rounded_sm()
                    .when_some(bar, |bar_div, color| bar_div.bg(color)),
            )
            .child(gutter)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(content)
                    .children(help)
                    .children(error)
                    .children(outputs),
            )
            .into_any_element()
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let document = self.document.read(cx);
        let status = document.kernel_status();
        let (dot, status_text) = status_display(status);
        let interpreter: SharedString = match document.launch() {
            Some(launch) => launch.label().into(),
            None => "Python (default)".into(),
        };
        let marimo = document.is_marimo();
        let busy = matches!(status, KernelStatus::Busy | KernelStatus::Starting);
        div()
            .flex()
            .flex_none()
            .flex_wrap()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border())
            .text_xs()
            .child(
                tool_button("nb-run-all", "▶▶ Run All")
                    .on_click(cx.listener(|this, _, window, cx| this.run_all(&RunAll, window, cx))),
            )
            .child(
                tool_button("nb-run-above", "Run Above").on_click(
                    cx.listener(|this, _, window, cx| this.run_above(&RunAbove, window, cx)),
                ),
            )
            .child(
                tool_button("nb-run-below", "Run Below").on_click(
                    cx.listener(|this, _, window, cx| this.run_below(&RunBelow, window, cx)),
                ),
            )
            .child(
                tool_button("nb-interrupt", "■ Interrupt")
                    .when(!busy, |button| button.text_color(theme::muted()))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.interrupt(&Interrupt, window, cx)),
                    ),
            )
            .child(
                tool_button("nb-restart", "↻ Restart").on_click(
                    cx.listener(|this, _, window, cx| this.restart(&Restart, window, cx)),
                ),
            )
            .child(
                tool_button("nb-restart-run", "Restart & Run All").on_click(cx.listener(
                    |this, _, window, cx| this.restart_and_run_all(&RestartAndRunAll, window, cx),
                )),
            )
            .child(tool_button("nb-clear", "Clear Outputs").on_click(
                cx.listener(|this, _, window, cx| this.clear_outputs(&ClearOutputs, window, cx)),
            ))
            .child(
                tool_button("nb-save", "Save")
                    .on_click(cx.listener(|this, _, window, cx| this.save(&Save, window, cx))),
            )
            .when(marimo, |toolbar| {
                toolbar
                    .child(
                        tool_button("nb-marimo", "Open in marimo").on_click(cx.listener(
                            |this, _, window, cx| this.open_in_marimo(&OpenInMarimo, window, cx),
                        )),
                    )
                    .child(
                        tool_button("nb-as-text", "Open as Text").on_click(cx.listener(
                            |this, _, window, cx| this.open_as_text(&OpenAsText, window, cx),
                        )),
                    )
            })
            .child(div().flex_1())
            .child(
                div()
                    .id("nb-kernel")
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::hover_row()))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.choose_interpreter(&ChooseInterpreter, window, cx)
                    }))
                    .child(div().size(px(8.)).rounded_full().bg(dot))
                    .child(div().text_color(theme::text()).child(status_text))
                    .child(div().text_color(theme::muted()).child(interpreter)),
            )
    }

    /// Kernel problems, the marimo note, notices.
    fn render_banners(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let document = self.document.read(cx);
        let mut banners = Vec::new();
        let banner = |id: &'static str, text: SharedString, color: Hsla| {
            div()
                .px_3()
                .py_1()
                .flex()
                .flex_wrap()
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
                        .text_color(color)
                        .child(selectable(id, text)),
                )
        };
        let choose = || {
            tool_button("nb-banner-choose", "Choose Interpreter").on_click(cx.listener(
                |this, _, window, cx| this.choose_interpreter(&ChooseInterpreter, window, cx),
            ))
        };
        let restart = || {
            tool_button("nb-banner-restart", "Restart Kernel")
                .on_click(cx.listener(|this, _, window, cx| this.restart(&Restart, window, cx)))
        };
        match document.kernel_status() {
            KernelStatus::Missing { python, modules } => {
                let what = if modules.is_empty() {
                    "ipykernel".to_owned()
                } else {
                    modules.join(", ")
                };
                let verb = if modules.len() > 1 { "are" } else { "is" };
                banners.push(
                    banner(
                        "nb-banner-missing",
                        format!("{what} {verb} not installed in {python}").into(),
                        theme::warning(),
                    )
                    .child(
                        tool_button("nb-banner-install", "Install ipykernel").on_click(
                            cx.listener(|_, _, _, cx| cx.emit(NotebookViewEvent::InstallKernel)),
                        ),
                    )
                    .child(choose())
                    .into_any_element(),
                );
            }
            KernelStatus::Dead(why) => banners.push(
                banner(
                    "nb-banner-dead",
                    format!("The kernel died: {why}").into(),
                    theme::error(),
                )
                .child(restart())
                .into_any_element(),
            ),
            KernelStatus::Failed(why) => banners.push(
                banner(
                    "nb-banner-failed",
                    format!("The kernel could not start: {why}").into(),
                    theme::error(),
                )
                .child(restart())
                .child(choose())
                .into_any_element(),
            ),
            _ => {}
        }
        if document.is_marimo() {
            banners.push(
                banner(
                    "nb-banner-marimo",
                    "marimo notebook: cells re-run when what they use changes, also when a mo.ui \
                     element changes. Tables, forms and other elements: open in marimo."
                        .into(),
                    theme::muted(),
                )
                .into_any_element(),
            );
        }
        for (id, notice) in [("nb-task", &self.task_status), ("nb-notice", &self.notice)] {
            if let Some(notice) = notice {
                let color = if notice.error {
                    theme::error()
                } else {
                    theme::text()
                };
                banners.push(banner(id, notice.text.clone(), color).into_any_element());
            }
        }
        banners
    }
}

/// The toolbar's dot colour and status word.
fn status_display(status: &KernelStatus) -> (Hsla, &'static str) {
    match status {
        KernelStatus::NotStarted => (theme::muted(), "no kernel"),
        KernelStatus::Starting => (theme::warning(), "starting"),
        KernelStatus::Idle => (theme::git_added(), "idle"),
        KernelStatus::Busy => (theme::warning(), "busy"),
        KernelStatus::Restarting => (theme::warning(), "restarting"),
        KernelStatus::Dead(_) => (theme::error(), "dead"),
        KernelStatus::Missing { .. } => (theme::error(), "ipykernel missing"),
        KernelStatus::Failed(_) => (theme::error(), "failed"),
    }
}

/// `0.4s`, `12.3s`, `2m 05s`.
fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs_f64();
    if seconds < 60. {
        format!("{seconds:.1}s")
    } else {
        let whole = elapsed.as_secs();
        format!("{}m {:02}s", whole / 60, whole % 60)
    }
}

fn tool_button(id: &'static str, label: &'static str) -> Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .py_0p5()
        .rounded_sm()
        .border_1()
        .border_color(theme::border())
        .cursor_pointer()
        .text_color(theme::text())
        .hover(|style| style.bg(theme::hover_row()))
        .child(label)
}

impl Focusable for NotebookView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for NotebookView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let path: PathBuf = self.document.read(cx).path().to_path_buf();
        self.output_max_lines = self
            .document
            .read(cx)
            .project()
            .settings_for(Some(&path))
            .notebook
            .output_max_lines
            .max(1);
        if std::mem::take(&mut self.reveal)
            && let Some(index) = self.cursor_index(cx)
        {
            self.reveal_cell(index);
        }
        let mut context = gpui::KeyContext::new_with_defaults();
        context.add(CONTEXT);
        if self.focus_handle.is_focused(window) {
            context.add("command");
        }
        let toolbar = self.render_toolbar(cx);
        let banners = self.render_banners(cx);
        let cells = list(
            self.list.clone(),
            cx.processor(|this, index, window, cx| this.render_cell(index, window, cx)),
        )
        .size_full();
        div()
            .key_context(context)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::run_cell))
            .on_action(cx.listener(Self::run_and_select_next))
            .on_action(cx.listener(Self::run_and_insert_below))
            .on_action(cx.listener(Self::run_all))
            .on_action(cx.listener(Self::run_above))
            .on_action(cx.listener(Self::run_below))
            .on_action(cx.listener(Self::interrupt))
            .on_action(cx.listener(Self::restart))
            .on_action(cx.listener(Self::restart_and_run_all))
            .on_action(cx.listener(Self::clear_outputs))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::save_from_editor))
            .on_action(cx.listener(Self::command_mode))
            .on_action(cx.listener(Self::edit_mode))
            .on_action(cx.listener(Self::insert_above))
            .on_action(cx.listener(Self::insert_below))
            .on_action(cx.listener(Self::delete_cells))
            .on_action(cx.listener(Self::undo_cells))
            .on_action(cx.listener(Self::redo_cells))
            .on_action(cx.listener(Self::make_code))
            .on_action(cx.listener(Self::make_markdown))
            .on_action(cx.listener(Self::make_raw))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::extend_up))
            .on_action(cx.listener(Self::extend_down))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::copy_cells))
            .on_action(cx.listener(Self::cut_cells))
            .on_action(cx.listener(Self::paste_below))
            .on_action(cx.listener(Self::paste_above))
            .on_action(cx.listener(Self::split_cell))
            .on_action(cx.listener(Self::show_help))
            .on_action(cx.listener(Self::move_cells_up))
            .on_action(cx.listener(Self::move_cells_down))
            .on_action(cx.listener(Self::toggle_outputs))
            .on_action(cx.listener(Self::toggle_source))
            .on_action(cx.listener(Self::choose_interpreter))
            .on_action(cx.listener(Self::open_in_marimo))
            .on_action(cx.listener(Self::open_as_text))
            .on_action(cx.listener(Self::format_cell))
            .on_action(cx.listener(Self::format_notebook))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::background())
            .text_color(theme::text())
            .child(toolbar)
            .children(banners)
            .child(div().flex_1().min_h_0().py_2().child(cells))
            .children(self.picker.as_ref().map(|(picker, _)| {
                div()
                    .absolute()
                    .top(px(36.))
                    .right(px(8.))
                    .child(picker.clone())
            }))
    }
}
