//! Pinpoint: showing the code an agent highlights (`reveal`).

use std::path::{Path, PathBuf};

use gpui::{Context, Window};

use super::{Tab, Workspace};
use crate::{layout::PanelId, pinpoint_text::CodeSpan};

impl Workspace {
    /// Shows `file` (opening it in the active pane when no pane has it)
    /// scrolled so `span` sits in the middle of the view; in a notebook,
    /// scrolled to cell `cell`. Keyboard focus and selections stay where
    /// they are: the user may be typing elsewhere.
    pub(crate) fn pinpoint_reveal(
        &mut self,
        file: &str,
        cell: Option<usize>,
        span: CodeSpan,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let path = self.pinpoint_path(file)?;
        let focused = window.focused(cx);
        let result = self.pinpoint_show(&path, cell, span, window, cx);
        if let Some(focused) = focused {
            window.focus(&focused);
        }
        result
    }

    fn pinpoint_show(
        &mut self,
        path: &Path,
        cell: Option<usize>,
        span: CodeSpan,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        // The file tab (a notebook) or editor of a tab showing `path`.
        let shown = |this: &Self| {
            this.panes
                .iter()
                .find_map(|pane| pane.active_tab().filter(|tab| tab.path == path))
                .map(|tab| (tab.file_tab().cloned(), tab.editor().cloned()))
        };
        let (file, editor) = match shown(self) {
            Some(views) => views,
            None => {
                let open = self.panes.iter().find_map(|pane| {
                    pane.tabs
                        .iter()
                        .position(|tab| tab.path == path && pinpoint_view(tab))
                        .map(|index| (pane.id, index))
                });
                match open {
                    Some((pane, index)) => self.activate(pane, index, window, cx),
                    None => {
                        let before = self.notice.clone();
                        self.open_editor(path, window, cx);
                        if self.notice != before
                            && let Some(notice) = &self.notice
                        {
                            return Err(notice.to_string());
                        }
                    }
                }
                shown(self).ok_or_else(|| format!("{} did not open", path.display()))?
            }
        };
        if let Some(file) = file {
            if let Some(cell) = cell
                && file.reveal_part(cell, cx) != Some(true)
            {
                return Err(format!("{} has no cell {cell}", path.display()));
            }
            return Ok(());
        }
        if cell.is_some() {
            return Err(format!("{} is not open as a notebook", path.display()));
        }
        let editor =
            editor.ok_or_else(|| format!("{} is not shown in an editor", path.display()))?;
        editor.update(cx, |editor, cx| editor.pinpoint_reveal(span, cx));
        Ok(())
    }

    /// Shows the Terminal panel with terminal `index` (1-based; `None`:
    /// its active one), scrolled to the lowest line holding `text`.
    pub(crate) fn pinpoint_reveal_terminal(
        &mut self,
        index: Option<usize>,
        text: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.show_panel(PanelId::Terminal, cx);
        self.terminal
            .update(cx, |panel, cx| panel.pinpoint_reveal(index, text, cx))
    }

    /// `file` as an agent names it: absolute, relative to a project
    /// folder, or `<folder name>/<relative>` in a multi-root project.
    fn pinpoint_path(&self, file: &str) -> Result<PathBuf, String> {
        let normalized = file.replace('\\', "/");
        let path = Path::new(file);
        if path.is_absolute() || normalized.starts_with('/') {
            return Ok(path.to_path_buf());
        }
        let relative = normalized.trim_start_matches("./");
        // Already open: its tab is the file meant.
        let open = self.all_tabs().find(|tab| {
            let tab = tab.path.to_string_lossy().replace('\\', "/");
            tab.ends_with(&format!("/{relative}"))
        });
        if let Some(tab) = open {
            return Ok(tab.path.clone());
        }
        let project = &self.project;
        let mut candidates = Vec::new();
        if let Some((root, rest)) = relative.split_once('/')
            && let Some(index) = project.root_named(root)
        {
            candidates.push(project.resolve_in(index, rest));
        }
        candidates
            .extend((0..project.roots().len()).map(|index| project.resolve_in(index, relative)));
        candidates
            .iter()
            .find(|path| matches!(project.kind(path), Ok(Some(_))))
            .cloned()
            .ok_or_else(|| format!("{file} is not in the project"))
    }
}

/// Tabs pinpoint can scroll: editors and file tabs (notebooks; not
/// diffs).
fn pinpoint_view(tab: &Tab) -> bool {
    tab.editor().is_some() || tab.file_tab().is_some()
}
