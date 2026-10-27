//! The Files tree's operations that involve open editors and language
//! servers: renames and moves (the servers' `workspace/willRenameFiles`
//! edits first, e.g. rust-analyzer's `mod` lines; then tabs, unsaved
//! edits, documents and breakpoints follow; `didRenameFiles` after),
//! deletes (one confirmation naming the items and any unsaved files; their
//! tabs close), new files opening in an editor, New File… from anywhere in
//! the window (File menu, palette), and Open in Terminal.

use std::{io, time::Duration};

use futures::future::{Either, select};
use lsp::types::WorkspaceEdit;
use text::Encoding;
use worktree::FileKind;

use super::*;
use crate::{
    file_ops::{self, FileMove},
    file_tree::{self, FileTreeEvent, NewFile, NewPythonFile},
    lsp_store::RunningServer,
};

/// How long a rename waits for each language server's edits.
const WILL_RENAME_TIMEOUT: Duration = Duration::from_secs(5);
/// Items named in the delete confirmation.
const LISTED_ITEMS: usize = 10;

/// A language server's edits for a rename: its name, encoding, the edit.
type RenameEdit = (SharedString, Encoding, WorkspaceEdit);

impl Workspace {
    pub(super) fn on_file_tree_event(
        &mut self,
        event: &FileTreeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            FileTreeEvent::Open { path, focus_editor } => {
                self.open(path, window, cx);
                if !focus_editor {
                    window.focus(&self.file_tree.focus_handle(cx));
                }
            }
            FileTreeEvent::Created(path) => self.open(path, window, cx),
            FileTreeEvent::Move(moves) => self.move_paths(moves.clone(), window, cx),
            FileTreeEvent::Delete(paths) => self.confirm_delete(paths.clone(), window, cx),
            FileTreeEvent::OpenInTerminal(dir) => {
                self.show_panel(PanelId::Terminal, cx);
                self.terminal
                    .update(cx, |terminal, cx| terminal.new_terminal_in(dir, window, cx));
            }
            FileTreeEvent::RemoveFolder(index) => self.remove_folder(*index, cx),
            FileTreeEvent::Notice(message) => self.file_notice(message.clone(), cx),
        }
    }

    /// New File… (File menu, palette) while focus is outside the tree:
    /// shows the Files panel and starts the name row in the active file's
    /// folder (else the tree's selection's).
    pub(super) fn new_file(&mut self, _: &NewFile, window: &mut Window, cx: &mut Context<Self>) {
        self.start_new_file(None, window, cx);
    }

    /// New Python File…: New File… with Python chosen.
    pub(super) fn new_python_file(
        &mut self,
        _: &NewPythonFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_new_file(file_tree::python_type(), window, cx);
    }

    fn start_new_file(
        &mut self,
        file_type: Option<&'static crate::file_types::FileType>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_panel(PanelId::Files, cx);
        let near = self
            .active_tab()
            .and_then(Tab::file_path)
            .map(Path::to_path_buf);
        self.file_tree.update(cx, |tree, cx| {
            tree.start_new_file(near, file_type, window, cx)
        });
    }

    fn file_notice(&mut self, message: String, cx: &mut Context<Self>) {
        self.notice = Some(message.into());
        cx.notify();
    }

    /// Renames or moves `(from, to)` pairs: all are checked first (nothing
    /// moves if one target is taken), running language servers that asked
    /// for `willRenameFiles` edit their references, then the files move and
    /// open tabs follow.
    pub(crate) fn move_paths(
        &mut self,
        moves: Vec<(PathBuf, PathBuf)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let servers: Vec<(SharedString, Arc<RunningServer>)> = self
            .lsp_store
            .read(cx)
            .running_servers()
            .into_iter()
            .map(|(_, name, server)| (name, server))
            .collect();
        let project = self.project.clone();
        cx.spawn_in(window, async move |this, cx| {
            let checked = cx
                .background_spawn(async move {
                    moves
                        .iter()
                        .map(|(from, to)| {
                            file_ops::check_move(&project, from, to)?;
                            let is_dir = project.kind(from)? == Some(FileKind::Dir);
                            Ok((from.clone(), to.clone(), is_dir))
                        })
                        .collect::<io::Result<Vec<FileMove>>>()
                })
                .await;
            let file_moves = match checked {
                Ok(file_moves) => file_moves,
                Err(err) => {
                    this.update(cx, |this, cx| {
                        this.file_notice(format!("Could not move: {err}"), cx)
                    })
                    .ok();
                    return;
                }
            };
            let running: Vec<Arc<RunningServer>> =
                servers.iter().map(|(_, server)| server.clone()).collect();
            let mut edits: Vec<RenameEdit> = Vec::new();
            let mut problems = Vec::new();
            for (server, params) in file_ops::rename_requests(&running, &file_moves, true) {
                let name = servers
                    .iter()
                    .find(|(_, running)| Arc::ptr_eq(running, &server))
                    .map(|(name, _)| name.clone())
                    .unwrap_or_default();
                let timer = cx.background_executor().timer(WILL_RENAME_TIMEOUT);
                let request = file_ops::will_rename(&server, params);
                futures::pin_mut!(request);
                match select(request, timer).await {
                    Either::Left((Ok(Some(edit)), _)) => edits.push((name, server.encoding, edit)),
                    Either::Left((Ok(None), _)) => {}
                    Either::Left((Err(err), _)) => problems.push(format!("{name}: {err}")),
                    Either::Right(_) => problems.push(format!(
                        "{name} sent no rename edits within {} s",
                        WILL_RENAME_TIMEOUT.as_secs()
                    )),
                }
            }
            this.update_in(cx, |this, window, cx| {
                this.finish_move(file_moves, edits, problems, running, window, cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_move(
        &mut self,
        file_moves: Vec<FileMove>,
        edits: Vec<RenameEdit>,
        mut problems: Vec<String>,
        servers: Vec<Arc<RunningServer>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut updates = Vec::new();
        for (name, encoding, edit) in edits {
            let report = self.apply_workspace_edit(edit, encoding, cx);
            if report.failure.is_some() {
                problems.push(report.summary(&format!("{name}'s rename edits"), &self.project));
            } else if !report.changed.is_empty() {
                let files = match report.changed.len() {
                    1 => "1 file".to_owned(),
                    n => format!("{n} files"),
                };
                updates.push(format!("{name} updated {files}"));
            }
        }
        self.file_tree.update(cx, |tree, _| {
            tree.expect_changes(
                file_moves
                    .iter()
                    .flat_map(|(from, to, _)| [from.clone(), to.clone()]),
            )
        });
        let project = self.project.clone();
        cx.spawn_in(window, async move |this, cx| {
            let results = cx
                .background_spawn(async move {
                    file_moves
                        .into_iter()
                        .map(|file_move| {
                            let result = file_ops::move_path(&project, &file_move.0, &file_move.1);
                            (file_move, result)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            this.update_in(cx, |this, _, cx| {
                let mut done: Vec<FileMove> = Vec::new();
                for (file_move, result) in results {
                    match result {
                        Ok(()) => done.push(file_move),
                        Err(err) => problems.push(format!(
                            "Could not move {}: {err}",
                            shown_path(&this.project, &file_move.0)
                        )),
                    }
                }
                for (from, to, _) in &done {
                    this.follow_rename(from, to, cx);
                }
                let pairs: Vec<(PathBuf, PathBuf)> = done
                    .iter()
                    .map(|(from, to, _)| (from.clone(), to.clone()))
                    .collect();
                if !pairs.is_empty() {
                    this.file_tree.update(cx, |tree, cx| tree.moved(&pairs, cx));
                }
                file_ops::did_rename(&servers, &done);
                if !problems.is_empty() {
                    this.file_notice(problems.join("; "), cx);
                } else if !updates.is_empty()
                    && let Some(editor) = this.active_tab().and_then(Tab::editor).cloned()
                {
                    let message = format!("Moved {} item(s); {}", done.len(), updates.join(", "));
                    editor.update(cx, |editor, cx| editor.show_notice(message, cx));
                }
                this.schedule_session_save(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Asks before deleting, naming the items (and unsaved files among
    /// them), and whether the Recycle Bin can bring them back.
    pub(crate) fn confirm_delete(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = file_ops::top_level(&paths, file_ops::windows_names(&self.project));
        if paths.is_empty() {
            return;
        }
        let shown = |path: &Path| shown_path(&self.project, path);
        let message = match paths.as_slice() {
            [path] => format!("Delete `{}`?", shown(path)),
            _ => format!("Delete {} items?", paths.len()),
        };
        let mut lines = Vec::new();
        if paths.len() > 1 {
            lines.extend(paths.iter().take(LISTED_ITEMS).map(|path| shown(path)));
            if paths.len() > LISTED_ITEMS {
                lines.push(format!("… and {} more", paths.len() - LISTED_ITEMS));
            }
            lines.push(String::new());
        }
        let mut unsaved: Vec<String> = Vec::new();
        for tab in self.all_tabs() {
            if tab.is_dirty(cx) && paths.iter().any(|path| tab.path.starts_with(path)) {
                let name = shown(&tab.path);
                if !unsaved.contains(&name) {
                    unsaved.push(name);
                }
            }
        }
        // Short lines: Linux's drawn prompt does not wrap.
        match unsaved.as_slice() {
            [] => {}
            [name] if paths.len() == 1 && *name == shown(&paths[0]) => {
                lines.push("Its unsaved changes will be lost.".into())
            }
            names => {
                lines.push("Unsaved changes will be lost in:".into());
                lines.extend(names.iter().cloned());
            }
        }
        let trash = self.project.can_trash();
        lines.push(
            if trash {
                "You can restore it from the Recycle Bin."
            } else if self.project.is_remote() {
                "This cannot be undone (no Recycle Bin\nin WSL and dev containers)."
            } else {
                "This cannot be undone."
            }
            .to_owned(),
        );
        let detail = lines.join("\n");
        let confirm = if trash {
            "Move to Recycle Bin"
        } else {
            "Delete"
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            Some(&detail),
            &[confirm, "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                this.update_in(cx, |this, window, cx| {
                    this.delete_paths(paths, trash, window, cx)
                })
                .ok();
            }
        })
        .detach();
    }

    /// Deletes, then closes the editor tabs of whatever is gone (without
    /// asking: the confirmation named unsaved files).
    fn delete_paths(
        &mut self,
        paths: Vec<PathBuf>,
        trash: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.file_tree
            .update(cx, |tree, _| tree.expect_changes(paths.iter().cloned()));
        let project = self.project.clone();
        cx.spawn_in(window, async move |this, cx| {
            let (result, gone) = cx
                .background_spawn(async move {
                    let result = file_ops::delete(&project, &paths, trash);
                    let gone: Vec<PathBuf> = paths
                        .into_iter()
                        .filter(|path| matches!(project.kind(path), Ok(None)))
                        .collect();
                    (result, gone)
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.close_tabs_under(&gone, window, cx);
                this.file_tree
                    .update(cx, |tree, cx| tree.deleted(&gone, cx));
                window.focus(&this.file_tree.focus_handle(cx));
                if let Err(err) = result {
                    this.file_notice(format!("Could not delete: {err}"), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn close_tabs_under(&mut self, paths: &[PathBuf], window: &mut Window, cx: &mut Context<Self>) {
        while let Some((pane, index)) = self.find_tab(|tab| {
            tab.editor().is_some() && paths.iter().any(|path| tab.path.starts_with(path))
        }) {
            self.remove_tab(pane, index, window, cx);
        }
    }
}

/// How file prompts name `path`: relative to its folder, after the
/// folder's name in multi-folder projects (`api › src/main.py`).
fn shown_path(project: &crate::project::Project, path: &Path) -> String {
    if project.is_multi_root() {
        return project.display_path(path);
    }
    path.strip_prefix(project.root())
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use gpui::{TestAppContext, WindowHandle};

    use super::*;

    fn project_dir(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("nc-files-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("a.txt"), "alpha\n").unwrap();
        fs::write(root.join("src/b.txt"), "beta\n").unwrap();
        root
    }

    fn cleanup(root: &Path) {
        fs::remove_dir_all(root).ok();
        if let Some(state) = crate::layout::state_root() {
            fs::remove_dir_all(state).ok();
        }
        if let Some(recent) = recent::path() {
            fs::remove_file(recent).ok();
        }
    }

    fn open_workspace(root: &Path, cx: &mut TestAppContext) -> WindowHandle<Workspace> {
        cx.update(|cx| {
            crate::editor::bind_keys(cx);
            crate::file_tree::bind_keys(cx);
            bind_keys(cx);
        });
        let project = Project::local(root.to_path_buf());
        let window = cx.add_window(|window, cx| Workspace::new(project, None, window, cx));
        cx.run_until_parked();
        window
    }

    fn tree_event(window: WindowHandle<Workspace>, event: FileTreeEvent, cx: &mut TestAppContext) {
        window
            .update(cx, |workspace, window, cx| {
                workspace.on_file_tree_event(&event, window, cx)
            })
            .unwrap();
        cx.run_until_parked();
    }

    /// Each tab's path, title, text and whether it is dirty.
    fn tabs(
        window: WindowHandle<Workspace>,
        cx: &mut TestAppContext,
    ) -> Vec<(PathBuf, String, String, bool)> {
        window
            .read_with(cx, |workspace, cx| {
                workspace
                    .all_tabs()
                    .map(|tab| {
                        let text = tab.editor().map(|e| e.read(cx).text()).unwrap_or_default();
                        (
                            tab.path.clone(),
                            tab.title.to_string(),
                            text,
                            tab.is_dirty(cx),
                        )
                    })
                    .collect()
            })
            .unwrap()
    }

    fn notice(window: WindowHandle<Workspace>, cx: &mut TestAppContext) -> Option<String> {
        window
            .read_with(cx, |workspace, _| {
                workspace.notice.as_ref().map(|n| n.to_string())
            })
            .unwrap()
    }

    #[gpui::test]
    fn renaming_an_open_file_keeps_its_unsaved_edits_and_saves_to_the_new_name(
        cx: &mut TestAppContext,
    ) {
        let root = project_dir("rename");
        let window = open_workspace(&root, cx);
        window
            .update(cx, |workspace, window, cx| {
                workspace.open(&root.join("a.txt"), window, cx)
            })
            .unwrap();
        cx.simulate_input(window.into(), "x");
        cx.run_until_parked();

        let renamed = root.join("renamed.txt");
        tree_event(
            window,
            FileTreeEvent::Move(vec![(root.join("a.txt"), renamed.clone())]),
            cx,
        );
        assert_eq!(
            tabs(window, cx),
            [(
                renamed.clone(),
                "renamed.txt".into(),
                "xalpha\n".into(),
                true
            )]
        );
        assert!(!root.join("a.txt").exists());
        assert_eq!(fs::read_to_string(&renamed).unwrap(), "alpha\n");

        cx.simulate_keystrokes(window.into(), "ctrl-s");
        cx.run_until_parked();
        assert_eq!(fs::read_to_string(&renamed).unwrap(), "xalpha\n");
        assert!(!root.join("a.txt").exists());
        cleanup(&root);
    }

    #[gpui::test]
    fn moving_a_folder_moves_the_tabs_of_its_files(cx: &mut TestAppContext) {
        let root = project_dir("move");
        fs::create_dir_all(root.join("lib")).unwrap();
        let window = open_workspace(&root, cx);
        window
            .update(cx, |workspace, window, cx| {
                workspace.open(&root.join("src/b.txt"), window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        let moved = root.join("lib").join("src");
        tree_event(
            window,
            FileTreeEvent::Move(vec![(root.join("src"), moved.clone())]),
            cx,
        );
        assert_eq!(
            tabs(window, cx),
            [(moved.join("b.txt"), "b.txt".into(), "beta\n".into(), false)]
        );
        assert!(moved.join("b.txt").is_file());

        // A taken target moves nothing and says why.
        fs::write(root.join("taken.txt"), "").unwrap();
        tree_event(
            window,
            FileTreeEvent::Move(vec![(root.join("a.txt"), root.join("taken.txt"))]),
            cx,
        );
        assert_eq!(
            notice(window, cx).as_deref(),
            Some(
                format!(
                    "Could not move: `taken.txt` already exists in {}",
                    root.display()
                )
                .as_str()
            )
        );
        assert!(root.join("a.txt").is_file());
        cleanup(&root);
    }

    #[gpui::test]
    fn deleting_asks_first_and_closes_the_files_tabs(cx: &mut TestAppContext) {
        let root = project_dir("delete");
        let window = open_workspace(&root, cx);
        window
            .update(cx, |workspace, window, cx| {
                workspace.open(&root.join("src/b.txt"), window, cx)
            })
            .unwrap();
        cx.simulate_input(window.into(), "edit ");
        cx.run_until_parked();

        tree_event(window, FileTreeEvent::Delete(vec![root.join("src")]), cx);
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(root.join("src/b.txt").is_file());
        assert_eq!(tabs(window, cx).len(), 1);

        tree_event(window, FileTreeEvent::Delete(vec![root.join("src")]), cx);
        // Local Windows projects go to the Recycle Bin.
        cx.simulate_prompt_answer(if cfg!(windows) {
            "Move to Recycle Bin"
        } else {
            "Delete"
        });
        cx.run_until_parked();
        assert!(!root.join("src").exists());
        assert!(tabs(window, cx).is_empty());
        cleanup(&root);
    }
}
