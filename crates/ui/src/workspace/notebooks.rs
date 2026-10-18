//! Notebook tabs (`.ipynb`, marimo `.py`): opening files as notebooks
//! (views in several panes share one `NotebookDocument` and its
//! language-server document), Open as Text /
//! Open as Notebook for marimo files, reloading after external changes,
//! Open in marimo and Install ipykernel in the Terminal panel, going to
//! language-server locations inside notebooks, and
//! stopping kernels with their last tab or the app.

use gpui::EntityId;

use super::*;
use crate::notebook::{NotebookDocument, launch, lsp::NotebookLsp};

impl Workspace {
    #[cfg(test)]
    pub(crate) fn active_notebook(&self) -> Option<Entity<NotebookView>> {
        self.active_tab().and_then(Tab::notebook).cloned()
    }

    /// Whether a file read as `text` opens in a notebook tab.
    pub(super) fn opens_as_notebook(&self, path: &Path, text: &str) -> bool {
        !self.text_notebooks.contains(path)
            && path.file_name().is_some_and(|name| {
                notebook::kind_of(&name.to_string_lossy(), Some(text)).is_some()
            })
    }

    /// Whether `path` has a notebook tab (where it opens again).
    pub(super) fn has_notebook_tab(&self, path: &Path) -> bool {
        !self.text_notebooks.contains(path)
            && self
                .all_tabs()
                .any(|tab| tab.path == path && tab.notebook().is_some())
    }

    /// Opens (or activates) `path` as a notebook in the active pane: a
    /// file open as a notebook in another pane gets a second view of the
    /// same document. `text` is the file's contents when already read.
    pub(super) fn open_notebook(
        &mut self,
        path: &Path,
        text: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Entity<NotebookView>, String> {
        let pane = self.active_pane;
        if let Some(index) = self.active_pane().notebook_index(path) {
            self.activate(pane, index, window, cx);
            return self
                .active_tab()
                .and_then(Tab::notebook)
                .cloned()
                .ok_or_else(|| "no notebook tab".to_owned());
        }
        let existing = self
            .all_tabs()
            .filter(|tab| tab.path == path)
            .find_map(Tab::notebook)
            .map(|view| {
                let view = view.read(cx);
                (view.document().clone(), view.language_server().cloned())
            });
        let (existing, language_server) = existing.unzip();
        let document = match existing {
            Some(document) => document,
            None => {
                let read;
                let text = match text {
                    Some(text) => text,
                    None => {
                        read = self
                            .project
                            .read_text(path)
                            .map_err(|err| format!("Cannot open {}: {err}", path.display()))?;
                        &read
                    }
                };
                NotebookDocument::create(self.project.clone(), path.to_path_buf(), text, cx)
                    .map_err(|err| format!("Cannot open {} as a notebook: {err}", path.display()))?
            }
        };
        let language_server = language_server.flatten().unwrap_or_else(|| {
            let store = self.lsp_store.clone();
            let document = document.clone();
            cx.new(|cx| NotebookLsp::new(document, store, cx))
        });
        let notes = self
            .project
            .relative(path)
            .map(|(root, file)| (self.note_store.clone(), root, file));
        let view = cx.new(|cx| {
            let mut view = NotebookView::new(document, window, cx);
            view.set_language_server(language_server, cx);
            if let Some((store, root, file)) = notes {
                view.set_note_store(store, root, file, cx);
            }
            view
        });
        let subscription = cx.subscribe_in(&view, window, Self::on_notebook_event);
        let title = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into();
        self.push_tab(
            TabContent::Notebook(view.clone()),
            path.to_path_buf(),
            title,
            subscription,
            window,
            cx,
        );
        Ok(view)
    }

    fn on_notebook_event(
        &mut self,
        view: &Entity<NotebookView>,
        event: &NotebookViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            NotebookViewEvent::Changed => cx.notify(),
            NotebookViewEvent::OpenAsText => self.notebook_as_text(view, window, cx),
            NotebookViewEvent::OpenInMarimo => {
                self.show_panel(PanelId::Terminal, cx);
                let ports = self.ports_panel.read(cx).ports().clone();
                launch::open_in_marimo(view.clone(), self.terminal.clone(), ports, window, cx);
            }
            NotebookViewEvent::InstallKernel => {
                self.show_panel(PanelId::Terminal, cx);
                launch::install_kernel(view.clone(), self.terminal.clone(), window, cx);
            }
            NotebookViewEvent::Close { discard } => {
                if let Some((pane, index)) = self.find_view(view.entity_id()) {
                    if *discard {
                        self.remove_tab(pane, index, window, cx);
                    } else {
                        self.close_tab(pane, index, window, cx);
                    }
                }
            }
            NotebookViewEvent::Navigate { locations, title } => {
                self.navigate(locations.clone(), title.clone(), window, cx)
            }
            NotebookViewEvent::ApplyEdit {
                edit,
                server,
                command,
                label,
            } => self.apply_editor_edit(
                None,
                edit.clone(),
                server.clone(),
                command.clone(),
                label,
                cx,
            ),
        }
    }

    /// A language-server location in a notebook's virtual document: opens
    /// the notebook at that cell. False when `location` is elsewhere.
    pub(super) fn jump_to_notebook(
        &mut self,
        location: &Location,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(document) = self.lsp_store.read(cx).notebook(&location.path) else {
            return false;
        };
        let (path, target) = {
            let document = document.borrow();
            (
                document.notebook_path().to_path_buf(),
                document.cell_position(location.range.start),
            )
        };
        match self.open_notebook(&path, None, window, cx) {
            Ok(view) => {
                if let Some(target) = target {
                    view.update(cx, |view, cx| {
                        view.go_to_cell(target.cell, target.position, window, cx)
                    });
                }
            }
            Err(err) => {
                self.notice = Some(err.into());
                cx.notify();
            }
        }
        true
    }

    /// Open as Text (marimo): a text editor replaces the file's notebook
    /// tabs (its kernel stops); the file opens as text for the rest of the
    /// session, or until Open as Notebook.
    fn notebook_as_text(
        &mut self,
        view: &Entity<NotebookView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = view.read(cx).path(cx);
        if view.read(cx).is_dirty(cx) {
            self.notice = Some(format!("Save {} before opening it as text", path.display()).into());
            return cx.notify();
        }
        let Some((pane, index)) = self.find_view(view.entity_id()) else {
            return;
        };
        let views: Vec<EntityId> = self
            .all_tabs()
            .filter(|tab| tab.path == path && tab.notebook().is_some())
            .map(Tab::entity_id)
            .collect();
        self.text_notebooks.insert(path.clone());
        self.active_pane = pane;
        let Some(editor) = self.open_editor(&path, window, cx) else {
            return;
        };
        for id in views {
            if let Some((pane, index)) = self.find_view(id) {
                self.remove_tab(pane, index, window, cx);
            }
        }
        self.put_tab_at(editor.entity_id(), pane, index, window, cx);
    }

    /// Moves the tab showing `view` to `index` of `pane` (where the tab it
    /// replaces was) and activates it.
    fn put_tab_at(
        &mut self,
        view: EntityId,
        pane: PaneId,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((at_pane, at)) = self.find_view(view) else {
            return;
        };
        if at_pane == pane
            && let Some(target) = self.pane_mut(pane)
        {
            let index = index.min(target.tabs.len() - 1);
            let tab = target.tabs.remove(at);
            target.tabs.insert(index, tab);
            self.activate(pane, index, window, cx);
        } else {
            self.activate(at_pane, at, window, cx);
        }
    }

    /// Open as Notebook: the file's text editors (without unsaved edits)
    /// make way for a notebook tab.
    pub(super) fn reopen_as_notebook(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editors: Vec<(EntityId, bool)> = self
            .all_tabs()
            .filter(|tab| tab.path == path && tab.editor().is_some())
            .map(|tab| (tab.entity_id(), tab.is_dirty(cx)))
            .collect();
        if editors.iter().any(|(_, dirty)| *dirty) {
            self.notice =
                Some(format!("Save {} before opening it as a notebook", path.display()).into());
            return cx.notify();
        }
        let place = self
            .active_tab()
            .filter(|tab| tab.path == path)
            .map(|_| (self.active_pane, self.active_pane().active))
            .or_else(|| editors.first().and_then(|(id, _)| self.find_view(*id)));
        self.text_notebooks.remove(path);
        if let Some((pane, _)) = place {
            self.active_pane = pane;
        }
        let view = match self.open_notebook(path, None, window, cx) {
            Ok(view) => view,
            Err(err) => {
                self.text_notebooks.insert(path.to_path_buf());
                self.notice = Some(err.into());
                return cx.notify();
            }
        };
        for (id, _) in editors {
            if let Some((pane, index)) = self.find_view(id) {
                self.remove_tab(pane, index, window, cx);
            }
        }
        if let Some((pane, index)) = place {
            self.put_tab_at(view.entity_id(), pane, index, window, cx);
        }
    }

    /// Open as Notebook from the command palette: the active text editor's
    /// file, if it is a notebook.
    pub(super) fn open_as_notebook(
        &mut self,
        _: &OpenAsNotebook,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_active_pane(window, cx);
        let Some(tab) = self.active_tab() else {
            return;
        };
        let Some(editor) = tab.editor() else {
            if tab.notebook().is_none() {
                self.notice = Some("Open as Notebook works on a file's editor".into());
                cx.notify();
            }
            return;
        };
        let path = tab.path.clone();
        let text = editor.read(cx).text();
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if notebook::kind_of(&name, Some(&text)).is_none() {
            self.notice = Some(format!("{name} is not a Jupyter or marimo notebook").into());
            return cx.notify();
        }
        self.reopen_as_notebook(&path, window, cx);
    }

    /// The Open as Notebook offer follows the active tab: shown for a text
    /// editor of a notebook the user opened as text.
    pub(super) fn offer_notebook_view(
        &mut self,
        pane: PaneId,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let path = self
            .pane(pane)
            .and_then(|pane| pane.tabs.get(index))
            .filter(|tab| tab.editor().is_some() && self.text_notebooks.contains(&tab.path))
            .map(|tab| tab.path.clone());
        let before = self.offers.len();
        self.offers.retain(|offer| match (&offer.kind, &path) {
            (OfferKind::OpenAsNotebook { path: shown }, Some(path)) => shown == path,
            (OfferKind::OpenAsNotebook { .. }, None) => false,
            _ => true,
        });
        if self.offers.len() != before {
            cx.notify();
        }
        if let Some(path) = path {
            let kind = OfferKind::OpenAsNotebook { path };
            if !self.offers.iter().any(|offer| offer.kind == kind) {
                self.offer(kind, "This marimo notebook is open as text.", cx);
            }
        }
    }

    /// Reloads notebooks changed on disk: clean ones at once (our own save
    /// is no change), dirty ones after asking.
    pub(super) fn notebooks_changed_on_disk(
        &mut self,
        batch: &Batch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut seen = HashSet::new();
        let affected: Vec<(Entity<NotebookView>, PathBuf, bool)> = self
            .all_tabs()
            .filter(|tab| tab.notebook().is_some() && seen.insert(tab.path.clone()))
            .filter_map(|tab| {
                let view = tab.notebook()?.clone();
                let removed = match batch.events.iter().find(|event| event.path == tab.path) {
                    Some(event) => event.kind == ChangeKind::Removed,
                    None if batch.rescan => matches!(self.project.kind(&tab.path), Ok(None)),
                    None => return None,
                };
                Some((view, tab.path.clone(), removed))
            })
            .collect();
        for (view, path, removed) in affected {
            if self.file_tree.read(cx).is_own_change(&path) {
                continue;
            }
            if removed {
                self.notice = Some(format!("{} was deleted on disk", path.display()).into());
                cx.notify();
                continue;
            }
            let Ok(text) = self.project.read_text(&path) else {
                continue;
            };
            let document = view.read(cx).document().clone();
            if !document.read(cx).changed_on_disk(&text) {
                continue;
            }
            if !document.read(cx).is_dirty() {
                if let Err(err) = document.update(cx, |document, cx| document.reload(&text, cx)) {
                    self.notice =
                        Some(format!("Could not reload {}: {err}", path.display()).into());
                    cx.notify();
                }
            } else if self.reload_prompts.insert(path.clone()) {
                self.prompt_notebook_reload(document, path, window, cx);
            }
        }
    }

    fn prompt_notebook_reload(
        &mut self,
        document: Entity<NotebookDocument>,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("{name} changed on disk"),
            Some("You have unsaved changes. Reload the notebook from disk and lose them?"),
            &["Keep My Changes", "Reload"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let reload = answer.await == Ok(1);
            this.update_in(cx, |this, _, cx| {
                this.reload_prompts.remove(&path);
                if !reload {
                    return;
                }
                let result = this
                    .project
                    .read_text(&path)
                    .map_err(|err| err.to_string())
                    .and_then(|text| {
                        document.update(cx, |document, cx| document.reload(&text, cx))
                    });
                if let Err(err) = result {
                    this.notice =
                        Some(format!("Could not reload {}: {err}", path.display()).into());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Stops every open notebook's kernel (the app quits, the project
    /// closes).
    pub(super) fn shutdown_notebooks(&mut self, cx: &mut App) {
        let documents: Vec<Entity<NotebookDocument>> = self
            .all_tabs()
            .filter_map(Tab::notebook)
            .map(|view| view.read(cx).document().clone())
            .collect();
        for document in documents {
            document.update(cx, |document, cx| document.shutdown(cx));
        }
    }

    /// One tab per open file: each file's language-server view, each
    /// notebook's first view.
    pub(super) fn file_tabs(&self) -> Vec<&Tab> {
        let mut notebooks = HashSet::new();
        self.all_tabs()
            .filter(|tab| match &tab.content {
                TabContent::Editor(_) => tab.lsp_owner,
                TabContent::Notebook(_) => notebooks.insert(tab.path.as_path()),
                TabContent::Diff(_) | TabContent::Compare(_) => false,
            })
            .collect()
    }
}
