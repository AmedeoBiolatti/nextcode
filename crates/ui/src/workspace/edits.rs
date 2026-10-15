//! Workspace edits from language servers (rename, code actions,
//! `workspace/applyEdit`): text edits go to the open view of a file (one
//! undo step per file; other views follow), or to the file on disk; file
//! operations happen on disk and open tabs follow renames.

use lsp::types::{Command, WorkspaceEdit};
use text::Encoding;

use super::*;
use crate::{
    lsp_store::{ApplyEditRequest, RunningServer},
    workspace_edit::{self, Documents, Failure, Report},
};

/// The workspace's open editors as targets of a workspace edit.
struct OpenEditors<'a, 'b> {
    workspace: &'a mut Workspace,
    cx: &'a mut Context<'b, Workspace>,
}

impl OpenEditors<'_, '_> {
    /// The view of `path` holding its server document, else any view.
    fn editor(&self, path: &Path) -> Option<Entity<Editor>> {
        let views = || {
            self.workspace
                .all_tabs()
                .filter(move |tab| tab.path == path && tab.editor().is_some())
        };
        views()
            .find(|tab| tab.lsp_owner)
            .or_else(|| views().next())
            .and_then(Tab::editor)
            .cloned()
    }
}

impl Documents for OpenEditors<'_, '_> {
    fn version(&mut self, path: &Path) -> Option<i32> {
        self.editor(path)?.read(self.cx).server_version()
    }

    fn edit(
        &mut self,
        path: &Path,
        edits: &[lsp::types::TextEdit],
        encoding: Encoding,
    ) -> Option<Result<(), String>> {
        let editor = self.editor(path)?;
        Some(editor.update(self.cx, |editor, cx| {
            editor.apply_server_edits(edits, encoding, cx)
        }))
    }

    fn renamed(&mut self, from: &Path, to: &Path) {
        let project = self.workspace.project.clone();
        let mut moved = Vec::new();
        for tab in self
            .workspace
            .panes
            .iter_mut()
            .flat_map(|pane| &mut pane.tabs)
        {
            let Ok(rest) = tab.path.strip_prefix(from) else {
                continue;
            };
            let path = if rest.as_os_str().is_empty() {
                to.to_path_buf()
            } else {
                project.join(to, &rest.to_string_lossy())
            };
            tab.title = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
                .into();
            tab.path = path.clone();
            if let Some(editor) = tab.editor() {
                moved.push((editor.clone(), path, tab.lsp_owner));
            }
        }
        for (editor, path, owner) in moved {
            editor.update(self.cx, |editor, cx| editor.set_file_path(path, cx));
            let language = editor.read(self.cx).language();
            if owner && let Some(language) = language {
                let server = self
                    .workspace
                    .lsp_store
                    .update(self.cx, |store, cx| store.server(language, cx));
                if let Some(server) = server {
                    editor.update(self.cx, |editor, cx| editor.attach_server(server, cx));
                }
            }
        }
        self.cx.notify();
    }

    fn deleted(&mut self, path: &Path) {
        if self
            .workspace
            .all_tabs()
            .any(|tab| tab.path.starts_with(path))
        {
            self.workspace.notice = Some(format!("{} was deleted", path.display()).into());
        }
    }
}

impl Workspace {
    fn apply_workspace_edit(
        &mut self,
        edit: WorkspaceEdit,
        encoding: Encoding,
        cx: &mut Context<Self>,
    ) -> Report {
        let operations = match workspace_edit::operations(edit) {
            Ok(operations) => operations,
            Err(message) => {
                return Report {
                    changed: Vec::new(),
                    failure: Some(Failure {
                        index: 0,
                        path: self.project.root().to_path_buf(),
                        message,
                    }),
                };
            }
        };
        let project = self.project.clone();
        let mut documents = OpenEditors {
            workspace: self,
            cx,
        };
        workspace_edit::apply(&project, &operations, encoding, &mut documents)
    }

    /// An editor's rename or code action: applies the edit, reports it in
    /// that editor's status bar (failures above the panes, like other
    /// errors), then runs the action's command.
    pub(super) fn apply_editor_edit(
        &mut self,
        editor: &Entity<Editor>,
        edit: WorkspaceEdit,
        server: Arc<RunningServer>,
        command: Option<Command>,
        label: &str,
        cx: &mut Context<Self>,
    ) {
        let report = self.apply_workspace_edit(edit, server.encoding, cx);
        let summary = report.summary(label, self.project.root());
        if report.failure.is_some() {
            self.notice = Some(summary.into());
            cx.notify();
        } else {
            editor.update(cx, |editor, cx| editor.show_notice(summary, cx));
        }
        let Some(command) = command.filter(|_| report.failure.is_none()) else {
            return;
        };
        let label = label.to_owned();
        cx.spawn(async move |this, cx| {
            if let Err(err) = crate::editor::execute_command(&server, command).await {
                this.update(cx, |this, cx| {
                    this.notice = Some(format!("{label} failed: {err}").into());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// A server's `workspace/applyEdit` request.
    pub(super) fn apply_edit_request(
        &mut self,
        request: &ApplyEditRequest,
        cx: &mut Context<Self>,
    ) {
        let report = self.apply_workspace_edit(request.edit.clone(), request.encoding, cx);
        request.respond(report.response());
        if report.failure.is_some() {
            let label = request.label.as_deref().unwrap_or("Edit");
            self.notice = Some(report.summary(label, self.project.root()).into());
            cx.notify();
        }
    }
}
