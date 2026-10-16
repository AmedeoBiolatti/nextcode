//! The file's document symbols (`textDocument/documentSymbol`): requested
//! when the document opens on a server and [`DEBOUNCE`] after edits, kept
//! as buffer offsets (moved through edits until fresh ones arrive). They
//! feed the Outline panel, the breadcrumbs above the text, and Go to
//! Symbol in File (`Ctrl+Shift+O`).

use std::path::{Path, PathBuf};

use language::symbols::{self as model, Symbol};
use lsp::types::{DocumentSymbolParams, OneOf, request::DocumentSymbolRequest};

use super::*;
use crate::lsp_store::request_retrying;

actions!(editor, [GoToSymbol]);

/// Pause after an edit before asking for fresh symbols.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// What the editor knows about its symbols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolsStatus {
    NoServer,
    /// The server does not serve document symbols.
    Unsupported,
    Loading,
    Ready,
}

/// A picker the editor asks the workspace to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickRequest {
    /// The file's symbols (Go to Symbol in File); from a breadcrumb, only
    /// the symbols sharing `siblings_of`'s parent, with it selected.
    Symbols { siblings_of: Option<usize> },
    /// A breadcrumb path segment: the entries of `dir`, `selected` first
    /// selected.
    Directory {
        dir: PathBuf,
        selected: Option<String>,
    },
}

/// Per-editor symbol state.
pub(super) struct SymbolState {
    /// Pre-order tree in buffer offsets.
    symbols: Vec<Symbol<usize>>,
    /// A response arrived (possibly empty) since the server attached.
    loaded: bool,
    /// Bumped whenever `symbols` is replaced or cleared.
    generation: u64,
    /// The pending request; dropping it cancels the request.
    task: Option<Task<()>>,
}

impl SymbolState {
    pub(super) fn new() -> Self {
        Self {
            symbols: Vec::new(),
            loaded: false,
            generation: 0,
            task: None,
        }
    }
}

fn serves_symbols(server: &RunningServer) -> bool {
    matches!(
        server.capabilities.document_symbol_provider,
        Some(OneOf::Left(true) | OneOf::Right(_))
    )
}

impl Editor {
    /// Asks the server for this document's symbols after `delay`, replacing
    /// any pending request.
    fn request_symbols(&mut self, delay: Duration, cx: &mut Context<Self>) {
        self.symbols.task = None;
        let Some(doc) = &self.lsp else {
            return;
        };
        if !serves_symbols(&doc.server) {
            return;
        }
        let server = doc.server.clone();
        let version = doc.version;
        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier::new(doc.uri.clone()),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        self.symbols.task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            executor.timer(delay).await;
            let result = request_retrying::<DocumentSymbolRequest>(
                &server,
                params,
                &executor,
                1,
                Duration::from_millis(400),
            )
            .await;
            this.update(cx, |this, cx| {
                let current = this
                    .lsp
                    .as_ref()
                    .is_some_and(|doc| Arc::ptr_eq(&doc.server, &server) && doc.version == version);
                let Ok(response) = result else {
                    return;
                };
                if !current {
                    return;
                }
                let symbols = response.map(model::flatten).unwrap_or_default();
                let (buffer, encoding) = (&this.buffer, server.encoding);
                this.symbols.symbols = symbols
                    .into_iter()
                    .map(|symbol| {
                        symbol.map(|range| {
                            language::to_range(
                                buffer,
                                lsp::types::Range::new(range.start, range.end),
                                encoding,
                            )
                        })
                    })
                    .collect();
                this.symbols.loaded = true;
                this.symbols.generation += 1;
                cx.emit(EditorEvent::SymbolsChanged);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Requests symbols now (the document opened on a server).
    pub(super) fn symbols_attached(&mut self, cx: &mut Context<Self>) {
        self.request_symbols(Duration::ZERO, cx);
    }

    /// Requests symbols for the edited text once typing pauses.
    pub(super) fn symbols_edited(&mut self, cx: &mut Context<Self>) {
        self.request_symbols(DEBOUNCE, cx);
    }

    /// The server document moved to another view: its pending request
    /// goes with it; the symbols stay (mapped through synced edits).
    pub(super) fn symbols_handed_over(&mut self) {
        self.symbols.task = None;
    }

    /// Forgets the symbols (the server went away).
    pub(super) fn clear_symbols(&mut self) {
        self.symbols.task = None;
        self.symbols.symbols.clear();
        self.symbols.loaded = false;
        self.symbols.generation += 1;
    }

    /// Moves symbol ranges across the drained `changes`.
    pub(super) fn map_symbols(&mut self) {
        if self.symbols.symbols.is_empty() {
            return;
        }
        for change in &self.changes {
            for symbol in &mut self.symbols.symbols {
                for range in [&mut symbol.range, &mut symbol.selection_range] {
                    let end = change.map_offset(range.end, Bias::Left);
                    let start = change.map_offset(range.start, Bias::Right).min(end);
                    *range = start..end;
                }
            }
        }
    }

    /// The file's symbols as a pre-order tree in buffer offsets.
    pub fn symbols(&self) -> &[Symbol<usize>] {
        &self.symbols.symbols
    }

    /// Changes whenever the symbol list is replaced or cleared.
    pub fn symbols_generation(&self) -> u64 {
        self.symbols.generation
    }

    pub fn symbols_status(&self) -> SymbolsStatus {
        match &self.lsp {
            _ if self.symbols.loaded => SymbolsStatus::Ready,
            None => SymbolsStatus::NoServer,
            Some(doc) if !serves_symbols(&doc.server) => SymbolsStatus::Unsupported,
            Some(_) => SymbolsStatus::Loading,
        }
    }

    /// The symbols containing the primary cursor, outermost first.
    pub fn symbol_path(&self) -> Vec<usize> {
        model::symbol_path_at(&self.symbols.symbols, self.primary().head)
    }

    /// Moves the cursor to symbol `index`'s name.
    pub fn reveal_symbol(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(symbol) = self.symbols.symbols.get(index) {
            self.set_cursor(symbol.selection_range.start, cx);
        }
    }

    /// Go to Symbol in File: asks the workspace for the picker, loading
    /// the symbols first if needed (the picker fills when they arrive).
    pub(super) fn go_to_symbol(&mut self, _: &GoToSymbol, _: &mut Window, cx: &mut Context<Self>) {
        match self.symbols_status() {
            SymbolsStatus::NoServer => {
                return self.show_notice("No language server for this file", cx);
            }
            SymbolsStatus::Unsupported => {
                return self.show_notice("The language server does not list symbols", cx);
            }
            SymbolsStatus::Loading if self.symbols.task.is_none() => {
                self.request_symbols(Duration::ZERO, cx)
            }
            SymbolsStatus::Loading | SymbolsStatus::Ready => {}
        }
        cx.emit(EditorEvent::Pick(PickRequest::Symbols {
            siblings_of: None,
        }));
    }

    /// The bar above the text: the file's project-relative path, then the
    /// symbols around the cursor. Segments open pickers of their siblings.
    pub(super) fn render_breadcrumbs(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let path = self.buffer.path()?;
        // The file's own folder (several in a multi-root workspace).
        let project = self.project.as_ref();
        let root = project.map(|project| project.root_for(path).path.as_path());
        let names = path_segments(root, path);
        // Multi-root: the folder's name leads; it lists that folder.
        let folder = project
            .filter(|project| project.is_multi_root() && project.root_index_of(path).is_some())
            .map(|project| project.root_for(path));
        let last = names.len().saturating_sub(1);
        let language = self.language();
        let separator = || div().flex_none().child("›");
        let mut bar = div()
            .id("breadcrumbs")
            .flex()
            .flex_none()
            .items_center()
            .gap_1()
            .px_3()
            .h(px(22.))
            .overflow_hidden()
            .whitespace_nowrap()
            .bg(theme::background())
            .border_b_1()
            .border_color(theme::border())
            .text_xs()
            .text_color(theme::muted());
        if let Some(folder) = folder {
            let dir = folder.path.clone();
            bar = bar
                .child(
                    div()
                        .id("crumb-folder")
                        .flex_none()
                        .px_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|style| style.bg(theme::hover_row()).text_color(theme::text()))
                        .child(folder.name.clone())
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(EditorEvent::Pick(PickRequest::Directory {
                                dir: dir.clone(),
                                selected: None,
                            }));
                        })),
                )
                .child(separator());
        }
        for (index, name) in names.iter().enumerate() {
            if index > 0 {
                bar = bar.child(separator());
            }
            let (dir, selected) = match (root, index < last) {
                // A folder: its entries.
                (Some(root), true) => (self.project_dir(root, &names[..=index]), None),
                // The file: its folder's entries, the file selected.
                (Some(root), false) => {
                    (self.project_dir(root, &names[..index]), Some(name.clone()))
                }
                (None, _) => (path.parent().map(Path::to_path_buf), Some(name.clone())),
            };
            bar = bar.child(
                div()
                    .id(("crumb-path", index))
                    .flex_none()
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::hover_row()).text_color(theme::text()))
                    .when(index == last, |segment| segment.text_color(theme::text()))
                    .child(name.clone())
                    .when_some(dir, |segment, dir| {
                        segment.on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(EditorEvent::Pick(PickRequest::Directory {
                                dir: dir.clone(),
                                selected: selected.clone(),
                            }));
                        }))
                    }),
            );
        }
        for index in self.symbol_path() {
            let symbol = &self.symbols.symbols[index];
            let (badge, color) = symbol_kind_badge(symbol.kind, language);
            bar = bar.child(separator()).child(
                div()
                    .id(("crumb-symbol", index))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::hover_row()).text_color(theme::text()))
                    .child(div().text_color(color).child(badge))
                    .child(symbol.name.clone())
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(EditorEvent::Pick(PickRequest::Symbols {
                            siblings_of: Some(index),
                        }));
                    })),
            );
        }
        Some(bar)
    }

    /// The project directory `root/segments…`.
    fn project_dir(&self, root: &Path, segments: &[String]) -> Option<PathBuf> {
        let project = self.project.as_ref()?;
        Some(if segments.is_empty() {
            root.to_path_buf()
        } else {
            project.join(root, &segments.join("/"))
        })
    }
}

/// Breadcrumb names of `path`: its components relative to the project
/// `root` (folders, then the file), or only the file name outside it.
pub(crate) fn path_segments(root: Option<&Path>, path: &Path) -> Vec<String> {
    let name = || {
        path.file_name()
            .map(|name| vec![name.to_string_lossy().into_owned()])
            .unwrap_or_default()
    };
    match root.and_then(|root| path.strip_prefix(root).ok()) {
        Some(relative) => {
            let segments: Vec<String> = relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect();
            if segments.is_empty() {
                name()
            } else {
                segments
            }
        }
        None => name(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_segments_are_project_relative() {
        let root = Path::new("/home/me/project");
        assert_eq!(
            path_segments(
                Some(root),
                Path::new("/home/me/project/src/editor/symbols.rs")
            ),
            ["src", "editor", "symbols.rs"]
        );
        assert_eq!(
            path_segments(Some(root), Path::new("/home/me/project/Cargo.toml")),
            ["Cargo.toml"]
        );
        // Outside the project: the file name alone.
        assert_eq!(
            path_segments(Some(root), Path::new("/usr/lib/rustlib/src/lib.rs")),
            ["lib.rs"]
        );
        assert_eq!(path_segments(None, Path::new("/tmp/a.py")), ["a.py"]);
    }
}
