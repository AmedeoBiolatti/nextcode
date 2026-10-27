//! Shell-level extension management and background command execution.
use super::*;
use crate::{
    extension_views::{ExtensionView, ExtensionViewEvent},
    extensions::{
        self, ExtensionPanel, ManageExtensions, OpenExtensionPanel, PanelEvent, RunExtensionCommand,
    },
    workspace::ExtensionPanelOrigin as PanelOrigin,
};
use extension_api::{API_VERSION, Invocation, Permission, Request};
use std::sync::atomic::Ordering;

impl Shell {
    pub(super) fn manage_extensions(
        &mut self,
        _: &ManageExtensions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu = None;
        if self.extension_panel.is_some() {
            return;
        }
        let store = self.extensions.clone();
        let panel = cx.new(|cx| ExtensionPanel::new(store, cx));
        let focus = window.focused(cx);
        let subscription = cx.subscribe_in(&panel, window, |this, _, event, window, cx| {
            match event {
                PanelEvent::Close => {
                    let focus = this.extension_panel.take().and_then(|(_, _, focus)| focus);
                    window.focus(focus.as_ref().unwrap_or(&this.focus_handle));
                }
                PanelEvent::Toggle(id) => this.toggle_extension(id, window, cx),
                PanelEvent::Settings(id) => {
                    let id = id.clone();
                    let task = cx.background_spawn(async move { extensions::ensure_settings(&id) });
                    cx.spawn_in(window, async move |this, cx| {
                        let result = task.await;
                        this.update_in(cx, |this, window, cx| {
                            match result {
                                Ok(path) => {
                                    this.extension_panel = None;
                                    this.open_config(path, window, cx);
                                }
                                Err(err) => this.notice = Some(format!("{err:#}").into()),
                            }
                            cx.notify();
                        })
                        .ok();
                    })
                    .detach();
                }
                PanelEvent::InstallFolder => {
                    this.install_extension(&extensions::InstallExtension, window, cx)
                }
                PanelEvent::OpenPanel { extension, panel } => {
                    let action = OpenExtensionPanel {
                        extension: extension.clone(),
                        panel: panel.clone(),
                    };
                    let focus = this.extension_panel.take().and_then(|(_, _, focus)| focus);
                    window.focus(focus.as_ref().unwrap_or(&this.focus_handle));
                    this.open_extension_panel(&action, window, cx);
                }
            }
            cx.notify();
        });
        window.focus(&panel.focus_handle(cx));
        self.extension_panel = Some((panel, subscription, focus));
        cx.notify();
    }

    pub(super) fn install_extension(
        &mut self,
        _: &extensions::InstallExtension,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Install Extension Folder".into()),
        });
        self.manage_extensions(&ManageExtensions, window, cx);
        let store = self.extensions.clone();
        cx.spawn(async move |_, cx| {
            if let Ok(Ok(Some(paths))) = paths.await
                && let Some(path) = paths.first()
            {
                store
                    .update(cx, |store, cx| store.install(path.clone(), cx))
                    .ok();
            }
        })
        .detach();
    }

    fn toggle_extension(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.extensions.read(cx);
        if store.loading {
            return;
        }
        let Some(entry) = store
            .entries
            .iter()
            .find(|entry| entry.extension.manifest.id == id)
        else {
            return;
        };
        if entry.enabled {
            self.extensions
                .update(cx, |store, cx| store.set_enabled(id, false, cx));
            return;
        }
        let manifest = &entry.extension.manifest;
        let permissions = manifest
            .permissions
            .iter()
            .map(|permission| match permission {
                Permission::ReadDocument => {
                    "Read the active document.\nIncludes path and selection."
                }
                Permission::EditDocument => "Edit that document.\nUndoable; never auto-saved.",
            })
            .collect::<Vec<_>>()
            .join("\n");
        // GPUI's platform prompt has a narrow fixed width on Linux. Explicit
        // lines keep the complete grant readable rather than clipping it.
        let identity = format!("{} v{}", manifest.id, manifest.version);
        let identity = identity
            .as_bytes()
            .chunks(24)
            .map(|chunk| std::str::from_utf8(chunk).expect("validated ASCII id and version"))
            .collect::<Vec<_>>()
            .join("\n");
        let detail = format!(
            "{identity}\n\n{}\n\nNo filesystem or network.\nNo processes or environment.\nSettings are shared with it.",
            if permissions.is_empty() {
                "No document permissions"
            } else {
                &permissions
            }
        );
        let approved = manifest.permissions.clone();
        let answer = window.prompt(
            PromptLevel::Warning,
            "Enable extension?",
            Some(&detail),
            &["Enable", "Cancel"],
            cx,
        );
        let id = id.to_owned();
        let store = self.extensions.clone();
        cx.spawn(async move |_, cx| {
            if answer.await == Ok(0) {
                store
                    .update(cx, |store, cx| {
                        // Refresh/install cannot silently change what the prompt approved.
                        if store.entries.iter().any(|entry| {
                            entry.extension.manifest.id == id
                                && entry.extension.manifest.permissions == approved
                        }) {
                            store.set_enabled(&id, true, cx);
                        }
                    })
                    .ok();
            }
        })
        .detach();
    }

    pub(super) fn run_extension_command(
        &mut self,
        action: &RunExtensionCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = self.active_extension_workspace();
        self.invoke_extension(
            &action.extension,
            Invocation::Command {
                id: action.command.clone(),
            },
            workspace,
            None,
            window,
            cx,
        );
    }

    pub(super) fn open_extension_panel(
        &mut self,
        action: &OpenExtensionPanel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.active_extension_workspace() else {
            self.extension_error(
                &action.extension,
                "Open a project before opening an extension panel".into(),
                cx,
            );
            return;
        };
        self.invoke_extension(
            &action.extension,
            Invocation::OpenPanel {
                id: action.panel.clone(),
            },
            Some(workspace),
            None,
            window,
            cx,
        );
    }

    fn active_extension_workspace(&self) -> Option<Entity<Workspace>> {
        self.tabs
            .get(self.active)
            .and_then(|tab| match &tab.content {
                Content::Workspace(workspace) => Some(workspace.clone()),
                _ => None,
            })
    }

    fn extension_error(&mut self, id: &str, error: String, cx: &mut Context<Self>) {
        self.notice = Some(format!("Extension {id}: {error}").into());
        self.extensions
            .update(cx, |store, cx| store.log(id, format!("Error: {error}"), cx));
        cx.notify();
    }

    pub(super) fn run_extension_ui(
        &mut self,
        workspace: Entity<Workspace>,
        view: Entity<ExtensionView>,
        input: ExtensionViewEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ExtensionViewEvent::Invoke {
            revision,
            event,
            inputs,
        } = input;
        let id = view.read(cx).extension().to_owned();
        if !workspace.read(cx).contains_extension_view(&view) {
            return;
        }
        let current = view.read(cx);
        let Some(model) = current.model() else {
            return;
        };
        if current.revision() != revision {
            return;
        }
        if let Err(err) = ::extensions::validate_panel_event(model, &event, &inputs) {
            self.extension_error(&id, format!("{err:#}"), cx);
            return;
        }
        let invocation = Invocation::UiEvent {
            panel: model.id.clone(),
            revision,
            state: model.state.clone(),
            inputs: inputs.clone(),
            event,
        };
        self.invoke_extension(
            &id,
            invocation,
            Some(workspace),
            Some(PanelOrigin {
                view,
                revision,
                submitted_inputs: Some(inputs),
            }),
            window,
            cx,
        );
    }

    fn invoke_extension(
        &mut self,
        id: &str,
        invocation: Invocation,
        workspace: Option<Entity<Workspace>>,
        mut origin: Option<PanelOrigin>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((reads_document, contributions)) = self
            .extensions
            .read(cx)
            .entries
            .iter()
            .find(|entry| entry.enabled && entry.extension.manifest.id == id)
            .map(|entry| {
                (
                    entry
                        .extension
                        .manifest
                        .permissions
                        .contains(&Permission::ReadDocument),
                    entry.extension.manifest.panels.clone(),
                )
            })
        else {
            self.extension_error(id, "Extension is disabled or not installed".into(), cx);
            return;
        };
        let opening = if let Invocation::OpenPanel { id: panel } = &invocation {
            let Some(contribution) = contributions
                .iter()
                .find(|candidate| candidate.id == *panel)
                .cloned()
            else {
                self.extension_error(id, format!("Unknown extension panel {panel}"), cx);
                return;
            };
            let Some(workspace) = workspace.as_ref() else {
                return;
            };
            // Reopening an existing panel reveals its state; it must not reset
            // the guest model or rerun its initial computation.
            if workspace
                .read(cx)
                .extension_panel(id, panel)
                .is_some_and(|view| view.read(cx).revision() > 0)
            {
                workspace.update(cx, |workspace, cx| {
                    workspace.prepare_extension_panel(id, &contribution, window, cx);
                });
                return;
            }
            Some(contribution)
        } else {
            None
        };
        let editor = if reads_document {
            workspace
                .as_ref()
                .and_then(|workspace| workspace.read(cx).extension_editor())
        } else {
            None
        };
        let document = match editor
            .as_ref()
            .map(|editor| editor.update(cx, |editor, cx| editor.extension_document(cx)))
            .transpose()
        {
            Ok(document) => document,
            Err(error) => {
                self.extension_error(id, error, cx);
                return;
            }
        };
        let version = document
            .as_ref()
            .map(|document| (document.version, document.path.clone()));
        let (extension, cancelled) =
            match self.extensions.update(cx, |store, cx| store.begin(id, cx)) {
                Ok(started) => started,
                Err(error) => {
                    self.extension_error(id, format!("{error:#}"), cx);
                    return;
                }
            };
        let generation = self.extensions.read(cx).generation;
        if let Some(contribution) = opening
            && let Some(workspace) = workspace.as_ref()
        {
            let view = workspace.update(cx, |workspace, cx| {
                workspace.prepare_extension_panel(id, &contribution, window, cx)
            });
            let revision = view.read(cx).revision();
            origin = Some(PanelOrigin {
                view,
                revision,
                submitted_inputs: None,
            });
        }
        self.notice = None;
        cx.notify();
        let id = id.to_owned();
        let background_id = id.clone();
        let task = cx.background_spawn(async move {
            let settings = extensions::read_settings(&background_id)?;
            extension.run(Request {
                api_version: API_VERSION,
                invocation,
                document,
                settings,
            })
        });
        let store = self.extensions.clone();
        let window = window.window_handle();
        // Application context must survive the originating window's closure so
        // the shared running slot always gets released.
        cx.spawn(async move |this, cx| {
            let mut result = task.await.map_err(|error| format!("{error:#}"));
            if cancelled.load(Ordering::Relaxed) {
                result = Err("Invocation cancelled; no changes applied".into());
            }
            let logs = result.as_mut().map(|response| std::mem::take(&mut response.logs)).unwrap_or_default();
            let applied = window.update(cx, |_, window, cx| this.update(cx, |this, cx| {
                let outcome = (|| -> Result<(), String> {
                    let response = result.as_mut().map_err(|error| error.clone())?;
                    if let Some(error) = &response.error { return Err(error.clone()); }
                    let model = this.extensions.read(cx);
                    if model.generation != generation || !model.entries.iter()
                        .any(|entry| entry.enabled && entry.extension.manifest.id == id) {
                        return Err("Extension was disabled or reloaded; no changes applied".into());
                    }
                    let target_exists = workspace.as_ref().is_some_and(|target| this.tabs.iter()
                        .any(|tab| matches!(&tab.content, Content::Workspace(workspace) if workspace == target)));
                    if (origin.is_some() || !response.edits.is_empty() || !response.panels.is_empty()) && !target_exists {
                        return Err("Project closed or unavailable; no extension changes applied".into());
                    }
                    if let Some(origin) = &origin {
                        let workspace = workspace.as_ref().unwrap();
                        if !workspace.read(cx).contains_extension_view(&origin.view)
                            || origin.view.read(cx).revision() != origin.revision {
                            return Err("Panel was closed or replaced; stale extension result discarded".into());
                        }
                    }
                    if !response.edits.is_empty() {
                        let (Some(editor), Some((version, path)), Some(workspace)) = (&editor, &version, &workspace) else {
                            return Err("No document is available for extension edits".into());
                        };
                        if !workspace.read(cx).contains_extension_editor(editor) {
                            return Err("Document closed while the extension was running; no changes applied".into());
                        }
                        editor.update(cx, |editor, cx| editor.apply_extension_edits(
                            *version, path.as_deref(), std::mem::take(&mut response.edits), cx))?;
                    }
                    if !response.panels.is_empty() {
                        let response_origin = origin.as_mut().map(|origin| PanelOrigin {
                            view: origin.view.clone(), revision: origin.revision,
                            submitted_inputs: origin.submitted_inputs.take(),
                        });
                        workspace.as_ref().unwrap().update(cx, |workspace, cx| {
                            workspace.apply_extension_panels(&id, &contributions,
                                std::mem::take(&mut response.panels), response_origin, window, cx)
                        })?;
                    }
                    Ok(())
                })();
                if let Err(error) = &outcome {
                    this.notice = Some(format!("Extension {id}: {error}").into());
                    if let (Some(workspace), Some(origin)) = (&workspace, &origin)
                        && workspace.read(cx).contains_extension_view(&origin.view)
                        && origin.view.read(cx).revision() == origin.revision {
                            origin.view.update(cx, |view, cx| view.fail(error.clone(), cx));
                    }
                }
                cx.notify();
                outcome
            }));
            let outcome = match applied {
                Ok(Ok(outcome)) => outcome,
                _ => Err("Window closed; extension result discarded".into()),
            };
            store.update(cx, |store, cx| store.finish(&id, logs, outcome, cx)).ok();
        }).detach();
    }
}
