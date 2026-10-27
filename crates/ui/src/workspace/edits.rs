//! Workspace edits from language servers (rename, code actions,
//! `workspace/applyEdit`) and the search panel's Replace: text edits go to
//! the open view of a file (one undo step per file; other views follow), or
//! to the file on disk; file operations happen on disk and open tabs follow
//! renames.

use lsp::types::{Command, WorkspaceEdit};
use text::Encoding;

use super::*;
use crate::{
    lsp_store::{ApplyEditRequest, RunningServer},
    workspace_edit::{self, Documents, Failure, FileTargets, ReplaceReport, Report},
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
        if let Some(notebook) = self.workspace.lsp_store.read(self.cx).notebook(path) {
            return Some(notebook.borrow().version());
        }
        self.editor(path)?.read(self.cx).server_version()
    }

    fn text(&mut self, path: &Path) -> Option<String> {
        if let Some(notebook) = self.workspace.lsp_store.read(self.cx).notebook(path) {
            return Some(notebook.borrow().text());
        }
        Some(self.editor(path)?.read(self.cx).text())
    }

    fn edit(
        &mut self,
        path: &Path,
        edits: &[lsp::types::TextEdit],
        encoding: Encoding,
    ) -> Option<Result<(), String>> {
        // A notebook's virtual document: the edits go to its cells.
        if let Some(notebook) = self.workspace.lsp_store.read(self.cx).notebook(path) {
            return Some(notebook.borrow().apply_edits(edits, encoding, self.cx));
        }
        let editor = self.editor(path)?;
        Some(editor.update(self.cx, |editor, cx| {
            editor.apply_server_edits(edits, encoding, cx)
        }))
    }

    fn renamed(&mut self, from: &Path, to: &Path) {
        self.workspace.follow_rename(from, to, self.cx);
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
    /// Tabs (editor, diff) of `from` or files inside it now show `to`:
    /// paths, titles, the editors' language server documents (unsaved
    /// edits stay) and breakpoints follow.
    pub(super) fn follow_rename(&mut self, from: &Path, to: &Path, cx: &mut Context<Self>) {
        let project = self.project.clone();
        let mut moved = Vec::new();
        for tab in self.panes.iter_mut().flat_map(|pane| &mut pane.tabs) {
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
            editor.update(cx, |editor, cx| editor.set_file_path(path.clone(), cx));
            let language = editor.read(cx).language();
            if owner && let Some(language) = language {
                let server = self
                    .lsp_store
                    .update(cx, |store, cx| store.server(language, &path, cx));
                if let Some(server) = server {
                    editor.update(cx, |editor, cx| editor.attach_server(server, cx));
                }
            }
        }
        let debug = self.debug.store.clone();
        debug.update(cx, |store, cx| store.renamed(from, to, &project, cx));
        cx.notify();
    }

    /// The search panel's Replace: open editors take the edits (left
    /// unsaved), other files are written.
    pub(super) fn apply_search_replace(
        &mut self,
        replacer: &search::Replacer,
        files: &[FileTargets],
        cx: &mut Context<Self>,
    ) -> ReplaceReport {
        let project = self.project.clone();
        let mut documents = OpenEditors {
            workspace: self,
            cx,
        };
        workspace_edit::replace(&project, replacer, files, &mut documents)
    }

    /// Open files with unsaved changes and their text, once per file.
    pub(super) fn unsaved_documents(&self, cx: &App) -> Vec<(PathBuf, String)> {
        let mut documents: Vec<(PathBuf, String)> = Vec::new();
        for tab in self.all_tabs().filter(|tab| tab.is_dirty(cx)) {
            if let Some(editor) = tab.editor()
                && !documents.iter().any(|(path, _)| *path == tab.path)
            {
                documents.push((tab.path.clone(), editor.read(cx).text()));
            }
        }
        documents
    }

    pub(super) fn apply_workspace_edit(
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

    /// An editor's rename or code action (`editor`: `None` for a notebook
    /// cell's): applies the edit, reports it in that editor's status bar
    /// (failures above the panes, like other errors), then runs the
    /// action's command.
    pub(super) fn apply_editor_edit(
        &mut self,
        editor: Option<&Entity<Editor>>,
        edit: WorkspaceEdit,
        server: Arc<RunningServer>,
        command: Option<Command>,
        label: &str,
        cx: &mut Context<Self>,
    ) {
        let report = self.apply_workspace_edit(edit, server.encoding, cx);
        let summary = report.summary(label, &self.project);
        if report.failure.is_some() {
            self.notice = Some(summary.into());
            cx.notify();
        } else if let Some(editor) = editor {
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
            self.notice = Some(report.summary(label, &self.project).into());
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, WindowHandle};

    use super::*;

    fn keys(window: WindowHandle<Workspace>, keystrokes: &str, cx: &mut TestAppContext) {
        cx.simulate_keystrokes(window.into(), keystrokes);
        cx.run_until_parked();
    }

    /// Runs the app until `done` holds (searches run on a thread).
    fn wait_until(
        window: WindowHandle<Workspace>,
        cx: &mut TestAppContext,
        done: impl Fn(&Workspace, &App) -> bool,
    ) {
        for _ in 0..500 {
            cx.run_until_parked();
            if window
                .read_with(cx, |workspace, cx| done(workspace, cx))
                .unwrap()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out");
    }

    fn status(window: WindowHandle<Workspace>, cx: &mut TestAppContext) -> (String, String) {
        window
            .read_with(cx, |workspace, cx| {
                let (status, notice) = workspace.search.read(cx).shown_status();
                (status.unwrap_or_default(), notice.unwrap_or_default())
            })
            .unwrap()
    }

    fn editor_text(window: WindowHandle<Workspace>, cx: &mut TestAppContext) -> (String, bool) {
        window
            .read_with(cx, |workspace, cx| {
                let tab = workspace.all_tabs().next().unwrap();
                (tab.editor().unwrap().read(cx).text(), tab.is_dirty(cx))
            })
            .unwrap()
    }

    #[gpui::test]
    fn replace_in_files_edits_unsaved_buffers_undoably_and_writes_closed_files(
        cx: &mut TestAppContext,
    ) {
        let root = std::env::temp_dir().join(format!("nc-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "let old = 1;\n").unwrap();
        std::fs::write(root.join("b.txt"), "old\nold(old)\n").unwrap();
        cx.update(|cx| {
            crate::editor::bind_keys(cx);
            super::super::bind_keys(cx);
            crate::search_panel::bind_keys(cx);
        });
        let project = Project::local(root.clone());
        let window = cx.add_window(|window, cx| Workspace::new(project, None, window, cx));
        cx.run_until_parked();
        window
            .update(cx, |workspace, window, cx| {
                workspace.open(&root.join("a.txt"), window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        // An unsaved edit moves the match: the buffer is searched, not the file.
        cx.simulate_input(window.into(), "// ");
        cx.run_until_parked();

        keys(window, "ctrl-shift-r", cx);
        cx.simulate_input(window.into(), "old");
        keys(window, "enter", cx);
        wait_until(window, cx, |workspace, cx| {
            workspace
                .search
                .read(cx)
                .shown_status()
                .0
                .is_some_and(|status| status.contains("files searched"))
        });
        assert!(status(window, cx).0.starts_with("4 matches in 2 files"));
        window
            .update(cx, |workspace, _, cx| {
                let input = workspace.search.read(cx).replacement_input();
                input.update(cx, |input, cx| input.set_text("new", cx));
            })
            .unwrap();
        cx.run_until_parked();

        // Ctrl+Alt+Enter asks first; Cancel changes nothing.
        keys(window, "ctrl-alt-enter", cx);
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(editor_text(window, cx).0, "// let old = 1;\n");
        keys(window, "ctrl-alt-enter", cx);
        cx.simulate_prompt_answer("Replace");
        cx.run_until_parked();

        assert_eq!(editor_text(window, cx), ("// let new = 1;\n".into(), true));
        let read = |name: &str| std::fs::read_to_string(root.join(name)).unwrap();
        assert_eq!(read("a.txt"), "let old = 1;\n", "open files are not saved");
        assert_eq!(read("b.txt"), "new\nnew(new)\n");
        wait_until(window, cx, |workspace, cx| {
            workspace
                .search
                .read(cx)
                .shown_status()
                .0
                .is_some_and(|status| status.starts_with("0 matches"))
        });
        assert_eq!(status(window, cx).1, "Replaced 4 matches in 2 files");

        // The replace is one undo step in the editor.
        window
            .update(cx, |workspace, window, cx| {
                workspace.focus_active(window, cx)
            })
            .unwrap();
        keys(window, "ctrl-z", cx);
        assert_eq!(editor_text(window, cx), ("// let old = 1;\n".into(), true));

        std::fs::remove_dir_all(&root).ok();
        if let Some(state) = crate::layout::state_root() {
            std::fs::remove_dir_all(state).ok();
        }
    }
}
