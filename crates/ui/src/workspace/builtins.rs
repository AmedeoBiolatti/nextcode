//! What built-in extensions (`crate::builtins`) use of the workspace,
//! besides `open`, `project`, `report`, `offer`, `withdraw_offer`,
//! `show_panel`, `toggle_panel`, `terminal_panel`, `run_in_terminal`,
//! `show_tests_and_run`, `navigate` and `apply_editor_edit`; and the
//! workspace's side of them: their models, panels, offers, tabs and hooks.

use gpui::{AnyEntity, EntityId};

use super::*;
use crate::builtins::{FileTabKind, PanelView};

/// The built-in extensions that are on in this workspace (it follows the
/// shared choice in `sync_builtins`), their models and their panels.
#[derive(Default)]
pub(super) struct Builtins {
    on: Vec<&'static str>,
    models: Vec<(&'static str, AnyEntity)>,
    panels: Vec<BuiltinPanel>,
}

struct BuiltinPanel {
    extension: &'static str,
    key: &'static str,
    view: PanelView,
}

impl Workspace {
    /// The active tab's file and editor, when it is a text editor.
    pub(crate) fn active_file_editor(&self) -> Option<(PathBuf, Entity<Editor>)> {
        let tab = self.active_tab()?;
        Some((tab.path.clone(), tab.editor()?.clone()))
    }

    /// The active tab's view when it is a built-in extension's `V`.
    pub(crate) fn active_builtin_view<V: 'static>(&self) -> Option<Entity<V>> {
        match &self.active_tab()?.content {
            TabContent::Builtin(tab) => tab.downcast(),
            _ => None,
        }
    }

    /// The views of built-in extensions' tabs that are `V`s, with their
    /// files, in pane order.
    pub(crate) fn builtin_views<V: 'static>(&self) -> Vec<(PathBuf, Entity<V>)> {
        self.all_tabs()
            .filter_map(|tab| match &tab.content {
                TabContent::Builtin(builtin) => Some((tab.path.clone(), builtin.downcast()?)),
                _ => None,
            })
            .collect()
    }

    /// The workspace's model of a built-in extension that is on
    /// (`Builtin::workspace_model`), when it is a `T`.
    pub(crate) fn builtin_model<T: 'static>(&self) -> Option<Entity<T>> {
        self.builtins
            .models
            .iter()
            .find_map(|(_, model)| model.clone().downcast().ok())
    }

    /// Whether a tab about `path` is titled `title`.
    pub(crate) fn has_tab(&self, path: &Path, title: &str) -> bool {
        self.all_tabs()
            .any(|tab| tab.path == path && tab.title.as_ref() == title)
    }

    /// Shows `tab` in the active pane; `path` is the file it is about.
    /// Built-in tabs are not restored with the session.
    pub(crate) fn open_builtin_tab(
        &mut self,
        tab: BuiltinTab,
        path: PathBuf,
        title: SharedString,
        subscription: Subscription,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.push_tab(
            TabContent::Builtin(tab),
            path,
            title,
            subscription,
            window,
            cx,
        );
    }

    /// Shows the `kind` tab of `path` in the active pane, when it has one.
    pub(crate) fn activate_file_tab(
        &mut self,
        path: &Path,
        kind: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<BuiltinTab> {
        let pane = self.active_pane;
        let index = self.active_pane().file_tab_index(path, kind)?;
        self.activate(pane, index, window, cx);
        self.active_tab()?.file_tab().cloned()
    }

    /// Shows `tab` (a `Builtin::file_tab` of `path`) in the active pane,
    /// restored with the session; the built-in extensions that are on get
    /// its view (`attach`).
    pub(crate) fn open_file_tab(
        &mut self,
        tab: BuiltinTab,
        path: &Path,
        subscription: Subscription,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.attach_builtins(FileView::Builtin(tab.clone()), cx);
        let title = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into();
        self.push_tab(
            TabContent::Builtin(tab),
            path.to_path_buf(),
            title,
            subscription,
            window,
            cx,
        );
    }

    /// Closes the tab showing `view`: at once with `discard`, else asking
    /// about unsaved changes.
    pub(crate) fn close_view(
        &mut self,
        view: EntityId,
        discard: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((pane, index)) = self.find_view(view) {
            if discard {
                self.remove_tab(pane, index, window, cx);
            } else {
                self.close_tab(pane, index, window, cx);
            }
        }
    }

    /// Whether `path`'s text editors have unsaved changes.
    pub(crate) fn editors_dirty(&self, path: &Path, cx: &App) -> bool {
        self.all_tabs()
            .any(|tab| tab.path == path && tab.editor().is_some() && tab.is_dirty(cx))
    }

    /// The file tabs of the file `view` shows (all panes) make way for a
    /// text editor in `view`'s place. The extension showing them must not
    /// claim the file any more (`FileTabKind::opens`, or it is off).
    pub(crate) fn reopen_as_text(
        &mut self,
        view: EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((pane, index)) = self.find_view(view) else {
            return;
        };
        let Some(path) = self
            .pane(pane)
            .and_then(|shown| shown.tabs.get(index))
            .map(|tab| tab.path.clone())
        else {
            return;
        };
        let views: Vec<EntityId> = self
            .all_tabs()
            .filter(|tab| tab.path == path && tab.file_tab().is_some())
            .map(Tab::entity_id)
            .collect();
        self.active_pane = pane;
        let editor = self.open_editor(&path, window, cx);
        for id in views {
            if let Some((pane, index)) = self.find_view(id) {
                self.remove_tab(pane, index, window, cx);
            }
        }
        if let Some(editor) = editor {
            self.put_tab_at(editor.entity_id(), pane, index, window, cx);
        }
    }

    /// The text editors of `path` make way for a `kind` tab, in the place
    /// of the active one or else the first. They must have no unsaved
    /// changes (`editors_dirty`).
    pub(crate) fn reopen_in_file_tab(
        &mut self,
        path: &Path,
        kind: &FileTabKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let editors: Vec<EntityId> = self
            .all_tabs()
            .filter(|tab| tab.path == path && tab.editor().is_some())
            .map(Tab::entity_id)
            .collect();
        let place = self
            .active_tab()
            .filter(|tab| tab.path == path)
            .map(|_| (self.active_pane, self.active_pane().active))
            .or_else(|| editors.first().and_then(|id| self.find_view(*id)));
        if let Some((pane, _)) = place {
            self.active_pane = pane;
        }
        (kind.open)(self, path, None, window, cx)?;
        let opened = self.active_tab().map(Tab::entity_id);
        for id in editors {
            if let Some((pane, index)) = self.find_view(id) {
                self.remove_tab(pane, index, window, cx);
            }
        }
        if let (Some((pane, index)), Some(opened)) = (place, opened) {
            self.put_tab_at(opened, pane, index, window, cx);
        }
        Ok(())
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

    /// Whether the Files panel just wrote `path` itself (no change from
    /// outside).
    pub(crate) fn is_own_change(&self, path: &Path, cx: &App) -> bool {
        self.file_tree.read(cx).is_own_change(path)
    }

    /// The forwarded ports.
    pub(crate) fn ports(&self, cx: &App) -> Entity<crate::ports::Ports> {
        self.ports_panel.read(cx).ports().clone()
    }

    /// The panel keyed `key` when its extension is on.
    pub(super) fn builtin_panel_view(&self, key: &str) -> Option<&PanelView> {
        self.builtins
            .panels
            .iter()
            .find(|panel| panel.key == key)
            .map(|panel| &panel.view)
    }

    /// The view of the panel keyed `key`, when its extension is on.
    pub(crate) fn builtin_panel<V: 'static>(&self, key: &str) -> Option<Entity<V>> {
        self.builtin_panel_view(key)?.downcast()
    }

    /// The count beside the title of the panel keyed `key`.
    pub(super) fn builtin_panel_badge(&self, key: &str, cx: &App) -> usize {
        match crate::builtins::panel(key).and_then(|panel| panel.badge) {
            Some(badge) if self.builtin_panel_view(key).is_some() => badge(self, cx),
            _ => 0,
        }
    }

    pub(crate) fn lsp_store(&self) -> &Entity<LspStore> {
        &self.lsp_store
    }

    /// Opens `path` in an editor (`None`: it opened in a built-in
    /// extension's file tab) as a jump Back returns from.
    pub(crate) fn open_file_editor(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<Editor>> {
        self.record_position(cx);
        self.forward.clear();
        self.open_editor(path, window, cx)
    }

    /// The kind of `path`'s open file tab, when its extension (on) still
    /// shows the file.
    pub(super) fn file_tab_kind_of(&self, path: &Path, cx: &App) -> Option<&'static FileTabKind> {
        self.all_tabs()
            .filter(|tab| tab.path == path)
            .filter_map(Tab::file_tab)
            .filter_map(|tab| crate::builtins::file_tab(tab.kind()))
            .find(|(builtin, _)| self.builtins.on.contains(&builtin.id))
            .map(|(_, kind)| kind)
            .filter(|kind| (kind.opens)(self, path, None, cx))
    }

    /// The file tab kind `path`, read as `text`, opens in.
    pub(super) fn file_tab_kind_for(
        &self,
        path: &Path,
        text: &str,
        cx: &App,
    ) -> Option<&'static FileTabKind> {
        crate::builtins::ALL
            .iter()
            .filter(|builtin| self.builtins.on.contains(&builtin.id))
            .flat_map(|builtin| builtin.file_tabs)
            .find(|kind| (kind.opens)(self, path, Some(text), cx))
    }

    /// One tab per open file: each file's language-server view, the first
    /// file tab of each other file.
    pub(super) fn file_tabs(&self) -> Vec<&Tab> {
        let mut files = HashSet::new();
        self.all_tabs()
            .filter(|tab| match &tab.content {
                TabContent::Editor(_) => tab.lsp_owner,
                TabContent::Builtin(file) if file.is_file() => files.insert(tab.path.as_path()),
                TabContent::Diff(_)
                | TabContent::Compare(_)
                | TabContent::Extension(_)
                | TabContent::Builtin(_) => false,
            })
            .collect()
    }

    /// Releases every file tab's file (the project closes, the app quits):
    /// no notebook kernel outlives them.
    pub(super) fn release_file_tabs(&mut self, cx: &mut App) {
        let files: Vec<BuiltinTab> = self
            .file_tabs()
            .into_iter()
            .filter_map(|tab| tab.file_tab().cloned())
            .collect();
        for file in files {
            file.release(cx);
        }
    }

    /// The views of open files.
    fn file_views(&self) -> Vec<FileView> {
        self.all_tabs()
            .filter_map(|tab| match &tab.content {
                TabContent::Editor(editor) => Some(FileView::Editor(editor.clone())),
                TabContent::Builtin(file) if file.is_file() => {
                    Some(FileView::Builtin(file.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// Hands a newly opened file's view to the built-in extensions that
    /// are on.
    pub(super) fn attach_builtins(&mut self, view: FileView, cx: &mut Context<Self>) {
        for builtin in crate::builtins::ALL {
            if self.builtins.on.contains(&builtin.id) {
                (builtin.attach)(self, &view, cx);
            }
        }
    }

    /// Follows which built-in extensions are on. One turned on gets its
    /// model and panels made and every open file's view; one turned off
    /// gives them back and loses its model, panels, offers and tabs (its
    /// file tabs without unsaved changes show their files as text).
    pub(super) fn sync_builtins(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for builtin in crate::builtins::ALL {
            let id = builtin.id;
            let on = crate::builtins::enabled(builtin, cx);
            if on == self.builtins.on.contains(&id) {
                continue;
            }
            if on {
                self.builtins.on.push(id);
                if let Some(model) = builtin.workspace_model {
                    let model = model(window, cx);
                    self.builtins.models.push((id, model));
                }
                for panel in builtin.panels {
                    let view = (panel.build)(self, window, cx);
                    self.builtins.panels.push(BuiltinPanel {
                        extension: id,
                        key: panel.key,
                        view,
                    });
                }
                for view in self.file_views() {
                    (builtin.attach)(self, &view, cx);
                }
            } else {
                self.builtins.on.retain(|other| *other != id);
                for view in self.file_views() {
                    (builtin.detach)(self, &view, cx);
                }
                self.builtins.panels.retain(|panel| panel.extension != id);
                self.offers.retain(
                    |offer| !matches!(offer.kind, OfferKind::Builtin { extension, .. } if extension == id),
                );
                let own = |tab: &Tab| matches!(&tab.content, TabContent::Builtin(tab) if tab.extension() == id);
                loop {
                    let clean = self
                        .all_tabs()
                        .find(|tab| own(tab) && tab.file_tab().is_some() && !tab.is_dirty(cx))
                        .map(Tab::entity_id);
                    let Some(view) = clean else {
                        break;
                    };
                    self.reopen_as_text(view, window, cx);
                }
                while let Some((pane, index)) =
                    self.find_tab(|tab| own(tab) && tab.file_tab().is_none())
                {
                    self.remove_tab(pane, index, window, cx);
                }
                self.builtins
                    .models
                    .retain(|(extension, _)| *extension != id);
            }
            cx.notify();
        }
    }

    pub(super) fn builtins_files_changed(
        &mut self,
        batch: &Batch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for builtin in crate::builtins::ALL {
            if self.builtins.on.contains(&builtin.id) {
                (builtin.files_changed)(self, batch, window, cx);
            }
        }
    }

    pub(super) fn builtins_tab_activated(&mut self, cx: &mut Context<Self>) {
        for builtin in crate::builtins::ALL {
            if self.builtins.on.contains(&builtin.id) {
                (builtin.tab_activated)(self, cx);
            }
        }
    }

    /// A built-in extension showed `location` (in a document of its own).
    pub(super) fn builtins_navigate(
        &mut self,
        location: &Location,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        crate::builtins::ALL.iter().any(|builtin| {
            self.builtins.on.contains(&builtin.id) && (builtin.navigate)(self, location, window, cx)
        })
    }

    pub(super) fn accept_builtin_offer(
        &mut self,
        extension: &str,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(builtin) = crate::builtins::ALL
            .iter()
            .find(|builtin| builtin.id == extension && self.builtins.on.contains(&builtin.id))
        {
            (builtin.accept_offer)(self, key, window, cx);
        }
    }

    /// Writes the open files with unsaved changes that `filter` selects;
    /// the task says whether all were written.
    pub(crate) fn save_files(
        &self,
        filter: impl Fn(&Path) -> bool,
        cx: &mut Context<Self>,
    ) -> Task<bool> {
        let saves: Vec<_> = self
            .file_tabs()
            .into_iter()
            .filter(|tab| filter(&tab.path) && tab.is_dirty(cx))
            .map(|tab| tab.content.clone())
            .collect();
        let tasks: Vec<_> = saves.iter().map(|content| content.save(cx)).collect();
        cx.spawn(async move |_, _| {
            futures::future::join_all(tasks)
                .await
                .into_iter()
                .all(|saved| saved)
        })
    }
}
