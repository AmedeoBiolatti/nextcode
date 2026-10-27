//! The document model shared by both formats: cells with their source and
//! outputs, plus what each format needs to write the file back the way it
//! was read (unknown metadata, cell ids, marimo header and cell names).

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// JSON object kept verbatim (notebook, cell and output metadata).
pub type Metadata = Map<String, Value>;

/// Output data by mime type. Text values (nbformat stores them as lists of
/// lines) are joined into one string; JSON mimes keep their JSON value.
pub type MimeBundle = BTreeMap<String, Value>;

#[derive(Debug, Clone, PartialEq)]
pub struct Notebook {
    pub cells: Vec<Cell>,
    pub format: Format,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Format {
    Ipynb(IpynbInfo),
    Marimo(MarimoInfo),
}

/// What an `.ipynb` file holds besides its cells.
#[derive(Debug, Clone, PartialEq)]
pub struct IpynbInfo {
    pub nbformat: u64,
    pub nbformat_minor: u64,
    /// The notebook's `metadata` (kernelspec, language_info, …) as read.
    pub metadata: Metadata,
    /// Unknown top-level keys, written back unchanged.
    pub extra: Metadata,
    /// Spaces per indentation level of the file as read (Jupyter writes 1,
    /// VS Code keeps the file's).
    pub indent: usize,
    /// Whether the file ended with a newline (Jupyter writes one).
    pub trailing_newline: bool,
}

/// What a marimo `.py` notebook holds besides its cells.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MarimoInfo {
    /// Everything before the `import marimo` line, verbatim (docstring,
    /// comments, a PEP 723 `# /// script` block).
    pub header: String,
    /// `__generated_with = "…"`'s value; `None` when the file has none.
    pub generated_with: Option<String>,
    /// The `app = marimo.App(…)` statement verbatim (it may span lines).
    pub app: String,
    /// Everything after the last cell verbatim (the `if __name__ ==
    /// "__main__":` block).
    pub footer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CellKind {
    Code,
    Markdown,
    Raw,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub kind: CellKind,
    /// The cell's text (joined from nbformat's list of lines). For a marimo
    /// cell shown as Markdown, the Markdown (see [`MarimoCell::original`]).
    pub source: String,
    /// nbformat ≥ 4.5 cell id; `None` when the file has none (older
    /// notebooks, marimo). The ipynb writer adds ids to cells without one
    /// when the notebook's `nbformat_minor` is ≥ 5.
    pub id: Option<String>,
    pub execution_count: Option<u64>,
    /// Code cells only (always empty for marimo: marimo files keep no
    /// outputs; the editor fills them while it runs).
    pub outputs: Vec<Output>,
    pub metadata: Metadata,
    /// Markdown/raw cell attachments, verbatim.
    pub attachments: Option<Value>,
    /// Unknown cell keys, written back unchanged.
    pub extra: Metadata,
    /// marimo cells only.
    pub marimo: Option<MarimoCell>,
}

impl Cell {
    /// A new cell of `kind` with `source`, no id (writers add one where the
    /// format needs it), no outputs.
    pub fn new(kind: CellKind, source: impl Into<String>) -> Self {
        Self {
            kind,
            source: source.into(),
            id: None,
            execution_count: None,
            outputs: Vec::new(),
            metadata: Metadata::new(),
            attachments: None,
            extra: Metadata::new(),
            marimo: None,
        }
    }

    /// Outputs collapsed: nbformat's `metadata.jupyter.outputs_hidden`, or
    /// the older `metadata.collapsed` (classic Notebook, JupyterLab).
    pub fn outputs_hidden(&self) -> bool {
        self.metadata.get("collapsed").and_then(Value::as_bool) == Some(true)
            || self.jupyter_flag("outputs_hidden")
    }

    /// Source collapsed: nbformat's `metadata.jupyter.source_hidden`.
    pub fn source_hidden(&self) -> bool {
        self.jupyter_flag("source_hidden")
    }

    /// Writes the outputs' collapsed state where every Jupyter front end
    /// reads it: hidden sets `jupyter.outputs_hidden` and `collapsed`;
    /// shown removes `jupyter.outputs_hidden` and turns a `collapsed` the
    /// cell has into `false` (cells that never had one get none).
    pub fn set_outputs_hidden(&mut self, hidden: bool) {
        if hidden == self.outputs_hidden() {
            return;
        }
        self.set_jupyter_flag("outputs_hidden", hidden);
        if hidden {
            self.metadata.insert("collapsed".into(), Value::Bool(true));
        } else if let Some(collapsed) = self.metadata.get_mut("collapsed") {
            *collapsed = Value::Bool(false);
        }
    }

    /// Sets (or removes) `metadata.jupyter.source_hidden`.
    pub fn set_source_hidden(&mut self, hidden: bool) {
        if hidden != self.source_hidden() {
            self.set_jupyter_flag("source_hidden", hidden);
        }
    }

    fn jupyter_flag(&self, key: &str) -> bool {
        self.metadata
            .get("jupyter")
            .and_then(|jupyter| jupyter.get(key))
            .and_then(Value::as_bool)
            == Some(true)
    }

    /// `true` sets `jupyter.<key>`; `false` removes it (and an emptied
    /// `jupyter` object).
    fn set_jupyter_flag(&mut self, key: &str, value: bool) {
        if value {
            let jupyter = self
                .metadata
                .entry("jupyter")
                .or_insert_with(|| Value::Object(Metadata::new()));
            if !jupyter.is_object() {
                *jupyter = Value::Object(Metadata::new());
            }
            if let Some(jupyter) = jupyter.as_object_mut() {
                jupyter.insert(key.into(), Value::Bool(true));
            }
        } else if let Some(Value::Object(jupyter)) = self.metadata.get_mut("jupyter") {
            jupyter.remove(key);
            if jupyter.is_empty() {
                self.metadata.remove("jupyter");
            }
        }
    }
}

/// How a marimo cell is written.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MarimoCell {
    /// The function's name: `_` for anonymous cells.
    pub name: String,
    pub kind: MarimoCellKind,
    /// The decorator's arguments verbatim without the parentheses
    /// (`hide_code=True`); empty for a bare `@app.cell`.
    pub config: String,
    /// For a cell shown as Markdown: `(markdown, code)` as read, so the
    /// original code (quotes, `r` prefix, indentation) is written back
    /// while the Markdown is unchanged.
    pub original: Option<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MarimoCellKind {
    /// `@app.cell def name(refs): … return (defs,)`.
    #[default]
    Cell,
    /// `with app.setup:` (runs first; its definitions are global).
    Setup,
    /// `@app.function` (the cell's code is the function definition).
    Function,
    /// `@app.class_definition` (the cell's code is the class definition).
    Class,
    /// `app._unparsable_cell(r"""…""", name="…")`.
    Unparsable,
}

/// One nbformat output.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    Stream {
        /// `stdout` or `stderr`.
        name: String,
        /// Already folded for `\r` and `\b` (see `outputs::push_stream`).
        text: String,
    },
    DisplayData {
        data: MimeBundle,
        metadata: Metadata,
        /// `transient.display_id` from the kernel (never saved): targets of
        /// `update_display_data`.
        display_id: Option<String>,
    },
    ExecuteResult {
        execution_count: Option<u64>,
        data: MimeBundle,
        metadata: Metadata,
    },
    Error {
        ename: String,
        evalue: String,
        /// Lines with ANSI escapes, as the kernel sent them.
        traceback: Vec<String>,
    },
}
