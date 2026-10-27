//! Notebooks (Jupyter `.ipynb` and marimo `.py`) in the editor: a
//! [`NotebookDocument`] (cells, kernel, run queue; `document`) shown by
//! notebook tabs (`view`), with the kernel bridge process in `kernel`.
//! Formats, marimo analysis and output folding live in the `notebook`
//! crate.

pub mod document;
pub mod html;
pub mod kernel;
pub mod launch;
pub mod lsp;
pub mod marimo_ui;
pub mod markdown;
pub mod output;
pub mod picker;
pub mod view;
pub mod widgets;

pub use document::{CellId, KernelStatus, NotebookDocument, NotebookEvent, RunState};
