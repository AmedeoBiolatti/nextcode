//! Shell-level extension management and background command execution.
use super::*;
use crate::{
    extension_views::{ExtensionView, ExtensionViewEvent},
    extensions::{
        self, ExtensionPanel, ManageExtensions, OpenExtensionPanel, PanelEvent, RunExtensionCommand,
    },
    workspace::ExtensionPanelOrigin as PanelOrigin,
};
use extension_api::{Invocation, Permission, Request};
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
                Permission::RunProcess => "Run declared programs.\nOn the project host.\nUses your account’s\nfiles, network and env.",
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
        let programs = manifest
            .processes
            .iter()
            .map(|p| {
                format!(
                    "Run in the project:\n{}",
                    p.program
                        .as_bytes()
                        .chunks(24)
                        .map(|s| std::str::from_utf8(s).expect("ASCII program name"))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let access = if programs.is_empty() {
            "No filesystem or network.\nNo processes or environment.".to_owned()
        } else {
            format!("{programs}\nPrograms may access\nfiles and the network.")
        };
        let detail = format!(
            "{identity}\n\n{}\n\n{access}\nSettings are shared with it.",
            if permissions.is_empty() {
                "No document permissions"
            } else {
                &permissions
            }
        );
        let approved = manifest.permissions.clone();
        let approved_programs = manifest.processes.clone();
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
                                && entry.extension.manifest.processes == approved_programs
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
        if matches!(input, ExtensionViewEvent::Background { .. })
            && (self.extensions.read(cx).running.is_some() || self.extensions.read(cx).loading)
        {
            view.update(cx, |view, _| view.defer_background(input));
            return;
        }
        let id = view.read(cx).extension().to_owned();
        if !workspace.read(cx).contains_extension_view(&view) {
            return;
        }
        let current = view.read(cx);
        let Some(model) = current.model() else {
            return;
        };
        let revision = input.revision();
        if current.revision() != revision {
            return;
        }
        let (invocation, inputs, direct, process_result) = match input {
            ExtensionViewEvent::Invoke { event, inputs, .. } => {
                if let Err(err) = ::extensions::validate_panel_event(model, &event, &inputs) {
                    self.extension_error(&id, format!("{err:#}"), cx);
                    return;
                }
                (
                    Invocation::UiEvent {
                        panel: model.id.clone(),
                        revision,
                        state: model.state.clone(),
                        inputs: inputs.clone(),
                        event,
                    },
                    inputs,
                    None,
                    false,
                )
            }
            ExtensionViewEvent::Background {
                process,
                direct,
                inputs,
                ..
            } => {
                if let Some(process) = process {
                    let stdout = (direct
                        && process.exit_code == Some(0)
                        && !process.truncated
                        && process.error.is_none())
                    .then(|| process.stdout.clone());
                    (
                        Invocation::ProcessExit {
                            panel: model.id.clone(),
                            revision,
                            state: model.state.clone(),
                            inputs: inputs.clone(),
                            process,
                        },
                        inputs,
                        stdout,
                        true,
                    )
                } else {
                    (
                        Invocation::Refresh {
                            panel: model.id.clone(),
                            revision,
                            state: model.state.clone(),
                            inputs: inputs.clone(),
                        },
                        inputs,
                        None,
                        false,
                    )
                }
            }
        };
        self.invoke_extension_reply(
            &id,
            invocation,
            Some(workspace),
            Some(PanelOrigin {
                view,
                revision,
                submitted_inputs: Some(inputs),
                process_result,
            }),
            direct,
            window,
            cx,
        );
    }

    fn invoke_extension(
        &mut self,
        id: &str,
        invocation: Invocation,
        workspace: Option<Entity<Workspace>>,
        origin: Option<PanelOrigin>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.invoke_extension_reply(id, invocation, workspace, origin, None, window, cx);
    }

    fn invoke_extension_reply(
        &mut self,
        id: &str,
        invocation: Invocation,
        workspace: Option<Entity<Workspace>>,
        mut origin: Option<PanelOrigin>,
        direct: Option<String>,
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
                process_result: false,
            });
        }
        self.notice = None;
        cx.notify();
        let id = id.to_owned();
        let background_id = id.clone();
        let task = cx.background_spawn(async move {
            let settings = extensions::read_settings(&background_id)?;
            let request = Request {
                api_version: extension.manifest.api_version,
                invocation,
                document,
                settings,
            };
            if let Some(stdout) = direct {
                anyhow::ensure!(
                    stdout.len() <= extension_api::MAX_RESPONSE_BYTES,
                    "process response exceeds 1 MiB"
                );
                let response = serde_json::from_str(&stdout)?;
                extension.validate_reply(&request, &response)?;
                Ok(response)
            } else {
                extension.run(request)
            }
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
                    let requests = std::mem::take(&mut response.processes);
                    if !requests.is_empty() {
                        let origin = origin.as_ref().ok_or("Processes require an originating panel")?;
                        origin.view.read(cx).can_start_processes(&requests)?;
                        let active = this.extensions.read(cx).process_count(&id);
                        if active + requests.len() > extension_api::MAX_PROCESSES { return Err("At most four processes may run per extension".into()); }
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
                            submitted_inputs: origin.submitted_inputs.take(), process_result: origin.process_result,
                        });
                        workspace.as_ref().unwrap().update(cx, |workspace, cx| {
                            workspace.apply_extension_panels(&id, &contributions,
                                std::mem::take(&mut response.panels), response_origin, window, cx)
                        })?;
                    }
                    if !requests.is_empty() {
                        let origin = origin.as_ref().unwrap();
                        let workspace = workspace.as_ref().unwrap();
                        let project = workspace.read(cx).project().clone();
                        this.start_extension_processes(&id, project, &origin.view, requests, cx)?;
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
                if let Some(origin) = &origin { origin.view.update(cx, |view, cx| view.finish_invocation(cx)); }
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
    fn start_extension_processes(
        &mut self,
        id: &str,
        project: crate::project::Project,
        view: &Entity<ExtensionView>,
        requests: Vec<extension_api::ProcessRequest>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let slots = self
            .extensions
            .update(cx, |store, _| store.reserve_processes(id, requests.len()))?;
        view.update(cx, |view, cx| {
            if view.model().is_none() {
                view.replace_from(
                    extension_api::PanelUpdate {
                        id: view.panel().to_owned(),
                        refresh_ms: None,
                        state: serde_json::json!({}),
                        root: extension_api::Widget::Label {
                            id: "loading".into(),
                            text: "Loading…".into(),
                        },
                    },
                    None,
                    false,
                    cx,
                );
            }
        });
        let lifetime = view.read(cx).lifetime.clone();
        for (request, (slot, token)) in requests.into_iter().zip(slots) {
            view.update(cx, |view, _| {
                view.start_process(request.id.clone(), token.clone())
            });
            let view = view.downgrade();
            let store = self.extensions.clone();
            let project = project.clone();
            let lifetime = lifetime.clone();
            let direct = request.response == Some(extension_api::ProcessResponse::Stdout);
            let worker_token = token.clone();
            let task = cx.background_spawn(async move {
                project.run_extension_process(&request, &worker_token, &lifetime)
            });
            cx.spawn(async move |_, cx| {
                let result = task.await;
                // Do not let a cancelled old job consume a newer job's reused ID.
                view.update(cx, |view, cx| {
                    view.process_finished(result, &token, direct, cx)
                })
                .ok();
                store
                    .update(cx, |store, cx| store.process_finished(slot, cx))
                    .ok();
            })
            .detach();
        }
        Ok(())
    }
}
