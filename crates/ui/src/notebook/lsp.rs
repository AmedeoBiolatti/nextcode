//! Language servers in notebook code cells, as VS Code does for Python
//! notebooks: the code cells, in order, form one virtual Python document
//! (`<notebook>.nc-notebook.py` next to the notebook, never written) that
//! the project's Python server sees through the usual `didOpen`/`didChange`.
//!
//! Each cell keeps its lines, so a position maps by its line alone: cell
//! line `l` is virtual line `start + l`, columns unchanged. IPython syntax
//! (`%magic`, `!command`, `x = !command`, `name?`) is replaced on its line
//! by `...`/`0` and spaces; a `%%magic` cell whose body is not Python
//! (`%%bash`, `%%html`…) is blank; Jupyter notebooks start with a short
//! prelude declaring `display`, `get_ipython`, `In` and `Out`. Edits
//! change only the lines that differ (one incremental change per edit or
//! structural change; the whole text for full-sync servers).
//!
//! Cell editors talk to the server through a [`CellLink`] (positions
//! shifted by the cell's start line). Diagnostics are filtered here
//! (outside cells, on IPython lines, the displayed last expression's
//! "unused expression", redeclarations across cells, top-level `await`)
//! and split per cell; semantic tokens are requested once per change for
//! the whole document and handed to each cell's editor. Workspace edits on
//! the virtual document (rename, code actions) edit the cells' sources.

use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
    rc::{Rc, Weak},
    sync::Arc,
    time::Duration,
};

use gpui::{App, Context, Entity, EventEmitter, Subscription, Task, WeakEntity};
use lsp::types::{
    Diagnostic, DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    NumberOrString, Position, SemanticToken, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentSyncKind, TextEdit, Uri,
    VersionedTextDocumentIdentifier, notification, request::SemanticTokensFullRequest,
};
use notebook::{CellKind, MarimoCellKind};
use serde_json::Value;
use syntax::Language;
use text::Encoding;

use super::document::{CellId, NotebookDocument, NotebookEvent};
use crate::{
    editor::{format, semantic},
    lsp_store::{LspStore, LspStoreEvent, RunningServer},
    settings::Settings,
    workspace_edit,
};

/// Appended to the notebook's file name for its virtual document.
pub const VIRTUAL_SUFFIX: &str = ".nc-notebook.py";

/// Pause after a change before asking for the notebook's semantic tokens.
const TOKENS_DEBOUNCE: Duration = Duration::from_millis(250);

/// `%%` magics whose body is Python.
const PYTHON_CELL_MAGICS: &[&str] = &["time", "timeit", "capture", "prun", "debug"];

/// Names IPython defines in a Jupyter kernel's namespace.
const IPYTHON_PRELUDE: &str = "\
from typing import Any as _NcAny
def display(*objs: object, **kwargs: object) -> None: ...
def get_ipython() -> _NcAny: ...
In: list[str] = []
Out: dict[int, _NcAny] = {}
";

/// The virtual document of the notebook at `notebook`.
pub fn virtual_path(notebook: &Path) -> PathBuf {
    let mut name = notebook.file_name().unwrap_or_default().to_os_string();
    name.push(VIRTUAL_SUFFIX);
    notebook.with_file_name(name)
}

/// A code cell's text as the server sees it (same lines), and the lines
/// whose IPython syntax was replaced. `ipython`: Jupyter syntax applies.
pub fn python_text(source: &str, ipython: bool) -> (String, Vec<u32>) {
    let mut masked = Vec::new();
    if !ipython {
        return (source.to_owned(), masked);
    }
    let mut out = String::with_capacity(source.len());
    // `%%name` on the first line: a cell magic; its body is Python for a few.
    let cell_magic = source.strip_prefix("%%").map(|rest| {
        let name = rest.split_whitespace().next().unwrap_or_default();
        PYTHON_CELL_MAGICS.contains(&name)
    });
    for (index, line) in source.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let replaced = match cell_magic {
            Some(python_body) if index == 0 || !python_body => {
                Some(match line.strip_suffix('\r') {
                    Some(_) => "\r".to_owned(),
                    None => String::new(),
                })
            }
            _ => mask_line(line),
        };
        match replaced {
            Some(text) => {
                masked.push(index as u32);
                out.push_str(&text);
            }
            None => out.push_str(line),
        }
    }
    (out, masked)
}

/// `line` with IPython syntax replaced by a Python placeholder (keeping
/// what precedes it), or `None` when it is Python.
fn mask_line(line: &str) -> Option<String> {
    let (body, cr) = match line.strip_suffix('\r') {
        Some(body) => (body, "\r"),
        None => (line, ""),
    };
    let trimmed = body.trim_start();
    let indent = &body[..body.len() - trimmed.len()];
    let is_escape =
        |text: &str| text.starts_with('%') || (text.starts_with('!') && !text.starts_with("!="));
    if is_escape(trimmed) || trimmed.starts_with('?') || is_help(trimmed) {
        return Some(format!(
            "{indent}{}{cr}",
            placeholder(trimmed.chars().count())
        ));
    }
    // `files = !ls`, `t = %timeit -o f()`.
    let eq = assignment(trimmed)?;
    let (target, value) = trimmed.split_at(eq + 1);
    let expression = value.trim_start();
    if !is_escape(expression) {
        return None;
    }
    let gap = &value[..value.len() - expression.len()];
    Some(format!(
        "{indent}{target}{gap}{}{cr}",
        placeholder(expression.chars().count())
    ))
}

/// `name?`, `obj.attr??`: IPython help.
fn is_help(text: &str) -> bool {
    let text = text.trim_end();
    let name = text.trim_end_matches('?');
    let marks = text.len() - name.len();
    (1..=2).contains(&marks)
        && !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
}

/// The byte index of an assignment's `=` when `text` assigns to names.
fn assignment(text: &str) -> Option<usize> {
    let eq = text.find('=')?;
    let after = text[eq + 1..].chars().next();
    let target = &text[..eq];
    (after != Some('=')
        && !target.trim().is_empty()
        && target
            .chars()
            .all(|c| c.is_alphanumeric() || "_ ,.[]()\t*".contains(c)))
    .then_some(eq)
}

/// A Python expression `width` characters wide (`...` and spaces).
fn placeholder(width: usize) -> String {
    let (text, used) = if width >= 3 { ("...", 3) } else { ("0", 1) };
    let mut out = String::with_capacity(width.max(used));
    out.push_str(text);
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    out
}

/// Lines of `text` as an editor counts them (`a\n` has two).
fn line_count(text: &str) -> u32 {
    text.split('\n').count() as u32
}

/// One code cell in the virtual document.
#[derive(Debug, Clone, PartialEq)]
struct Part {
    id: CellId,
    /// 1-based position among all the notebook's cells.
    number: usize,
    /// The cell's text as the server sees it.
    text: String,
    /// Cell lines whose IPython syntax was replaced.
    masked: Vec<u32>,
    lines: u32,
    /// Virtual line of the cell's first line.
    start: u32,
}

/// Where a virtual position is in the notebook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellPosition {
    pub cell: CellId,
    /// 1-based position among all cells (as shown: "cell 3").
    pub number: usize,
    /// In the cell's text (the server's encoding).
    pub position: Position,
}

/// The notebook's virtual document and the server it is open on.
pub struct VirtualDocument {
    notebook_path: PathBuf,
    path: PathBuf,
    uri: Option<Uri>,
    ipython: bool,
    prelude: &'static str,
    prelude_lines: u32,
    parts: Vec<Part>,
    version: i32,
    server: Option<Arc<RunningServer>>,
    document: WeakEntity<NotebookDocument>,
}

impl VirtualDocument {
    pub fn new(document: &Entity<NotebookDocument>, cx: &App) -> Self {
        let notebook = document.read(cx);
        let path = virtual_path(notebook.path());
        let ipython = !notebook.is_marimo();
        let prelude = if ipython { IPYTHON_PRELUDE } else { "" };
        let mut this = Self {
            notebook_path: notebook.path().to_path_buf(),
            uri: lsp::path_to_uri(&path),
            path,
            ipython,
            prelude,
            prelude_lines: line_count(prelude) - 1,
            parts: Vec::new(),
            version: 0,
            server: None,
            document: document.downgrade(),
        };
        this.parts = this.parts_of(notebook);
        this.layout(0);
        this
    }

    /// The virtual document's path (never on disk).
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn notebook_path(&self) -> &Path {
        &self.notebook_path
    }

    pub fn version(&self) -> i32 {
        self.version
    }

    /// Virtual `line`'s text when it is in a cell (as the server sees it).
    pub fn line_text(&self, line: u32) -> Option<&str> {
        let (index, local) = self.locate(line)?;
        self.parts[index].text.split('\n').nth(local as usize)
    }

    /// The whole text the server sees.
    pub fn text(&self) -> String {
        let size = self.prelude.len() + self.parts.iter().map(|p| p.text.len() + 1).sum::<usize>();
        let mut text = String::with_capacity(size);
        text.push_str(self.prelude);
        for part in &self.parts {
            text.push_str(&part.text);
            text.push('\n');
        }
        text
    }

    /// The code cells of `notebook` (marimo cells that do not parse count
    /// as blank lines).
    fn parts_of(&self, notebook: &NotebookDocument) -> Vec<Part> {
        notebook
            .cells()
            .iter()
            .enumerate()
            .filter(|(_, cell)| cell.cell.kind == CellKind::Code)
            .map(|(index, cell)| {
                let unparsable = cell
                    .cell
                    .marimo
                    .as_ref()
                    .is_some_and(|marimo| marimo.kind == MarimoCellKind::Unparsable);
                let (text, masked) = if unparsable {
                    let lines = line_count(&cell.cell.source);
                    ("\n".repeat(lines as usize - 1), (0..lines).collect())
                } else {
                    python_text(&cell.cell.source, self.ipython)
                };
                Part {
                    id: cell.id,
                    number: index + 1,
                    lines: line_count(&text),
                    text,
                    masked,
                    start: 0,
                }
            })
            .collect()
    }

    /// Recomputes start lines from part `from` on.
    fn layout(&mut self, from: usize) {
        let mut start = match from.checked_sub(1) {
            Some(previous) => self.parts[previous].start + self.parts[previous].lines,
            None => self.prelude_lines,
        };
        for part in &mut self.parts[from..] {
            part.start = start;
            start += part.lines;
        }
    }

    fn index_of(&self, id: CellId) -> Option<usize> {
        self.parts.iter().position(|part| part.id == id)
    }

    /// Virtual line of cell `id`'s first line (`None`: not a code cell).
    pub fn start_of(&self, id: CellId) -> Option<u32> {
        Some(self.parts[self.index_of(id)?].start)
    }

    /// The part holding virtual `line`, and the line in it.
    fn locate(&self, line: u32) -> Option<(usize, u32)> {
        let index = self.parts.partition_point(|part| part.start <= line);
        let part = self.parts.get(index.checked_sub(1)?)?;
        (line < part.start + part.lines).then(|| (index - 1, line - part.start))
    }

    /// The cell holding virtual `position`.
    pub fn cell_position(&self, position: Position) -> Option<CellPosition> {
        let (index, line) = self.locate(position.line)?;
        let part = &self.parts[index];
        Some(CellPosition {
            cell: part.id,
            number: part.number,
            position: Position::new(line, position.character),
        })
    }

    /// Opens the document on `server` (closing it on the previous one).
    pub fn open(&mut self, server: Arc<RunningServer>) {
        self.close();
        let Some(uri) = self.uri.clone() else {
            return;
        };
        self.version += 1;
        server
            .connection
            .notify::<notification::DidOpenTextDocument>(DidOpenTextDocumentParams {
                text_document: TextDocumentItem::new(
                    uri,
                    Language::Python.id().into(),
                    self.version,
                    self.text(),
                ),
            });
        self.server = Some(server);
    }

    /// Closes the document on its server (if it still runs).
    pub fn close(&mut self) {
        if let (Some(server), Some(uri)) = (self.server.take(), self.uri.clone())
            && !server.connection.has_exited()
        {
            server
                .connection
                .notify::<notification::DidCloseTextDocument>(DidCloseTextDocumentParams {
                    text_document: TextDocumentIdentifier::new(uri),
                });
        }
    }

    /// Forgets a server that exited.
    pub fn server_exited(&mut self) {
        if self
            .server
            .as_ref()
            .is_some_and(|server| server.connection.has_exited())
        {
            self.server = None;
        }
    }

    /// Replaces virtual lines `lines` with `text` on the server.
    fn send_change(&mut self, lines: (u32, u32), text: String) {
        self.version += 1;
        let (Some(server), Some(uri)) = (&self.server, &self.uri) else {
            return;
        };
        let change = if server.sync == TextDocumentSyncKind::INCREMENTAL {
            TextDocumentContentChangeEvent {
                range: Some(lsp::types::Range::new(
                    Position::new(lines.0, 0),
                    Position::new(lines.1, 0),
                )),
                range_length: None,
                text,
            }
        } else if server.sync == TextDocumentSyncKind::FULL {
            TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: self.text(),
            }
        } else {
            return;
        };
        server
            .connection
            .notify::<notification::DidChangeTextDocument>(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier::new(uri.clone(), self.version),
                content_changes: vec![change],
            });
    }

    /// Cell `id`'s text is now `source`: sends the lines that changed.
    /// False when nothing changed (or `id` is not a code cell).
    pub fn set_cell(&mut self, id: CellId, source: &str) -> bool {
        let Some(index) = self.index_of(id) else {
            return false;
        };
        let (text, masked) = python_text(source, self.ipython);
        let part = &self.parts[index];
        if part.text == text {
            return false;
        }
        let old: Vec<&str> = part.text.split('\n').collect();
        let new: Vec<&str> = text.split('\n').collect();
        let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
        let room = old.len().min(new.len()) - prefix;
        let suffix = old
            .iter()
            .rev()
            .zip(new.iter().rev())
            .take(room)
            .take_while(|(a, b)| a == b)
            .count();
        let mut replacement = String::new();
        for line in &new[prefix..new.len() - suffix] {
            replacement.push_str(line);
            replacement.push('\n');
        }
        let start = part.start;
        let lines = (start + prefix as u32, start + (old.len() - suffix) as u32);
        let part = &mut self.parts[index];
        part.lines = new.len() as u32;
        part.text = text;
        part.masked = masked;
        self.layout(index + 1);
        self.send_change(lines, replacement);
        true
    }

    /// Follows cells inserted, deleted, moved or changed in kind (and any
    /// source changes): one change from the first differing cell to the
    /// last.
    pub fn sync_structure(&mut self, notebook: &NotebookDocument) {
        let mut parts = self.parts_of(notebook);
        let same = |a: &Part, b: &Part| a.id == b.id && a.text == b.text;
        let prefix = self
            .parts
            .iter()
            .zip(&parts)
            .take_while(|(a, b)| same(a, b))
            .count();
        let room = self.parts.len().min(parts.len()) - prefix;
        let suffix = self
            .parts
            .iter()
            .rev()
            .zip(parts.iter().rev())
            .take(room)
            .take_while(|(a, b)| same(a, b))
            .count();
        let changed = prefix < self.parts.len() - suffix || prefix < parts.len() - suffix;
        let first = match self.parts.get(prefix) {
            Some(part) => part.start,
            None => self
                .parts
                .last()
                .map_or(self.prelude_lines, |p| p.start + p.lines),
        };
        let last = self.parts[..self.parts.len() - suffix]
            .last()
            .map_or(self.prelude_lines, |p| p.start + p.lines)
            .max(first);
        let mut replacement = String::new();
        for part in &parts[prefix..parts.len() - suffix] {
            replacement.push_str(&part.text);
            replacement.push('\n');
        }
        // Numbers may change without the text (a Markdown cell inserted).
        std::mem::swap(&mut self.parts, &mut parts);
        self.layout(0);
        if changed {
            self.send_change((first, last), replacement);
        }
    }

    /// The server's diagnostics that belong to cells and are not
    /// artefacts of the concatenation.
    pub fn filter_diagnostics(&self, diagnostics: Vec<Diagnostic>) -> Vec<Diagnostic> {
        diagnostics
            .into_iter()
            .filter(|diagnostic| self.keeps(diagnostic))
            .collect()
    }

    fn keeps(&self, diagnostic: &Diagnostic) -> bool {
        let Some((index, line)) = self.locate(diagnostic.range.start.line) else {
            return false;
        };
        let part = &self.parts[index];
        if part.masked.contains(&line) {
            return false;
        }
        let code = match &diagnostic.code {
            Some(NumberOrString::String(code)) => Some(code.as_str()),
            _ => None,
        };
        match code {
            // Jupyter displays a cell's last expression.
            Some("reportUnusedExpression") => {
                let end = diagnostic.range.end.line.saturating_sub(part.start);
                end < last_statement_line(&part.text)
            }
            // Defining a name again in a later cell is how notebooks work.
            Some("reportRedeclaration") => {
                diagnostic
                    .related_information
                    .iter()
                    .flatten()
                    .all(|related| {
                        self.locate(related.location.range.start.line)
                            .is_some_and(|(other, _)| other == index)
                    })
            }
            // Notebooks await at the top level.
            None => {
                let top_level = part
                    .text
                    .split('\n')
                    .nth(line as usize)
                    .is_some_and(|text| !text.starts_with([' ', '\t']));
                !(top_level
                    && (diagnostic
                        .message
                        .contains("\"await\" allowed only within async")
                        || diagnostic
                            .message
                            .contains("not allowed outside of async function")))
            }
            Some(_) => true,
        }
    }

    /// Diagnostics (virtual positions) grouped by cell.
    pub fn split_diagnostics(
        &self,
        diagnostics: &[Diagnostic],
    ) -> HashMap<CellId, Vec<Diagnostic>> {
        let mut by_cell: HashMap<CellId, Vec<Diagnostic>> = HashMap::new();
        for diagnostic in diagnostics {
            if let Some((index, _)) = self.locate(diagnostic.range.start.line) {
                by_cell
                    .entry(self.parts[index].id)
                    .or_default()
                    .push(diagnostic.clone());
            }
        }
        by_cell
    }

    /// Applies a server's edits of the virtual document to the cells'
    /// sources (each cell once, so one undo step per cell).
    pub fn apply_edits(
        &self,
        edits: &[TextEdit],
        encoding: Encoding,
        cx: &mut App,
    ) -> Result<(), String> {
        let document = self.document.upgrade().ok_or("the notebook was closed")?;
        let mut by_cell: Vec<(usize, Vec<TextEdit>)> = Vec::new();
        for edit in edits {
            let (index, line) = self
                .locate(edit.range.start.line)
                .ok_or("the edit is outside the notebook's cells")?;
            let part = &self.parts[index];
            // A range may end at the line after the cell (its end).
            let end = edit.range.end.line;
            if end < part.start || end > part.start + part.lines {
                return Err("the edit spans several cells".into());
            }
            let end = if end == part.start + part.lines {
                let last = part.text.split('\n').next_back().unwrap_or_default();
                let width = match encoding {
                    Encoding::Utf8 => last.len(),
                    Encoding::Utf16 => last.encode_utf16().count(),
                };
                Position::new(part.lines - 1, width as u32)
            } else {
                Position::new(end - part.start, edit.range.end.character)
            };
            let local = TextEdit {
                range: lsp::types::Range::new(Position::new(line, edit.range.start.character), end),
                new_text: edit.new_text.clone(),
            };
            match by_cell.iter_mut().find(|(i, _)| *i == index) {
                Some((_, edits)) => edits.push(local),
                None => by_cell.push((index, vec![local])),
            }
        }
        let mut sources = Vec::with_capacity(by_cell.len());
        {
            let notebook = document.read(cx);
            for (index, edits) in &by_cell {
                let id = self.parts[*index].id;
                let source = &notebook.cell(id).ok_or("the cell was deleted")?.cell.source;
                sources.push((id, workspace_edit::edit_text(source, edits, encoding)?));
            }
        }
        document.update(cx, |document, cx| {
            for (id, source) in sources {
                document.set_source(id, source, None, cx);
            }
        });
        Ok(())
    }
}

impl Drop for VirtualDocument {
    fn drop(&mut self) {
        self.close();
    }
}

/// The local line of a cell's last statement: its last line that is not
/// blank or a comment.
fn last_statement_line(text: &str) -> u32 {
    text.split('\n')
        .enumerate()
        .filter(|(_, line)| {
            let line = line.trim();
            !line.is_empty() && !line.starts_with('#')
        })
        .map(|(index, _)| index as u32)
        .last()
        .unwrap_or(0)
}

/// A cell editor's view of the virtual document.
#[derive(Clone)]
pub struct CellLink {
    document: Rc<RefCell<VirtualDocument>>,
    cell: CellId,
}

impl CellLink {
    /// Virtual line of the cell's first line (`None`: no longer a code
    /// cell of the document).
    pub fn start(&self) -> Option<u32> {
        self.document.borrow().start_of(self.cell)
    }

    /// The cell's text is now `text` (an edit in its editor): sends the
    /// change at once, before requests about it.
    pub fn sync(&self, text: &str) {
        self.document.borrow_mut().set_cell(self.cell, text);
    }

    /// The virtual document's URI.
    pub fn uri(&self) -> Option<Uri> {
        self.document.borrow().uri.clone()
    }
}

/// Decoded semantic tokens of the whole document at `version`.
struct Tokens {
    version: i32,
    /// Absolute (line, start, length, type, modifiers), in order.
    rows: Vec<[u32; 5]>,
}

pub enum NotebookLspEvent {
    /// The document opened on a server or lost it: cell editors attach or
    /// detach.
    ServerChanged,
    /// New diagnostics for these cells.
    Diagnostics(Vec<CellId>),
    /// New semantic tokens per cell.
    Tokens,
}

/// One notebook's language-server document, shared by its views.
pub struct NotebookLsp {
    document: Rc<RefCell<VirtualDocument>>,
    store: WeakEntity<LspStore>,
    /// The notebook's language is Python (another kernel's notebook gets
    /// no server).
    python: bool,
    diagnostics: HashMap<CellId, Vec<Diagnostic>>,
    tokens: Option<Tokens>,
    tokens_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<NotebookLspEvent> for NotebookLsp {}

impl NotebookLsp {
    /// Opens `document`'s virtual document on the project's Python server
    /// (starting it if needed, or when it starts).
    pub fn new(
        document: Entity<NotebookDocument>,
        store: Entity<LspStore>,
        cx: &mut Context<Self>,
    ) -> Self {
        let virtual_document = Rc::new(RefCell::new(VirtualDocument::new(&document, cx)));
        store.update(cx, |store, _| store.register_notebook(&virtual_document));
        let subscriptions = vec![
            cx.subscribe(&document, |this, document, event, cx| {
                this.on_document_event(&document, event, cx)
            }),
            cx.subscribe(&store, |this, _, event, cx| this.on_store_event(event, cx)),
        ];
        let mut this = Self {
            document: virtual_document,
            store: store.downgrade(),
            python: document
                .read(cx)
                .language()
                .is_none_or(|language| language.eq_ignore_ascii_case("python")),
            diagnostics: HashMap::new(),
            tokens: None,
            tokens_task: None,
            _subscriptions: subscriptions,
        };
        this.connect(cx);
        this
    }

    /// The server the document is open on.
    pub fn server(&self) -> Option<Arc<RunningServer>> {
        self.document.borrow().server.clone()
    }

    #[cfg(test)]
    pub fn virtual_document(&self) -> &Rc<RefCell<VirtualDocument>> {
        &self.document
    }

    /// The project's settings (for the formatter).
    pub fn settings(&self, cx: &App) -> Arc<Settings> {
        self.store
            .upgrade()
            .map(|store| store.read(cx).settings())
            .unwrap_or_default()
    }

    /// Cell `id`'s link for its editor (`None`: not a code cell).
    pub fn link(&self, id: CellId) -> Option<CellLink> {
        self.document.borrow().start_of(id)?;
        Some(CellLink {
            document: self.document.clone(),
            cell: id,
        })
    }

    /// Cell `id`'s diagnostics, in virtual positions.
    pub fn diagnostics(&self, id: CellId) -> &[Diagnostic] {
        self.diagnostics.get(&id).map_or(&[], Vec::as_slice)
    }

    /// Cell `id`'s semantic tokens, relative-encoded from its first line;
    /// `None` until the server sent tokens for the current text.
    pub fn tokens(&self, id: CellId) -> Option<Vec<SemanticToken>> {
        let tokens = self.tokens.as_ref()?;
        let document = self.document.borrow();
        if tokens.version != document.version {
            return None;
        }
        let part = &document.parts[document.index_of(id)?];
        let first = tokens.rows.partition_point(|row| row[0] < part.start);
        let end = tokens
            .rows
            .partition_point(|row| row[0] < part.start + part.lines);
        let (mut line, mut start) = (part.start, 0);
        Some(
            tokens.rows[first..end]
                .iter()
                .map(|&[row, column, length, token_type, modifiers]| {
                    let delta_line = row - line;
                    let delta_start = if delta_line == 0 {
                        column - start
                    } else {
                        column
                    };
                    (line, start) = (row, column);
                    SemanticToken {
                        delta_line,
                        delta_start,
                        length,
                        token_type,
                        token_modifiers_bitset: modifiers,
                    }
                })
                .collect(),
        )
    }

    /// Opens the document on the Python server when it runs.
    fn connect(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.store.upgrade().filter(|_| self.python) else {
            return;
        };
        let path = self.document.borrow().path.clone();
        let server = store.update(cx, |store, cx| store.server(Language::Python, &path, cx));
        if let Some(server) = server {
            self.open_on(server, cx);
        }
    }

    /// Opens the document on `server` (no-op if already open there).
    pub(crate) fn open_on(&mut self, server: Arc<RunningServer>, cx: &mut Context<Self>) {
        let current = self.document.borrow().server.clone();
        if current.is_some_and(|current| Arc::ptr_eq(&current, &server)) {
            return;
        }
        self.document.borrow_mut().open(server);
        self.tokens = None;
        cx.emit(NotebookLspEvent::ServerChanged);
        self.request_tokens(Duration::ZERO, cx);
    }

    fn on_document_event(
        &mut self,
        document: &Entity<NotebookDocument>,
        event: &NotebookEvent,
        cx: &mut Context<Self>,
    ) {
        let changed = match event {
            NotebookEvent::Structure | NotebookEvent::Reloaded => {
                let version = self.document.borrow().version;
                self.document.borrow_mut().sync_structure(document.read(cx));
                version != self.document.borrow().version
            }
            NotebookEvent::Source { cell, .. } => {
                let notebook = document.read(cx);
                match notebook.cell(*cell) {
                    Some(cell) => {
                        // Usually already sent by the cell's editor.
                        self.document
                            .borrow_mut()
                            .set_cell(cell.id, &cell.cell.source);
                        true
                    }
                    None => false,
                }
            }
            _ => false,
        };
        if changed {
            self.request_tokens(TOKENS_DEBOUNCE, cx);
        }
    }

    fn on_store_event(&mut self, event: &LspStoreEvent, cx: &mut Context<Self>) {
        match event {
            LspStoreEvent::ServerStarted(Language::Python, _) => self.connect(cx),
            LspStoreEvent::ServerStopped(Language::Python) => {
                let had = self.document.borrow().server.is_some();
                self.document.borrow_mut().server_exited();
                if had && self.document.borrow().server.is_none() {
                    self.tokens = None;
                    self.tokens_task = None;
                    cx.emit(NotebookLspEvent::ServerChanged);
                }
            }
            LspStoreEvent::DiagnosticsUpdated(path) if *path == self.document.borrow().path => {
                let Some(store) = self.store.upgrade() else {
                    return;
                };
                let diagnostics = self
                    .document
                    .borrow()
                    .split_diagnostics(store.read(cx).diagnostics(path));
                // Only cells whose diagnostics changed are redrawn.
                let mut changed: Vec<CellId> = diagnostics
                    .iter()
                    .filter(|(id, new)| self.diagnostics.get(*id) != Some(*new))
                    .map(|(id, _)| *id)
                    .collect();
                changed.extend(
                    self.diagnostics
                        .keys()
                        .filter(|id| !diagnostics.contains_key(*id)),
                );
                self.diagnostics = diagnostics;
                if !changed.is_empty() {
                    cx.emit(NotebookLspEvent::Diagnostics(changed));
                }
            }
            LspStoreEvent::SemanticTokensRefresh(Language::Python) => {
                self.request_tokens(Duration::ZERO, cx)
            }
            _ => {}
        }
    }

    /// Asks for the whole document's semantic tokens after `delay`.
    fn request_tokens(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let document = self.document.borrow();
        let (Some(server), Some(uri)) = (document.server.clone(), document.uri.clone()) else {
            return;
        };
        if server.semantic_legend.is_none() || !crate::theme::semantic_highlighting() {
            return;
        }
        let version = document.version;
        drop(document);
        self.tokens_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let result = server
                .connection
                .request::<SemanticTokensFullRequest>(semantic::params(uri))
                .await;
            this.update(cx, |this, cx| {
                if this.document.borrow().version != version {
                    return;
                }
                let Ok(result) = result else {
                    return;
                };
                let (mut line, mut start) = (0, 0);
                let rows = semantic::response_data(result)
                    .iter()
                    .map(|token| {
                        if token.delta_line > 0 {
                            line += token.delta_line;
                            start = 0;
                        }
                        start += token.delta_start;
                        [
                            line,
                            start,
                            token.length,
                            token.token_type,
                            token.token_modifiers_bitset,
                        ]
                    })
                    .collect();
                this.tokens = Some(Tokens { version, rows });
                cx.emit(NotebookLspEvent::Tokens);
            })
            .ok();
        }));
    }
}

/// Formats code cells `ids` of `document` with the Python formatter (the
/// configured program, `ruff format` by default), one cell at a time:
/// cells with IPython syntax go to ruff as a one-cell notebook, which
/// keeps the magics. Returns how many cells changed, or why some could not
/// be formatted (the others are still applied).
pub fn format_cells(
    document: &Entity<NotebookDocument>,
    ids: &[CellId],
    settings: &Settings,
    server: Option<Arc<RunningServer>>,
    cx: &mut App,
) -> Task<Result<usize, String>> {
    let notebook = document.read(cx);
    let project = notebook.project().clone();
    let ipython = !notebook.is_marimo();
    let notebook_path = notebook.path().to_path_buf();
    let cells: Vec<(CellId, String)> = notebook
        .cells()
        .iter()
        .filter(|cell| cell.cell.kind == CellKind::Code && ids.contains(&cell.id))
        .map(|cell| (cell.id, cell.cell.source.clone()))
        .collect();
    let plan = format::plan(settings, Language::Python, server);
    let executor = cx.background_executor().clone();
    let document = document.downgrade();
    cx.spawn(async move |cx| {
        let mut errors = Vec::new();
        let mut formatted = Vec::new();
        for (id, source) in cells {
            if source.trim().is_empty() {
                continue;
            }
            let (command, args) = match &plan {
                Ok(format::Plan::External { command, args, .. }) => (command.clone(), args.clone()),
                Ok(_) => {
                    errors.push("the language server cannot format notebook cells".to_owned());
                    break;
                }
                Err(err) => {
                    errors.push(err.clone());
                    break;
                }
            };
            let has_ipython = ipython && !python_text(&source, true).1.is_empty();
            let ruff = Path::new(&command)
                .file_stem()
                .is_some_and(|stem| stem == "ruff");
            if has_ipython && !ruff {
                errors.push(format!("{command} cannot format cells with IPython magics"));
                continue;
            }
            let (path, text) = if has_ipython {
                (
                    notebook_path.with_file_name(".nc-cell.ipynb"),
                    one_cell_notebook(&source),
                )
            } else {
                (virtual_path(&notebook_path), source.clone())
            };
            let job = format::Job {
                path,
                uri: None,
                text,
                range: None,
                options: Default::default(),
            };
            let plan = format::Plan::External {
                command: command.clone(),
                args,
                fallback: None,
            };
            let result = format::run(plan, Some(project.clone()), job, executor.clone()).await;
            let result = result.and_then(|output| match has_ipython {
                true => cell_of_notebook(&output).ok_or_else(|| format!("{command}: no cell")),
                false => Ok(output),
            });
            match result {
                Ok(mut output) => {
                    // A cell has no final newline unless it had one.
                    if !source.ends_with('\n') && output.ends_with('\n') {
                        output.pop();
                    }
                    if output != source {
                        formatted.push((id, output));
                    }
                }
                Err(err) => errors.push(err),
            }
        }
        let changed = formatted.len();
        if let Some(document) = document.upgrade() {
            document
                .update(cx, |document, cx| {
                    for (id, source) in formatted {
                        document.set_source(id, source, None, cx);
                    }
                })
                .ok();
        }
        errors.dedup();
        match errors.is_empty() {
            true => Ok(changed),
            false => Err(errors.join("; ")),
        }
    })
}

/// `source` as the only cell of an nbformat 4 notebook.
fn one_cell_notebook(source: &str) -> String {
    serde_json::json!({
        "cells": [{
            "cell_type": "code",
            "execution_count": null,
            "id": "nc",
            "metadata": {},
            "outputs": [],
            "source": source,
        }],
        "metadata": { "language_info": { "name": "python" } },
        "nbformat": 4,
        "nbformat_minor": 5,
    })
    .to_string()
}

/// The first cell's source of a notebook's JSON.
fn cell_of_notebook(json: &str) -> Option<String> {
    let notebook: Value = serde_json::from_str(json).ok()?;
    match &notebook["cells"][0]["source"] {
        Value::String(source) => Some(source.clone()),
        Value::Array(lines) => Some(lines.iter().filter_map(Value::as_str).collect()),
        _ => None,
    }
}

/// Registry entries hold the documents weakly: a closed notebook's entry
/// is simply dead.
pub(crate) type Registered = Weak<RefCell<VirtualDocument>>;

#[cfg(test)]
mod tests;
