//! Headless workflows: `workflows_list`, `workflow_run`, `workflow_status`,
//! `workflow_stop`, `workflow_continue`, `workflow_restart`,
//! `workflow_retry`, and `workflow_event` notifications. The same
//! [`Workflows`] model as the GUI; steps for other projects open them
//! without a window (local folders at once, WSL and dev containers like
//! `Project::open`), and commands run in terminals nobody sees (their last
//! line is in `workflow_status`). URLs are reported, not opened.

use std::time::Instant;

use workflow::{Ready, Status};

use super::*;
use crate::{
    terminal_panel::{self, TerminalView},
    workflows::{Host, Workflows, WorkflowsEvent},
};

/// A project being opened on a thread for a step.
enum Opening {
    Pending(Arc<Mutex<Option<Result<Project, String>>>>),
    Done(Result<Project, String>),
}

pub(super) struct HeadlessHost {
    project: Project,
    location: Location,
    others: Vec<(Location, Opening)>,
}

impl HeadlessHost {
    pub(super) fn new(project: Project) -> Self {
        Self {
            location: project.location(),
            project,
            others: Vec::new(),
        }
    }
}

impl Host for HeadlessHost {
    fn project(&mut self, location: &Location, _: &mut App) -> Result<Option<Project>, String> {
        if location.opens_same(&self.location) {
            return Ok(Some(self.project.clone()));
        }
        let at = match self
            .others
            .iter()
            .position(|(other, _)| other.opens_same(location))
        {
            Some(at) => at,
            None => {
                let opening = match location {
                    Location::Local(root) if !root.is_dir() => {
                        Opening::Done(Err(format!("{} is not a folder", root.display())))
                    }
                    Location::Local(root) => Opening::Done(Ok(Project::local(root.clone()))),
                    remote => {
                        let slot = Arc::new(Mutex::new(None));
                        let (target, result) = (remote.clone(), slot.clone());
                        thread::spawn(move || {
                            let opened = Project::open(&target, &mut |_| {})
                                .map_err(|err| format!("cannot open {target}: {err}"));
                            *result.lock() = Some(opened);
                        });
                        Opening::Pending(slot)
                    }
                };
                self.others.push((location.clone(), opening));
                self.others.len() - 1
            }
        };
        let opening = &mut self.others[at].1;
        let finished = match opening {
            Opening::Pending(slot) => slot.lock().take(),
            Opening::Done(_) => None,
        };
        if let Some(result) = finished {
            *opening = Opening::Done(result);
        }
        match opening {
            Opening::Pending(_) => Ok(None),
            Opening::Done(result) => result.clone().map(Some),
        }
    }

    fn projects(&mut self, _: &mut App) -> Vec<(Location, Project)> {
        let mut projects = vec![(self.location.clone(), self.project.clone())];
        for (location, opening) in &self.others {
            if let Opening::Done(Ok(project)) = opening {
                projects.push((location.clone(), project.clone()));
            }
        }
        projects
    }

    fn start(
        &mut self,
        _: &Location,
        project: &Project,
        title: &str,
        program: &str,
        args: &[String],
        tap: terminal::OutputTap,
        cx: &mut App,
    ) -> Result<Entity<TerminalView>, String> {
        terminal_panel::spawn_program(Some(project), title, program, args, Some(tap), cx)
            .map_err(|err| format!("could not start {program}: {err}"))
    }

    fn show(&mut self, _: &Location, _: &Entity<TerminalView>, _: &mut App) {}

    fn open_url(&mut self, _: &str, _: &mut App) {}
}

/// Forwards the model's events as `workflow_event` lines.
pub(super) fn forward(out: &Out, event: &WorkflowsEvent) {
    match event {
        WorkflowsEvent::Step {
            run,
            workflow,
            step,
            status,
            detail,
        } => out.event(
            "workflow_event",
            json!({
                "type": "step",
                "run": run,
                "workflow": workflow,
                "step": step,
                "status": status.name(),
                "reason": match status {
                    Status::Failed(reason) => Some(reason.as_str()),
                    _ => None,
                },
                "detail": detail,
            }),
        ),
        WorkflowsEvent::Outcome {
            run,
            workflow,
            outcome,
        } => out.event(
            "workflow_event",
            json!({ "type": "outcome", "run": run, "workflow": workflow, "outcome": outcome.name() }),
        ),
        WorkflowsEvent::UrlOpened {
            run,
            workflow,
            step,
            url,
        } => out.event(
            "workflow_event",
            json!({ "type": "url", "run": run, "workflow": workflow, "step": step, "url": url }),
        ),
        WorkflowsEvent::Discovered => {}
    }
}

fn ready_json(ready: &Ready) -> Value {
    match ready {
        Ready::Exit => json!({ "exit": true }),
        Ready::Output(pattern) => json!({ "output": pattern }),
        Ready::Port(port) => json!({ "port": port }),
        Ready::Manual(prompt) => json!({ "manual": prompt }),
        Ready::Delay(ms) => json!({ "delay_ms": ms }),
    }
}

fn list_json(workflows: &Workflows) -> Value {
    let list: Vec<Value> = workflows
        .discovered()
        .iter()
        .map(|found| {
            let steps: Vec<Value> = found
                .parsed
                .as_ref()
                .map(|workflow| {
                    workflow
                        .steps
                        .iter()
                        .map(|step| {
                            json!({
                                "id": step.id,
                                "title": step.title,
                                "needs": step.needs,
                                "project": step.project.as_ref().map(|spec| {
                                    crate::workflows::location_of(spec)
                                        .map_or_else(|err| err, |location| location.to_string())
                                }),
                                "run": step.run,
                                "check": step.check,
                                "ready": ready_json(&step.ready),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            json!({
                "id": found.id,
                "name": found.name,
                "path": path_json(&found.path),
                "source": found.source.label(),
                "error": found.parsed.as_ref().err(),
                "steps": steps,
                "outcome": workflows.run_of(&found.id).map(|run| run.run.outcome().name()),
            })
        })
        .collect();
    json!(list)
}

fn status_json(workflows: &Workflows, id: &str, cx: &App) -> Result<Value, String> {
    let found = workflows
        .find(id)
        .ok_or_else(|| format!("no workflow `{id}`"))?;
    let active = workflows
        .run_of(id)
        .ok_or_else(|| format!("{} has not run", found.name))?;
    let now = Instant::now();
    let steps: Vec<Value> = active
        .run
        .workflow()
        .steps
        .iter()
        .zip(active.run.steps())
        .enumerate()
        .map(|(index, (step, state))| {
            json!({
                "id": step.id,
                "title": step.label(),
                "status": state.status.name(),
                "reason": match &state.status {
                    Status::Failed(reason) => Some(reason.as_str()),
                    _ => None,
                },
                "detail": state.detail,
                "last_line": state.last_line(),
                "check_output": state.check_output,
                "exit_code": state.exit.flatten(),
                "exited": state.exit.is_some(),
                "elapsed_ms": state.elapsed(now).map(|elapsed| elapsed.as_millis() as u64),
                "location": active.location(index).map(ToString::to_string),
                "terminal": active.terminal(index).map(|view| view.read(cx).run_name().map(str::to_owned)),
            })
        })
        .collect();
    Ok(json!({
        "run": active.id,
        "workflow": found.name,
        "outcome": active.run.outcome().name(),
        "steps": steps,
    }))
}

/// The workflow a request names: `name`, `path` or `id`.
fn query(request: &Value) -> Result<String, String> {
    ["name", "path", "id"]
        .iter()
        .find_map(|key| request[*key].as_str())
        .map(str::to_owned)
        .ok_or_else(|| "missing `name`, `path` or `id`".into())
}

impl Session {
    pub(super) fn workflows(
        &mut self,
        id: Value,
        command: &str,
        request: &Value,
        cx: &mut Context<Self>,
    ) {
        let workflows = self.workflows.clone();
        let out = self.out.clone();
        // Every command reads the files again first: they may have changed.
        let refresh = workflows.update(cx, |workflows, cx| workflows.refresh(cx));
        let command = command.to_owned();
        let request = request.clone();
        cx.spawn(async move |_, cx| {
            refresh.await;
            let result = workflows.update(cx, |workflows, cx| -> Result<Value, String> {
                if command == "workflows_list" {
                    return Ok(list_json(workflows));
                }
                let name = query(&request)?;
                let found = workflows
                    .lookup(&name)
                    .ok_or_else(|| format!("no workflow `{name}`"))?
                    .id
                    .clone();
                let step = |request: &Value| -> Result<usize, String> {
                    let step = param_str(request, "step")?;
                    workflows
                        .run_of(&found)
                        .and_then(|active| active.run.step(step))
                        .ok_or_else(|| format!("no running step `{step}`"))
                };
                match command.as_str() {
                    "workflow_run" => {
                        let run = workflows.start(&found, cx)?;
                        Ok(json!({ "run": run, "id": found }))
                    }
                    "workflow_status" => status_json(workflows, &found, cx),
                    "workflow_stop" => Ok(json!(workflows.stop(&found, cx))),
                    "workflow_retry" => Ok(json!(workflows.retry_failed(&found, cx))),
                    "workflow_restart" => {
                        let step = step(&request)?;
                        Ok(json!(workflows.restart_step(&found, step, cx)))
                    }
                    "workflow_continue" => {
                        let step = step(&request)?;
                        if workflows.continue_step(&found, step, cx) {
                            Ok(Value::Null)
                        } else {
                            Err("the step is not waiting for Continue".into())
                        }
                    }
                    other => Err(format!("unknown command `{other}`")),
                }
            });
            out.reply(&id, result.unwrap_or_else(|err| Err(err.to_string())));
        })
        .detach();
    }
}
