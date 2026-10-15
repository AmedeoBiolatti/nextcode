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
        let running = self.lsp_store.read(cx).running_servers();
        let language = self
            .active_tab()
            .and_then(Tab::editor)
            .and_then(|editor| editor.read(cx).language());
        let servers = match language
            .and_then(|language| running.iter().find(|(running, _)| *running == language))
        {
            Some(server) => vec![server.clone()],
            None => running,
        };
        let root = self.project.root().to_path_buf();
        let picker = cx.new(|cx| SymbolPicker::workspace(servers, root, cx));
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
