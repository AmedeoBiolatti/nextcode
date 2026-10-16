//! Symbol pickers: Go to Symbol in File (asked for by the editor), Go to
//! Symbol in Workspace, and the breadcrumbs' sibling lists.

use super::*;
use crate::{
    editor::PickRequest,
    symbol_picker::{SymbolPicker, SymbolPickerEvent},
};

actions!(workspace, [GoToWorkspaceSymbol]);

pub fn bind_keys(cx: &mut App) {
    let ctx = Some("Workspace");
    cx.bind_keys([
        KeyBinding::new("secondary-t", GoToWorkspaceSymbol, ctx),
        // JetBrains' Navigate > Symbol.
        KeyBinding::new("ctrl-alt-shift-n", GoToWorkspaceSymbol, ctx),
    ]);
}

impl Workspace {
    /// Opens the picker an editor asked for (Ctrl+Shift+O, a breadcrumb).
    pub(super) fn open_pick_request(
        &mut self,
        editor: Entity<Editor>,
        request: PickRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let picker = match request {
            PickRequest::Symbols { siblings_of } => {
                cx.new(|cx| SymbolPicker::document(editor, siblings_of, cx))
            }
            PickRequest::Directory { dir, selected } => {
                let project = self.project.clone();
                cx.new(|cx| SymbolPicker::directory(project, dir, selected, cx))
            }
        };
        self.show_symbol_picker(picker, window, cx);
    }

    /// `Ctrl+T`: searches the active editor's language server, or every
    /// running one when the active tab has none.
    pub(super) fn go_to_workspace_symbol(
        &mut self,
        _: &GoToWorkspaceSymbol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let running: Vec<_> = self
            .lsp_store
            .read(cx)
            .running_servers()
            .into_iter()
            .map(|(language, _, server)| (language, server))
            .collect();
        let language = self
            .active_tab()
            .and_then(Tab::editor)
            .and_then(|editor| editor.read(cx).language());
        // Every running server of the active language, else all of them.
        let servers = match language
            .filter(|&language| running.iter().any(|(running, _)| *running == language))
        {
            Some(language) => running
                .into_iter()
                .filter(|(running, _)| *running == language)
                .collect(),
            None => running,
        };
        let project = self.project.clone();
        let picker = cx.new(|cx| SymbolPicker::workspace(servers, project, cx));
        self.show_symbol_picker(picker, window, cx);
    }

    fn show_symbol_picker(
        &mut self,
        picker: Entity<SymbolPicker>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let subscription =
            cx.subscribe_in(&picker, window, |this, _, event, window, cx| match event {
                SymbolPickerEvent::Reveal { editor, offset } => {
                    this.symbol_picker = None;
                    this.record_position(cx);
                    this.forward.clear();
                    editor.update(cx, |editor, cx| editor.set_cursor(*offset, cx));
                    window.focus(&editor.focus_handle(cx));
                    cx.notify();
                }
                SymbolPickerEvent::Open(location) => {
                    this.symbol_picker = None;
                    this.jump_to(location, window, cx);
                }
                SymbolPickerEvent::OpenFile(path) => {
                    this.symbol_picker = None;
                    this.record_position(cx);
                    this.forward.clear();
                    this.open(path, window, cx);
                }
                SymbolPickerEvent::Dismissed => {
                    this.symbol_picker = None;
                    this.focus_active(window, cx);
                    cx.notify();
                }
            });
        window.focus(&picker.focus_handle(cx));
        self.symbol_picker = Some((picker, subscription));
        cx.notify();
    }
}
