//! The notebook's language server in its view (`notebook::lsp`): code
//! cell editors attach to the virtual document and get their cell's
//! diagnostics and semantic tokens; go-to and workspace edits from a cell
//! go to the workspace (which maps virtual positions back to cells); Format
//! Cell (Shift+Alt+F in a cell) and Format Notebook (Shift+Alt+F in
//! command mode).

use lsp::types::Position;

use super::*;
use crate::notebook::lsp::{NotebookLsp, NotebookLspEvent, format_cells};

actions!(notebook, [FormatCell, FormatNotebook]);

pub(super) fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("shift-alt-f", FormatCell, Some("Editor && notebook_cell")),
        KeyBinding::new("shift-alt-f", FormatNotebook, Some("Notebook && command")),
    ]);
}

impl NotebookView {
    /// Shows the cells on `lsp`'s language-server document (shared by the
    /// document's views).
    pub fn set_language_server(&mut self, lsp: Entity<NotebookLsp>, cx: &mut Context<Self>) {
        let subscription = cx.subscribe(&lsp, |this, _, event, cx| this.on_lsp_event(event, cx));
        self.lsp = Some((lsp, subscription));
        for id in self.code_editors() {
            self.link_editor(id, cx);
        }
    }

    pub fn language_server(&self) -> Option<&Entity<NotebookLsp>> {
        self.lsp.as_ref().map(|(lsp, _)| lsp)
    }

    /// Code cells with an editor.
    fn code_editors(&self) -> Vec<CellId> {
        self.cells
            .iter()
            .filter(|(_, state)| state.kind == CellKind::Code && state.editor.is_some())
            .map(|(id, _)| *id)
            .collect()
    }

    /// Attaches (or detaches) cell `id`'s editor to the server document,
    /// with the cell's diagnostics and tokens.
    pub(super) fn link_editor(&mut self, id: CellId, cx: &mut Context<Self>) {
        let Some((lsp, _)) = &self.lsp else {
            return;
        };
        let Some(editor) = self
            .cells
            .get(&id)
            .filter(|state| state.kind == CellKind::Code)
            .and_then(|state| state.editor.clone())
        else {
            return;
        };
        let lsp = lsp.read(cx);
        match (lsp.server(), lsp.link(id)) {
            (Some(server), Some(link)) => {
                let diagnostics = lsp.diagnostics(id).to_vec();
                let tokens = lsp.tokens(id);
                editor.update(cx, |editor, cx| {
                    editor.attach_cell_server(server, link, cx);
                    editor.set_diagnostics(&diagnostics, cx);
                    editor.set_cell_semantic_tokens(tokens, cx);
                });
            }
            _ => editor.update(cx, |editor, cx| {
                if editor.server().is_some() {
                    editor.detach_server(false);
                    editor.set_diagnostics(&[], cx);
                }
            }),
        }
    }

    fn on_lsp_event(&mut self, event: &NotebookLspEvent, cx: &mut Context<Self>) {
        let Some((lsp, _)) = &self.lsp else {
            return;
        };
        let lsp = lsp.clone();
        let ids = match event {
            NotebookLspEvent::Diagnostics(changed) => changed.clone(),
            _ => self.code_editors(),
        };
        for id in ids {
            let editor = self
                .cells
                .get(&id)
                .filter(|state| state.kind == CellKind::Code)
                .and_then(|state| state.editor.clone());
            let Some(editor) = editor else {
                continue;
            };
            match event {
                NotebookLspEvent::ServerChanged => self.link_editor(id, cx),
                NotebookLspEvent::Diagnostics(_) => {
                    // What another cell defines changed: types may have too.
                    let diagnostics = lsp.read(cx).diagnostics(id).to_vec();
                    editor.update(cx, |editor, cx| {
                        editor.set_diagnostics(&diagnostics, cx);
                        editor.refresh_inlay_hints(cx);
                    });
                }
                NotebookLspEvent::Tokens => {
                    let tokens = lsp.read(cx).tokens(id);
                    editor.update(cx, |editor, cx| editor.set_cell_semantic_tokens(tokens, cx));
                }
            }
        }
    }

    /// Edit mode in cell `id` with the cursor at `position` (the server's
    /// encoding), e.g. a go-to target or a Problems entry.
    pub fn go_to_cell(
        &mut self,
        id: CellId,
        position: Position,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = self.enter_edit(id, window, cx) {
            editor.update(cx, |editor, cx| editor.go_to(position, cx));
            if let Some(index) = self.order.iter().position(|other| *other == id) {
                self.reveal_cell(index);
            }
            self.follow_cursor(id, window, cx);
        }
    }

    pub(super) fn format_cell(&mut self, _: &FormatCell, _: &mut Window, cx: &mut Context<Self>) {
        let ids = self.selected_ids(cx);
        self.format(ids, cx);
    }

    pub(super) fn format_notebook(
        &mut self,
        _: &FormatNotebook,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ids = self
            .document
            .read(cx)
            .cells()
            .iter()
            .map(|c| c.id)
            .collect();
        self.format(ids, cx);
    }

    /// Formats the code cells among `ids`; failures show as a notice.
    fn format(&mut self, ids: Vec<CellId>, cx: &mut Context<Self>) {
        let (settings, server) = match &self.lsp {
            Some((lsp, _)) => (lsp.read(cx).settings(cx), lsp.read(cx).server()),
            None => (Default::default(), None),
        };
        let task = format_cells(&self.document, &ids, &settings, server, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.notice = match result {
                    Ok(_) => None,
                    Err(err) => Some(Notice {
                        text: format!("Format failed: {err}").into(),
                        error: true,
                    }),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}
