//! Workflows in a window: its commands (Run Workflow: <name>, Open
//! Workflow…, Open Workflow File…, Add Workflow Folder…, New Workflow…),
//! its workflow tabs, and its project tabs for runs ([`WindowHost`]).

use std::path::Path;

use gpui::{
    AnyEntity, AnyWindowHandle, App, AppContext, Context, Div, Entity, Focusable,
    InteractiveElement, PathPromptOptions, Window,
};
use serde_json::{Value, json};
use terminal::OutputTap;

use super::{
    ActiveRun, AddWorkflowFolder, BUILTIN, Host, NewWorkflow, OpenWorkflow, OpenWorkflowFile,
    RunWorkflow, Source, WorkflowPicker, WorkflowPickerEvent, WorkflowView, WorkflowViewEvent,
    Workflows,
};
use crate::{
    builtins::{WindowTab, WindowTabKind},
    command_palette::Command,
    pinpoint::{
        context::{Limits, MAX_ITEMS, path_text},
        file_key,
    },
    project::{Location, Project},
    shell::Shell,
    terminal_panel::TerminalView,
};

pub(super) const TAB: WindowTabKind = WindowTabKind {
    key: "workflow",
    icon: "◈",
    waiting: "Opening the workflow's project…",
    open: open_tab,
};

pub(super) fn actions(element: Div, cx: &mut Context<Shell>) -> Div {
    element
        .on_action(cx.listener(run_workflow))
        .on_action(cx.listener(open_picker))
        .on_action(cx.listener(open_workflow_file))
        .on_action(cx.listener(add_workflow_folder))
        .on_action(cx.listener(new_workflow))
}

/// The window's [`Workflows`]: they open project tabs here; their tabs
/// show them.
pub(super) fn model(window: &mut Window, cx: &mut Context<Shell>) -> AnyEntity {
    super::for_window(window, cx).into()
}

fn workflows(shell: &Shell) -> Option<Entity<Workflows>> {
    shell.builtin_model::<Workflows>()
}

/// One "Run Workflow: <name>" per workflow found (as last read; the list
/// is read again for next time).
pub(super) fn palette(shell: &mut Shell, cx: &mut Context<Shell>) -> Vec<Command> {
    let Some(workflows) = workflows(shell) else {
        return Vec::new();
    };
    let commands = workflows
        .read(cx)
        .discovered()
        .iter()
        .map(|found| {
            Command::labeled(
                Box::new(RunWorkflow {
                    id: found.id.clone(),
                }),
                format!("Run Workflow: {}", found.name),
                "Workflows",
            )
        })
        .collect();
    refresh(shell, cx);
    commands
}

/// Reads the workflows again: a project tab is ready (its
/// `.next_code/workflows` join the list and the palette's "Run Workflow:"
/// entries), or a folder was added.
pub(super) fn refresh(shell: &mut Shell, cx: &mut Context<Shell>) {
    if let Some(workflows) = workflows(shell) {
        workflows.update(cx, |workflows, cx| workflows.refresh(cx).detach());
    }
}

/// Shows the tab of workflow file `path` (from `source`), opening one if
/// this window has none. `None` when the file cannot be read (a notice
/// says why) or its remote project is still opening.
pub(super) fn open(
    shell: &mut Shell,
    source: &Source,
    path: &Path,
    window: &mut Window,
    cx: &mut Context<Shell>,
) -> Option<Entity<WorkflowView>> {
    let project = match source {
        Source::User | Source::Folder(_) => None,
        Source::Project(location) => Some(location.clone()),
    };
    shell
        .open_file_tab(TAB.key, path, project, window, cx)?
        .downcast()
        .ok()
}

/// The view of a workflow file: a user one (`project: None`) read from
/// disk, a project one through its project (`Ok(None)` while a remote
/// project still opens, in a background tab).
fn open_tab(
    shell: &mut Shell,
    path: &Path,
    project: Option<&Location>,
    window: &mut Window,
    cx: &mut Context<Shell>,
) -> Result<Option<WindowTab>, String> {
    let workflows = workflows(shell).ok_or("Workflows are turned off")?;
    let (source, file_project) = match project {
        None => {
            let dir = path.parent().map(Path::to_path_buf);
            // A file outside the user's folder (Open Workflow File…, a
            // restored tab) is listed too.
            if dir.as_deref() != workflows.read(cx).user_dir() {
                let file = path.to_path_buf();
                workflows.update(cx, |workflows, cx| {
                    workflows.remember_file(file);
                    workflows.refresh(cx).detach();
                });
            }
            (Source::User, Project::local(dir.unwrap_or_default()))
        }
        Some(location) => {
            let opened = match (shell.workspace_at(location), location) {
                (Some((_, workspace)), _) => workspace.read(cx).project().clone(),
                (None, Location::Local(root)) => Project::local(root.clone()),
                (None, _) => match shell.project_tab(location, window, cx)? {
                    Some(project) => project,
                    None => return Ok(None),
                },
            };
            (Source::Project(location.clone()), opened)
        }
    };
    let text = file_project
        .read_text(path)
        .map_err(|err| format!("Cannot open {}: {err}", path.display()))?;
    let path = path.to_path_buf();
    let view =
        cx.new(|cx| WorkflowView::new(workflows, source, path, file_project, &text, window, cx));
    // A converted file's tab shows the new file.
    let converted = cx.subscribe_in(
        &view,
        window,
        |shell, view, event: &WorkflowViewEvent, window, cx| match event {
            WorkflowViewEvent::Converted { to } => {
                shell.retarget_file_tab(view.entity_id(), to, window, cx)
            }
        },
    );
    Ok(Some(WindowTab::new(view, vec![converted], cx)))
}

/// Run Workflow: <name> (palette): opens its tab and runs it.
fn run_workflow(
    shell: &mut Shell,
    action: &RunWorkflow,
    window: &mut Window,
    cx: &mut Context<Shell>,
) {
    let found = workflows(shell).and_then(|workflows| workflows.read(cx).find(&action.id).cloned());
    let Some(found) = found else {
        return shell.report(format!("No workflow `{}`", action.id), cx);
    };
    if let Some(view) = open(shell, &found.source, &found.path, window, cx) {
        view.update(cx, |view, cx| view.run_all(cx));
    }
}

/// Open Workflow…: a picker over the workflows found (user and open
/// projects' ones).
fn open_picker(shell: &mut Shell, _: &OpenWorkflow, window: &mut Window, cx: &mut Context<Shell>) {
    let Some(workflows) = workflows(shell) else {
        return;
    };
    let picker = cx.new(|cx| WorkflowPicker::new(workflows, cx));
    let subscription = cx.subscribe_in(
        &picker,
        window,
        |shell, _, event: &WorkflowPickerEvent, window, cx| {
            shell.close_modal(window, cx);
            if let WorkflowPickerEvent::Open { source, path } = event {
                open(shell, source, path, window, cx);
            }
        },
    );
    let focus = picker.focus_handle(cx);
    shell.show_modal(BUILTIN.id, picker.into(), &focus, subscription, window, cx);
}

/// Open Workflow File…: any workflow file on this machine, opened in its
/// tab and listed (palette, picker, headless) for the session.
fn open_workflow_file(
    _: &mut Shell,
    _: &OpenWorkflowFile,
    window: &mut Window,
    cx: &mut Context<Shell>,
) {
    let paths = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Open Workflow".into()),
    });
    cx.spawn_in(window, async move |this, cx| {
        let Ok(Ok(Some(paths))) = paths.await else {
            return;
        };
        let Some(path) = paths.into_iter().next() else {
            return;
        };
        this.update_in(cx, |shell, window, cx| {
            open(shell, &Source::User, &path, window, cx);
        })
        .ok();
    })
    .detach();
}

/// Add Workflow Folder…: a folder whose workflows are listed from now on,
/// saved in the settings' `"workflows": {"folders": […]}`.
fn add_workflow_folder(
    _: &mut Shell,
    _: &AddWorkflowFolder,
    window: &mut Window,
    cx: &mut Context<Shell>,
) {
    let paths = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some("Add Workflow Folder".into()),
    });
    cx.spawn_in(window, async move |this, cx| {
        let Ok(Ok(Some(paths))) = paths.await else {
            return;
        };
        let Some(folder) = paths.into_iter().next() else {
            return;
        };
        this.update(cx, |shell, cx| {
            let saved = crate::settings::Settings::path()
                .ok_or_else(|| "no config directory".to_owned())
                .and_then(|path| {
                    crate::settings::add_workflow_folder(&path, &folder.to_string_lossy())
                });
            shell.report(
                match saved {
                    Ok(true) => format!("Workflows in {} are listed now", folder.display()),
                    Ok(false) => format!("{} is listed already", folder.display()),
                    Err(err) => format!("Could not add the folder: {err}"),
                },
                cx,
            );
            refresh(shell, cx);
        })
        .ok();
    })
    .detach();
}

/// New Workflow…: a commented template in the user's workflows folder,
/// opened in its tab.
fn new_workflow(shell: &mut Shell, _: &NewWorkflow, window: &mut Window, cx: &mut Context<Shell>) {
    let Some(workflows) = workflows(shell) else {
        return;
    };
    let created = workflows
        .read(cx)
        .user_dir()
        .ok_or_else(|| std::io::Error::other("no config directory"))
        .and_then(super::create_template);
    match created {
        Ok(path) => {
            open(shell, &Source::User, &path, window, cx);
            refresh(shell, cx);
        }
        Err(err) => shell.report(format!("Could not create a workflow file: {err}"), cx),
    }
}

/// The window's project tabs, through its [`Shell`].
pub(super) struct WindowHost(pub(super) AnyWindowHandle);

impl WindowHost {
    fn update<R>(
        &self,
        cx: &mut App,
        update: impl FnOnce(&mut Shell, &mut Window, &mut Context<Shell>) -> R,
    ) -> Result<R, String> {
        let shell = self
            .0
            .downcast::<Shell>()
            .ok_or("this window has no project tabs")?;
        shell
            .update(cx, update)
            .map_err(|_| "the window is closed".to_owned())
    }
}

impl Host for WindowHost {
    fn project(&mut self, location: &Location, cx: &mut App) -> Result<Option<Project>, String> {
        self.update(cx, |shell, window, cx| {
            shell.project_tab(location, window, cx)
        })?
    }

    fn projects(&mut self, cx: &mut App) -> Vec<(Location, Project)> {
        self.update(cx, |shell, _, cx| shell.projects(cx))
            .unwrap_or_default()
    }

    /// In a terminal tab of the project at `location`.
    fn start(
        &mut self,
        location: &Location,
        _: &Project,
        title: &str,
        program: &str,
        args: &[String],
        tap: OutputTap,
        cx: &mut App,
    ) -> Result<Entity<TerminalView>, String> {
        self.update(cx, |shell, window, cx| {
            let (_, workspace) = shell
                .workspace_at(location)
                .ok_or_else(|| format!("{location} is not open"))?;
            let terminal = workspace.read(cx).terminal_panel();
            terminal.update(cx, |terminal, cx| {
                terminal.run_program(title, program, args, Some(tap), window, cx)
            })
        })?
    }

    /// Show Terminal: the step's project tab, its Terminal panel and tab.
    fn show(&mut self, location: &Location, view: &Entity<TerminalView>, cx: &mut App) {
        self.update(cx, |shell, window, cx| {
            if let Some(workspace) = shell.show_project(location, window, cx) {
                workspace.update(cx, |workspace, cx| {
                    workspace.show_terminal_view(view, window, cx)
                });
            }
        })
        .ok();
    }

    fn open_url(&mut self, url: &str, cx: &mut App) {
        cx.open_url(url);
    }

    fn open_file(&mut self, location: Option<&Location>, path: &Path, cx: &mut App) {
        self.update(cx, |shell, window, cx| {
            open_file(shell, location, path, window, cx)
        })
        .ok();
    }
}

/// Opens a step's artifact `path` in an editor tab of the project at
/// `location` (opened if needed), and shows that tab. `None` (a step on
/// this machine): the local project tab holding the file, else the file's
/// folder as a new project tab.
fn open_file(
    shell: &mut Shell,
    location: Option<&Location>,
    path: &Path,
    window: &mut Window,
    cx: &mut Context<Shell>,
) {
    let location = match location {
        Some(location) => location.clone(),
        None => {
            let holding = shell
                .projects(cx)
                .into_iter()
                .filter(|(_, project)| !project.is_remote() && path.starts_with(project.root()))
                .max_by_key(|(_, project)| project.root().components().count());
            match holding {
                Some((location, _)) => location,
                None => match path.parent() {
                    Some(folder) => Location::Local(folder.to_path_buf()),
                    None => return,
                },
            }
        }
    };
    if !matches!(shell.project_tab(&location, window, cx), Ok(Some(_))) {
        return shell.report(
            format!("Could not open {} in {location}", path.display()),
            cx,
        );
    }
    if let Some(workspace) = shell.show_project(&location, window, cx) {
        workspace.update(cx, |workspace, cx| workspace.open(path, window, cx));
    }
}

/// A workflow's file and latest run, as `next_code context` reports it.
pub(super) fn workflow_json(
    name: String,
    path: &Path,
    project: &Project,
    run: Option<&ActiveRun>,
    limits: Limits,
) -> Value {
    let mut value = json!({
        "name": name,
        "file": file_key(project, path),
        "path": path_text(project, path),
    });
    let Some(active) = run else {
        value["outcome"] = "idle".into();
        return value;
    };
    let run = &active.run;
    value["outcome"] = run.outcome().name().into();
    let tail = (limits.max_lines / 4).max(3);
    let steps: Vec<Value> = run
        .nodes()
        .iter()
        .zip(run.steps())
        .take(MAX_ITEMS)
        .map(|(node, state)| {
            let mut step = json!({
                "id": node.path,
                "status": state.status.name(),
            });
            if !state.detail.is_empty() {
                step["detail"] = state.detail.as_str().into();
            }
            match &state.status {
                workflow::run::Status::Failed(error) => {
                    step["error"] = error.as_str().into();
                    let lines = state.last_lines(tail);
                    if !lines.is_empty() {
                        step["last_lines"] = lines.join("\n").into();
                    }
                }
                workflow::run::Status::Skipped(why) => step["reason"] = why.as_str().into(),
                _ => {}
            }
            step
        })
        .collect();
    value["steps"] = steps.into();
    value
}
