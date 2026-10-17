//! A notebook cell's editor on its notebook's language-server document
//! (`crate::notebook::lsp`): requests and answers are in the virtual
//! document, where the cell starts at some line, so positions shift by
//! that line; answers about other cells are dropped (navigation to them
//! goes through the notebook). The notebook opens and closes the document,
//! and requests its semantic tokens once for every cell.

use lsp::types::{
    CompletionItem, CompletionTextEdit, Position, SemanticToken as LspToken, TextEdit,
};

use super::*;

impl Editor {
    /// Attaches this cell editor to its notebook's document on `server`
    /// (opened by the notebook; `link` locates the cell).
    pub fn attach_cell_server(
        &mut self,
        server: Arc<RunningServer>,
        link: CellLink,
        cx: &mut Context<Self>,
    ) {
        if self
            .lsp
            .as_ref()
            .is_some_and(|doc| Arc::ptr_eq(&doc.server, &server) && doc.cell.is_some())
        {
            return;
        }
        let Some(uri) = link.uri() else {
            return;
        };
        link.sync(&self.text());
        self.lsp = Some(LspDocument {
            server,
            uri,
            version: 1,
            cell: Some(link),
        });
        self.refresh_inlay_hints(cx);
    }

    /// Whether this editor is a notebook cell on a server document.
    pub(super) fn is_cell_document(&self) -> bool {
        self.lsp.as_ref().is_some_and(|doc| doc.cell.is_some())
    }

    /// The server line of this buffer's first line: 0 for a file, the
    /// cell's start in its notebook's document (`None` once the cell left
    /// it).
    pub(super) fn server_line_shift(&self) -> Option<u32> {
        match self.lsp.as_ref().and_then(|doc| doc.cell.as_ref()) {
            Some(link) => link.start(),
            None => Some(0),
        }
    }

    /// `offset` as a position in the server's document.
    pub(super) fn server_position(&self, offset: usize, encoding: Encoding) -> Position {
        let mut position = language::to_position(&self.buffer, offset, encoding);
        position.line += self.server_line_shift().unwrap_or(0);
        position
    }

    /// A server position as a position of this buffer; `None` outside it
    /// (another cell).
    pub(super) fn local_position(&self, position: Position) -> Option<Position> {
        let shift = self.server_line_shift()?;
        let line = position.line.checked_sub(shift)?;
        (!self.is_cell_document() || (line as usize) < self.buffer.line_count())
            .then(|| Position::new(line, position.character))
    }

    /// A server range as a byte range of this buffer.
    pub(super) fn local_range(
        &self,
        range: lsp::types::Range,
        encoding: Encoding,
    ) -> Option<Range<usize>> {
        let range = lsp::types::Range::new(
            self.local_position(range.start)?,
            self.local_position(range.end)?,
        );
        Some(language::to_range(&self.buffer, range, encoding))
    }

    fn local_edit(&self, edit: &TextEdit) -> Option<TextEdit> {
        Some(TextEdit {
            range: lsp::types::Range::new(
                self.local_position(edit.range.start)?,
                self.local_position(edit.range.end)?,
            ),
            new_text: edit.new_text.clone(),
        })
    }

    /// Moves a completion item's edits into this cell's lines; edits
    /// elsewhere (an import added to another cell) are dropped.
    pub(super) fn localize_completion(&self, item: &mut CompletionItem) {
        if !self.is_cell_document() {
            return;
        }
        item.text_edit = match item.text_edit.take() {
            Some(CompletionTextEdit::Edit(edit)) => {
                self.local_edit(&edit).map(CompletionTextEdit::Edit)
            }
            Some(CompletionTextEdit::InsertAndReplace(mut edit)) => {
                let local = |range: lsp::types::Range| {
                    Some(lsp::types::Range::new(
                        self.local_position(range.start)?,
                        self.local_position(range.end)?,
                    ))
                };
                match (local(edit.insert), local(edit.replace)) {
                    (Some(insert), Some(replace)) => {
                        edit.insert = insert;
                        edit.replace = replace;
                        Some(CompletionTextEdit::InsertAndReplace(edit))
                    }
                    _ => None,
                }
            }
            None => None,
        };
        if let Some(edits) = &mut item.additional_text_edits {
            *edits = edits
                .iter()
                .filter_map(|edit| self.local_edit(edit))
                .collect();
        }
    }

    /// The cell's semantic tokens from its notebook (relative-encoded from
    /// the cell's first line); `None` keeps the current ones.
    pub fn set_cell_semantic_tokens(
        &mut self,
        tokens: Option<Vec<LspToken>>,
        cx: &mut Context<Self>,
    ) {
        let (Some(tokens), Some(doc)) = (tokens, &self.lsp) else {
            return;
        };
        let Some(legend) = doc.server.semantic_legend.as_ref() else {
            return;
        };
        if !theme::semantic_highlighting() {
            return;
        }
        self.semantic.tokens = semantic::decode(&tokens, legend, &self.buffer, doc.server.encoding);
        cx.notify();
    }
}

/// Probes for the notebook's language-server tests.
#[cfg(test)]
impl Editor {
    /// `offset` as the position sent to the server.
    pub(crate) fn server_position_of(&self, offset: usize) -> Position {
        let encoding = self
            .lsp
            .as_ref()
            .map_or(Encoding::Utf16, |doc| doc.server.encoding);
        self.server_position(offset, encoding)
    }

    /// The diagnostics shown: byte range in the buffer and message.
    pub(crate) fn diagnostic_ranges(&self) -> Vec<(Range<usize>, String)> {
        self.diagnostics
            .iter()
            .map(|d| (d.range.clone(), d.message.to_string()))
            .collect()
    }

    /// Asks for completions at the cursor (Ctrl+Space).
    pub(crate) fn complete_at_cursor(&mut self, cx: &mut Context<Self>) {
        self.request_completions(None, cx);
    }

    /// The open completion menu's labels as listed.
    pub(crate) fn completion_labels(&self) -> Vec<String> {
        self.completion.as_ref().map_or_else(Vec::new, |menu| {
            menu.candidates
                .iter()
                .map(|candidate| menu.items[candidate.index].label.clone())
                .collect()
        })
    }

    /// Accepts the completion menu's `index`-th item.
    pub(crate) fn accept_completion_at(&mut self, index: usize, cx: &mut Context<Self>) {
        self.accept_completion(Some(index), cx);
    }
}
