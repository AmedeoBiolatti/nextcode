use std::collections::BTreeMap;

use extension_api::PanelPlacement;
use gpui::{AnyElement, EntityId};

use super::*;

pub(super) struct ExtensionPanel {
    extension: String,
    panel: String,
    title: SharedString,
    view: Entity<ExtensionView>,
    docked: bool,
    _subscription: Subscription,
}

pub(crate) struct ExtensionPanelOrigin {
    pub view: Entity<ExtensionView>,
    pub revision: u64,
    /// Only UI events have a submitted form; opening replies may also
    /// initialise other declared panels.
    pub submitted_inputs: Option<BTreeMap<String, InputValue>>,
}

impl Workspace {
    /// Only ordinary document tabs are extension edit targets.
    pub(crate) fn extension_editor(&self) -> Option<Entity<Editor>> {
        self.active_tab().and_then(Tab::editor).cloned()
    }

    pub(crate) fn contains_extension_editor(&self, editor: &Entity<Editor>) -> bool {
        self.all_tabs().any(|tab| tab.editor() == Some(editor))
    }

    pub(crate) fn extension_panel(
        &self,
        extension: &str,
        panel: &str,
    ) -> Option<Entity<ExtensionView>> {
        self.extension_panels
            .iter()
            .find(|entry| entry.extension == extension && entry.panel == panel)
            .map(|entry| entry.view.clone())
    }

    pub(crate) fn contains_extension_view(&self, view: &Entity<ExtensionView>) -> bool {
        self.extension_panels
            .iter()
            .any(|entry| entry.view == *view)
    }

    pub(crate) fn prepare_extension_panel(
        &mut self,
        extension: &str,
        contribution: &PanelContribution,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ExtensionView> {
        if let Some(view) = self.extension_panel(extension, &contribution.id) {
            self.reveal_extension_panel(&view, window, cx);
            return view;
        }
        let view = cx.new(|cx| {
            ExtensionView::new(
                extension.to_owned(),
                contribution.id.clone(),
                contribution.title.clone(),
                cx,
            )
        });
        let subscription = cx.subscribe(&view, |this, view, event, cx| {
            if !this.contains_extension_view(&view) {
                return;
            }
            let ExtensionViewEvent::Invoke {
                revision,
                event,
                inputs,
            } = event;
            if view.read(cx).revision() != *revision {
                return;
            }
            cx.emit(WorkspaceEvent::ExtensionUi {
                view,
                revision: *revision,
                event: event.clone(),
                inputs: inputs.clone(),
            });
        });
        let docked = matches!(contribution.placement, PanelPlacement::Dock);
        self.extension_panels.push(ExtensionPanel {
            extension: extension.to_owned(),
            panel: contribution.id.clone(),
            view: view.clone(),
            title: contribution.title.clone().into(),
            docked,
            _subscription: subscription,
        });
        let model = crate::extensions::shared(cx);
        let busy = model.read(cx).running.is_some();
        view.update(cx, |view, cx| view.set_busy(busy, cx));
        if docked {
            self.active_extension_panel = Some(view.entity_id());
            self.show_panel(PanelId::Extensions, cx);
            window.focus(&view.focus_handle(cx));
        } else {
            self.add_extension_editor(view.clone(), window, cx);
        }
        cx.notify();
        view
    }

    pub(crate) fn apply_extension_panels(
        &mut self,
        extension: &str,
        contributions: &[PanelContribution],
        updates: Vec<PanelUpdate>,
        origin: Option<ExtensionPanelOrigin>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        // Validate the whole reply before creating or replacing any view.
        if let Some(origin) = &origin
            && (!self.contains_extension_view(&origin.view)
                || origin.view.read(cx).revision() != origin.revision
                || origin.view.read(cx).extension() != extension)
        {
            return Err(
                "Extension panel was closed or changed while the extension was running".into(),
            );
        }
        let mut ids = HashSet::new();
        for update in &updates {
            if !ids.insert(update.id.as_str())
                || !contributions.iter().any(|item| item.id == update.id)
            {
                return Err("Extension returned an undeclared or duplicate panel".into());
            }
            if let Some(origin) = &origin
                && origin.submitted_inputs.is_some()
                && update.id != origin.view.read(cx).panel()
            {
                return Err("Extension event updated a different panel".into());
            }
        }
        for update in updates {
            let view = match self.extension_panel(extension, &update.id) {
                Some(view) => view,
                None => {
                    let contribution = contributions
                        .iter()
                        .find(|item| item.id == update.id)
                        .unwrap();
                    self.prepare_extension_panel(extension, contribution, window, cx)
                }
            };
            let submitted = origin
                .as_ref()
                .filter(|origin| origin.view == view)
                .and_then(|origin| origin.submitted_inputs.as_ref());
            view.update(cx, |view, cx| view.replace(update, submitted, cx));
        }
        cx.notify();
        Ok(())
    }

    fn reveal_extension_panel(
        &mut self,
        view: &Entity<ExtensionView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((pane, index)) = self.find_view(view.entity_id()) {
            self.activate(pane, index, window, cx);
        } else {
            self.active_extension_panel = Some(view.entity_id());
            self.show_panel(PanelId::Extensions, cx);
            window.focus(&view.focus_handle(cx));
        }
    }

    fn add_extension_editor(
        &mut self,
        view: Entity<ExtensionView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title: SharedString = view.read(cx).title().to_owned().into();
        let subscription = cx.observe(&view, |_, _, cx| cx.notify());
        // No fabricated filesystem path: every file-only operation excludes this variant.
        self.push_tab(
            TabContent::Extension(view),
            PathBuf::new(),
            title,
            subscription,
            window,
            cx,
        );
    }

    pub(super) fn active_docked_extension(&self) -> Option<Entity<ExtensionView>> {
        self.extension_panels
            .iter()
            .find(|entry| {
                entry.docked && Some(entry.view.entity_id()) == self.active_extension_panel
            })
            .map(|entry| entry.view.clone())
    }

    pub(super) fn forget_extension_view(&mut self, id: EntityId) {
        self.extension_panels
            .retain(|entry| entry.view.entity_id() != id);
        if self.active_extension_panel == Some(id) {
            self.active_extension_panel = self
                .extension_panels
                .iter()
                .find(|entry| entry.docked)
                .map(|entry| entry.view.entity_id());
        }
    }

    pub(super) fn close_extension_panel(
        &mut self,
        id: EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((pane, index)) = self.find_view(id) {
            self.remove_tab(pane, index, window, cx);
        } else {
            self.forget_extension_view(id);
            cx.notify();
        }
    }

    pub(super) fn move_extension_to_dock(
        &mut self,
        id: EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((pane, index)) = self.find_view(id) else {
            return;
        };
        self.take_tab(pane, index);
        self.prune_empty_panes();
        if let Some(entry) = self
            .extension_panels
            .iter_mut()
            .find(|entry| entry.view.entity_id() == id)
        {
            entry.docked = true;
        }
        self.active_extension_panel = Some(id);
        self.show_panel(PanelId::Extensions, cx);
        self.schedule_session_save(cx);
        if let Some(view) = self.active_docked_extension() {
            window.focus(&view.focus_handle(cx));
        }
    }

    pub(super) fn move_extension_to_editor(
        &mut self,
        id: EntityId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self
            .extension_panels
            .iter_mut()
            .find(|entry| entry.view.entity_id() == id && entry.docked)
        else {
            return;
        };
        entry.docked = false;
        let view = entry.view.clone();
        self.active_extension_panel = self
            .extension_panels
            .iter()
            .find(|entry| entry.docked)
            .map(|entry| entry.view.entity_id());
        self.add_extension_editor(view, window, cx);
    }

    pub(super) fn sync_extension_panels(
        &mut self,
        model: &Entity<crate::extensions::Extensions>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let model = model.read(cx);
        let reloaded = self.extension_generation != model.generation;
        self.extension_generation = model.generation;
        let stale: Vec<_> = self
            .extension_panels
            .iter()
            .filter(|panel| {
                reloaded
                    || !model.entries.iter().any(|entry| {
                        entry.enabled && entry.extension.manifest.id == panel.extension
                    })
            })
            .map(|entry| entry.view.entity_id())
            .collect();
        let busy = model.running.is_some();
        for id in stale {
            self.close_extension_panel(id, window, cx);
        }
        for entry in &self.extension_panels {
            entry.view.update(cx, |view, cx| view.set_busy(busy, cx));
        }
        cx.notify();
    }

    pub(super) fn render_extension_dock(&self, cx: &mut Context<Self>) -> AnyElement {
        let active = self.active_docked_extension();
        div().size_full().min_w_0().min_h_0().flex().flex_col()
            .child(div().id("extension-panel-tabs").flex().flex_none().min_w_0().overflow_x_scroll().border_b_1().border_color(theme::border())
                .children(self.extension_panels.iter().filter(|entry| entry.docked).map(|entry| {
                    let id = entry.view.entity_id();
                    let selected = self.active_extension_panel == Some(id);
                    div().id(("extension-tab", id)).flex().flex_none().items_center().gap_2().px_2().py_1().text_sm().cursor_pointer()
                        .when(selected, |tab| tab.bg(theme::active_row()))
                        .child(entry.title.clone())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.active_extension_panel = Some(id);
                            if let Some(view) = this.active_docked_extension() { window.focus(&view.focus_handle(cx)); }
                            cx.notify();
                        }))
                        .child(div().id(("close-extension", id)).child("×")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_extension_panel(id, window, cx);
                            })).pin("close").label("Close extension panel"))
                        .pin(format!("tab:{}:{}", entry.extension, entry.panel))
                }))
                .when_some(active.as_ref(), |header, view| {
                    let id = view.entity_id();
                    header.child(div().id("extension-to-editor").px_2().py_1().text_sm().cursor_pointer()
                        .child("Move to Editor").on_click(cx.listener(move |this, _, window, cx| {
                            this.move_extension_to_editor(id, window, cx);
                        })).pin("move-to-editor").label("Move extension panel to editor"))
                }))
            .child(div().flex_1().min_h_0().min_w_0().flex()
                .when_some(active, |body, view| body.child(view))
                .when(self.active_docked_extension().is_none(), |body| body.p_3().text_sm().text_color(theme::muted())
                    .child("Open an extension panel from the command palette or Extensions manager.")))
            .into_any_element()
    }
}
