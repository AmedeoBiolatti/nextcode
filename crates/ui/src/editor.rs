//! Multi-line code editor view: buffer state, keyboard/mouse handling, IME, and painting.

use std::{ops::Range, sync::Arc, time::Duration};

use gpui::{
    AnyElement, App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, Font,
    FontStyle, FontWeight, GlobalElementId, Hsla, InspectorElementId, KeyBinding, KeyContext,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    ScrollWheelEvent, ShapedLine, SharedString, StrikethroughStyle, Style, Task, TextRun,
    UTF16Selection, UnderlineStyle, Window, actions, div, fill, point, prelude::*, px, relative,
    size,
};
use lsp::types::{
    DiagnosticSeverity, DiagnosticTag, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentSyncKind, Uri,
    VersionedTextDocumentIdentifier, notification,
};
use syntax::{HighlightSpan, Language, Syntax};
use text::{Bias, Buffer, Change, Encoding};

use crate::{
    lsp_store::RunningServer,
    project::Project,
    theme::{self, SyntaxStyle},
};

mod brackets;
mod completion;
mod editing;
mod find;
mod go_to_line;
mod lines;
mod navigation;
mod selections;
pub(crate) mod semantic;
#[cfg(test)]
mod tests;

use editing::{HistoryEntry, SelectionEdit};
use find::FindBar;
use go_to_line::GoToLineInput;
use lines::IndentStyle;
use selections::{Selection, SelectionSet, is_whole_word, is_word_char};

pub use brackets::JumpToBracket;
pub use find::{
    CloseFind, Find, FindNext, FindPrevious, FindReplace, ReplaceAll, ReplaceNext,
    SelectAllMatches, ToggleMatchCase, ToggleRegex, ToggleWholeWord,
};
pub use go_to_line::{CancelGoToLine, ConfirmGoToLine, GoToLine};
pub use lines::{
    CopyLineDown, CopyLineUp, DeleteLine, Indent, InsertLineAbove, InsertLineBelow, MoveLineDown,
    MoveLineUp, Outdent, ToggleBlockComment, ToggleLineComment,
};
pub use navigation::{FindReferences, GoToDefinition};
pub use selections::{
    AddCursorAbove, AddCursorBelow, CollapseSelections, EditorState, SelectAllOccurrences,
    SelectNextOccurrence,
};

actions!(
    editor,
    [
        Backspace,
        Delete,
        DeleteWordLeft,
        DeleteWordRight,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        WordLeft,
        WordRight,
        SelectWordLeft,
        SelectWordRight,
        Home,
        End,
        SelectHome,
        SelectEnd,
        DocStart,
        DocEnd,
        SelectDocStart,
        SelectDocEnd,
        PageUp,
        PageDown,
        Newline,
        Tab,
        SelectAll,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        Save,
        NextDiagnostic,
        PrevDiagnostic,
        ShowCompletions,
        SelectPrevCompletion,
        SelectNextCompletion,
        ConfirmCompletion,
        CloseCompletions,
    ]
);

const CONTEXT: &str = "Editor";
/// Around the find bar and the text, for commands that work from both.
const PANE_CONTEXT: &str = "EditorPane";
const GUTTER_PADDING: Pixels = px(12.);

#[cfg(target_os = "macos")]
const WORD: &str = "alt";
#[cfg(not(target_os = "macos"))]
const WORD: &str = "ctrl";

pub fn bind_keys(cx: &mut App) {
    let ctx = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("shift-backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new(&format!("{WORD}-backspace"), DeleteWordLeft, ctx),
        KeyBinding::new(&format!("{WORD}-delete"), DeleteWordRight, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("up", Up, ctx),
        KeyBinding::new("down", Down, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("shift-up", SelectUp, ctx),
        KeyBinding::new("shift-down", SelectDown, ctx),
        KeyBinding::new(&format!("{WORD}-left"), WordLeft, ctx),
        KeyBinding::new(&format!("{WORD}-right"), WordRight, ctx),
        KeyBinding::new(&format!("{WORD}-shift-left"), SelectWordLeft, ctx),
        KeyBinding::new(&format!("{WORD}-shift-right"), SelectWordRight, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("shift-home", SelectHome, ctx),
        KeyBinding::new("shift-end", SelectEnd, ctx),
        KeyBinding::new("secondary-home", DocStart, ctx),
        KeyBinding::new("secondary-end", DocEnd, ctx),
        KeyBinding::new("secondary-shift-home", SelectDocStart, ctx),
        KeyBinding::new("secondary-shift-end", SelectDocEnd, ctx),
        KeyBinding::new("pageup", PageUp, ctx),
        KeyBinding::new("pagedown", PageDown, ctx),
        KeyBinding::new("enter", Newline, ctx),
        KeyBinding::new("shift-enter", Newline, ctx),
        KeyBinding::new("tab", Tab, ctx),
        KeyBinding::new("secondary-a", SelectAll, ctx),
        KeyBinding::new("secondary-c", Copy, ctx),
        KeyBinding::new("secondary-x", Cut, ctx),
        KeyBinding::new("secondary-v", Paste, ctx),
        KeyBinding::new("secondary-z", Undo, ctx),
        KeyBinding::new("secondary-shift-z", Redo, ctx),
        KeyBinding::new("secondary-y", Redo, ctx),
        KeyBinding::new("secondary-s", Save, ctx),
        KeyBinding::new("f8", NextDiagnostic, ctx),
        KeyBinding::new("shift-f8", PrevDiagnostic, ctx),
        KeyBinding::new("f12", GoToDefinition, ctx),
        KeyBinding::new("secondary-b", GoToDefinition, ctx),
        KeyBinding::new("shift-f12", FindReferences, ctx),
        KeyBinding::new("alt-f7", FindReferences, ctx),
        KeyBinding::new("ctrl-space", ShowCompletions, ctx),
        // Multiple cursors (VS Code).
        KeyBinding::new("secondary-d", SelectNextOccurrence, ctx),
        KeyBinding::new("secondary-shift-l", SelectAllOccurrences, ctx),
        KeyBinding::new("ctrl-alt-up", AddCursorAbove, ctx),
        KeyBinding::new("ctrl-alt-down", AddCursorBelow, ctx),
        // Lines.
        KeyBinding::new("alt-up", MoveLineUp, ctx),
        KeyBinding::new("alt-down", MoveLineDown, ctx),
        KeyBinding::new("shift-alt-up", CopyLineUp, ctx),
        KeyBinding::new("shift-alt-down", CopyLineDown, ctx),
        KeyBinding::new("secondary-shift-k", DeleteLine, ctx),
        KeyBinding::new("secondary-enter", InsertLineBelow, ctx),
        KeyBinding::new("secondary-shift-enter", InsertLineAbove, ctx),
        KeyBinding::new("secondary-/", ToggleLineComment, ctx),
        KeyBinding::new("shift-tab", Outdent, ctx),
        KeyBinding::new("secondary-[", Outdent, ctx),
        KeyBinding::new("secondary-]", Indent, ctx),
        KeyBinding::new("secondary-g", GoToLine, ctx),
        KeyBinding::new("secondary-f", Find, ctx),
        KeyBinding::new("secondary-h", FindReplace, ctx),
        KeyBinding::new(
            "escape",
            CollapseSelections,
            Some("Editor && multiple_selections"),
        ),
        // Some platforms report the shifted character instead; the last
        // binding is the one shown.
        KeyBinding::new("secondary-?", ToggleBlockComment, ctx),
        KeyBinding::new("secondary-|", JumpToBracket, ctx),
        KeyBinding::new("shift-alt-a", ToggleBlockComment, ctx),
        KeyBinding::new("secondary-shift-\\", JumpToBracket, ctx),
    ]);
    // Find bar: from its inputs and from the text while it is open. Bound
    // after the plain editor keys so they win in the inputs.
    let pane = Some("EditorPane && find_open");
    let query = Some("FindInput > Editor");
    let bar = Some("FindBar");
    cx.bind_keys([
        KeyBinding::new("f3", FindNext, Some(PANE_CONTEXT)),
        KeyBinding::new("shift-f3", FindPrevious, Some(PANE_CONTEXT)),
        KeyBinding::new("escape", CloseFind, pane),
        KeyBinding::new("escape", CloseFind, bar),
        KeyBinding::new("alt-c", ToggleMatchCase, pane),
        KeyBinding::new("alt-w", ToggleWholeWord, pane),
        KeyBinding::new("alt-r", ToggleRegex, pane),
        KeyBinding::new("enter", FindNext, query),
        KeyBinding::new("shift-enter", FindPrevious, query),
        KeyBinding::new("alt-enter", SelectAllMatches, pane),
        KeyBinding::new("enter", ReplaceNext, Some("ReplaceInput > Editor")),
        KeyBinding::new("secondary-alt-enter", ReplaceAll, pane),
        KeyBinding::new("enter", ConfirmGoToLine, Some("GoToLine > Editor")),
        KeyBinding::new("escape", CancelGoToLine, Some("GoToLine")),
    ]);
    // Bound after the plain editor keys so they win while the menu is open.
    let menu = Some("Editor && showing_completions");
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevCompletion, menu),
        KeyBinding::new("down", SelectNextCompletion, menu),
        KeyBinding::new("enter", ConfirmCompletion, menu),
        KeyBinding::new("tab", ConfirmCompletion, menu),
        KeyBinding::new("escape", CloseCompletions, menu),
    ]);
}

pub enum ReloadOutcome {
    Unchanged,
    Reloaded,
    /// The buffer has unsaved edits that differ from the file on disk.
    Conflict,
    /// The file could not be read (e.g. deleted).
    Failed,
}

pub enum EditorEvent {
    /// The buffer gained or lost unsaved changes.
    DirtyChanged,
    /// The text changed.
    Edited,
    /// The buffer was written to its file.
    Saved,
    /// Open one of these locations (a picker when there are several).
    Navigate {
        locations: Vec<language::Location>,
        title: SharedString,
    },
}

impl EventEmitter<EditorEvent> for Editor {}

pub struct Editor {
    focus_handle: FocusHandle,
    buffer: Buffer,
    /// Never empty; see `SelectionSet`.
    selections: SelectionSet,
    /// Selections around each recent undo step, by transaction id.
    history: Vec<HistoryEntry>,
    /// Offsets of closers inserted by auto-closing, which typing the
    /// closer steps over. Mapped through edits, cleared when cursors move.
    auto_closed: Vec<usize>,
    /// The word Ctrl+D started from: its occurrences must be whole words.
    occurrence_word: Option<String>,
    indent: IndentStyle,
    /// Bumped on every edit; keys caches (find matches, bracket pair).
    version: u64,
    bracket_cache: Option<brackets::BracketCache>,
    /// IME composition range.
    marked: Option<Range<usize>>,
    scroll: Point<Pixels>,
    /// Scroll to this line on the next layout (restored state).
    pending_scroll_line: Option<usize>,
    /// Bring the primary cursor into view on the next layout.
    autoscroll: bool,
    drag: Option<Drag>,
    layout: Option<EditorLayout>,
    error: Option<SharedString>,
    /// Where the buffer's file lives; `None` for input fields.
    project: Option<Project>,
    syntax: Option<Syntax>,
    /// Input-field mode: no gutter or status bar; Enter, Tab and vertical
    /// motion bubble up to the parent view.
    single_line: bool,
    lsp: Option<LspDocument>,
    /// Language-server status shown in the status bar, with its color.
    lsp_status: Option<(SharedString, fn() -> Hsla)>,
    /// Scratch space for changes drained from the buffer.
    changes: Vec<Change>,
    /// Sorted by start; kept in sync with edits until the server republishes.
    diagnostics: Vec<EditorDiagnostic>,
    semantic: semantic::SemanticState,
    hover: Option<HoverPopover>,
    _hover_delay: Option<Task<()>>,
    /// Word the hover popover shows, or is waiting to show, information for.
    hover_word: Option<Range<usize>>,
    completion: Option<completion::CompletionMenu>,
    /// Bumped per completion request; stale responses are dropped.
    completion_generation: usize,
    _completion_task: Option<Task<()>>,
    _resolve_task: Option<Task<()>>,
    _navigation_task: Option<Task<()>>,
    /// Informational status-bar text, cleared by the next edit.
    notice: Option<SharedString>,
    /// Scroll the cursor to the middle of the view on the next layout.
    center_cursor: bool,
    find: Option<FindBar>,
    go_to_line: Option<GoToLineInput>,
}

/// A mouse drag in progress.
#[derive(Debug, Clone, Copy)]
enum Drag {
    /// Extends the primary selection.
    Select,
    /// Shift+Alt or middle drag: a column selection from (line, column).
    Column { line: usize, column: usize },
}

/// Most severe first, so `min` picks what to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

impl Severity {
    fn color(self) -> Hsla {
        match self {
            Self::Error => theme::error(),
            Self::Warning => theme::warning(),
            Self::Information => theme::accent_text(),
            Self::Hint => theme::muted(),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "info",
            Self::Hint => "hint",
        }
    }
}

struct EditorDiagnostic {
    range: Range<usize>,
    severity: Severity,
    message: SharedString,
    source: Option<SharedString>,
    /// Rendered faded (e.g. unused imports).
    unnecessary: bool,
    /// Rendered struck through.
    deprecated: bool,
}

/// Popover shown when the mouse rests on text: diagnostics and hover docs.
struct HoverPopover {
    line: usize,
    x: Pixels,
    messages: Vec<(Severity, SharedString)>,
    docs: Vec<language::markdown::Block>,
}

/// This editor's document as open on a language server.
struct LspDocument {
    server: Arc<RunningServer>,
    uri: Uri,
    version: i32,
}

/// A view's open server document, moved between views of one file
/// (`Editor::take_server` / `adopt_server`).
pub struct ServerDocument(LspDocument);

/// Geometry from the last prepaint, used for painting and hit-testing.
struct EditorLayout {
    bounds: Bounds<Pixels>,
    text_bounds: Bounds<Pixels>,
    gutter_width: Pixels,
    line_height: Pixels,
    em_width: Pixels,
    first_line: usize,
    lines: Vec<ShapedLine>,
    line_numbers: Vec<ShapedLine>,
    /// Most severe diagnostic per visible line.
    gutter_marks: Vec<Option<Severity>>,
}

/// Lines read to detect the indentation style.
const INDENT_SAMPLE_LINES: usize = 2000;

impl Editor {
    pub fn new(mut buffer: Buffer, cx: &mut Context<Self>) -> Self {
        let language = buffer.path().and_then(Language::from_path);
        let syntax = language.map(|language| Syntax::new(language, &buffer));
        buffer.drain_changes();
        let sample: Vec<String> = (0..buffer.line_count().min(INDENT_SAMPLE_LINES))
            .map(|line| buffer.line_text(line))
            .collect();
        let indent = IndentStyle::detect(sample.iter().map(String::as_str), language);
        Self {
            focus_handle: cx.focus_handle(),
            buffer,
            selections: SelectionSet::single(Selection::cursor(0)),
            history: Vec::new(),
            auto_closed: Vec::new(),
            occurrence_word: None,
            indent,
            version: 0,
            bracket_cache: None,
            marked: None,
            scroll: Point::default(),
            pending_scroll_line: None,
            autoscroll: false,
            drag: None,
            layout: None,
            error: None,
            project: None,
            syntax,
            single_line: false,
            lsp: None,
            lsp_status: None,
            changes: Vec::new(),
            diagnostics: Vec::new(),
            semantic: semantic::SemanticState::new(),
            hover: None,
            _hover_delay: None,
            hover_word: None,
            completion: None,
            completion_generation: 0,
            _completion_task: None,
            _resolve_task: None,
            _navigation_task: None,
            notice: None,
            center_cursor: false,
            find: None,
            go_to_line: None,
        }
    }

    pub fn language(&self) -> Option<Language> {
        self.syntax.as_ref().map(Syntax::language)
    }

    pub fn set_lsp_status(
        &mut self,
        status: Option<(SharedString, fn() -> Hsla)>,
        cx: &mut Context<Self>,
    ) {
        self.lsp_status = status;
        cx.notify();
    }

    /// Opens this document on `server` (no-op if already open there) and
    /// asks for its semantic tokens.
    pub fn attach_server(&mut self, server: Arc<RunningServer>, cx: &mut Context<Self>) {
        if self
            .lsp
            .as_ref()
            .is_some_and(|doc| Arc::ptr_eq(&doc.server, &server))
        {
            return;
        }
        let (Some(language), Some(uri)) = (
            self.language(),
            self.buffer.path().and_then(lsp::path_to_uri),
        ) else {
            return;
        };
        server
            .connection
            .notify::<notification::DidOpenTextDocument>(DidOpenTextDocumentParams {
                text_document: TextDocumentItem::new(
                    uri.clone(),
                    language.id().into(),
                    1,
                    self.text(),
                ),
            });
        self.lsp = Some(LspDocument {
            server,
            uri,
            version: 1,
        });
        self.refresh_semantic_tokens(cx);
    }

    /// Forgets the server; `close` also tells it the document closed.
    pub fn detach_server(&mut self, close: bool) {
        self.clear_semantic_tokens();
        if let Some(doc) = self.lsp.take()
            && close
            && !doc.server.connection.has_exited()
        {
            doc.server
                .connection
                .notify::<notification::DidCloseTextDocument>(DidCloseTextDocumentParams {
                    text_document: TextDocumentIdentifier::new(doc.uri),
                });
        }
    }

    /// Hands this view's server document (same URI and version) to another
    /// view of the same linked buffer, without closing it on the server.
    /// Semantic colors and popovers go with it on the next frame.
    pub fn take_server(&mut self) -> Option<ServerDocument> {
        let doc = self.lsp.take()?;
        self.clear_semantic_tokens();
        self.hover = None;
        self.hover_word = None;
        self._hover_delay = None;
        self.completion = None;
        self.completion_generation += 1;
        Some(ServerDocument(doc))
    }

    /// Takes over a document from `take_server` and asks for its semantic
    /// tokens.
    pub fn adopt_server(&mut self, doc: ServerDocument, cx: &mut Context<Self>) {
        self.lsp = Some(doc.0);
        self.refresh_semantic_tokens(cx);
    }

    /// Sends the drained `changes` to the server: one incremental edit when
    /// possible, otherwise the full text. Returns whether it sent them.
    fn sync_changes(&mut self) -> bool {
        let Some(doc) = &mut self.lsp else {
            return false;
        };
        let server = &doc.server;
        let content_changes =
            if server.sync == TextDocumentSyncKind::INCREMENTAL && self.changes.len() == 1 {
                let change = self.changes[0];
                let (start, end) = match server.encoding {
                    Encoding::Utf8 => (change.start_point, change.old_end_point),
                    Encoding::Utf16 => (change.start_utf16, change.old_end_utf16),
                };
                vec![TextDocumentContentChangeEvent {
                    range: Some(lsp::types::Range::new(
                        lsp_position(start),
                        lsp_position(end),
                    )),
                    range_length: None,
                    text: self.buffer.slice(change.start..change.new_end),
                }]
            } else if server.sync != TextDocumentSyncKind::NONE {
                vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: self.buffer.slice(0..self.buffer.len()),
                }]
            } else {
                return false;
            };
        doc.version += 1;
        server
            .connection
            .notify::<notification::DidChangeTextDocument>(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier::new(doc.uri.clone(), doc.version),
                content_changes,
            });
        true
    }

    pub fn single_line(cx: &mut Context<Self>) -> Self {
        Self {
            single_line: true,
            ..Self::new(Buffer::new(""), cx)
        }
    }

    /// An editor for a file of `project`; the buffer must have its path.
    pub fn for_file(buffer: Buffer, project: Project, cx: &mut Context<Self>) -> Self {
        Self {
            project: Some(project),
            ..Self::new(buffer, cx)
        }
    }

    pub fn text(&self) -> String {
        self.buffer.slice(0..self.buffer.len())
    }

    /// Replaces all text (as one undoable edit), e.g. to clear an input.
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let end = text.len();
        self.commit(
            vec![(0..self.buffer.len(), text.to_owned())],
            |_| SelectionSet::single(Selection::cursor(end)),
            cx,
        );
    }

    /// Selects all text, e.g. to replace an input's content by typing.
    pub fn select_all_text(&mut self, cx: &mut Context<Self>) {
        let selection = Selection::new(0, self.buffer.len());
        self.set_selections(SelectionSet::single(selection), cx);
    }

    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    /// Brings in the file's current contents after an external change.
    /// Unsaved edits are only overwritten when `force` is set.
    pub fn reload_from_disk(&mut self, force: bool, cx: &mut Context<Self>) -> ReloadOutcome {
        let Some(text) = self
            .project
            .as_ref()
            .zip(self.buffer.path())
            .and_then(|(project, path)| project.read_text(path).ok())
        else {
            return ReloadOutcome::Failed;
        };
        let was_dirty = self.buffer.is_dirty();
        if was_dirty && !force && self.buffer.slice(0..self.buffer.len()) != text {
            return ReloadOutcome::Conflict;
        }
        if !self.buffer.reload(&text) {
            // Same text as on disk (e.g. our own save): nothing to redraw
            // except a cleared dirty marker.
            self.finish_change(was_dirty, cx);
            return ReloadOutcome::Unchanged;
        }
        let before = self.selections.clone();
        let transaction = self.buffer.transaction_id();
        self.selections = before.clamped(&self.buffer);
        self.record_history(transaction, before);
        self.finish_change(was_dirty, cx);
        ReloadOutcome::Reloaded
    }

    pub fn save(&mut self, cx: &mut Context<Self>) -> bool {
        let was_dirty = self.buffer.is_dirty();
        let written = match (&self.project, self.buffer.path()) {
            (Some(project), Some(path)) => {
                project.write(path, self.buffer.slice(0..self.buffer.len()).into_bytes())
            }
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "buffer has no file",
            )),
        };
        let saved = match written {
            Ok(()) => {
                self.buffer.mark_saved();
                self.error = None;
                if let Some(doc) = &self.lsp {
                    doc.server
                        .connection
                        .notify::<notification::DidSaveTextDocument>(DidSaveTextDocumentParams {
                            text_document: TextDocumentIdentifier::new(doc.uri.clone()),
                            text: doc.server.save_includes_text.then(|| self.text()),
                        });
                }
                cx.emit(EditorEvent::Saved);
                // A saved `settings.json` or theme file applies at once.
                theme::refresh(cx);
                true
            }
            Err(err) => {
                self.error = Some(format!("Save failed: {err}").into());
                false
            }
        };
        self.finish_change(was_dirty, cx);
        saved
    }

    /// Moves every selection's head to `target`'s offset (with its goal
    /// column); without `select` the selection collapses there.
    fn move_selections(
        &mut self,
        select: bool,
        target: impl Fn(&Self, &Selection) -> (usize, Option<usize>),
        cx: &mut Context<Self>,
    ) {
        let selections = self.selections.map(|s| {
            let (head, goal) = target(self, s);
            Selection {
                anchor: if select { s.anchor } else { head },
                head,
                goal,
            }
        });
        self.set_selections(selections, cx);
    }

    fn horizontal(
        &mut self,
        select: bool,
        target: impl Fn(&Self, &Selection) -> usize,
        cx: &mut Context<Self>,
    ) {
        self.move_selections(select, |editor, s| (target(editor, s), None), cx);
    }

    fn vertical(&mut self, lines: isize, select: bool, cx: &mut Context<Self>) {
        self.move_selections(
            select,
            |editor, s| {
                let buffer = &editor.buffer;
                let goal = s.goal.unwrap_or_else(|| buffer.column(s.head));
                let line = buffer.line_of(s.head) as isize + lines;
                let offset = if line < 0 {
                    0
                } else if line as usize >= buffer.line_count() {
                    buffer.len()
                } else {
                    buffer.offset_at(line as usize, goal)
                };
                (offset, Some(goal))
            },
            cx,
        );
    }

    fn left_target(&self, s: &Selection, select: bool) -> usize {
        if !select && !s.is_empty() {
            s.start()
        } else {
            self.buffer.prev_grapheme(s.head)
        }
    }

    fn right_target(&self, s: &Selection, select: bool) -> usize {
        if !select && !s.is_empty() {
            s.end()
        } else {
            self.buffer.next_grapheme(s.head)
        }
    }

    /// First non-blank column of the line, or the line start if already there.
    fn home_target(&self, s: &Selection) -> usize {
        let line = self.buffer.line_of(s.head);
        let start = self.buffer.line_start(line);
        let text = self.buffer.line_text(line);
        let indented = start + text.len() - text.trim_start().len();
        if s.head == indented { start } else { indented }
    }

    fn end_target(&self, s: &Selection) -> usize {
        self.buffer.line_end(self.buffer.line_of(s.head))
    }

    fn page_lines(&self) -> isize {
        self.layout.as_ref().map_or(20, |layout| {
            (layout.text_bounds.size.height / layout.line_height)
                .floor()
                .max(1.) as isize
        })
    }

    /// Applies edits made through another view of a linked buffer
    /// (`text::Buffer::link`): selections follow the text, nothing scrolls.
    pub fn sync_buffer(&mut self, cx: &mut Context<Self>) {
        if !self.buffer.sync() {
            return;
        }
        // Dirty state is shared by the views; the editing view reports it.
        let was_dirty = self.buffer.is_dirty();
        let changes: Vec<Change> = self.buffer.drain_changes().collect();
        let map = |offset: usize, bias: Bias| {
            changes
                .iter()
                .fold(offset, |offset, change| change.map_offset(offset, bias))
        };
        self.selections = self
            .selections
            .map(|s| {
                let (anchor, head) = if s.reversed() {
                    (Bias::Left, Bias::Right)
                } else {
                    (Bias::Right, Bias::Left)
                };
                Selection {
                    anchor: map(s.anchor, if s.is_empty() { Bias::Right } else { anchor }),
                    head: map(s.head, if s.is_empty() { Bias::Right } else { head }),
                    goal: s.goal,
                }
            })
            .clamped(&self.buffer);
        self.marked = None;
        self.finish_change_with(changes, was_dirty, cx);
    }

    fn finish_change(&mut self, was_dirty: bool, cx: &mut Context<Self>) {
        let changes = self.buffer.drain_changes().collect();
        self.finish_change_with(changes, was_dirty, cx);
    }

    /// Updates everything that follows the text after `changes`.
    fn finish_change_with(
        &mut self,
        changes: Vec<Change>,
        was_dirty: bool,
        cx: &mut Context<Self>,
    ) {
        self.changes = changes;
        let edited = !self.changes.is_empty();
        if edited {
            self.version += 1;
        }
        for change in &self.changes {
            for diagnostic in &mut self.diagnostics {
                let end = change.map_offset(diagnostic.range.end, Bias::Left);
                let start = change
                    .map_offset(diagnostic.range.start, Bias::Right)
                    .min(end);
                diagnostic.range = start..end;
            }
            // Closers deleted by the change are gone; others move with the text.
            self.auto_closed.retain_mut(|offset| {
                let deleted = change.start <= *offset && *offset < change.old_end;
                *offset = change.map_offset(*offset, Bias::Right);
                !deleted
            });
        }
        self.shift_semantic_tokens();
        if edited {
            self.hover = None;
            self.hover_word = None;
            self._hover_delay = None;
            self.notice = None;
        }
        if let Some(syntax) = &mut self.syntax
            && edited
        {
            for change in &self.changes {
                syntax.edit(change);
            }
            syntax.reparse(&self.buffer);
        }
        if edited && self.sync_changes() {
            self.semantic_tokens_edited(cx);
        }
        if edited {
            cx.emit(EditorEvent::Edited);
        }
        if edited {
            self.update_completion_menu(cx);
        }
        if was_dirty != self.buffer.is_dirty() {
            cx.emit(EditorEvent::DirtyChanged);
        }
        cx.notify();
    }

    /// Converts pasted text to the buffer's line ending.
    fn normalize_line_endings(&self, text: &str) -> String {
        let text = text.replace("\r\n", "\n");
        match self.buffer.line_ending() {
            "\n" => text,
            ending => text.replace('\n', ending),
        }
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_selections_or(|editor, head| editor.backspace_range(head), cx);
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_selections_or(|editor, head| head..editor.buffer.next_grapheme(head), cx);
    }

    fn delete_word_left(&mut self, _: &DeleteWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_selections_or(|editor, head| editor.buffer.prev_word_start(head)..head, cx);
    }

    fn delete_word_right(&mut self, _: &DeleteWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_selections_or(|editor, head| head..editor.buffer.next_word_end(head), cx);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, |e, s| e.left_target(s, false), cx);
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, |e, s| e.right_target(s, false), cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, |e, s| e.left_target(s, true), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, |e, s| e.right_target(s, true), cx);
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(-1, false, cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(1, false, cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(-1, true, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(1, true, cx);
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, |e, s| e.buffer.prev_word_start(s.head), cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, |e, s| e.buffer.next_word_end(s.head), cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, |e, s| e.buffer.prev_word_start(s.head), cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, |e, s| e.buffer.next_word_end(s.head), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, Self::home_target, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, Self::end_target, cx);
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, Self::home_target, cx);
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, Self::end_target, cx);
    }

    fn doc_start(&mut self, _: &DocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, |_, _| 0, cx);
    }

    fn doc_end(&mut self, _: &DocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, |e, _| e.buffer.len(), cx);
    }

    fn select_doc_start(&mut self, _: &SelectDocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, |_, _| 0, cx);
    }

    fn select_doc_end(&mut self, _: &SelectDocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, |e, _| e.buffer.len(), cx);
    }

    fn page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(-self.page_lines(), false, cx);
    }

    fn page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(self.page_lines(), false, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.select_all_text(cx);
    }

    fn save_action(&mut self, _: &Save, _: &mut Window, cx: &mut Context<Self>) {
        self.save(cx);
    }

    fn offset_for_position(&self, position: Point<Pixels>) -> usize {
        let Some(layout) = &self.layout else {
            return self.primary().head;
        };
        let row = (position.y - layout.text_bounds.top() + self.scroll.y) / layout.line_height;
        let line = (row.max(0.) as usize).min(self.buffer.line_count() - 1);
        let x = position.x - layout.text_bounds.left() + self.scroll.x;
        let line_start = self.buffer.line_start(line);
        match line
            .checked_sub(layout.first_line)
            .and_then(|i| layout.lines.get(i))
        {
            Some(shaped) => line_start + shaped.closest_index_for_x(x),
            None => self
                .buffer
                .offset_at(line, (x / layout.em_width).round().max(0.) as usize),
        }
    }

    /// Line and char column under `position`, counting columns past the
    /// line end (column selections keep them).
    fn line_column_at(&self, position: Point<Pixels>) -> (usize, usize) {
        let Some(layout) = &self.layout else {
            let head = self.primary().head;
            return (self.buffer.line_of(head), self.buffer.column(head));
        };
        let row = (position.y - layout.text_bounds.top() + self.scroll.y) / layout.line_height;
        let line = (row.max(0.) as usize).min(self.buffer.line_count() - 1);
        let x = position.x - layout.text_bounds.left() + self.scroll.x;
        (line, (x / layout.em_width).round().max(0.) as usize)
    }

    fn start_mouse(&mut self, window: &mut Window) {
        window.focus(&self.focus_handle);
        self.hover = None;
        self.hover_word = None;
        self._hover_delay = None;
        self.completion = None;
    }

    fn start_column_drag(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let (line, column) = self.line_column_at(position);
        self.drag = Some(Drag::Column { line, column });
        let selections = self.column_selection((line, column), (line, column));
        self.set_selections(selections, cx);
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_mouse(window);
        let offset = self.offset_for_position(event.position);
        let modifiers = event.modifiers;
        let multi = !self.single_line;
        if multi && modifiers.alt && modifiers.shift {
            self.start_column_drag(event.position, cx);
        } else if event.click_count >= 2 {
            let end = self.buffer.next_word_end(offset);
            let word = Selection::new(self.buffer.prev_word_start(end), end);
            let selections = if multi && modifiers.alt {
                self.selections.with(word)
            } else {
                SelectionSet::single(word)
            };
            self.set_selections(selections, cx);
        } else if multi && modifiers.alt {
            // Alt+click adds a cursor, or removes the one clicked.
            let clicked = self
                .selections
                .all()
                .iter()
                .position(|s| s.is_empty() && s.head == offset);
            match clicked {
                Some(index) if self.selections.len() > 1 => {
                    let selections = self.selections.without(index);
                    self.set_selections(selections, cx);
                }
                _ => {
                    let selections = self.selections.with(Selection::cursor(offset));
                    self.set_selections(selections, cx);
                    self.drag = Some(Drag::Select);
                }
            }
        } else if modifiers.secondary() {
            self.set_selections(SelectionSet::single(Selection::cursor(offset)), cx);
            self.request_definition(offset, cx);
        } else {
            let anchor = if modifiers.shift {
                self.primary().anchor
            } else {
                offset
            };
            self.set_selections(SelectionSet::single(Selection::new(anchor, offset)), cx);
            self.drag = Some(Drag::Select);
        }
    }

    /// Middle drag: a column selection, like Shift+Alt+drag.
    fn on_middle_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.single_line {
            return;
        }
        self.start_mouse(window);
        self.start_column_drag(event.position, cx);
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.drag = None;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        match self.drag {
            Some(Drag::Select) => {
                let offset = self.offset_for_position(event.position);
                let primary = self.primary();
                let selections = self
                    .selections
                    .with_primary(Selection::new(primary.anchor, offset));
                self.set_selections(selections, cx);
            }
            Some(Drag::Column { line, column }) => {
                let head = self.line_column_at(event.position);
                let selections = self.column_selection((line, column), head);
                self.set_selections(selections, cx);
            }
            None => self.update_hover(event.position, cx),
        }
    }

    /// Line and offset of the character under `position`, if it is over text.
    fn character_at(&self, position: Point<Pixels>) -> Option<(usize, usize)> {
        let layout = self.layout.as_ref()?;
        if !layout.text_bounds.contains(&position) {
            return None;
        }
        let row = (position.y - layout.text_bounds.top() + self.scroll.y) / layout.line_height;
        let line = row.max(0.) as usize;
        let shaped = layout.lines.get(line.checked_sub(layout.first_line)?)?;
        let x = position.x - layout.text_bounds.left() + self.scroll.x;
        let index = shaped.index_for_x(x)?;
        Some((line, self.buffer.line_start(line) + index))
    }

    fn clear_hover(&mut self, hovered: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if !hovered && (self.hover.is_some() || self._hover_delay.is_some()) {
            self.hover = None;
            self.hover_word = None;
            self._hover_delay = None;
            cx.notify();
        }
    }

    /// Replaces diagnostics with a server's, given in its position encoding.
    pub fn set_diagnostics(
        &mut self,
        diagnostics: &[lsp::types::Diagnostic],
        cx: &mut Context<Self>,
    ) {
        let encoding = self
            .lsp
            .as_ref()
            .map_or(Encoding::Utf16, |doc| doc.server.encoding);
        let offset = |position: lsp::types::Position| {
            let point = text::Point {
                row: position.line as usize,
                column: position.character as usize,
            };
            self.buffer.offset_of(point, encoding)
        };
        self.diagnostics = diagnostics
            .iter()
            .map(|diagnostic| {
                let tags = diagnostic.tags.as_deref().unwrap_or_default();
                EditorDiagnostic {
                    range: self.visible_range(
                        offset(diagnostic.range.start),
                        offset(diagnostic.range.end),
                    ),
                    severity: match diagnostic.severity {
                        Some(DiagnosticSeverity::WARNING) => Severity::Warning,
                        Some(DiagnosticSeverity::INFORMATION) => Severity::Information,
                        Some(DiagnosticSeverity::HINT) => Severity::Hint,
                        _ => Severity::Error,
                    },
                    message: diagnostic.message.clone().into(),
                    source: diagnostic.source.clone().map(Into::into),
                    unnecessary: tags.contains(&DiagnosticTag::UNNECESSARY),
                    deprecated: tags.contains(&DiagnosticTag::DEPRECATED),
                }
            })
            .collect();
        self.diagnostics
            .sort_by_key(|d| (d.range.start, d.severity));
        self.hover = None;
        cx.notify();
    }

    /// Widens an empty range to one character so it can be underlined.
    fn visible_range(&self, start: usize, end: usize) -> Range<usize> {
        if start < end {
            return start..end;
        }
        let line = self.buffer.line_of(start);
        if start < self.buffer.line_end(line) {
            start..self.buffer.next_grapheme(start)
        } else if start > self.buffer.line_start(line) {
            self.buffer.prev_grapheme(start)..start
        } else {
            start..start
        }
    }

    fn jump_to_diagnostic(&mut self, forward: bool, cx: &mut Context<Self>) {
        let head = self.primary().head;
        let starts = self
            .diagnostics
            .iter()
            .filter(|d| d.severity < Severity::Hint)
            .map(|d| d.range.start);
        let target = if forward {
            starts
                .clone()
                .find(|&start| start > head)
                .or_else(|| starts.clone().next())
        } else {
            starts
                .clone()
                .rfind(|&start| start < head)
                .or_else(|| starts.clone().next_back())
        };
        if let Some(target) = target {
            self.set_selections(SelectionSet::single(Selection::cursor(target)), cx);
        }
    }

    fn next_diagnostic(&mut self, _: &NextDiagnostic, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_to_diagnostic(true, cx);
    }

    fn prev_diagnostic(&mut self, _: &PrevDiagnostic, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_to_diagnostic(false, cx);
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line_height = self
            .layout
            .as_ref()
            .map_or(px(theme::EDITOR_LINE_HEIGHT), |layout| layout.line_height);
        let mut delta = event.delta.pixel_delta(line_height);
        if event.modifiers.shift && delta.x == px(0.) {
            delta = point(delta.y, px(0.));
        }
        // Upper bounds are clamped in `layout`, which knows the viewport.
        self.scroll.x = (self.scroll.x - delta.x).max(px(0.));
        self.scroll.y = (self.scroll.y - delta.y).max(px(0.));
        self.hover = None;
        self._hover_delay = None;
        cx.notify();
    }

    /// Shapes the visible lines and resolves scrolling for `bounds`.
    fn layout(&mut self, bounds: Bounds<Pixels>, window: &mut Window) {
        let style = window.text_style();
        let font = style.font();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let run = |len: usize, color| TextRun {
            len,
            font: font.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let text_system = window.text_system().clone();
        let shape = |text: String, runs: &[TextRun]| {
            text_system.shape_line(text.into(), font_size, runs, None)
        };

        let em_width = shape("m".into(), &[run(1, style.color)]).width;
        let line_count = self.buffer.line_count();
        let digits = line_count.to_string().len().max(3);
        let gutter_width = if self.single_line {
            px(0.)
        } else {
            em_width * digits as f32 + GUTTER_PADDING * 2.
        };
        let text_bounds = Bounds::from_corners(
            point(bounds.left() + gutter_width, bounds.top()),
            bounds.bottom_right(),
        );
        let viewport = text_bounds.size;

        let primary_head = self.primary().head;
        let cursor_line = self.buffer.line_of(primary_head);
        if let Some(line) = self.pending_scroll_line.take() {
            self.scroll.y = line_height * line as f32;
            self.autoscroll = false;
        }
        if self.center_cursor {
            self.center_cursor = false;
            self.autoscroll = false;
            self.scroll.y = line_height * cursor_line as f32 - viewport.height / 2.;
        }
        if self.autoscroll {
            let top = line_height * cursor_line as f32;
            if top < self.scroll.y {
                self.scroll.y = top;
            } else if top + line_height > self.scroll.y + viewport.height {
                self.scroll.y = top + line_height - viewport.height;
            }
        }
        let max_scroll_y = line_height * line_count.saturating_sub(1) as f32;
        self.scroll.y = self.scroll.y.min(max_scroll_y).max(px(0.));

        let first_line = (self.scroll.y / line_height).floor() as usize;
        let last_line =
            (first_line + (viewport.height / line_height).ceil() as usize + 1).min(line_count);
        let text_color = theme::text();
        let muted = theme::muted();
        let theme = theme::current();
        let legend = self
            .lsp
            .as_ref()
            .and_then(|doc| doc.server.semantic_legend.as_ref());
        let styler = semantic::Styler::new(&theme, self.language(), legend);
        let base_run = run(0, text_color);
        let visible = self.buffer.line_start(first_line)
            ..self
                .buffer
                .line_end(last_line.saturating_sub(1).max(first_line));
        let spans = self
            .syntax
            .as_ref()
            .map(|syntax| syntax.highlights(&self.buffer, visible))
            .unwrap_or_default();

        let mut lines = Vec::with_capacity(last_line - first_line);
        let mut line_numbers = Vec::with_capacity(last_line - first_line);
        let mut cuts = Vec::new();
        let mut gutter_marks = Vec::with_capacity(last_line - first_line);
        let mut line_diagnostics: Vec<&EditorDiagnostic> = Vec::new();
        let mut cursor_lines: Vec<usize> = self
            .selections
            .all()
            .iter()
            .map(|s| self.buffer.line_of(s.head))
            .collect();
        cursor_lines.dedup();
        for line in first_line..last_line {
            let start = self.buffer.line_start(line);
            // Tabs render as one space for now; byte offsets stay aligned.
            let text = self.buffer.line_text(line).replace('\t', " ");
            let range = start..start + text.len();
            let first_span = spans.partition_point(|span| span.range.end <= range.start);
            let line_spans = &spans[first_span..];
            let line_spans =
                &line_spans[..line_spans.partition_point(|span| span.range.start < range.end)];
            let highlights = LineHighlights {
                spans: line_spans,
                tokens: semantic::overlapping(&self.semantic.tokens, range.clone()),
            };
            line_diagnostics.clear();
            let candidates = self
                .diagnostics
                .partition_point(|d| d.range.start < range.end.max(range.start + 1));
            line_diagnostics.extend(
                self.diagnostics[..candidates]
                    .iter()
                    .filter(|d| d.range.end > range.start || d.range.start == range.start),
            );
            gutter_marks.push(
                line_diagnostics
                    .iter()
                    .map(|d| d.severity)
                    .filter(|&severity| severity < Severity::Hint)
                    .min(),
            );
            let runs = line_runs(
                range,
                &highlights,
                &styler,
                self.marked.as_ref(),
                &line_diagnostics,
                &base_run,
                &mut cuts,
            );
            lines.push(shape(text, &runs));

            if !self.single_line {
                let number = (line + 1).to_string();
                let color = if cursor_lines.binary_search(&line).is_ok() {
                    text_color
                } else {
                    muted
                };
                line_numbers.push(shape(number.clone(), &[run(number.len(), color)]));
            }
        }

        if self.autoscroll
            && let Some(shaped) = cursor_line
                .checked_sub(first_line)
                .and_then(|i| lines.get(i))
        {
            let x = shaped.x_for_index(primary_head - self.buffer.line_start(cursor_line));
            let margin = em_width * 2.;
            if x < self.scroll.x {
                self.scroll.x = (x - margin).max(px(0.));
            } else if x + margin > self.scroll.x + viewport.width {
                self.scroll.x = x + margin - viewport.width;
            }
        }
        self.autoscroll = false;

        self.layout = Some(EditorLayout {
            bounds,
            text_bounds,
            gutter_width,
            line_height,
            em_width,
            first_line,
            lines,
            line_numbers,
            gutter_marks,
        });
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let brackets = if self.single_line {
            None
        } else {
            self.cached_bracket_pair()
        };
        let Some(layout) = &self.layout else {
            return;
        };
        let line_height = layout.line_height;
        let line_top = |line: usize| bounds.top() + line_height * line as f32 - self.scroll.y;
        let text_left = layout.text_bounds.left() - self.scroll.x;
        let selections = self.selections.all();
        let focused = self.focus_handle.is_focused(window);
        let (matches, active_match) = match &self.find {
            Some(find) if find.open => (find.matches.as_slice(), self.active_match()),
            _ => (&[][..], None),
        };
        let bracket_ranges: Vec<Range<usize>> = brackets
            .into_iter()
            .flat_map(|(a, b)| [a..a + 1, b..b + 1])
            .collect();

        for (i, number) in layout.line_numbers.iter().enumerate() {
            let x = bounds.left() + layout.gutter_width - GUTTER_PADDING - number.width;
            number
                .paint(
                    point(x, line_top(layout.first_line + i)),
                    line_height,
                    window,
                    cx,
                )
                .ok();
        }
        for (i, mark) in layout.gutter_marks.iter().enumerate() {
            if let Some(severity) = mark {
                let top = line_top(layout.first_line + i);
                window.paint_quad(fill(
                    Bounds::new(
                        point(bounds.left() + px(3.), top + px(3.)),
                        size(px(3.), line_height - px(6.)),
                    ),
                    severity.color(),
                ));
            }
        }

        window.with_content_mask(
            Some(ContentMask {
                bounds: layout.text_bounds,
            }),
            |window| {
                for (i, shaped) in layout.lines.iter().enumerate() {
                    let line = layout.first_line + i;
                    let top = line_top(line);
                    let start = self.buffer.line_start(line);
                    let end = self.buffer.line_end(line);
                    // Backgrounds over the part of `range` on this line.
                    let mut highlight = |range: &Range<usize>, color: Hsla| {
                        if range.start > end || range.end < start || range.is_empty() {
                            return;
                        }
                        let x0 = shaped.x_for_index(range.start.max(start) - start);
                        let mut x1 = shaped.x_for_index(range.end.min(end) - start);
                        if range.end > end {
                            // Show the selected line break.
                            x1 += layout.em_width / 2.;
                        }
                        window.paint_quad(fill(
                            Bounds::from_corners(
                                point(text_left + x0, top),
                                point(text_left + x1, top + line_height),
                            ),
                            color,
                        ));
                    };
                    let first_match = matches.partition_point(|m| m.end < start);
                    for m in matches[first_match..].iter().take_while(|m| m.start <= end) {
                        highlight(m, theme::find_match());
                    }
                    let first = selections.partition_point(|s| s.end() < start);
                    let on_line = || selections[first..].iter().take_while(|s| s.start() <= end);
                    for s in on_line() {
                        highlight(&s.range(), theme::selection());
                    }
                    if let Some(active) = active_match.and_then(|i| matches.get(i)) {
                        highlight(active, theme::find_match_active());
                    }
                    for bracket in &bracket_ranges {
                        highlight(bracket, theme::bracket_match());
                    }

                    shaped
                        .paint(point(text_left, top), line_height, window, cx)
                        .ok();

                    if focused {
                        for s in on_line().filter(|s| (start..=end).contains(&s.head)) {
                            let x = text_left + shaped.x_for_index(s.head - start);
                            window.paint_quad(fill(
                                Bounds::from_corners(
                                    point(x, top),
                                    point(x + px(2.), top + line_height),
                                ),
                                theme::cursor(),
                            ));
                        }
                    }
                }
            },
        );
    }
}

fn lsp_position(point: text::Point) -> lsp::types::Position {
    lsp::types::Position::new(point.row as u32, point.column as u32)
}

/// What highlights one line: tree-sitter spans and semantic tokens
/// overlapping it.
struct LineHighlights<'a> {
    spans: &'a [HighlightSpan],
    tokens: &'a [semantic::SemanticToken],
}

/// Text runs for one line: syntax styles from `highlights` (resolved by
/// `styler`), wavy underlines from `diagnostics`, IME composition
/// underlined. `cuts` is scratch space reused across lines.
fn line_runs(
    line: Range<usize>,
    highlights: &LineHighlights,
    styler: &semantic::Styler,
    marked: Option<&Range<usize>>,
    diagnostics: &[&EditorDiagnostic],
    base: &TextRun,
    cuts: &mut Vec<usize>,
) -> Vec<TextRun> {
    let clamp = |offset: usize| offset.clamp(line.start, line.end);
    cuts.clear();
    cuts.extend([line.start, line.end]);
    for span in highlights.spans {
        cuts.extend([clamp(span.range.start), clamp(span.range.end)]);
    }
    for token in highlights.tokens {
        cuts.extend([clamp(token.range.start), clamp(token.range.end)]);
    }
    for diagnostic in diagnostics {
        cuts.extend([clamp(diagnostic.range.start), clamp(diagnostic.range.end)]);
    }
    if let Some(marked) = marked {
        cuts.extend([clamp(marked.start), clamp(marked.end)]);
    }
    cuts.sort_unstable();
    cuts.dedup();

    cuts.windows(2)
        .map(|cut| {
            let start = cut[0];
            let covering = || diagnostics.iter().filter(move |d| d.range.contains(&start));
            let syntax = highlights
                .spans
                .iter()
                .find(|span| span.range.contains(&start))
                .map(|span| span.highlight);
            let token = highlights
                .tokens
                .iter()
                .find(|token| token.range.contains(&start));
            let style = styler.style(syntax, token);
            let color = if covering().any(|d| d.unnecessary) {
                theme::muted()
            } else {
                style.color.unwrap_or(base.color)
            };
            let plain_underline = UnderlineStyle {
                color: Some(color),
                thickness: px(1.),
                wavy: false,
            };
            let underline = match marked.filter(|marked| marked.contains(&start)) {
                Some(_) => Some(plain_underline),
                None => covering()
                    .map(|d| d.severity)
                    .filter(|&severity| severity < Severity::Hint)
                    .min()
                    .map(|severity| UnderlineStyle {
                        color: Some(severity.color()),
                        thickness: px(1.),
                        wavy: true,
                    })
                    .or((style.underline == Some(true)).then_some(plain_underline)),
            };
            let strikethrough = (covering().any(|d| d.deprecated)
                || style.strikethrough == Some(true))
            .then_some(StrikethroughStyle {
                color: Some(color),
                thickness: px(1.),
            });
            let bold = style.bold == Some(true);
            let italic = style.italic == Some(true);
            let font = if bold || italic {
                Font {
                    weight: if bold {
                        FontWeight::BOLD
                    } else {
                        base.font.weight
                    },
                    style: if italic {
                        FontStyle::Italic
                    } else {
                        base.font.style
                    },
                    ..base.font.clone()
                }
            } else {
                base.font.clone()
            };
            TextRun {
                len: cut[1] - start,
                font,
                color,
                background_color: style.background,
                underline,
                strikethrough,
            }
        })
        .collect()
}

impl EntityInputHandler for Editor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.buffer.offset_from_utf16(range_utf16.start)
            ..self.buffer.offset_from_utf16(range_utf16.end);
        actual_range.replace(
            self.buffer.offset_to_utf16(range.start)..self.buffer.offset_to_utf16(range.end),
        );
        Some(self.buffer.slice(range))
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let selection = self.primary();
        let range = selection.range();
        Some(UTF16Selection {
            range: self.buffer.offset_to_utf16(range.start)..self.buffer.offset_to_utf16(range.end),
            reversed: selection.reversed(),
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|range| {
            self.buffer.offset_to_utf16(range.start)..self.buffer.offset_to_utf16(range.end)
        })
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .map(|r| self.buffer.offset_from_utf16(r.start)..self.buffer.offset_from_utf16(r.end))
            .or(self.marked.clone());
        match range {
            Some(range) => {
                self.replace_composition(range, text, cx);
            }
            None => self.type_text(text, cx),
        }
        self.marked = None;
        self.on_typed(text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .map(|r| self.buffer.offset_from_utf16(r.start)..self.buffer.offset_from_utf16(r.end))
            .or(self.marked.clone())
            .unwrap_or_else(|| self.primary().range());
        let inserted = self.replace_composition(range, text, cx);
        self.marked = (!text.is_empty()).then(|| inserted.clone());
        if let Some(selected) = new_selected_range_utf16 {
            // Relative to the inserted text, in UTF-16 units.
            let base = self.buffer.offset_to_utf16(inserted.start);
            let selection = Selection::new(
                self.buffer.offset_from_utf16(base + selected.start),
                self.buffer.offset_from_utf16(base + selected.end),
            );
            self.selections = self.selections.with_primary(selection);
        }
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.layout.as_ref()?;
        let start = self.buffer.offset_from_utf16(range_utf16.start);
        let end = self.buffer.offset_from_utf16(range_utf16.end);
        let line = self.buffer.line_of(start);
        let shaped = layout.lines.get(line.checked_sub(layout.first_line)?)?;
        let line_start = self.buffer.line_start(line);
        let end = end.min(self.buffer.line_end(line));
        let left = layout.text_bounds.left() - self.scroll.x;
        let top = layout.text_bounds.top() + layout.line_height * line as f32 - self.scroll.y;
        Some(Bounds::from_corners(
            point(left + shaped.x_for_index(start - line_start), top),
            point(
                left + shaped.x_for_index(end - line_start),
                top + layout.line_height,
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.buffer.offset_to_utf16(self.offset_for_position(point)))
    }
}

struct EditorElement {
    editor: Entity<Editor>,
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.editor
            .update(cx, |editor, _| editor.layout(bounds, window));
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.editor.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );
        self.editor
            .update(cx, |editor, cx| editor.paint(bounds, window, cx));
    }
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // `settings.json` may have toggled semantic highlighting.
        self.sync_semantic_setting(cx);
        if self.find_open() {
            self.refresh_find(cx);
        }
        // A modal (finder, picker) took focus: don't leave a popover over it.
        let focused = self.focus_handle.contains_focused(window, cx);
        let head = self.primary().head;
        let line = self.buffer.line_of(head) + 1;
        let column = self.buffer.column(head) + 1;
        let language = self
            .syntax
            .as_ref()
            .map_or("Plain Text", |syntax| syntax.language().name());
        let line_ending = if self.buffer.line_ending() == "\r\n" {
            "CRLF"
        } else {
            "LF"
        };
        let (message, message_color) = match (&self.error, &self.notice) {
            (Some(error), _) => (error.clone(), theme::error()),
            (None, Some(notice)) => (notice.clone(), theme::text()),
            (None, None) => (
                self.buffer
                    .path()
                    .map(|path| path.display().to_string().into())
                    .unwrap_or_default(),
                theme::muted(),
            ),
        };
        let count = |severity| {
            self.diagnostics
                .iter()
                .filter(|d| d.severity == severity)
                .count()
        };
        let (errors, warnings) = (count(Severity::Error), count(Severity::Warning));
        let selection_count = match self.selections.len() {
            1 => String::new(),
            n => format!("{n} selections   "),
        };
        let indent = self.indent.label();

        let editor = div()
            .key_context({
                let mut context = KeyContext::default();
                context.add(CONTEXT);
                if self.completion.is_some() {
                    context.add("showing_completions");
                }
                if self.selections.len() > 1 {
                    context.add("multiple_selections");
                }
                context
            })
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::background())
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::doc_start))
            .on_action(cx.listener(Self::doc_end))
            .on_action(cx.listener(Self::select_doc_start))
            .on_action(cx.listener(Self::select_doc_end))
            .when(!self.single_line, |editor| {
                editor
                    .on_action(cx.listener(Self::up))
                    .on_action(cx.listener(Self::down))
                    .on_action(cx.listener(Self::select_up))
                    .on_action(cx.listener(Self::select_down))
                    .on_action(cx.listener(Self::page_up))
                    .on_action(cx.listener(Self::page_down))
                    .on_action(cx.listener(Self::newline))
                    .on_action(cx.listener(Self::tab))
                    .on_action(cx.listener(Self::outdent))
                    .on_action(cx.listener(Self::indent))
                    .on_action(cx.listener(Self::select_next_occurrence))
                    .on_action(cx.listener(Self::select_all_occurrences))
                    .on_action(cx.listener(Self::add_cursor_above))
                    .on_action(cx.listener(Self::add_cursor_below))
                    .on_action(cx.listener(Self::collapse_selections))
                    .on_action(cx.listener(Self::move_line_up))
                    .on_action(cx.listener(Self::move_line_down))
                    .on_action(cx.listener(Self::copy_line_up))
                    .on_action(cx.listener(Self::copy_line_down))
                    .on_action(cx.listener(Self::delete_line))
                    .on_action(cx.listener(Self::insert_line_below))
                    .on_action(cx.listener(Self::insert_line_above))
                    .on_action(cx.listener(Self::toggle_line_comment))
                    .on_action(cx.listener(Self::toggle_block_comment))
                    .on_action(cx.listener(Self::jump_to_bracket))
                    .on_action(cx.listener(Self::go_to_line_action))
            })
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::save_action))
            .on_action(cx.listener(Self::next_diagnostic))
            .on_action(cx.listener(Self::prev_diagnostic))
            .on_action(cx.listener(Self::go_to_definition))
            .on_action(cx.listener(Self::find_references))
            .on_action(cx.listener(Self::show_completions))
            .on_action(cx.listener(Self::select_prev_completion))
            .on_action(cx.listener(Self::select_next_completion))
            .on_action(cx.listener(Self::confirm_completion))
            .on_action(cx.listener(Self::close_completions))
            .child(
                div()
                    .id("editor-text")
                    .flex_1()
                    .relative()
                    .overflow_hidden()
                    .cursor(CursorStyle::IBeam)
                    .font_family(theme::MONO_FONT)
                    .text_size(px(theme::EDITOR_FONT_SIZE))
                    .line_height(px(theme::EDITOR_LINE_HEIGHT))
                    .text_color(theme::text())
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_middle_mouse_down))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::on_mouse_up))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
                    .on_hover(cx.listener(Self::clear_hover))
                    .child(EditorElement {
                        editor: cx.entity(),
                    })
                    .when(focused, |text| text.children(self.render_hover()))
                    .children(self.render_completions(cx)),
            )
            .when(!self.single_line, |editor| {
                editor.child(
                    div()
                        .flex()
                        .justify_between()
                        .px_3()
                        .py_1()
                        .border_t_1()
                        .border_color(theme::border())
                        .bg(theme::panel())
                        .text_xs()
                        .text_color(theme::muted())
                        .child(div().text_color(message_color).child(message))
                        .child(
                            div()
                                .flex()
                                .gap_4()
                                .children(
                                    self.lsp_status.clone().map(|(status, color)| {
                                        div().text_color(color()).child(status)
                                    }),
                                )
                                .child(format!(
                                    "{selection_count}Ln {line}, Col {column}   {indent}   {line_ending}   UTF-8   {language}"
                                ))
                                .when(errors + warnings > 0, |row| {
                                    row.child(
                                        div()
                                            .flex()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .text_color(theme::error())
                                                    .child(format!("✕ {errors}")),
                                            )
                                            .child(
                                                div()
                                                    .text_color(theme::warning())
                                                    .child(format!("⚠ {warnings}")),
                                            ),
                                    )
                                }),
                        ),
                )
            });
        if self.single_line {
            return editor.into_any_element();
        }
        // The find bar and go-to-line input sit beside the text, not inside
        // it, so keys they don't handle never reach the text's commands.
        div()
            .key_context({
                let mut context = KeyContext::default();
                context.add(PANE_CONTEXT);
                if self.find_open() {
                    context.add("find_open");
                }
                context
            })
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .on_action(cx.listener(Self::find))
            .on_action(cx.listener(Self::find_replace))
            .on_action(cx.listener(Self::find_next))
            .on_action(cx.listener(Self::find_previous))
            .on_action(cx.listener(Self::select_all_matches))
            .on_action(cx.listener(Self::replace_next))
            .on_action(cx.listener(Self::replace_all))
            .on_action(cx.listener(Self::close_find))
            .on_action(cx.listener(Self::toggle_match_case))
            .on_action(cx.listener(Self::toggle_whole_word))
            .on_action(cx.listener(Self::toggle_regex))
            .on_action(cx.listener(Self::confirm_go_to_line))
            .on_action(cx.listener(Self::cancel_go_to_line))
            .children(self.render_find_bar(cx))
            .child(div().flex_1().min_h_0().child(editor))
            .children(self.render_go_to_line(cx))
            .into_any_element()
    }
}
