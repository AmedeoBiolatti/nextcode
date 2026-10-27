//! What built-in extensions (`crate::builtins`) use of the window, besides
//! `project_tab`, `projects`, `workspace_at`, `show_project` and `report`;
//! and the window's side of them: their models, file tabs and dialog.

use gpui::AnyEntity;

use super::*;

/// The built-in extensions that are on in this window (it follows the
/// shared choice in `sync_builtins`), their models and the dialog one
/// shows.
#[derive(Default)]
pub(super) struct Builtins {
    on: Vec<&'static str>,
    models: Vec<(&'static str, AnyEntity)>,
    modal: Option<Modal>,
}

struct Modal {
    extension: &'static str,
    view: AnyView,
    /// Where focus returns.
    previous: Option<FocusHandle>,
    _subscription: Subscription,
}

impl Builtins {
    pub(super) fn modal(&self) -> Option<AnyView> {
        self.modal.as_ref().map(|modal| modal.view.clone())
    }
}

impl Shell {
    /// The window's model of a built-in extension that is on
    /// (`Builtin::window_model`), when it is a `T`.
    pub(crate) fn builtin_model<T: 'static>(&self) -> Option<Entity<T>> {
        self.builtins
            .models
            .iter()
            .find_map(|(_, model)| model.clone().downcast().ok())
    }

    /// Shows `view` (focused by `focus`) over the window for `extension`
    /// until `close_modal`.
    pub(crate) fn show_modal(
        &mut self,
        extension: &'static str,
        view: AnyView,
        focus: &FocusHandle,
        subscription: Subscription,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu = None;
        let previous = window.focused(cx);
        window.focus(focus);
        self.builtins.modal = Some(Modal {
            extension,
            view,
            previous,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Closes the dialog of `show_modal`; focus returns where it was.
    pub(crate) fn close_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(modal) = self.builtins.modal.take() {
            window.focus(modal.previous.as_ref().unwrap_or(&self.focus_handle));
            cx.notify();
        }
    }

    /// Shows the tab of `file` in a `kind` tab (`project`: the project
    /// holding it), opening one if this window has none. Its view, or
    /// `None` when it cannot open (a notice says why) or waits for its
    /// project.
    pub(crate) fn open_file_tab(
        &mut self,
        kind: &str,
        file: &Path,
        project: Option<Location>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyView> {
        let target = TabTarget::File {
            kind: kind.to_owned(),
            path: file.to_path_buf(),
            project,
        };
        let index = match self.tabs.iter().position(|tab| tab.target == target) {
            Some(index) => index,
            None => {
                let TabTarget::File { project, .. } = &target else {
                    unreachable!()
                };
                let content = match self.open_window_tab(kind, file, project.as_ref(), window, cx) {
                    Ok(Some(tab)) => Content::Builtin(tab),
                    Ok(None) => Content::Unloaded,
                    Err(error) => {
                        self.report(error, cx);
                        return None;
                    }
                };
                self.tabs.push(Tab {
                    target,
                    content,
                    _subscription: None,
                });
                self.tabs.len() - 1
            }
        };
        self.activate(index, window, cx);
        self.tabs[index].content.view()
    }

    /// The view of `file` in a new `kind` tab.
    pub(super) fn open_window_tab(
        &mut self,
        kind: &str,
        file: &Path,
        project: Option<&Location>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Option<WindowTab>, String> {
        match crate::builtins::window_tab(kind) {
            Some((builtin, kind)) if self.builtins.on.contains(&builtin.id) => {
                (kind.open)(self, file, project, window, cx)
            }
            Some((builtin, _)) => Err(format!("{} is turned off", builtin.name)),
            None => Err(format!("Cannot open {} here", file.display())),
        }
    }

    /// The file tab showing `view` shows `file` instead (e.g. its old file
    /// was converted to it).
    pub(crate) fn retarget_file_tab(
        &mut self,
        view: EntityId,
        file: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.content.id() == Some(view))
        else {
            return;
        };
        if let TabTarget::File { path, .. } = &mut self.tabs[index].target {
            *path = file.to_path_buf();
        }
        self.load(index, None, window, cx);
        self.activate(index, window, cx);
    }

    /// The shown tab's view, when it is a file tab (`next_code context`).
    pub(crate) fn active_window_tab(&self) -> Option<&WindowTab> {
        match &self.tabs.get(self.active)?.content {
            Content::Builtin(tab) => Some(tab),
            _ => None,
        }
    }

    pub(super) fn builtins_project_opened(&mut self, cx: &mut Context<Self>) {
        for builtin in crate::builtins::ALL {
            if self.builtins.on.contains(&builtin.id) {
                (builtin.project_opened)(self, cx);
            }
        }
    }

    /// Follows which built-in extensions are on. One turned on gets its
    /// model; one turned off loses it, its dialog and its file tabs
    /// (those with unsaved changes stay until closed).
    pub(super) fn sync_builtins(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for builtin in crate::builtins::ALL {
            let id = builtin.id;
            let on = crate::builtins::enabled(builtin, cx);
            if on == self.builtins.on.contains(&id) {
                continue;
            }
            if on {
                self.builtins.on.push(id);
                if let Some(model) = builtin.window_model {
                    let model = model(window, cx);
                    self.builtins.models.push((id, model));
                }
            } else {
                self.builtins.on.retain(|other| *other != id);
                self.builtins
                    .models
                    .retain(|(extension, _)| *extension != id);
                if self
                    .builtins
                    .modal
                    .as_ref()
                    .is_some_and(|modal| modal.extension == id)
                {
                    self.close_modal(window, cx);
                }
                let closes = |tab: &Tab, cx: &App| match &tab.target {
                    TabTarget::File { kind, .. } => {
                        builtin.window_tabs.iter().any(|own| own.key == kind)
                            && tab.dirty_count(cx) == 0
                    }
                    TabTarget::Project(_) => false,
                };
                while let Some(index) = self.tabs.iter().position(|tab| closes(tab, cx)) {
                    self.remove_tab(index, window, cx);
                }
            }
            cx.notify();
        }
    }
}
