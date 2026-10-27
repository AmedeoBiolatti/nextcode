//! Multi-line code editor view: buffer state, keyboard/mouse handling, IME, and painting.

use std::{ops::Range, rc::Rc, sync::Arc, time::Duration};

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ClipboardItem, ContentMask, Context, CursorStyle,
    Element, ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle,
    Focusable, Font, FontStyle, FontWeight, Global, GlobalElementId, Hitbox, HitboxBehavior, Hsla,
    InspectorElementId, KeyBinding, KeyContext, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollWheelEvent, ShapedLine, SharedString,
    StrikethroughStyle, Style, Task, TextRun, UTF16Selection, UnderlineStyle, Window, actions, div,
    fill, point, prelude::*, px, relative, size,
};
use lsp::types::{
    DiagnosticSeverity, DiagnosticTag, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, TextDocumentContentChangeEvent, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentSyncKind, Uri, VersionedTextDocumentIdentifier, notification,
};
use syntax::{HighlightSpan, Language, Syntax};
use text::{Bias, Buffer, Change, Encoding};

use crate::{
    lsp_store::RunningServer,
    pinpoint::Pin,
    project::Project,
    selectable::selectable,
    theme::{self, SyntaxStyle},
};

mod blame;
mod brackets;
mod call_hierarchy;
mod cell_server;
mod code_actions;
mod completion;
mod debug_gutter;
mod display_map;
mod editing;
mod extensions;
mod find;
mod folds;
pub(crate) mod format;
mod git_gutter;
mod go_to_line;
mod indent_guides;
mod inlay_hints;
mod lines;
mod lsp_sync;
mod minimap;
mod navigation;
mod notes;
mod pin_text;
mod pins;
mod project_settings;
mod rename;
pub(crate) mod save;
mod scrollbar;
mod selections;
pub(crate) mod semantic;
mod server_edits;
pub(crate) mod signature_help;
pub(crate) mod snippets;
pub(crate) mod spell;
mod sticky_scroll;
mod symbols;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_large;
mod vim;

use display_map::{DisplayMap, DisplayRow};
use editing::{HistoryEntry, SelectionEdit};
use find::FindBar;
use go_to_line::GoToLineInput;
use inlay_hints::RowInlays;
use lines::IndentStyle;
use selections::{Selection, SelectionSet, is_whole_word, is_word_char};

pub use self::notes::{AddNote, NoteFact};
pub use brackets::JumpToBracket;
pub use call_hierarchy::ShowCallHierarchy;
pub use cell_server::CellLink;
pub use code_actions::{
    CloseCodeActions, ConfirmCodeAction, QuickFix, SelectNextCodeAction, SelectPrevCodeAction,
};
pub(crate) use code_actions::{
    execute_command, info as code_action_info, resolve as resolve_code_action,
    usable as usable_code_actions,
};
pub(crate) use completion::symbol_kind_badge;
pub use debug_gutter::{
    CancelBreakpointEdit, ConfirmBreakpointEdit, DebugEvaluator, DebugLine, GutterBreakpoint,
    GutterMarker, GutterMarkerKind, NextBreakpointField,
};
pub use find::{
    CloseFind, Find, FindNext, FindPrevious, FindReplace, ReplaceAll, ReplaceNext,
    SelectAllMatches, ToggleMatchCase, ToggleRegex, ToggleWholeWord,
};
pub use folds::{Fold, FoldAll, FoldRecursively, ToggleFold, Unfold, UnfoldAll, UnfoldRecursively};
pub use format::{FormatDocument, FormatSelection};
pub use git_gutter::{CloseHunkPopup, GitBase, NextChange, PreviousChange, RollbackLines};
pub use go_to_line::{CancelGoToLine, ConfirmGoToLine, GoToLine};
pub use inlay_hints::ToggleInlayHints;
pub use lines::{
    CopyLineDown, CopyLineUp, DeleteLine, Indent, InsertLineAbove, InsertLineBelow, MoveLineDown,
    MoveLineUp, Outdent, ToggleBlockComment, ToggleLineComment,
};
pub use navigation::{FindReferences, GoToDefinition, GoToImplementation, GoToTypeDefinition};
pub use rename::{CancelRename, ConfirmRename, RenameSymbol};
pub use selections::{
    AddCursorAbove, AddCursorBelow, CollapseSelections, EditorState, SelectAllOccurrences,
    SelectNextOccurrence,
};
pub use signature_help::{CloseSignatureHelp, NextSignature, PreviousSignature, ShowSignatureHelp};
pub use snippets::{EndSnippet, NextTabStop, PreviousTabStop};
pub use symbols::{GoToSymbol, PickRequest, SymbolsStatus};

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
        ToggleWordWrap,
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
        // VS Code's Ctrl+F12, JetBrains' Ctrl+Alt+B and Ctrl+Shift+B.
        KeyBinding::new("secondary-f12", GoToImplementation, ctx),
        KeyBinding::new("secondary-alt-b", GoToImplementation, ctx),
        KeyBinding::new("secondary-shift-b", GoToTypeDefinition, ctx),
        KeyBinding::new("ctrl-shift-space", ShowSignatureHelp, ctx),
        KeyBinding::new("ctrl-alt-i", ToggleInlayHints, ctx),
        // VS Code; JetBrains' Ctrl+Alt+H.
        KeyBinding::new("shift-alt-h", ShowCallHierarchy, ctx),
        KeyBinding::new("ctrl-alt-h", ShowCallHierarchy, ctx),
        KeyBinding::new("secondary-shift-o", GoToSymbol, ctx),
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
        // Word wrap and folding (VS Code).
        KeyBinding::new("alt-z", ToggleWordWrap, ctx),
        KeyBinding::new("secondary-{", Fold, ctx),
        KeyBinding::new("secondary-}", Unfold, ctx),
        KeyBinding::new("secondary-shift-[", Fold, ctx),
        KeyBinding::new("secondary-shift-]", Unfold, ctx),
        KeyBinding::new("secondary-k secondary-l", ToggleFold, ctx),
        KeyBinding::new("secondary-k secondary-0", FoldAll, ctx),
        KeyBinding::new("secondary-k secondary-j", UnfoldAll, ctx),
        KeyBinding::new("secondary-k secondary-[", FoldRecursively, ctx),
        KeyBinding::new("secondary-k secondary-]", UnfoldRecursively, ctx),
        // Refactoring: VS Code, then JetBrains (Shift+F6, Alt+Enter).
        KeyBinding::new("f2", RenameSymbol, ctx),
        KeyBinding::new("shift-f6", RenameSymbol, ctx),
        KeyBinding::new("secondary-.", QuickFix, ctx),
        // The find bar's Select All Matches keeps Alt+Enter while it is open.
        KeyBinding::new("alt-enter", QuickFix, Some("Editor && !find_open")),
        KeyBinding::new("shift-alt-f", FormatDocument, ctx),
        KeyBinding::new("secondary-k secondary-f", FormatSelection, ctx),
        // Git changes: VS Code, then JetBrains (shown in menus).
        KeyBinding::new("alt-f5", NextChange, ctx),
        KeyBinding::new("shift-alt-f5", PreviousChange, ctx),
        KeyBinding::new("ctrl-alt-shift-down", NextChange, ctx),
        KeyBinding::new("ctrl-alt-shift-up", PreviousChange, ctx),
        KeyBinding::new("ctrl-alt-z", RollbackLines, ctx),
        // PyCharm's Copy Reference.
        KeyBinding::new("ctrl-alt-shift-c", pin_text::CopyReference, ctx),
    ]);
    vim::init(cx);
    self::notes::bind_keys(cx);
    cx.bind_keys([KeyBinding::new(
        "escape",
        CloseHunkPopup,
        Some("Editor && showing_hunk_popup"),
    )]);
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
        KeyBinding::new("enter", ConfirmRename, Some("RenameInput > Editor")),
        KeyBinding::new("escape", CancelRename, Some("RenameInput")),
        KeyBinding::new(
            "enter",
            ConfirmBreakpointEdit,
            Some("BreakpointEditor > Editor"),
        ),
        KeyBinding::new(
            "tab",
            NextBreakpointField,
            Some("BreakpointEditor > Editor"),
        ),
        KeyBinding::new("escape", CancelBreakpointEdit, Some("BreakpointEditor")),
    ]);
    // Snippet tab stops; before the signature-help and completion keys,
    // which win while those popovers show.
    let snippet = Some("Editor && in_snippet");
    cx.bind_keys([
        KeyBinding::new("tab", NextTabStop, snippet),
        KeyBinding::new("shift-tab", PreviousTabStop, snippet),
        KeyBinding::new("escape", EndSnippet, snippet),
    ]);
    // Before the completion keys, which win while both popovers show.
    let signature = Some("Editor && showing_signature_help");
    let signatures = Some("Editor && showing_signature_help && multiple_signatures");
    cx.bind_keys([
        KeyBinding::new("escape", CloseSignatureHelp, signature),
        KeyBinding::new("up", PreviousSignature, signatures),
        KeyBinding::new("down", NextSignature, signatures),
        KeyBinding::new("alt-up", PreviousSignature, signatures),
        KeyBinding::new("alt-down", NextSignature, signatures),
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
    // After the completion keys: the two menus never show together.
    let actions = Some("Editor && showing_code_actions");
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevCodeAction, actions),
        KeyBinding::new("down", SelectNextCodeAction, actions),
        KeyBinding::new("enter", ConfirmCodeAction, actions),
        KeyBinding::new("tab", ConfirmCodeAction, actions),
        KeyBinding::new("escape", CloseCodeActions, actions),
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
    /// A save found the file changed on disk since the buffer last matched
    /// it and left it alone; `Editor::save_overwriting` writes anyway.
    SaveConflict,
    /// Open one of these locations (a picker when there are several).
    Navigate {
        locations: Vec<language::Location>,
        title: SharedString,
    },
    /// Show the call hierarchy of these items (from `prepareCallHierarchy`).
    CallHierarchy {
        server: Arc<RunningServer>,
        items: Vec<lsp::types::CallHierarchyItem>,
    },
    /// The document symbols were replaced (fresh from the server or
    /// cleared).
    SymbolsChanged,
    /// Open a picker (Go to Symbol in File, a breadcrumb segment).
    Pick(PickRequest),
    /// Apply a language server's workspace edit (rename, code action)
    /// across the project, then run `command` on that server.
    ApplyEdit {
        edit: lsp::types::WorkspaceEdit,
        server: Arc<RunningServer>,
        command: Option<lsp::types::Command>,
        label: SharedString,
    },
    /// A click in the breakpoint column.
    ToggleBreakpoint {
        line: usize,
    },
    /// The breakpoint editor set the line's breakpoint.
    SetBreakpoint {
        line: usize,
        enabled: bool,
        condition: Option<String>,
        hit_condition: Option<String>,
        log_message: Option<String>,
    },
    RemoveBreakpoint {
        line: usize,
    },
    /// Edits moved the breakpoints given with `set_breakpoints` to these
    /// lines (same order).
    BreakpointsMoved(Vec<usize>),
    /// A click on a gutter marker.
    GutterMarkerClicked {
        line: usize,
        kind: GutterMarkerKind,
    },
    /// Stage Hunk: the file's index entry becomes `contents`.
    StageHunk {
        contents: String,
    },
    /// Show Diff from the hunk popup: the file's diff at current `line`.
    ShowChangeDiff {
        line: usize,
    },
    /// Annotations were turned on: run blame (see `Editor::set_blame`).
    LoadBlame,
    /// A click on an annotation: show this commit.
    ShowCommit(String),
    /// With `spell.in_problems`: the file's unknown words (empty when the
    /// setting is turned off).
    Spelling(Vec<spell::SpellingProblem>),
    /// Vim's `:q` (`discard`: `:q!`): close this editor's tab.
    Close {
        discard: bool,
    },
    /// A notebook cell editor (`Editor::notebook_cell`): Up on its first
    /// screen row (`up`) or Down on its last moves to the neighbouring cell.
    CellBoundary {
        up: bool,
    },
}

/// Completions not from a language server (a notebook kernel): the text and
/// the cursor's byte offset in, candidates and the byte range of the text
/// they replace out (`None`: nothing to offer).
pub type CompletionProvider =
    std::rc::Rc<dyn Fn(String, usize, &mut App) -> Task<Option<(Vec<String>, Range<usize>)>>>;

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
    /// Screen rows: word wrap and folds of this view.
    display: DisplayMap,
    /// Alt+Z's choice (mirrors [`WordWrapOverride`]); `None` follows
    /// `"word_wrap"`.
    word_wrap: Option<bool>,
    /// Wrap at this many columns instead of the view's width.
    #[cfg(test)]
    wrap_columns: Option<usize>,
    /// Foldable regions of the last gutter drawn: version, lines, regions.
    fold_cache: Option<(u64, Range<usize>, Vec<folds::Region>)>,
    /// The mouse is over the gutter: fold chevrons show.
    gutter_hovered: bool,
    /// The mouse is over the scrollbar: its thumb brightens.
    scrollbar_hovered: bool,
    error: Option<SharedString>,
    /// Where the buffer's file lives; `None` for input fields.
    project: Option<Project>,
    syntax: Option<Syntax>,
    /// Reduced features for a very large file (see `LARGE_FILE_BYTES`):
    /// no syntax tree, language server, spell check or minimap colours;
    /// the status bar says so.
    large_file: bool,
    /// Input-field mode: no gutter or status bar; Enter, Tab and vertical
    /// motion bubble up to the parent view.
    single_line: bool,
    /// Multi-line text field (a commit message): no gutter, status bar,
    /// find bar or breadcrumbs; lines wrap.
    text_area: bool,
    /// Notebook cell (`Editor::notebook_cell`): code features without the
    /// file chrome; as tall as its content; Up/Down past the ends emit
    /// `EditorEvent::CellBoundary`.
    notebook_cell: bool,
    /// Completions when no language server is attached.
    completion_provider: Option<CompletionProvider>,
    lsp: Option<LspDocument>,
    /// Language-server status shown in the status bar, with its color.
    lsp_status: Option<(SharedString, fn() -> Hsla)>,
    /// Scratch space for changes drained from the buffer.
    changes: Vec<Change>,
    /// Sorted by start; kept in sync with edits until the server republishes.
    diagnostics: Vec<EditorDiagnostic>,
    semantic: semantic::SemanticState,
    symbols: symbols::SymbolState,
    inlay: inlay_hints::InlayHintState,
    signature: signature_help::SignatureHelpState,
    /// The Ctrl+hover link: the identifier under the mouse while Ctrl is
    /// held, underlined where the server has a definition (Ctrl+click goes
    /// to it).
    link: navigation::LinkState,
    /// Where the mouse last moved over the text, for `link` when only a
    /// modifier changes.
    mouse_position: Option<Point<Pixels>>,
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
    /// Pinpoint reveal: bring these bytes to the middle of the view on the
    /// next layout, without moving the cursor.
    pin_reveal: Option<Range<usize>>,
    /// The enclosing pins' path in the last frame (a notebook cell's
    /// reference names its notebook and cell from it).
    pin_prefix: SharedString,
    /// The text area's right-click menu, where it opened.
    text_menu: Option<Point<Pixels>>,
    find: Option<FindBar>,
    go_to_line: Option<GoToLineInput>,
    rename: Option<rename::RenameInput>,
    _rename_task: Option<Task<()>>,
    code_actions: code_actions::CodeActionState,
    format: format::FormatState,
    /// Tab stops of the snippet last inserted, until it is left.
    snippet: Option<snippets::SnippetSession>,
    /// Breakpoints, run markers, the paused line, hover values.
    debug: debug_gutter::DebugGutterState,
    /// Git change markers against a base version.
    git: git_gutter::GitGutterState,
    /// Annotate with Git Blame.
    blame: blame::BlameState,
    /// The gutter's right-click menu.
    gutter_menu: Option<git_gutter::GutterMenu>,
    /// Unknown words and their quick fixes.
    spell: spell::SpellState,
    /// The minimap's cached blocks and hover.
    minimap: minimap::MinimapState,
    /// Sticky scroll's cached regions and hover.
    sticky: sticky_scroll::StickyState,
    /// Vim emulation (`"vim_mode"`).
    vim: vim::VimState,
    /// Sticky notes on this file's lines.
    notes: self::notes::NotesState,
    /// The save under way and the ones asked for meanwhile.
    save: save::SaveState,
}

/// A mouse drag in progress.
#[derive(Debug, Clone, Copy)]
enum Drag {
    /// Extends the primary selection.
    Select,
    /// Shift+Alt or middle drag: a column selection from (line, column).
    Column { line: usize, column: usize },
    /// The scrollbar thumb, held `grab` below its top.
    Scrollbar { grab: Pixels },
    /// The minimap's slider, from window `y` where the scroll was `scroll`.
    Minimap { y: Pixels, scroll: Pixels },
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
    /// Rendered faded (e.g. unused imports).
    unnecessary: bool,
    /// Rendered struck through.
    deprecated: bool,
    /// As the server sent it (code actions get it back, at `range`).
    lsp: lsp::types::Diagnostic,
}

/// Popover shown when the mouse rests on text: diagnostics and hover docs.
struct HoverPopover {
    /// Screen row of the hovered word.
    row: usize,
    x: Pixels,
    messages: Vec<(Severity, SharedString)>,
    docs: Vec<language::markdown::Block>,
}

/// This editor's document as open on a language server.
struct LspDocument {
    server: Arc<RunningServer>,
    uri: Uri,
    version: i32,
    /// A notebook cell: its place in the notebook's virtual document
    /// (`uri`), which the notebook opens and closes.
    cell: Option<Rc<dyn CellLink>>,
}

/// A view's open server document, moved between views of one file
/// (`Editor::take_server` / `adopt_server`).
pub struct ServerDocument(LspDocument);

/// Geometry from the last prepaint, used for painting and hit-testing.
struct EditorLayout {
    bounds: Bounds<Pixels>,
    text_bounds: Bounds<Pixels>,
    line_height: Pixels,
    em_width: Pixels,
    /// Right edge of the line numbers.
    numbers_right: Pixels,
    /// Left edges of the breakpoint column, the run-marker column (when
    /// there are markers) and the lightbulb.
    breakpoint_left: Pixels,
    marker_left: Option<Pixels>,
    bulb_left: Pixels,
    /// Left edge of the fold chevrons' column (it ends at the text).
    fold_left: Pixels,
    /// Left edge of the change-marker column (file editors).
    change_left: Option<Pixels>,
    /// Left edge of the blame column, while annotations show.
    blame_left: Option<Pixels>,
    /// The cursor line's inline blame: index in `rows` and its text.
    inline_blame: Option<(usize, ShapedLine)>,
    /// Screen row of `rows[0]`.
    first_row: usize,
    rows: Vec<RowLayout>,
    /// `⋯` after folded lines.
    placeholder: ShapedLine,
    /// Chevrons: folded (▸) and foldable (▾).
    chevrons: [ShapedLine; 2],
    /// ▶ of run markers.
    run_marker: ShapedLine,
    /// Indentation guides of the shown lines (not in input fields).
    guides: Option<indent_guides::IndentGuides>,
    /// When the rows don't fit.
    scrollbar: Option<scrollbar::ScrollbarLayout>,
    /// The minimap column, when shown.
    minimap: Option<minimap::MinimapLayout>,
    /// Scope headers pinned at the top of the text.
    sticky: Option<sticky_scroll::StickyLayout>,
}

/// One visible screen row.
struct RowLayout {
    row: DisplayRow,
    /// The text of `clip`.
    shaped: ShapedLine,
    /// The bytes of the row that are laid out: all of it unless the row
    /// runs past the view, then the part in and around it (see
    /// `row_clip`). Offsets outside are placed a column per byte.
    clip: Range<usize>,
    /// Where `clip` starts, from the row's text start: an em per column
    /// before it.
    clip_x: Pixels,
    em_width: Pixels,
    /// Space before the text (continuation indent).
    indent: Pixels,
    /// The line number, on a line's first row.
    number: Option<ShapedLine>,
    /// Most severe diagnostic of the line, on its first row.
    mark: Option<Severity>,
    /// Chevron on a line's first row: `Some(true)` folded, `Some(false)`
    /// foldable.
    chevron: Option<bool>,
    /// On the last row of a folded line: the fold's start (its `⋯`).
    folded: Option<usize>,
    /// Inlay hints spliced into `shaped`.
    inlays: RowInlays,
    /// The blame annotation, on the first row of a group's first line.
    blame: Option<ShapedLine>,
}

impl RowLayout {
    /// x of buffer `offset` (on this row) from the laid-out text's start
    /// (`EditorLayout::text_left`); an inlay hint at `offset` is drawn
    /// after it. Offsets outside `clip` are estimated a column per byte:
    /// they are off the view.
    fn x_for(&self, offset: usize) -> Pixels {
        if offset < self.clip.start {
            return -self.em_width * (self.clip.start - offset) as f32;
        }
        if offset > self.clip.end {
            return self.shaped.width + self.em_width * (offset - self.clip.end) as f32;
        }
        let index = self.inlays.display_index(offset - self.clip.start);
        self.shaped.x_for_index(index)
    }

    /// The buffer offset nearest `x` (from the laid-out text's start);
    /// clicks on an inlay hint land beside it, ones outside `clip` on its
    /// nearer end.
    fn offset_closest_to(&self, x: Pixels) -> usize {
        let index = self.shaped.closest_index_for_x(x);
        self.clip.start + self.inlays.buffer_offset(index)
    }

    /// The buffer offset of the character under `x`, if text (not an
    /// inlay hint) is there.
    fn offset_under(&self, x: Pixels) -> Option<usize> {
        let index = self.shaped.index_for_x(x)?;
        (!self.inlays.is_inlay(index)).then(|| self.clip.start + self.inlays.buffer_offset(index))
    }

    /// Width of the whole row's text from its start (`clip_x` included),
    /// estimated past `clip`.
    #[allow(dead_code)]
    fn text_width(&self) -> Pixels {
        self.clip_x + self.x_for(self.row.range.end)
    }
}

impl EditorLayout {
    /// The screen row at window `y` (rows above the view count as 0).
    fn row_at(&self, y: Pixels, scroll_y: Pixels) -> usize {
        ((y - self.text_bounds.top() + scroll_y) / self.line_height).max(0.) as usize
    }

    /// The laid-out row at window `y`, with its index in `rows`.
    fn row_layout_at(&self, y: Pixels, scroll_y: Pixels) -> Option<(usize, &RowLayout)> {
        let index = self.row_at(y, scroll_y).checked_sub(self.first_row)?;
        self.rows.get(index).map(|row| (index, row))
    }

    /// Window y of the top of `rows[index]`.
    fn row_top(&self, index: usize, scroll_y: Pixels) -> Pixels {
        self.text_bounds.top() + self.line_height * (self.first_row + index) as f32 - scroll_y
    }

    /// Window x where `row`'s laid-out text starts (its `clip`; the whole
    /// row unless it runs past the view).
    fn text_left(&self, row: &RowLayout, scroll_x: Pixels) -> Pixels {
        self.text_bounds.left() - scroll_x + row.indent + row.clip_x
    }

    /// The `⋯` box after `rows[index]`'s text.
    fn placeholder_bounds(&self, index: usize, scroll: Point<Pixels>) -> Bounds<Pixels> {
        let row = &self.rows[index];
        let left = self.text_left(row, scroll.x) + row.shaped.width + self.em_width / 2.;
        let top = self.row_top(index, scroll.y);
        Bounds::new(
            point(left, top + px(2.)),
            size(
                self.placeholder.width + self.em_width,
                self.line_height - px(4.),
            ),
        )
    }
}

/// Narrowest wrap width, in columns.
const MIN_WRAP_COLUMNS: usize = 16;

/// Lines read to detect the indentation style.
const INDENT_SAMPLE_LINES: usize = 2000;

/// Files larger than this, or with a line longer than `LARGE_LINE_BYTES`,
/// open with reduced features (`Editor::large_file`): parsing, a language
/// server and spell checking would stall the UI on them.
const LARGE_FILE_BYTES: usize = 8 * 1024 * 1024;
const LARGE_LINE_BYTES: usize = 64 * 1024;

/// Columns laid out beyond each side of the view on rows longer than it
/// (see `row_clip`).
const CLIP_MARGIN_COLUMNS: usize = 128;

/// Whether `buffer` gets reduced features: one pass over its lines, only
/// when it is big enough for a line to be too long.
fn is_large_file(buffer: &Buffer) -> bool {
    buffer.len() > LARGE_FILE_BYTES
        || (buffer.len() > LARGE_LINE_BYTES && buffer.longest_line_bytes() > LARGE_LINE_BYTES)
}

/// Files up to this size are reloaded from disk through a diff (only the
/// changed text is edited); bigger ones are replaced whole.
const MINIMAL_RELOAD_BYTES: usize = 2 * 1024 * 1024;

/// `selections` after `changes` (in order): cursors move after text
/// inserted at them, selection edges stay outside it.
fn selections_through(changes: &[Change], selections: &SelectionSet) -> SelectionSet {
    let map = |offset: usize, bias: Bias| {
        changes
            .iter()
            .fold(offset, |offset, change| change.map_offset(offset, bias))
    };
    selections.map(|s| {
        let (anchor, head) = if s.reversed() {
            (Bias::Left, Bias::Right)
        } else {
            (Bias::Right, Bias::Left)
        };
        Selection {
            anchor: map(s.anchor, if s.is_empty() { Bias::Right } else { anchor }),
            head: map(s.head, if s.is_empty() { Bias::Right } else { head }),
            goal: s.goal,
            at_row_end: false,
        }
    })
}

/// The part of a row that is laid out: its bytes (the whole row unless
/// it runs past the view) and the columns before them.
struct RowClip {
    range: Range<usize>,
    columns_before: usize,
}

/// Cuts `row` to the columns in `window` (the view and a margin; `None`
/// when rows wrap, as nothing scrolls sideways then), snapped to chars,
/// so shaping and highlighting a long line cost the view's width, not
/// the line's (VS Code's `stopRenderingLineAfter`). Rows that fit are
/// not scanned.
fn row_clip(buffer: &Buffer, row: &DisplayRow, window: Option<&Range<usize>>) -> RowClip {
    let full = RowClip {
        range: row.range.clone(),
        columns_before: 0,
    };
    let Some(window) = window else {
        return full;
    };
    // Every column is at least a byte: such a row is inside the window.
    if window.start == 0 && row.range.len() <= window.end {
        return full;
    }
    let (mut start, mut columns_before) = (None, 0);
    let mut end = row.range.end;
    let (mut columns, mut at) = (0, row.range.start);
    for c in buffer.chunks(row.range.clone()).flat_map(str::chars) {
        if start.is_none() && columns >= window.start {
            start = Some(at);
            columns_before = columns;
        }
        if columns >= window.end {
            end = at;
            break;
        }
        columns += display_map::char_width(c);
        at += c.len_utf8();
    }
    // A row ending before the window shows nothing of itself.
    let start = start.unwrap_or_else(|| {
        columns_before = columns;
        row.range.end
    });
    RowClip {
        range: start..end.max(start),
        columns_before,
    }
}

impl Editor {
    pub fn new(mut buffer: Buffer, cx: &mut Context<Self>) -> Self {
        let language = buffer.path().and_then(Language::from_path);
        let large_file = is_large_file(&buffer);
        let syntax = if large_file {
            None
        } else {
            language.and_then(|language| Syntax::new(language, &buffer))
        };
        buffer.drain_changes();
        let sample: Vec<String> = (0..buffer.line_count().min(INDENT_SAMPLE_LINES))
            .map(|line| buffer.line_text(line))
            .collect();
        let indent = IndentStyle::detect(sample.iter().map(String::as_str), language);
        Self {
            focus_handle: cx.focus_handle(),
            display: DisplayMap::new(&buffer),
            word_wrap: None,
            #[cfg(test)]
            wrap_columns: None,
            fold_cache: None,
            gutter_hovered: false,
            scrollbar_hovered: false,
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
            large_file,
            single_line: false,
            text_area: false,
            notebook_cell: false,
            completion_provider: None,
            lsp: None,
            lsp_status: None,
            changes: Vec::new(),
            diagnostics: Vec::new(),
            semantic: semantic::SemanticState::new(),
            symbols: symbols::SymbolState::new(),
            inlay: inlay_hints::InlayHintState::default(),
            signature: signature_help::SignatureHelpState::default(),
            link: navigation::LinkState::default(),
            mouse_position: None,
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
            pin_reveal: None,
            pin_prefix: SharedString::default(),
            text_menu: None,
            find: None,
            go_to_line: None,
            rename: None,
            _rename_task: None,
            code_actions: code_actions::CodeActionState::default(),
            format: format::FormatState::default(),
            snippet: None,
            debug: Default::default(),
            git: Default::default(),
            blame: Default::default(),
            gutter_menu: None,
            spell: Default::default(),
            minimap: Default::default(),
            sticky: Default::default(),
            vim: Default::default(),
            notes: Default::default(),
            save: Default::default(),
        }
    }

    pub fn language(&self) -> Option<Language> {
        self.syntax.as_ref().map(Syntax::language)
    }

    /// Whether lines wrap at the view's width: Alt+Z's choice, else
    /// `"word_wrap"` for the language.
    pub fn wraps(&self) -> bool {
        !self.single_line && self.word_wrap.unwrap_or_else(|| self.word_wrap_setting())
    }

    /// Like VS Code, for every editor (also those opened later) until the
    /// editor exits; `"word_wrap"` in settings.json is left as it is.
    fn toggle_word_wrap(&mut self, _: &ToggleWordWrap, _: &mut Window, cx: &mut Context<Self>) {
        let wrap = !self.wraps();
        cx.set_global(WordWrapOverride(wrap));
        self.word_wrap = Some(wrap);
        self.autoscroll = true;
        cx.refresh_windows();
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
        if self.large_file
            || self
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
            cell: None,
        });
        self.refresh_semantic_tokens(cx);
        self.symbols_attached(cx);
        self.refresh_inlay_hints(cx);
    }

    /// Forgets the server; `close` also tells it the document closed.
    pub fn detach_server(&mut self, close: bool) {
        self.clear_semantic_tokens();
        self.clear_symbols();
        self.clear_inlay_hints();
        self.dismiss_signature_help();
        self.link = navigation::LinkState::default();
        if let Some(doc) = self.lsp.take()
            && close
            && doc.cell.is_none()
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
        self.symbols_handed_over();
        self.clear_inlay_hints();
        self.dismiss_signature_help();
        self.link = navigation::LinkState::default();
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
        self.symbols_attached(cx);
        self.refresh_inlay_hints(cx);
    }

    /// Sends the drained `changes` to the server: one incremental event
    /// per change when the server takes them (and the batch allows it,
    /// see `lsp_sync::incremental_changes`), otherwise the full text.
    /// Returns whether it sent them.
    fn sync_changes(&mut self) -> bool {
        let Some(doc) = &mut self.lsp else {
            return false;
        };
        if let Some(link) = &doc.cell {
            link.sync(&self.buffer.slice(0..self.buffer.len()));
            doc.version += 1;
            return true;
        }
        let server = &doc.server;
        if server.sync == TextDocumentSyncKind::NONE {
            return false;
        }
        let incremental = (server.sync == TextDocumentSyncKind::INCREMENTAL)
            .then(|| lsp_sync::incremental_changes(&self.changes, &self.buffer, server.encoding))
            .flatten();
        let content_changes = incremental.unwrap_or_else(|| {
            vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: self.buffer.slice(0..self.buffer.len()),
            }]
        });
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

    /// A multi-line input with wrapped lines and no code-editor chrome.
    pub fn text_area(cx: &mut Context<Self>) -> Self {
        Self {
            text_area: true,
            word_wrap: Some(true),
            ..Self::new(Buffer::new(""), cx)
        }
    }

    /// A notebook cell editor: code features (tree-sitter highlighting for
    /// `language`, brackets and auto-close, undo, Vim, spell check,
    /// completion from `set_completion_provider`, the notebook's language
    /// server through `attach_cell_server`) without gutter, status bar,
    /// scrollbar, minimap, sticky scroll, breadcrumbs or find bar (a text
    /// area's chrome). Its element is as tall as
    /// its content (screen rows × line height, at least one row), so a list
    /// can size it; popovers draw over neighbouring cells. Code cells don't
    /// wrap; Markdown cells (`Language::Markdown`) do. Save is left to the
    /// parent, and Up on the first row / Down on the last emit
    /// `EditorEvent::CellBoundary`.
    ///
    /// Its key context adds `notebook_cell`, and `cell_escape` while
    /// Escape has nothing to close in the editor (no completion menu,
    /// signature help, code actions, snippet or extra cursors). A parent
    /// binds its keys in `Editor && notebook_cell` (Escape in
    /// `Editor && cell_escape`) after `bind_keys`: at the same context depth
    /// the later binding wins over the editor's own (Shift+Enter,
    /// Ctrl+Enter, Alt+Enter). Vim sees keys first: Escape leaves insert
    /// or visual mode, and reaches the parent from normal mode; Vim never
    /// takes Shift+Enter in a cell, and its k/j past the ends emit
    /// `CellBoundary` like Up/Down.
    pub fn notebook_cell(text: &str, language: Option<Language>, cx: &mut Context<Self>) -> Self {
        let mut buffer = Buffer::new(text);
        let syntax = language.and_then(|language| Syntax::new(language, &buffer));
        buffer.drain_changes();
        let sample: Vec<String> = (0..buffer.line_count().min(INDENT_SAMPLE_LINES))
            .map(|line| buffer.line_text(line))
            .collect();
        let indent = IndentStyle::detect(sample.iter().map(String::as_str), language);
        Self {
            notebook_cell: true,
            text_area: true,
            word_wrap: Some(language == Some(Language::Markdown)),
            syntax,
            indent,
            ..Self::new(buffer, cx)
        }
    }

    /// Completions from `provider`: alone while no language server is
    /// attached (identifier characters and `.` ask it, as does
    /// Ctrl+Space), after the server's items otherwise.
    pub fn set_completion_provider(&mut self, provider: Option<CompletionProvider>) {
        self.completion_provider = provider;
    }

    /// An editor for a file of `project`; the buffer must have its path.
    /// The document joins the crash recovery dump (`startup::register_document`).
    pub fn for_file(mut buffer: Buffer, project: Project, cx: &mut Context<Self>) -> Self {
        if let Some(path) = buffer.path().map(|path| path.to_path_buf()) {
            if buffer.disk_stamp().is_none() {
                buffer.set_disk_stamp(save::disk_stamp(&project, &path));
            }
            crate::startup::register_document(path, buffer.document_handle());
        }
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

    /// Selects bytes `start..end` (clamped; on character boundaries), e.g.
    /// a file name without its extension.
    pub fn select_text_range(&mut self, start: usize, end: usize, cx: &mut Context<Self>) {
        let end = end.min(self.buffer.len());
        let selection = Selection::new(start.min(end), end);
        self.set_selections(SelectionSet::single(selection), cx);
    }

    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    /// Selections (primary first), lines on screen and diagnostics, read
    /// when an agent asks what the person looks at (`pinpoint::context`).
    pub(crate) fn context_facts(&self) -> crate::pinpoint::context::EditorFacts {
        let primary = self.primary();
        let mut selections = vec![(primary.anchor, primary.head)];
        selections.extend(
            self.selections
                .all()
                .iter()
                .map(|s| (s.anchor, s.head))
                .filter(|s| *s != (primary.anchor, primary.head)),
        );
        let visible = self.visible_lines();
        let diagnostics = self
            .diagnostics
            .iter()
            .map(|d| crate::pinpoint::context::DiagnosticFact {
                range: d.range.clone(),
                severity: d.severity.label(),
                message: d.message.to_string(),
                source: d.lsp.source.clone(),
                code: d.lsp.code.as_ref().map(|code| match code {
                    lsp::types::NumberOrString::Number(n) => n.to_string(),
                    lsp::types::NumberOrString::String(s) => s.clone(),
                }),
            })
            .collect();
        crate::pinpoint::context::EditorFacts {
            selections,
            visible: visible.start..visible.end.min(self.buffer.line_count()),
            diagnostics,
        }
    }

    /// The width the text was last laid out at.
    #[cfg(test)]
    pub(crate) fn text_width(&self) -> Option<Pixels> {
        Some(self.layout.as_ref()?.text_bounds.size.width)
    }

    /// Brings in the file's current contents after an external change.
    /// Unsaved edits are only overwritten when `force` is set.
    pub fn reload_from_disk(&mut self, force: bool, cx: &mut Context<Self>) -> ReloadOutcome {
        let Some((project, path)) = self
            .project
            .clone()
            .zip(self.buffer.path().map(|path| path.to_path_buf()))
        else {
            return ReloadOutcome::Failed;
        };
        // Taken before the read: a file that changes in between looks
        // changed to the next save instead of being overwritten.
        let stamp = save::disk_stamp(&project, &path);
        let Ok(text) = project.read_text(&path) else {
            return ReloadOutcome::Failed;
        };
        let was_dirty = self.buffer.is_dirty();
        let same = self.buffer.equals(&text);
        if was_dirty && !force && !same {
            return ReloadOutcome::Conflict;
        }
        self.buffer.set_disk_stamp(stamp);
        // Only what differs is edited (as one undo step), so folds,
        // diagnostics, selections and notes elsewhere stay where they are;
        // a big file is replaced whole rather than diffed on the UI thread.
        let reloaded = if same || self.large_file || text.len() > MINIMAL_RELOAD_BYTES {
            self.buffer.reload(&text)
        } else {
            let edits = crate::workspace_edit::minimal_edits(&self.text(), &text);
            match self.buffer.reload_with_edits(&text, edits) {
                Ok(reloaded) => reloaded,
                Err(_) => self.buffer.reload(&text),
            }
        };
        if !reloaded {
            // Same text as on disk (e.g. our own save): nothing to redraw
            // except a cleared dirty marker.
            self.finish_change(was_dirty, cx);
            if was_dirty {
                self.notes_reloaded(cx);
            }
            return ReloadOutcome::Unchanged;
        }
        let before = self.selections.clone();
        let transaction = self.buffer.transaction_id();
        let changes: Vec<Change> = self.buffer.drain_changes().collect();
        self.selections = selections_through(&changes, &before).clamped(&self.buffer);
        self.record_history(transaction, before);
        self.finish_change_with(changes, was_dirty, cx);
        if was_dirty {
            // Unsaved edits discarded: their note positions go too.
            self.notes_reloaded(cx);
        } else {
            // The file changed on disk and the notes followed: that is saved text.
            self.write_note_anchors(cx);
        }
        ReloadOutcome::Reloaded
    }

    // Saving (`Editor::save`, `Editor::save_overwriting`) is in `save.rs`.

    /// Moves every selection's head to `target`'s offset (with its goal
    /// column and row-end affinity); without `select` the selection
    /// collapses there.
    fn move_selections(
        &mut self,
        select: bool,
        target: impl Fn(&Self, &Selection) -> (usize, Option<usize>, bool),
        cx: &mut Context<Self>,
    ) {
        let selections = self.selections.map(|s| {
            let (target, goal, at_row_end) = target(self, s);
            // Motions step over folded text instead of opening it.
            let bias = if target > s.head {
                Bias::Right
            } else {
                Bias::Left
            };
            let head = self.display.clip(&self.buffer, target, bias);
            Selection {
                anchor: if select { s.anchor } else { head },
                head,
                goal,
                at_row_end: at_row_end && head == target,
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
        self.move_selections(select, |editor, s| (target(editor, s), None, false), cx);
    }

    /// Moves by screen rows, keeping the goal column (in screen columns).
    fn vertical(&mut self, rows: isize, select: bool, cx: &mut Context<Self>) {
        self.move_selections(
            select,
            |editor, s| {
                let (buffer, display) = (&editor.buffer, &editor.display);
                let (row, column) = display.point(buffer, s.head, s.at_row_end);
                let goal = s.goal.unwrap_or(column);
                let target = row as isize + rows;
                let (offset, at_row_end) = if target < 0 {
                    (0, false)
                } else if target as usize >= display.row_count() {
                    display.offset_at(buffer, display.row_count() - 1, usize::MAX)
                } else {
                    display.offset_at(buffer, target as usize, goal)
                };
                (offset, Some(goal), at_row_end)
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

    /// The start of a wrapped row, then the first non-blank column of the
    /// line, then the line start.
    fn home_target(&self, s: &Selection) -> usize {
        let (_, row) = self.display.row_of(&self.buffer, s.head, s.at_row_end);
        if !row.first && s.head != row.range.start {
            return row.range.start;
        }
        let line = self.buffer.line_of(s.head);
        let start = self.buffer.line_start(line);
        let text = self.buffer.line_text(line);
        let indented = start + text.len() - text.trim_start().len();
        if s.head == indented { start } else { indented }
    }

    /// The end of a wrapped row (drawn there, not at the next row's start),
    /// then the end of the line.
    fn end_target(&self, s: &Selection) -> (usize, Option<usize>, bool) {
        let (_, row) = self.display.row_of(&self.buffer, s.head, s.at_row_end);
        if !row.last && s.head != row.range.end {
            (row.range.end, None, true)
        } else {
            (self.buffer.line_end(row.line), None, false)
        }
    }

    /// Screen rows in a page.
    fn page_rows(&self) -> isize {
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
                    at_row_end: false,
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
        self.vim_edited();
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
        self.map_symbols();
        self.snippet_edited();
        self.spell_edited();
        self.find_edited();
        self.minimap_edited();
        self.debug_gutter_edited(cx);
        self.notes_edited(cx);
        // Folds move with the text (or open when edited inside); edited
        // lines wrap again.
        self.display.edit(&self.buffer, &self.changes);
        self.reveal_selections();
        if edited {
            self.hover = None;
            self.hover_word = None;
            self._hover_delay = None;
            self.clear_link();
            self.notice = None;
            self.git_text_edited(cx);
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
            self.symbols_edited(cx);
            self.inlay_hints_edited(cx);
        }
        if edited {
            cx.emit(EditorEvent::Edited);
        }
        if edited {
            self.update_completion_menu(cx);
            self.signature_help_moved(cx);
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
        self.delete_selections_or(|editor, head| editor.buffer.prev_grapheme(head)..head, cx);
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_selections_or(|editor, head| head..editor.buffer.next_grapheme(head), cx);
    }

    fn delete_word_left(&mut self, _: &DeleteWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_selections_or(|editor, head| editor.delete_word_left_range(head), cx);
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
        if !self.cell_boundary(true, cx) {
            self.vertical(-1, false, cx);
        }
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        if !self.cell_boundary(false, cx) {
            self.vertical(1, false, cx);
        }
    }

    /// In a notebook cell, Up on the first screen row (Down on the last)
    /// with one cursor and no selection leaves the cell: emits
    /// `CellBoundary` and returns true.
    fn cell_boundary(&mut self, up: bool, cx: &mut Context<Self>) -> bool {
        let s = self.primary();
        if !self.notebook_cell || self.selections.len() > 1 || !s.is_empty() {
            return false;
        }
        let (row, _) = self.display.point(&self.buffer, s.head, s.at_row_end);
        let at_edge = if up {
            row == 0
        } else {
            row + 1 >= self.display.row_count()
        };
        if at_edge {
            cx.emit(EditorEvent::CellBoundary { up });
        }
        at_edge
    }

    /// A popover over the text; a notebook cell's draws over the cells
    /// around it (its own bounds are only its text).
    fn popover(&self, popover: impl IntoElement) -> AnyElement {
        if self.notebook_cell {
            gpui::deferred(popover).with_priority(1).into_any_element()
        } else {
            popover.into_any_element()
        }
    }

    /// The primary cursor's screen row: its top (from the top of a notebook
    /// cell's text) and height, to scroll the notebook to it.
    pub fn cursor_row_bounds(&self) -> (Pixels, Pixels) {
        let s = self.primary();
        let (row, _) = self.display.point(&self.buffer, s.head, s.at_row_end);
        let line_height = self
            .layout
            .as_ref()
            .map_or(px(theme::EDITOR_LINE_HEIGHT), |layout| layout.line_height);
        (line_height * row as f32, line_height)
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
        self.move_selections(false, Self::end_target, cx);
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, Self::home_target, cx);
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(true, Self::end_target, cx);
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
        self.vertical(-self.page_rows(), false, cx);
    }

    fn page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(self.page_rows(), false, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.select_all_text(cx);
    }

    fn save_action(&mut self, _: &Save, _: &mut Window, cx: &mut Context<Self>) {
        self.save_with_format(cx);
    }

    fn offset_for_position(&self, position: Point<Pixels>) -> usize {
        self.cursor_for_position(position).0
    }

    /// The cursor nearest `position`: its offset and whether it sits at the
    /// end of a wrapped row (a click past the row's end).
    fn cursor_for_position(&self, position: Point<Pixels>) -> (usize, bool) {
        let Some(layout) = &self.layout else {
            let primary = self.primary();
            return (primary.head, primary.at_row_end);
        };
        let x = position.x - layout.text_bounds.left() + self.scroll.x;
        match layout.row_layout_at(position.y, self.scroll.y) {
            Some((_, row)) => {
                let offset = row.offset_closest_to(x - row.indent - row.clip_x);
                let offset = offset.min(row.row.range.end);
                (offset, row.row.wraps_at(offset))
            }
            // Dragged past the visible rows.
            None => self.display.offset_at(
                &self.buffer,
                layout.row_at(position.y, self.scroll.y),
                (x / layout.em_width).round().max(0.) as usize,
            ),
        }
    }

    /// Line and char column under `position`, counting columns past the
    /// line end (column selections keep them). Wrapped lines count buffer
    /// columns, so a column selection takes the same chars of each line.
    fn line_column_at(&self, position: Point<Pixels>) -> (usize, usize) {
        let offset = self.offset_for_position(position);
        let line = self.buffer.line_of(offset);
        let mut column = self.buffer.column(offset);
        if let Some(layout) = &self.layout
            && offset == self.buffer.line_end(line)
            && let Some((_, row)) = layout.row_layout_at(position.y, self.scroll.y)
            && row.row.last
        {
            let x = position.x - layout.text_left(row, self.scroll.x) - row.shaped.width;
            column += (x / layout.em_width).round().max(0.) as usize;
        }
        (line, column)
    }

    fn start_mouse(&mut self, window: &mut Window) {
        window.focus(&self.focus_handle);
        self.hover = None;
        self.hover_word = None;
        self._hover_delay = None;
        self.completion = None;
        self.rename = None;
        self.code_actions.menu = None;
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
        if self.scrollbar_mouse_down(event.position, cx)
            || self.minimap_mouse_down(event.position, cx)
            || self.sticky_mouse_down(event.position, cx)
        {
            return;
        }
        if !self.single_line && !self.text_area && self.gutter_menu.take().is_some() {
            cx.notify();
        }
        if !self.single_line
            && !self.text_area
            && (self.change_marker_click(event.position, cx)
                || self.blame_click(event.position, cx)
                || self.debug_gutter_click(event.position, cx)
                || self.lightbulb_click(event.position, cx)
                || self.note_marker_click(event.position, window, cx)
                || self.fold_click(event.position, cx))
        {
            return;
        }
        let (offset, at_row_end) = self.cursor_for_position(event.position);
        let cursor = Selection {
            at_row_end,
            ..Selection::cursor(offset)
        };
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
                    let selections = self.selections.with(cursor);
                    self.set_selections(selections, cx);
                    self.drag = Some(Drag::Select);
                }
            }
        } else if modifiers.secondary() {
            // Ctrl+click: Go to Definition there; no selection drag.
            self.clear_link();
            self.set_selections(SelectionSet::single(cursor), cx);
            self.request_definition(offset, cx);
        } else {
            let anchor = if modifiers.shift {
                self.primary().anchor
            } else {
                offset
            };
            let selection = Selection { anchor, ..cursor };
            self.set_selections(SelectionSet::single(selection), cx);
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

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if matches!(
            self.drag,
            Some(Drag::Scrollbar { .. } | Drag::Minimap { .. })
        ) {
            // The thumb dims again.
            cx.notify();
        }
        self.drag = None;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        match self.drag {
            Some(Drag::Select) => {
                let (head, at_row_end) = self.cursor_for_position(event.position);
                let primary = self.primary();
                let selections = self.selections.with_primary(Selection {
                    at_row_end,
                    ..Selection::new(primary.anchor, head)
                });
                self.set_selections(selections, cx);
            }
            Some(Drag::Column { line, column }) => {
                let head = self.line_column_at(event.position);
                let selections = self.column_selection((line, column), head);
                self.set_selections(selections, cx);
            }
            Some(Drag::Scrollbar { grab }) => self.scrollbar_drag(event.position.y, grab, cx),
            Some(Drag::Minimap { y, scroll }) => self.minimap_drag(event.position.y, y, scroll, cx),
            None => {
                let over_scrollbar = self.over_scrollbar(event.position);
                if over_scrollbar != self.scrollbar_hovered {
                    self.scrollbar_hovered = over_scrollbar;
                    cx.notify();
                }

                let over_gutter = self.layout.as_ref().is_some_and(|layout| {
                    layout.bounds.contains(&event.position)
                        && event.position.x < layout.text_bounds.left()
                        && !self.over_minimap(event.position)
                });
                if over_gutter != self.gutter_hovered {
                    self.gutter_hovered = over_gutter;
                    cx.notify();
                }
                self.mouse_position = Some(event.position);
                self.minimap_mouse_moved(event.position, cx);
                self.sticky_mouse_moved(event.position, cx);
                self.blame_mouse_moved(event.position, cx);
                self.link_mouse_moved(event.position, event.modifiers.secondary(), cx);
                self.update_hover(event.position, cx)
            }
        }
    }

    /// A click on a fold chevron or a `⋯` placeholder: folds or unfolds.
    fn fold_click(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some(layout) = &self.layout else {
            return false;
        };
        let Some((index, row)) = layout.row_layout_at(position.y, self.scroll.y) else {
            return false;
        };
        let line = row.row.line;
        if (layout.fold_left..layout.text_bounds.left()).contains(&position.x) && row.row.first {
            let foldable = row.chevron.is_some()
                || self
                    .fold_regions(line..line + 1)
                    .iter()
                    .any(|&(header, _)| header == line);
            if foldable {
                self.toggle_fold_at(line, cx);
                return true;
            }
        }
        if let Some(start) = row.folded
            && layout
                .placeholder_bounds(index, self.scroll)
                .contains(&position)
        {
            self.unfold_at(start, cx);
            return true;
        }
        false
    }

    /// Screen row and offset of the character under `position`, if it is
    /// over text.
    fn character_at(&self, position: Point<Pixels>) -> Option<(usize, usize)> {
        let layout = self.layout.as_ref()?;
        if !layout.text_bounds.contains(&position)
            || self.over_scrollbar(position)
            || self.over_sticky(position)
        {
            return None;
        }
        let (index, row) = layout.row_layout_at(position.y, self.scroll.y)?;
        let offset = row.offset_under(position.x - layout.text_left(row, self.scroll.x))?;
        Some((layout.first_row + index, offset))
    }

    /// Where `offset` is drawn, if its row is laid out: window x and the
    /// row's top relative to the text area.
    fn screen_position(&self, offset: usize) -> Option<(Pixels, Pixels)> {
        let layout = self.layout.as_ref()?;
        let index = layout
            .rows
            .iter()
            .position(|row| row.row.shows(offset, false))?;
        let row = &layout.rows[index];
        let x = layout.text_left(row, self.scroll.x) + row.x_for(offset);
        let top = layout.line_height * (layout.first_row + index) as f32 - self.scroll.y;
        Some((x, top))
    }

    fn clear_hover(&mut self, hovered: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if !hovered && (self.gutter_hovered || self.scrollbar_hovered) {
            self.gutter_hovered = false;
            self.scrollbar_hovered = false;
            cx.notify();
        }
        if !hovered && (self.hover.is_some() || self._hover_delay.is_some()) {
            self.hover = None;
            self.hover_word = None;
            self._hover_delay = None;
            cx.notify();
        }
        if !hovered {
            self.clear_blame_hover(cx);
            self.mouse_position = None;
            self.link_mouse_left(cx);
        }
    }

    /// Pressing or releasing Ctrl over an identifier shows or hides its link.
    fn on_modifiers_changed(
        &mut self,
        event: &gpui::ModifiersChangedEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.link_modifiers_changed(event.modifiers.secondary(), cx);
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
        // A cell gets its notebook's diagnostics (virtual positions).
        let cell = self.lsp.as_ref().is_some_and(|doc| doc.cell.is_some());
        let shift = self.server_line_shift();
        let lines = self.buffer.line_count() as u32;
        let diagnostics = diagnostics.iter().filter(|diagnostic| {
            !cell
                || shift.is_some_and(|shift| {
                    (shift..shift + lines).contains(&diagnostic.range.start.line)
                })
        });
        let offset = |position: lsp::types::Position| {
            let point = text::Point {
                row: position.line.saturating_sub(shift.unwrap_or(0)) as usize,
                column: position.character as usize,
            };
            self.buffer.offset_of(point, encoding)
        };
        self.diagnostics = diagnostics
            .map(|diagnostic| {
                let end = offset(diagnostic.range.end);
                let start = offset(diagnostic.range.start).min(end);
                let tags = diagnostic.tags.as_deref().unwrap_or_default();
                EditorDiagnostic {
                    range: self.visible_range(start, end),
                    severity: match diagnostic.severity {
                        Some(DiagnosticSeverity::WARNING) => Severity::Warning,
                        Some(DiagnosticSeverity::INFORMATION) => Severity::Information,
                        Some(DiagnosticSeverity::HINT) => Severity::Hint,
                        _ => Severity::Error,
                    },
                    message: diagnostic.message.clone().into(),
                    unnecessary: tags.contains(&DiagnosticTag::UNNECESSARY),
                    deprecated: tags.contains(&DiagnosticTag::DEPRECATED),
                    lsp: diagnostic.clone(),
                }
            })
            .collect();
        self.diagnostics
            .sort_by_key(|d| (d.range.start, d.severity));
        self.hover = None;
        self.code_action_diagnostics_changed();
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
        // A notebook cell shows all its rows: vertical scrolling is the
        // notebook's (the event goes on to it).
        if self.notebook_cell {
            if delta.x == px(0.) {
                return;
            }
            delta.y = px(0.);
        }
        // Upper bounds are clamped in `layout`, which knows the viewport.
        self.scroll.x = (self.scroll.x - delta.x).max(px(0.));
        self.scroll.y = (self.scroll.y - delta.y).max(px(0.));
        self.hover = None;
        self._hover_delay = None;
        self.clear_link();
        cx.notify();
    }

    /// Columns rows wrap at for a text area `width` wide.
    fn wrap_columns(&self, width: Pixels, em_width: Pixels) -> usize {
        #[cfg(test)]
        if let Some(columns) = self.wrap_columns {
            return columns;
        }
        ((width - em_width) / em_width)
            .floor()
            .max(MIN_WRAP_COLUMNS as f32) as usize
    }

    /// A notebook cell's screen rows when its text is `width` wide (wrapped
    /// if it wraps; at least one).
    fn content_rows(&mut self, width: Option<Pixels>, em_width: Pixels) -> usize {
        let columns = match width {
            Some(width) if self.wraps() => Some(self.wrap_columns(width, em_width)),
            _ => None,
        };
        self.display.set_wrap(&self.buffer, columns);
        self.display.row_count().max(1)
    }

    /// Shapes the visible rows and resolves scrolling for `bounds`.
    fn layout(&mut self, bounds: Bounds<Pixels>, window: &mut Window) {
        // The minimap's column comes off the gutter and text's part.
        let full_bounds = bounds;
        let (bounds, minimap_column) = self.minimap_split(bounds);
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
        // Gutter: breakpoints (and run markers), the code-action
        // lightbulb, blame annotations, line numbers, change markers, then
        // the fold chevrons' column.
        let mut change_left = None;
        let mut blame_left = None;
        let (numbers_right, fold_left, gutter_width, breakpoint_left, marker_left, bulb_left) =
            if self.single_line || self.text_area {
                let left = bounds.left();
                (left, left, px(0.), left, None, left)
            } else {
                let (breakpoint_left, marker_left, debug_width) =
                    self.debug_columns(bounds.left(), em_width);
                let bulb = em_width * code_actions::LIGHTBULB_COLUMNS;
                let blame = self.blame_width(em_width);
                if blame > px(0.) {
                    blame_left = Some(bounds.left() + debug_width + bulb);
                }
                let numbers =
                    debug_width + bulb + blame + GUTTER_PADDING / 2. + em_width * digits as f32;
                let changes = if self.project.is_some() || self.git.base.is_some() {
                    change_left = Some(bounds.left() + numbers + px(2.));
                    git_gutter::CHANGE_COLUMN
                } else {
                    px(0.)
                };
                let fold_left = numbers + changes + GUTTER_PADDING / 2.;
                let gutter = fold_left + em_width * 1.5 + GUTTER_PADDING / 2.;
                (
                    bounds.left() + numbers,
                    bounds.left() + fold_left,
                    gutter,
                    breakpoint_left,
                    marker_left,
                    bounds.left() + debug_width,
                )
            };
        let text_bounds = Bounds::from_corners(
            point(bounds.left() + gutter_width, bounds.top()),
            bounds.bottom_right(),
        );
        let viewport = text_bounds.size;

        // Rows wrap at the view's width, so nothing scrolls sideways then.
        let columns = self
            .wraps()
            .then(|| self.wrap_columns(viewport.width, em_width));
        self.display.set_wrap(&self.buffer, columns);
        if columns.is_some() {
            self.scroll.x = px(0.);
        }

        let primary = self.primary();
        let (primary_head, primary_at_row_end) = (primary.head, primary.at_row_end);
        let (cursor_row, _) = self
            .display
            .point(&self.buffer, primary_head, primary_at_row_end);
        if let Some(line) = self.pending_scroll_line.take() {
            self.scroll.y = line_height * self.display.line_row(line) as f32;
            self.autoscroll = false;
        }
        if self.center_cursor {
            self.center_cursor = false;
            self.autoscroll = false;
            self.scroll.y = line_height * cursor_row as f32 - viewport.height / 2.;
        }
        if let Some(range) = self.pin_reveal.take() {
            self.autoscroll = false;
            self.scroll = self.reveal_scroll(range, line_height, em_width, viewport, columns);
        }
        if self.autoscroll {
            let top = line_height * cursor_row as f32;
            if top < self.scroll.y {
                self.scroll.y = top;
            } else if top + line_height > self.scroll.y + viewport.height {
                self.scroll.y = top + line_height - viewport.height;
            }
            // Not under the pinned scope headers either.
            self.scroll.y =
                self.sticky_autoscroll(cursor_row, self.scroll.y, line_height, viewport.height);
        }
        let max_scroll_y = if self.notebook_cell {
            px(0.)
        } else {
            line_height * self.display.row_count().saturating_sub(1) as f32
        };
        self.scroll.y = self.scroll.y.min(max_scroll_y).max(px(0.));

        let first_row = (self.scroll.y / line_height).floor() as usize;
        let visible_rows = (viewport.height / line_height).ceil() as usize + 1;
        let display_rows = self.display.rows(&self.buffer, first_row, visible_rows);
        // Where the view will be once it follows the cursor, roughly (a
        // column per char), so the rows are cut around there; the exact
        // adjustment comes below, once they are shaped.
        if self.autoscroll && columns.is_none() {
            let x = em_width * self.buffer.column(primary_head) as f32;
            let margin = em_width * 2.;
            if x < self.scroll.x {
                self.scroll.x = (x - margin).max(px(0.));
            } else if x + margin > self.scroll.x + viewport.width {
                self.scroll.x = x + margin - viewport.width;
            }
        }
        // What each row shows of its text: rows longer than the view are
        // cut to the columns in and around it (none wrap then).
        let clip_window = columns.is_none().then(|| {
            let first = (self.scroll.x / em_width).floor().max(0.) as usize;
            let count = (viewport.width / em_width).ceil().max(0.) as usize;
            first.saturating_sub(CLIP_MARGIN_COLUMNS)..first + count + CLIP_MARGIN_COLUMNS
        });
        let clips: Vec<RowClip> = display_rows
            .iter()
            .map(|row| row_clip(&self.buffer, row, clip_window.as_ref()))
            .collect();

        // Chevrons: folded lines always, foldable ones while the mouse is
        // over the gutter (VS Code).
        let foldable: Vec<usize> = match (display_rows.first(), display_rows.last()) {
            (Some(first), Some(last)) if self.gutter_hovered && !self.single_line => {
                let lines = first.line..last.line + 1;
                let regions = self.cached_fold_regions(lines);
                regions.iter().map(|&(header, _)| header).collect()
            }
            _ => Vec::new(),
        };
        let text_color = theme::text();
        let muted = theme::muted();
        let theme = theme::current();
        let legend = self
            .lsp
            .as_ref()
            .and_then(|doc| doc.server.semantic_legend.as_ref());
        let styler = semantic::Styler::new(&theme, self.language(), legend);
        let base_run = run(0, text_color);
        // Highlighted per run of consecutive whole rows (folded text is
        // never highlighted); a cut row is queried for its shown part
        // alone, so a long line's tokens off the view are never visited.
        let mut spans = Vec::new();
        if let Some(syntax) = &self.syntax {
            // The run of whole rows being collected and its last line.
            let mut run: Option<(Range<usize>, usize)> = None;
            for (row, clip) in display_rows.iter().zip(&clips) {
                let whole = clip.range == row.range;
                if whole
                    && let Some((range, last_line)) = &mut run
                    && row.line <= *last_line + 1
                {
                    range.end = row.range.end;
                    *last_line = row.line;
                    continue;
                }
                if let Some((range, _)) = run.take() {
                    spans.extend(syntax.highlights(&self.buffer, range));
                }
                if whole {
                    run = Some((row.range.clone(), row.line));
                } else if !clip.range.is_empty() {
                    spans.extend(syntax.highlights(&self.buffer, clip.range.clone()));
                }
            }
            if let Some((range, _)) = run {
                spans.extend(syntax.highlights(&self.buffer, range));
            }
        }

        let mut rows = Vec::with_capacity(display_rows.len());
        let mut cuts = Vec::new();
        let mut diagnostics: Vec<&EditorDiagnostic> = Vec::new();
        let mut cursor_lines: Vec<usize> = self
            .selections
            .all()
            .iter()
            .map(|s| self.buffer.line_of(s.head))
            .collect();
        cursor_lines.dedup();
        let inlay_run = TextRun {
            background_color: Some(theme::inlay_hint_background()),
            ..run(0, theme::inlay_hint())
        };
        let mut row_inlays = Vec::new();
        for (row, clip) in display_rows.into_iter().zip(clips) {
            // The shown part of the row (all of it, usually).
            let range = clip.range.clone();
            // Tabs render as one space for now; byte offsets stay aligned.
            let text = self.buffer.slice(range.clone()).replace('\t', " ");
            let first_span = spans.partition_point(|span| span.range.end <= range.start);
            let row_spans = &spans[first_span..];
            let row_spans =
                &row_spans[..row_spans.partition_point(|span| span.range.start < range.end)];
            let highlights = LineHighlights {
                spans: row_spans,
                tokens: semantic::overlapping(&self.semantic.tokens, range.clone()),
            };
            diagnostics_in(&self.diagnostics, &range, &mut diagnostics);
            let runs = line_runs(
                range,
                &highlights,
                &styler,
                self.marked.as_ref(),
                &diagnostics,
                &base_run,
                &mut cuts,
            );
            // Inlay hints on the row: one at its end belongs to the next
            // row of the line, if there is one.
            row_inlays.clear();
            row_inlays.extend(
                self.display
                    .inlays_in(clip.range.clone())
                    .iter()
                    .filter(|inlay| inlay.offset < row.range.end || row.last)
                    .map(|inlay| (inlay.offset - clip.range.start, inlay)),
            );
            let (text, runs, inlays) =
                inlay_hints::splice_inlays(&text, runs, &row_inlays, &inlay_run);
            let shaped = shape(text, &runs);

            let line = row.line;
            let gutter = row.first && !self.single_line && !self.text_area;
            let folded_header = self.display.is_folded_header(&self.buffer, line);
            let number = gutter.then(|| {
                let number = (line + 1).to_string();
                let color = if cursor_lines.binary_search(&line).is_ok() {
                    text_color
                } else {
                    muted
                };
                shape(number.clone(), &[run(number.len(), color)])
            });
            let mark = if gutter {
                let line_range = self.buffer.line_start(line)..self.buffer.line_end(line);
                diagnostics_in(&self.diagnostics, &line_range, &mut diagnostics);
                diagnostics
                    .iter()
                    .map(|d| d.severity)
                    .filter(|&severity| severity < Severity::Hint)
                    .min()
            } else {
                None
            };
            let chevron = if !gutter {
                None
            } else if folded_header {
                Some(true)
            } else {
                foldable.binary_search(&line).is_ok().then_some(false)
            };
            let blame = (gutter && blame_left.is_some())
                .then(|| self.blame_label(line))
                .flatten()
                .map(|(label, color)| shape(label.clone(), &[run(label.len(), color)]));
            rows.push(RowLayout {
                indent: em_width * row.indent as f32,
                folded: (row.last && folded_header).then(|| self.buffer.line_end(line)),
                row,
                shaped,
                clip_x: em_width * clip.columns_before as f32,
                clip: clip.range,
                em_width,
                number,
                mark,
                chevron,
                inlays,
                blame,
            });
        }
        let inline_blame = self.inline_blame_text().and_then(|text| {
            let line = self.buffer.line_of(primary_head);
            let index = rows
                .iter()
                .position(|row| row.row.line == line && row.row.last)?;
            let color = theme::muted().opacity(0.8);
            Some((index, shape(text.clone(), &[run(text.len(), color)])))
        });

        if self.autoscroll
            && columns.is_none()
            && let Some(row) = rows
                .iter()
                .find(|row| row.row.shows(primary_head, primary_at_row_end))
        {
            // A cursor still off the row's laid-out part (wide chars
            // threw the estimate above off by more than the margin) is
            // placed by counting the columns up to it, once; the view
            // lands on it and another frame lays that part out.
            let columns_between = |range: Range<usize>| {
                let columns: usize = self
                    .buffer
                    .chunks(range)
                    .flat_map(str::chars)
                    .map(display_map::char_width)
                    .sum();
                em_width * columns as f32
            };
            let x = if primary_head < row.clip.start {
                window.refresh();
                columns_between(row.row.range.start..primary_head)
            } else if primary_head > row.clip.end {
                window.refresh();
                row.clip_x + row.shaped.width + columns_between(row.clip.end..primary_head)
            } else {
                row.clip_x + row.x_for(primary_head)
            };
            let margin = em_width * 2.;
            if x < self.scroll.x {
                self.scroll.x = (x - margin).max(px(0.));
            } else if x + margin > self.scroll.x + viewport.width {
                self.scroll.x = x + margin - viewport.width;
            }
        }
        self.autoscroll = false;

        let guides = match (rows.first(), rows.last()) {
            (Some(first), Some(last)) if !self.single_line && !self.text_area => {
                let unit = if self.indent.tabs {
                    1
                } else {
                    self.indent.width
                };
                Some(indent_guides::IndentGuides::new(
                    &self.buffer,
                    first.row.line..last.row.line + 1,
                    Some(self.buffer.line_of(primary_head)),
                    unit,
                    self.language(),
                ))
            }
            _ => None,
        };
        let scrollbar = (!self.single_line && !self.text_area)
            .then(|| {
                scrollbar::ScrollbarLayout::new(
                    minimap::scrollbar_area(text_bounds, full_bounds, minimap_column),
                    line_height,
                    self.display.row_count(),
                    self.scroll.y,
                )
            })
            .flatten();

        let minimap =
            minimap_column.map(|column| self.layout_minimap(column, line_height, viewport.height));
        let sticky = self.layout_sticky(window, bounds.left(), text_bounds, line_height);
        let glyph = |glyph: &str| shape(glyph.into(), &[run(glyph.len(), muted)]);
        self.layout = Some(EditorLayout {
            bounds: full_bounds,
            text_bounds,
            line_height,
            em_width,
            numbers_right,
            breakpoint_left,
            marker_left,
            bulb_left,
            fold_left,
            change_left,
            blame_left,
            inline_blame,
            first_row,
            rows,
            placeholder: glyph("⋯"),
            chevrons: [glyph("▸"), glyph("▾")],
            run_marker: shape("▶".into(), &[run("▶".len(), theme::run_marker())]),
            guides,
            scrollbar,
            minimap,
            sticky,
        });
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let brackets = if self.single_line {
            None
        } else {
            self.cached_bracket_pair()
        };
        self.update_vim_highlights(cx);
        let Some(layout) = &self.layout else {
            return;
        };
        let line_height = layout.line_height;
        let selections = self.selections.all();
        let focused = self.focus_handle.is_focused(window);
        let (matches, active_match) = match &self.find {
            Some(find) if find.open => (find.matches.as_slice(), self.active_match()),
            _ => (self.vim_highlights(), None),
        };
        let bracket_ranges: Vec<Range<usize>> = brackets
            .into_iter()
            .flat_map(|(a, b)| [a..a + 1, b..b + 1])
            .collect();
        // Lines of the cursors without a selection, across gutter and text.
        let mut current_lines: Vec<usize> = if focused && !self.single_line && !self.text_area {
            selections
                .iter()
                .filter(|s| s.is_empty())
                .map(|s| self.buffer.line_of(s.head))
                .collect()
        } else {
            Vec::new()
        };
        current_lines.sort_unstable();
        current_lines.dedup();

        for (i, row) in layout.rows.iter().enumerate() {
            let top = layout.row_top(i, self.scroll.y);
            if current_lines.binary_search(&row.row.line).is_ok() {
                window.paint_quad(fill(
                    Bounds::from_corners(
                        point(bounds.left(), top),
                        point(bounds.right(), top + line_height),
                    ),
                    theme::current_line(),
                ));
            }
            if let Some(number) = &row.number {
                let origin = point(layout.numbers_right - number.width, top);
                number.paint(origin, line_height, window, cx).ok();
            }
            if let Some(severity) = row.mark {
                window.paint_quad(fill(
                    Bounds::new(
                        point(bounds.left() + px(3.), top + px(3.)),
                        size(px(3.), line_height - px(6.)),
                    ),
                    severity.color(),
                ));
            }
            if let Some(folded) = row.chevron {
                let chevron = &layout.chevrons[if folded { 0 } else { 1 }];
                let column = layout.text_bounds.left() - layout.fold_left;
                let x = layout.fold_left + (column - chevron.width) / 2.;
                chevron.paint(point(x, top), line_height, window, cx).ok();
            }
            if row.number.is_some() {
                self.paint_note_marker(layout, row.row.line, top, window, cx);
                self.paint_lightbulb(layout, row.row.line, top, window);
                self.paint_debug_gutter(layout, row.row.line, top, window, cx);
            }
            if let (Some(label), Some(left)) = (&row.blame, layout.blame_left) {
                label
                    .paint(
                        point(left + layout.em_width / 2., top),
                        line_height,
                        window,
                        cx,
                    )
                    .ok();
            }
            self.paint_change_marker(layout, row, top, window);
        }
        if let Some(left) = layout.blame_left {
            let right = left + layout.em_width * blame::BLAME_COLUMNS - px(1.);
            window.paint_quad(fill(
                Bounds::from_corners(
                    point(right, bounds.top()),
                    point(right + px(1.), bounds.bottom()),
                ),
                theme::border(),
            ));
        }

        window.with_content_mask(
            Some(ContentMask {
                bounds: layout.text_bounds,
            }),
            |window| {
                for (i, row) in layout.rows.iter().enumerate() {
                    let top = layout.row_top(i, self.scroll.y);
                    let text_left = layout.text_left(row, self.scroll.x);
                    let shaped = &row.shaped;
                    let (start, end) = (row.row.range.start, row.row.range.end);
                    let last = row.row.last;
                    self.paint_debug_line(layout, row, top, window);
                    if let Some(guides) = &layout.guides {
                        let line_left = layout.text_bounds.left() - self.scroll.x;
                        for column in guides.columns(row.row.line) {
                            let x = (line_left + layout.em_width * column as f32).floor();
                            if x < layout.text_bounds.left() {
                                continue;
                            }
                            let color = if guides.is_active(row.row.line, column) {
                                theme::indent_guide_active()
                            } else {
                                theme::indent_guide()
                            };
                            window.paint_quad(fill(
                                Bounds::new(point(x, top), size(px(1.), line_height)),
                                color,
                            ));
                        }
                    }
                    // Backgrounds over the part of `range` on this row.
                    let mut highlight = |range: &Range<usize>, color: Hsla| {
                        if range.start > end || range.end < start || range.is_empty() {
                            return;
                        }
                        // A row that continues below ends where the next starts.
                        if !last && range.start == end {
                            return;
                        }
                        let x0 = row.x_for(range.start.max(start));
                        let mut x1 = row.x_for(range.end.min(end));
                        if range.end > end && last {
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
                    let on_row = || selections[first..].iter().take_while(|s| s.start() <= end);
                    for s in on_row() {
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
                    if let Some((index, inline)) = &layout.inline_blame
                        && *index == i
                    {
                        let x = text_left + shaped.width + layout.em_width * 3.;
                        inline.paint(point(x, top), line_height, window, cx).ok();
                    }

                    self.paint_spelling(layout, row, text_left, top, window);

                    // The Ctrl+hover link to a definition.
                    if let Some(link) = &self.link.shown
                        && link.start < end
                        && start < link.end
                    {
                        let x0 = text_left + row.x_for(link.start.max(start));
                        let x1 = text_left + row.x_for(link.end.min(end));
                        let y = top + line_height - px(3.);
                        window.paint_quad(fill(
                            Bounds::from_corners(point(x0, y), point(x1, y + px(1.))),
                            theme::accent_text(),
                        ));
                    }

                    if let Some(fold_start) = row.folded {
                        let placeholder = layout.placeholder_bounds(i, self.scroll);
                        let fold_end = self
                            .display
                            .folds()
                            .iter()
                            .find(|fold| fold.start == fold_start)
                            .map_or(fold_start, |fold| fold.end);
                        let selected = selections
                            .iter()
                            .any(|s| s.start() <= fold_start && fold_end <= s.end());
                        let background = if selected {
                            theme::selection()
                        } else {
                            theme::fold_placeholder()
                        };
                        window.paint_quad(fill(placeholder, background).corner_radii(px(3.)));
                        let origin = point(placeholder.left() + layout.em_width / 2., top);
                        layout
                            .placeholder
                            .paint(origin, line_height, window, cx)
                            .ok();
                    }

                    if focused {
                        for s in on_row() {
                            let head = self.cursor_head(s);
                            if !row.row.shows(head, s.at_row_end) {
                                continue;
                            }
                            let (cursor, color) =
                                self.cursor_quad(head, row, text_left, top, layout);
                            window.paint_quad(fill(cursor, color));
                        }
                    }
                }
            },
        );
        self.paint_sticky(layout, window, cx);
        self.paint_minimap(layout, window);
        self.paint_scrollbar(layout, window);
    }
}

fn lsp_position(point: text::Point) -> lsp::types::Position {
    lsp::types::Position::new(point.row as u32, point.column as u32)
}

/// Diagnostics touching `range` (an empty one at its start counts), in
/// `out`.
fn diagnostics_in<'a>(
    diagnostics: &'a [EditorDiagnostic],
    range: &Range<usize>,
    out: &mut Vec<&'a EditorDiagnostic>,
) {
    out.clear();
    let candidates =
        diagnostics.partition_point(|d| d.range.start < range.end.max(range.start + 1));
    out.extend(
        diagnostics[..candidates]
            .iter()
            .filter(|d| d.range.end > range.start || d.range.start == range.start),
    );
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
        if !self.single_line {
            self.signature_help_typed(text, cx);
        }
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
        let index = layout
            .rows
            .iter()
            .position(|row| row.row.shows(start, false))?;
        let row = &layout.rows[index];
        let end = end.min(row.row.range.end);
        let left = layout.text_left(row, self.scroll.x);
        let top = layout.row_top(index, self.scroll.y);
        let x = |offset: usize| left + row.x_for(offset);
        Some(Bounds::from_corners(
            point(x(start), top),
            point(x(end), top + layout.line_height),
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
    /// Pinpoint path of the enclosing pins, for the painted parts' nodes.
    pin_prefix: SharedString,
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    /// Areas with their own mouse cursor: scrollbar, minimap, sticky lines.
    type PrepaintState = Vec<(Hitbox, CursorStyle)>;

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
        self.pin_prefix = crate::pinpoint::current_path().unwrap_or_default();
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        if !self.editor.read(cx).notebook_cell {
            style.size.height = relative(1.).into();
            return (window.request_layout(style, [], cx), ());
        }
        // A notebook cell: its rows (wrapped at the width it gets) tall.
        let text_style = window.text_style();
        let font_size = text_style.font_size.to_pixels(window.rem_size());
        let run = TextRun {
            len: 1,
            font: text_style.font(),
            color: text_style.color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let em_width = window
            .text_system()
            .shape_line("m".into(), font_size, &[run], None)
            .width;
        let line_height = window.line_height();
        let editor = self.editor.clone();
        let id = window.request_measured_layout(style, move |known, available, _, cx| {
            let width = known.width.or(match available.width {
                AvailableSpace::Definite(width) => Some(width),
                _ => None,
            });
            let rows = editor.update(cx, |editor, _| editor.content_rows(width, em_width));
            size(width.unwrap_or_default(), line_height * rows as f32)
        });
        (id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<(Hitbox, CursorStyle)> {
        self.editor.update(cx, |editor, cx| {
            let before = editor.note_geometry();
            editor.layout(bounds, window);
            // Cards are placed while rendering, from the last layout; a
            // notify while drawing is dropped, so ask after this frame.
            if editor.note_geometry() != before {
                let id = cx.entity_id();
                window.on_next_frame(move |_, cx| cx.notify(id));
            }
            editor.record_pins(&self.pin_prefix, window);
            editor.record_text_pins(&self.pin_prefix, window);
            let Some(layout) = editor.layout.as_ref() else {
                return Vec::new();
            };
            let areas = [
                layout
                    .scrollbar
                    .as_ref()
                    .map(|s| (s.track, CursorStyle::Arrow)),
                layout
                    .minimap
                    .as_ref()
                    .map(|m| (m.bounds, CursorStyle::Arrow)),
                layout.sticky.as_ref().map(|s| {
                    (
                        s.bounds(layout.text_bounds.top()),
                        CursorStyle::PointingHand,
                    )
                }),
            ];
            areas
                .into_iter()
                .flatten()
                .map(|(area, style)| (window.insert_hitbox(area, HitboxBehavior::Normal), style))
                .collect()
        })
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        hitboxes: &mut Vec<(Hitbox, CursorStyle)>,
        window: &mut Window,
        cx: &mut App,
    ) {
        for (hitbox, style) in hitboxes.iter() {
            window.set_cursor_style(*style, hitbox);
        }
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
/// Alt+Z's choice for all editors of the app.
struct WordWrapOverride(bool);

impl Global for WordWrapOverride {}

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // `settings.json` may have toggled semantic highlighting.
        self.sync_semantic_setting(cx);
        // Vim mode toggled, or the mouse/IDE changed the selection.
        self.sync_vim(cx);
        if !self.single_line {
            // The inlay-hint setting or toggle, and hints for scrolled-to
            // lines.
            self.sync_inlay_hints(cx);
            // Edits and cursor moves (by any path) close the code-action
            // menu and ask again for the lightbulb, and end a snippet left
            // behind.
            self.follow_cursor_for_code_actions(cx);
            self.snippet_selections_changed();
            self.follow_spelling(cx);
        }
        // Alt+Z in another editor (notebook cells wrap by cell kind).
        if let Some(&WordWrapOverride(wrap)) = cx.try_global::<WordWrapOverride>()
            && self.word_wrap != Some(wrap)
            && !self.notebook_cell
        {
            self.word_wrap = Some(wrap);
        }
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
        let (message, message_color) = match self.vim_command_line() {
            Some(line) => (line, theme::text()),
            None => (message, message_color),
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
                if let Some(popover) = &self.signature.popover {
                    context.add("showing_signature_help");
                    if popover.has_several() {
                        context.add("multiple_signatures");
                    }
                }
                if self.selections.len() > 1 {
                    context.add("multiple_selections");
                }
                if self.code_actions.menu.is_some() {
                    context.add("showing_code_actions");
                }
                if self.in_snippet() {
                    context.add("in_snippet");
                }
                if self.hunk_popup_open() {
                    context.add("showing_hunk_popup");
                }
                if self.notebook_cell {
                    context.add("notebook_cell");
                    let closable = self.completion.is_some()
                        || self.signature.popover.is_some()
                        || self.selections.len() > 1
                        || self.code_actions.menu.is_some()
                        || self.in_snippet();
                    if !closable {
                        context.add("cell_escape");
                    }
                }
                context
            })
            .track_focus(&self.focus_handle)
            // A notebook cell is as tall as its text.
            .map(|editor| {
                if self.notebook_cell {
                    editor.w_full()
                } else {
                    editor.size_full()
                }
            })
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
                    .on_action(cx.listener(Self::go_to_symbol))
                    .on_action(cx.listener(Self::toggle_word_wrap))
                    .on_action(cx.listener(Self::toggle_vim_mode))
                    .on_action(cx.listener(Self::fold))
                    .on_action(cx.listener(Self::unfold))
                    .on_action(cx.listener(Self::toggle_fold))
                    .on_action(cx.listener(Self::fold_all))
                    .on_action(cx.listener(Self::unfold_all))
                    .on_action(cx.listener(Self::fold_recursively))
                    .on_action(cx.listener(Self::unfold_recursively))
            })
            .when(!self.single_line && !self.text_area, |editor| {
                editor
                    .on_action(cx.listener(Self::next_change))
                    .on_action(cx.listener(Self::previous_change))
                    .on_action(cx.listener(Self::rollback_lines))
                    .on_action(cx.listener(Self::close_hunk_popup))
                    .on_action(cx.listener(Self::toggle_blame))
            })
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::copy_reference))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            // A notebook cell has no file: its notebook saves.
            .when(!self.notebook_cell, |editor| {
                editor.on_action(cx.listener(Self::save_action))
            })
            .on_action(cx.listener(Self::next_diagnostic))
            .on_action(cx.listener(Self::prev_diagnostic))
            .on_action(cx.listener(Self::go_to_definition))
            .on_action(cx.listener(Self::go_to_declaration))
            .on_action(cx.listener(Self::go_to_type_definition))
            .on_action(cx.listener(Self::go_to_implementation))
            .on_action(cx.listener(Self::find_references))
            .on_action(cx.listener(Self::show_call_hierarchy))
            .on_action(cx.listener(Self::show_signature_help))
            .on_action(cx.listener(Self::close_signature_help))
            .on_action(cx.listener(Self::next_signature))
            .on_action(cx.listener(Self::previous_signature))
            .on_action(cx.listener(Self::toggle_inlay_hints))
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .on_action(cx.listener(Self::show_completions))
            .on_action(cx.listener(Self::select_prev_completion))
            .on_action(cx.listener(Self::select_next_completion))
            .on_action(cx.listener(Self::confirm_completion))
            .on_action(cx.listener(Self::close_completions))
            .when(!self.single_line, |editor| {
                editor
                    .on_action(cx.listener(Self::rename_symbol))
                    .on_action(cx.listener(Self::quick_fix))
                    .on_action(cx.listener(Self::select_prev_code_action))
                    .on_action(cx.listener(Self::select_next_code_action))
                    .on_action(cx.listener(Self::confirm_code_action))
                    .on_action(cx.listener(Self::close_code_actions))
                    .on_action(cx.listener(Self::format_document))
                    .on_action(cx.listener(Self::format_selection))
                    .on_action(cx.listener(Self::next_tab_stop))
                    .on_action(cx.listener(Self::previous_tab_stop))
                    .on_action(cx.listener(Self::end_snippet))
                    // Only while sticky notes are on (a store was given).
                    .when(self.notes_enabled(), |editor| {
                        editor
                            .on_action(cx.listener(Self::add_note))
                            .on_action(cx.listener(Self::submit_note))
                            .on_action(cx.listener(Self::cancel_note))
                            .on_action(cx.listener(Self::toggle_resolved_notes))
                    })
            })
            .child(
                div()
                    .id("editor-text")
                    .when(!self.notebook_cell, |text| text.flex_1())
                    .relative()
                    .overflow_hidden()
                    .cursor(if self.link.shown.is_some() {
                        CursorStyle::PointingHand
                    } else {
                        CursorStyle::IBeam
                    })
                    .font_family(theme::MONO_FONT)
                    .text_size(px(theme::EDITOR_FONT_SIZE))
                    .line_height(px(theme::EDITOR_LINE_HEIGHT))
                    .text_color(theme::text())
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_middle_mouse_down))
                    .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_mouse_down))
                    .on_mouse_down(MouseButton::Right, cx.listener(Self::on_text_right_mouse_down))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::on_mouse_up))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
                    .on_hover(cx.listener(Self::clear_hover))
                    .child(EditorElement {
                        editor: cx.entity(),
                        pin_prefix: SharedString::default(),
                    })
                    .when(focused, |text| {
                        text.children(self.render_hover().map(|p| self.popover(p)))
                    })
                    .when(focused, |text| {
                        text.children(self.render_signature_help(cx).map(|p| self.popover(p)))
                    })
                    .children(self.render_completions(cx).map(|p| self.popover(p)))
                    .children(self.render_code_actions(cx).map(|p| self.popover(p)))
                    .when(focused, |text| text.children(self.render_blame_hover()))
                    .children(self.render_notes(cx)),
            )
            .when(!self.single_line && !self.text_area, |editor| {
                editor.child(
                    div()
                        .flex()
                        .min_w_0()
                        .overflow_hidden()
                        .justify_between()
                        .gap_4()
                        .px_3()
                        .py_1()
                        .border_t_1()
                        .border_color(theme::border())
                        .bg(theme::panel())
                        .text_xs()
                        .text_color(theme::muted())
                        .whitespace_nowrap()
                        // Vim's mode, pending keys and recording.
                        .children(self.render_vim_status(cx))
                        // In a narrow pane the path and the language-server
                        // status share the shrinking (both start at their
                        // natural width) before the position gives way.
                        .child(
                            div()
                                .flex_auto()
                                .min_w_0()
                                .truncate()
                                .text_color(message_color)
                                .child(selectable("status-message", message))
                                .pin("message"),
                        )
                        .child(
                            div()
                                .flex()
                                .gap_4()
                                .min_w_0()
                                .children(self.large_file.then(|| {
                                    div()
                                        .flex_none()
                                        .text_color(theme::warning())
                                        .child(selectable(
                                            "large-file",
                                            "Large file: reduced features",
                                        ))
                                        .pin("large-file")
                                        .label("Large file: no syntax, language server or spell check")
                                }))
                                .children(
                                    self.lsp_status.clone().map(|(status, color)| {
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_color(color())
                                            .child(selectable("server-status", status))
                                            .pin("server-status")
                                            .label("Language server")
                                    }),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .child(selectable(
                                            "position",
                                            format!(
                                                "{selection_count}Ln {line}, Col {column}   {indent}   {line_ending}   UTF-8   {language}"
                                            ),
                                        ))
                                        .pin("position"),
                                )
                                .when(errors + warnings > 0, |row| {
                                    row.child(
                                        div()
                                            .flex()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .text_color(theme::error())
                                                    .child(selectable("errors", format!("✕ {errors}")))
                                                    .pin("errors")
                                                    .label(format!("{errors} errors")),
                                            )
                                            .child(
                                                div()
                                                    .text_color(theme::warning())
                                                    .child(selectable(
                                                        "warnings",
                                                        format!("⚠ {warnings}"),
                                                    ))
                                                    .pin("warnings")
                                                    .label(format!("{warnings} warnings")),
                                            ),
                                    )
                                }),
                        )
                        .pin("status"),
                )
            });
        if self.notebook_cell && self.rename.is_some() {
            // A cell's rename input draws over the neighbouring cells.
            return div()
                .w_full()
                .on_action(cx.listener(Self::confirm_rename))
                .on_action(cx.listener(Self::cancel_rename))
                .child(editor)
                .children(self.render_rename().map(|rename| self.popover(rename)))
                .into_any_element();
        }
        if self.single_line || self.text_area {
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
            .on_action(cx.listener(Self::confirm_rename))
            .on_action(cx.listener(Self::cancel_rename))
            .on_action(cx.listener(Self::confirm_breakpoint_edit))
            .on_action(cx.listener(Self::cancel_breakpoint_edit))
            .on_action(cx.listener(Self::next_breakpoint_field))
            .children(self.render_breadcrumbs(cx))
            .children(self.render_find_bar(cx))
            .child(div().flex_1().min_h_0().child(editor))
            .children(self.render_go_to_line(cx))
            .children(self.render_rename())
            .children(self.render_breakpoint_editor(cx))
            .children(self.render_hunk_popup(cx))
            .children(self.render_gutter_menu(cx))
            .children(self.render_text_menu(cx))
            .into_any_element()
    }
}
