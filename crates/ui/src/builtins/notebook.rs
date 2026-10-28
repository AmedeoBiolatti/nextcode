//! Notebooks, a built-in extension: Jupyter `.ipynb` and marimo `.py`
//! files open in notebook tabs (`tabs`): a [`NotebookDocument`] (cells,
//! kernel, run queue; `document`) shown by notebook views (`view`), with
//! the kernel bridge process in `kernel`. Formats, marimo analysis and
//! output folding live in the `notebook` crate.

use crate::builtins::{Builtin, Entry, MenuItem};

pub mod document;
pub mod html;
pub mod kernel;
pub mod launch;
pub mod lsp;
pub mod marimo_ui;
pub mod output;
pub mod picker;
mod tabs;
pub mod view;
pub mod widgets;

pub use document::{CellId, KernelStatus, NotebookDocument, NotebookEvent, RunState};

/// How many items `old` and `new` share at their start and, after that,
/// at their end (`same` compares two).
fn common_ends<T>(old: &[T], new: &[T], same: impl Fn(&T, &T) -> bool) -> (usize, usize) {
    let prefix = old
        .iter()
        .zip(new)
        .take_while(|(old, new)| same(old, new))
        .count();
    let room = old.len().min(new.len()) - prefix;
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(room)
        .take_while(|(old, new)| same(old, new))
        .count();
    (prefix, suffix)
}

pub const BUILTIN: Builtin = Builtin {
    id: "notebook",
    name: "Notebooks",
    description: "Jupyter (.ipynb) and marimo notebooks in notebook tabs: cells, kernels, rich outputs and widgets.",
    default_enabled: true,
    labels: &[
        ("notebook::RunCell", "Run Cell"),
        (
            "notebook::RunCellAndSelectNext",
            "Run Cell and Select Below",
        ),
        (
            "notebook::RunCellAndInsertBelow",
            "Run Cell and Insert Below",
        ),
        ("notebook::RunAll", "Run All Cells"),
        ("notebook::RunAbove", "Run All Above Selected Cell"),
        ("notebook::RunBelow", "Run Selected Cell and All Below"),
        ("notebook::Interrupt", "Interrupt Kernel"),
        ("notebook::Restart", "Restart Kernel"),
        ("notebook::RestartAndRunAll", "Restart Kernel and Run All"),
        ("notebook::ClearOutputs", "Clear All Outputs"),
        ("notebook::Save", "Save Notebook"),
        ("notebook::InsertAbove", "Insert Cell Above"),
        ("notebook::InsertBelow", "Insert Cell Below"),
        ("notebook::DeleteCells", "Delete Cells"),
        ("notebook::UndoCells", "Undo Cell Operation"),
        ("notebook::RedoCells", "Redo Cell Operation"),
        ("notebook::ToCode", "Change Cell to Code"),
        ("notebook::ToMarkdown", "Change Cell to Markdown"),
        ("notebook::ToRaw", "Change Cell to Raw"),
        ("notebook::CopyCells", "Copy Cells"),
        ("notebook::CutCells", "Cut Cells"),
        ("notebook::PasteBelow", "Paste Cells Below"),
        ("notebook::PasteAbove", "Paste Cells Above"),
        ("notebook::SplitCell", "Split Cell at Cursor"),
        ("notebook::MoveCellsUp", "Move Cells Up"),
        ("notebook::MoveCellsDown", "Move Cells Down"),
        ("notebook::ToggleOutputs", "Collapse/Expand Outputs"),
        ("notebook::ToggleSource", "Collapse/Expand Cell Source"),
        ("notebook::ShowHelp", "Show Kernel Help at Cursor"),
        ("notebook::FormatCell", "Format Cell"),
        ("notebook::FormatNotebook", "Format Notebook"),
        (
            "notebook::ChooseInterpreter",
            "Choose Notebook Interpreter…",
        ),
        ("notebook::OpenInMarimo", "Open in marimo"),
        ("notebook::OpenAsText", "Open Notebook as Text"),
        ("notebook::OpenAsNotebook", "Open as Notebook"),
    ],
    // Command mode's keys.
    unlisted: &[
        "notebook::CommandMode",
        "notebook::EditMode",
        "notebook::SelectPrevious",
        "notebook::SelectNext",
        "notebook::ExtendSelectionUp",
        "notebook::ExtendSelectionDown",
        "notebook::SelectFirst",
        "notebook::SelectLast",
    ],
    menu_items: &[MenuItem {
        menu: "Run/Notebooks",
        after: "",
        items: &[
            Entry::Command("notebook::RunCell"),
            Entry::Command("notebook::RunAll"),
            Entry::Command("notebook::Interrupt"),
            Entry::Command("notebook::Restart"),
        ],
    }],
    workspace_actions: tabs::actions,
    workspace_model: Some(tabs::model),
    file_tabs: &[tabs::TAB],
    tab_activated: tabs::tab_activated,
    files_changed: tabs::files_changed,
    navigate: tabs::navigate,
    accept_offer: tabs::accept_offer,
    ..Builtin::NONE
};
