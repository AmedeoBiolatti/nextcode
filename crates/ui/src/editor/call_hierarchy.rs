//! Show Call Hierarchy: prepares the hierarchy at the primary cursor; the
//! workspace shows the items in the Call Hierarchy panel.

use lsp::types::{
    CallHierarchyPrepareParams, CallHierarchyServerCapability, request::CallHierarchyPrepare,
};

use super::*;
use crate::lsp_store::request_retrying;

actions!(editor, [ShowCallHierarchy]);

/// Whether `server` answers `textDocument/prepareCallHierarchy`.
fn supports_call_hierarchy(server: &RunningServer) -> bool {
    matches!(
        server.capabilities.call_hierarchy_provider,
        Some(
            CallHierarchyServerCapability::Simple(true) | CallHierarchyServerCapability::Options(_)
        )
    )
}

impl Editor {
    pub(super) fn show_call_hierarchy(
        &mut self,
        _: &ShowCallHierarchy,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(server) = self.server() else {
            return self.show_notice("No language server for this file", cx);
        };
        if !supports_call_hierarchy(&server) {
            return self.show_notice("The language server cannot show call hierarchies", cx);
        }
        let Some(position) = self.position_params(self.primary().head) else {
            return;
        };
        let params = CallHierarchyPrepareParams {
            text_document_position_params: position,
            work_done_progress_params: Default::default(),
        };
        self._navigation_task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let result = request_retrying::<CallHierarchyPrepare>(
                &server,
                params,
                &executor,
                1,
                Duration::from_millis(400),
            )
            .await;
            this.update(cx, |this, cx| match result {
                Ok(Some(items)) if !items.is_empty() => {
                    cx.emit(EditorEvent::CallHierarchy { server, items })
                }
                Ok(_) => this.show_notice("No call hierarchy here", cx),
                Err(err) => this.show_notice(format!("Call hierarchy failed: {err}"), cx),
            })
            .ok();
        }));
    }
}
