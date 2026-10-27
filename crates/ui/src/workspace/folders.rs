//! The project's folders: File > Add Folder to Workspace…, the Files
//! tree's Remove Folder from Workspace, and File > Save Workspace As….
//!
//! A project's folders are fixed for its life (`Project::roots`): changing
//! them saves the session for the new set and reopens the project in the
//! same tab (`WorkspaceEvent::Reopen`), as VS Code reloads its window when
//! a folder becomes a workspace.

use std::path::{Path, PathBuf};

use gpui::{Context, PathPromptOptions, Window, prelude::*};

use super::{AddFolderToWorkspace, SaveWorkspaceAs, Workspace, WorkspaceEvent};
use crate::{
    git_dialogs::InputDialog,
    project::Location,
    workspace_file::{self, Folder},
};

impl Workspace {
    /// Reads each folder's `.next_code/settings.json` (layered over the
    /// workspace file's and the user's settings) on a background thread,
    /// so editors find them cached; problems show as a notice.
    pub(super) fn load_folder_settings(&mut self, cx: &mut Context<Self>) {
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            let errors: Vec<String> = cx
                .background_spawn(async move {
                    project
                        .roots()
                        .iter()
                        .filter_map(|root| project.layered_settings(Some(&root.path)).1)
                        .collect()
                })
                .await;
            this.update(cx, |this, cx| {
                if !errors.is_empty() {
                    this.notice = Some(format!("Settings ignored: {}", errors.join("; ")).into());
                }
                // Editors re-read wrap and hints for their folder.
                cx.refresh_windows();
            })
            .ok();
        })
        .detach();
    }

    /// File > Add Folder to Workspace…: the system folder dialog for local
    /// projects (where there is one), else a typed path.
    pub(super) fn add_folder_to_workspace(
        &mut self,
        _: &AddFolderToWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(error) = self.project.with_folders(self.project.folders()) {
            self.notice = Some(error.into());
            return cx.notify();
        }
        if self.project.is_remote() {
            return self.typed_folder_dialog(window, cx);
        }
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("Add Folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let chosen = paths.await;
            this.update_in(cx, |this, window, cx| match chosen {
                Ok(Ok(Some(paths))) => {
                    let paths = paths
                        .iter()
                        .map(|path| path.to_string_lossy().into_owned())
                        .collect();
                    this.add_folders(paths, cx);
                }
                Ok(Ok(None)) => {}
                // No dialog here (e.g. Linux without a desktop portal).
                _ => this.typed_folder_dialog(window, cx),
            })
            .ok();
        })
        .detach();
    }

    /// Asks for a folder path (in the project's path syntax).
    fn typed_folder_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The first folder's parent, ready for a sibling's name.
        let separator = if self.project.location().posix() {
            "/"
        } else {
            "\\"
        };
        let start = self
            .project
            .root()
            .parent()
            .map_or(String::new(), |parent| {
                let parent = parent.to_string_lossy();
                format!("{}{separator}", parent.trim_end_matches(['/', '\\']))
            });
        let dialog = cx.new(|cx| {
            InputDialog::new(
                "Add Folder to Workspace (path)",
                &start,
                false,
                "Add",
                None,
                window,
                cx,
            )
        });
        self.open_input_dialog(
            dialog,
            |this, path, _, _, cx| this.add_folders(vec![path.trim().to_owned()], cx),
            window,
            cx,
        );
    }

    /// Adds folders (paths on the project's machine) and reopens; folders
    /// already in the project are skipped, missing ones refused.
    pub(crate) fn add_folders(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        let mut folders = self.project.folders();
        for path in paths {
            let path = match self.folder_path(&path) {
                Ok(path) => path,
                Err(error) => {
                    self.notice = Some(error.into());
                    return cx.notify();
                }
            };
            if !matches!(self.project.kind(&path), Ok(Some(worktree::FileKind::Dir))) {
                self.notice = Some(format!("{} is not a folder", path.display()).into());
                return cx.notify();
            }
            let path = path.to_string_lossy().into_owned();
            if !folders.iter().any(|folder| folder.path == path) {
                folders.push(Folder { path, name: None });
            }
        }
        self.set_folders(folders, cx);
    }

    /// A chosen folder as a path of this project's machine: WSL folders
    /// only join WSL projects of the same distro, local ones local ones.
    fn folder_path(&self, input: &str) -> Result<PathBuf, String> {
        let elsewhere = || {
            Err(format!(
                "{input} is not on the same machine as this project: a workspace's \
                 folders are all local or all in one WSL distro"
            ))
        };
        match (Location::parse(input), self.project.location()) {
            (Some(Location::Local(path)), _) if !self.project.is_remote() => {
                Ok(std::path::absolute(&path).unwrap_or(path))
            }
            (
                Some(Location::Wsl { distro, path }),
                Location::Wsl { distro: here, .. }
                | Location::Workspace {
                    distro: Some(here), ..
                },
            ) if distro == here => Ok(PathBuf::from(path)),
            // A typed POSIX path in a WSL project.
            (Some(Location::Local(path)), _)
                if self.project.is_remote() && input.starts_with('/') =>
            {
                Ok(path)
            }
            _ => elsewhere(),
        }
    }

    /// The Files tree's Remove Folder from Workspace.
    pub(crate) fn remove_folder(&mut self, index: usize, cx: &mut Context<Self>) {
        let mut folders = self.project.folders();
        if index >= folders.len() {
            return;
        }
        if folders.len() == 1 {
            self.notice = Some("The last folder of a workspace cannot be removed".into());
            return cx.notify();
        }
        folders.remove(index);
        self.set_folders(folders, cx);
    }

    /// Reopens the project with `folders`, rewriting its workspace file
    /// (next_code's own; a VS Code file is left alone and the workspace
    /// becomes untitled). Refused while files have unsaved edits.
    pub(crate) fn set_folders(&mut self, folders: Vec<Folder>, cx: &mut Context<Self>) {
        let mut location = match self.project.with_folders(folders) {
            Ok(location) => location,
            Err(error) => {
                self.notice = Some(error.into());
                return cx.notify();
            }
        };
        let dirty = self.dirty_count(cx);
        if dirty > 0 {
            self.notice = Some(
                format!(
                    "Save or close the {dirty} file(s) with unsaved changes first: \
                     changing the workspace folders reopens the project"
                )
                .into(),
            );
            return cx.notify();
        }
        if let Location::Workspace {
            file: file @ Some(_),
            folders,
            ..
        } = &mut location
        {
            let path = file.clone().unwrap_or_default();
            match workspace_file::Format::of(&path) {
                Some(workspace_file::Format::NextCode) => {
                    let text = workspace_file::to_text(
                        folders,
                        self.project.workspace_settings(),
                        &path,
                        self.project.location().posix(),
                    );
                    if let Err(err) = self.project.write(Path::new(&path), text.into_bytes()) {
                        self.notice = Some(format!("Could not save {path}: {err}").into());
                        return cx.notify();
                    }
                }
                _ => *file = None,
            }
        }
        self.reopen_as(location, cx);
    }

    /// Saves the session and layout for `location`, then asks the window
    /// to open it in this tab.
    fn reopen_as(&mut self, location: Location, cx: &mut Context<Self>) {
        let key = location
            .workspace_state_key()
            .unwrap_or_else(|| match &location {
                Location::Wsl { distro, path } => format!("wsl:{distro}:{path}"),
                Location::Local(path) => path.to_string_lossy().into_owned(),
                other => other.to_string(),
            });
        if self.restore_session {
            self.session(cx).save(&key).ok();
        }
        self.layout.save_for_project(&key).ok();
        cx.emit(WorkspaceEvent::Reopen(location));
    }

    /// File > Save Workspace As…: writes a `*.next_code-workspace` file
    /// (folders relative to it where they share a folder) and reopens the
    /// project as that workspace.
    pub(super) fn save_workspace_as(
        &mut self,
        _: &SaveWorkspaceAs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.project.in_container() {
            self.notice = Some(
                "A dev container project has a single folder and cannot be saved as a workspace"
                    .into(),
            );
            return cx.notify();
        }
        let directory = self.project.root().to_path_buf();
        let suggested = format!(
            "{}.{}",
            self.project.roots()[0].name,
            workspace_file::EXTENSION
        );
        if self.project.is_remote() {
            return self.typed_save_dialog(&directory, &suggested, window, cx);
        }
        let chosen = cx.prompt_for_new_path(&directory, Some(&suggested));
        cx.spawn_in(window, async move |this, cx| {
            let chosen = chosen.await;
            this.update_in(cx, |this, window, cx| match chosen {
                Ok(Ok(Some(path))) => {
                    this.write_workspace_file(&path.to_string_lossy(), cx);
                }
                Ok(Ok(None)) => {}
                _ => this.typed_save_dialog(&directory, &suggested, window, cx),
            })
            .ok();
        })
        .detach();
    }

    fn typed_save_dialog(
        &mut self,
        directory: &Path,
        suggested: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let initial = self.project.join(directory, suggested);
        let dialog = cx.new(|cx| {
            InputDialog::new(
                "Save Workspace As (path)",
                &initial.to_string_lossy(),
                false,
                "Save",
                None,
                window,
                cx,
            )
        });
        self.open_input_dialog(
            dialog,
            |this, path, _, _, cx| this.write_workspace_file(path.trim(), cx),
            window,
            cx,
        );
    }

    /// Writes the workspace file at `path` (the extension added when
    /// missing) and reopens the project from it.
    pub(crate) fn write_workspace_file(&mut self, path: &str, cx: &mut Context<Self>) {
        let mut path = path.to_owned();
        if workspace_file::Format::of(&path) != Some(workspace_file::Format::NextCode) {
            path = format!("{path}.{}", workspace_file::EXTENSION);
        }
        let dirty = self.dirty_count(cx);
        if dirty > 0 {
            self.notice = Some(
                format!(
                    "Save or close the {dirty} file(s) with unsaved changes first: \
                     saving the workspace reopens the project from its file"
                )
                .into(),
            );
            return cx.notify();
        }
        let text = self.project.workspace_text(&path);
        if let Err(err) = self.project.write(Path::new(&path), text.into_bytes()) {
            self.notice = Some(format!("Could not save {path}: {err}").into());
            return cx.notify();
        }
        let distro = match self.project.location() {
            Location::Wsl { distro, .. } => Some(distro),
            Location::Workspace { distro, .. } => distro,
            _ => None,
        };
        self.reopen_as(
            Location::Workspace {
                distro,
                file: Some(path),
                folders: self.project.folders(),
            },
            cx,
        );
    }
}
