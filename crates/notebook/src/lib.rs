//! Notebooks: the document model (`model`), the `.ipynb` (`ipynb`) and
//! marimo (`marimo`) file formats, marimo's static dependency analysis,
//! folding kernel output messages into cell outputs (`outputs`), ANSI
//! colours of text outputs (`ansi`) and the JSON-lines protocol of the
//! kernel bridge (`protocol`, script in `assets/bridge.py`).
//!
//! Pure: no I/O, no UI. The editor (`crates/ui/src/notebook`) and headless
//! mode build on it.

pub mod ansi;
pub mod ipynb;
pub mod marimo;
pub mod model;
pub mod outputs;
pub mod protocol;

pub use model::*;

/// The notebook formats the editor opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Ipynb,
    Marimo,
}

/// Whether a file opens as a notebook: any `.ipynb`, or a `.py` file whose
/// `text` is a marimo notebook (`None` text: not known yet, so only
/// `.ipynb` qualifies).
pub fn kind_of(file_name: &str, text: Option<&str>) -> Option<Kind> {
    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".ipynb") {
        Some(Kind::Ipynb)
    } else if lower.ends_with(".py") && text.is_some_and(marimo::is_marimo) {
        Some(Kind::Marimo)
    } else {
        None
    }
}

impl Notebook {
    /// Reads `text` as a notebook of `kind`.
    pub fn parse(kind: Kind, text: &str) -> Result<Self, String> {
        match kind {
            Kind::Ipynb => ipynb::parse(text),
            Kind::Marimo => marimo::parse(text),
        }
    }

    /// The file's text in its own format.
    pub fn to_text(&self) -> String {
        match &self.format {
            Format::Ipynb(info) => ipynb::to_text(info, &self.cells),
            Format::Marimo(info) => marimo::to_text(info, &self.cells),
        }
    }

    pub fn kind(&self) -> Kind {
        match self.format {
            Format::Ipynb(_) => Kind::Ipynb,
            Format::Marimo(_) => Kind::Marimo,
        }
    }
}
