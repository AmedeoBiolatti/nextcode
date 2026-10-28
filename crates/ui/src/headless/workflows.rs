//! Headless workflows: `workflows_list`, `workflow_run`, `workflow_status`,
//! `workflow_stop`, `workflow_continue`, `workflow_restart`,
//! `workflow_retry`, `workflow_check_artifacts`, and `workflow_event`
//! notifications. The same [`Workflows`] model as the GUI; steps for other
//! projects open them without a window (local folders at once, WSL and
//! dev containers like `Project::open`), and commands run in terminals
//! nobody sees (their last line is in `workflow_status`). URLs are
//! reported, not opened. Steps in groups go by qualified ids
//! (`group/step`); groups have status rows and events of their own.
//! `workflow_run` takes the workflow's inputs as JSON (defaults for the
//! others; the GUI form's remembered values are not used).

use std::time::Instant;

use workflow::{Artifact, ArtifactTarget, Input, InputKind, Ready, Status, StepNode};

use super::*;
use crate::{
    builtins::workflows::{Host, Workflows, WorkflowsEvent},
    terminal_panel::{self, TerminalView},
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

    fn open_file(&mut self, _: Option<&Location>, _: &std::path::Path, _: &mut App) {}
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
                    Status::Failed(reason) | Status::Skipped(reason) => Some(reason.as_str()),
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
    }
}

fn ready_json(ready: &Ready) -> Value {
    match ready {
        Ready::Exit => json!({ "exit": true }),
        Ready::Output(pattern) => json!({ "output": pattern }),
        Ready::Port(port) => json!({ "port": port }),
        Ready::Manual(prompt) => json!({ "manual": prompt }),
        Ready::Delay(delay) => json!({ "delay": workflow::format_duration(*delay) }),
        Ready::Http(http) => json!({ "http": http_json(http) }),
    }
}

/// `{url, status}` (`status`: the accepted codes, `[]` for 200–399).
fn http_json(http: &workflow::HttpProbe) -> Value {
    json!({ "url": http.url, "status": http.status })
}

/// A step's `restart`, `healthcheck`, `if` and `env_file` as declared.
fn policies_json(step: &workflow::Step) -> Value {
    let format = workflow::format_duration;
    json!({
        "restart": {
            "policy": step.restart.policy.name(),
            "max": step.restart.max,
            "backoff": format(step.restart.backoff),
        },
        "healthcheck": step.healthcheck.as_ref().map(|check| {
            let probe = match &check.probe {
                workflow::Probe::Command(command) => json!({ "test": command }),
                workflow::Probe::Http(http) => json!({ "http": http_json(http) }),
                workflow::Probe::Port(port) => json!({ "port": port }),
            };
            json!({
                "probe": probe,
                "interval": format(check.interval),
                "timeout": format(check.timeout),
                "retries": check.retries,
                "start_period": format(check.start_period),
            })
        }),
        "if": step.condition,
        "env_file": step.env_file.iter().map(|file| json!({ "path": file.path, "required": file.required })).collect::<Vec<_>>(),
    })
}

/// `{"<id>": "ready" | "completed"}`, in the order written.
fn depends_json(step: &workflow::Step) -> Value {
    Value::Object(
        step.depends_on
            .iter()
            .map(|dependency| (dependency.step.clone(), json!(dependency.condition.name())))
            .collect(),
    )
}

/// `{label, path}` or `{label, url}` as declared (`label` null without one).
fn artifact_json(artifact: &Artifact) -> Value {
    match &artifact.target {
        ArtifactTarget::Path(path) => json!({ "label": artifact.label, "path": path }),
        ArtifactTarget::Url(url) => json!({ "label": artifact.label, "url": url }),
    }
}

/// An input as declared: `{id, label, type, description, default,
/// required, placeholder}`, plus `options` (`[{value, label}]`) for a
/// choice and `min`, `max`, `integer` for a number.
fn input_json(input: &Input) -> Value {
    let mut value = json!({
        "id": input.id,
        "label": input.label(),
        "type": input.type_name(),
        "description": input.description,
        "default": input.default.to_json(),
        "required": input.required,
        "placeholder": input.placeholder,
    });
    match &input.kind {
        InputKind::Choice(options) => {
            value["options"] = options
                .iter()
                .map(|option| json!({ "value": option.value, "label": option.label() }))
                .collect();
        }
        InputKind::Number { min, max, integer } => {
            value["min"] = json!(min);
            value["max"] = json!(max);
            value["integer"] = json!(integer);
        }
        InputKind::String | InputKind::Boolean => {}
    }
    value
}

/// Node `at` of a flattened workflow as listed: a step with its fields, or
/// a group with its `steps` nested. `project` is where it runs (inherited
/// from its groups).
fn node_json(nodes: &[StepNode], at: usize) -> Value {
    let node = &nodes[at];
    let step = &node.step;
    let project = step.project.as_ref().map(|spec| {
        crate::builtins::workflows::location_of(spec)
            .map_or_else(|err| err, |location| location.to_string())
    });
    if node.is_group() {
        return json!({
            "id": step.id,
            "path": node.path,
            "kind": "group",
            "title": step.title,
            "depends_on": depends_json(step),
            "if": step.condition,
            "project": project,
            "steps": node.children.iter().map(|&child| node_json(nodes, child)).collect::<Vec<_>>(),
        });
    }
    json!({
        "id": step.id,
        "path": node.path,
        "kind": "step",
        "title": step.title,
        "depends_on": depends_json(step),
        "project": project,
        "run": step.run,
        "check": step.check,
        "ready": ready_json(&step.ready),
        "timeout": step.timeout.map(workflow::format_duration),
        "stop_grace_period": workflow::format_duration(step.stop_grace()),
        "artifacts": step.artifacts.iter().map(artifact_json).collect::<Vec<_>>(),
        "policies": policies_json(step),
    })
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
                    let nodes = workflow::flatten(workflow);
                    (0..nodes.len())
                        .filter(|&at| nodes[at].parent.is_none())
                        .map(|at| node_json(&nodes, at))
                        .collect()
                })
                .unwrap_or_default();
            json!({
                "id": found.id,
                "name": found.name,
                "path": path_json(&found.path),
                "source": found.source.label(),
                "error": found.parsed.as_ref().err(),
                "inputs": found.parsed.as_ref().map_or_else(|_| Vec::new(), |workflow| {
                    workflow.inputs.iter().map(input_json).collect()
                }),
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
    let nodes = active.run.nodes();
    let steps: Vec<Value> = nodes
        .iter()
        .zip(active.run.steps())
        .enumerate()
        .map(|(index, (node, state))| {
            let step = &node.step;
            let parent = node.parent.map(|parent| nodes[parent].path.as_str());
            let reason = match &state.status {
                Status::Failed(reason) | Status::Skipped(reason) => Some(reason.as_str()),
                _ => None,
            };
            let elapsed_ms = state
                .elapsed(now)
                .map(|elapsed| elapsed.as_millis() as u64);
            if node.is_group() {
                return json!({
                    "id": node.path,
                    "kind": "group",
                    "parent": parent,
                    "title": step.label(),
                    "status": state.status.name(),
                    "reason": reason,
                    "detail": state.detail,
                    "blocked": state.is_blocked(),
                    "elapsed_ms": elapsed_ms,
                    "steps": node.children.iter().map(|&child| nodes[child].path.as_str()).collect::<Vec<_>>(),
                });
            }
            let checked = state.artifacts.as_ref().and_then(|checked| checked.as_ref().ok());
            let artifacts: Vec<Value> = active
                .run
                .definition(index)
                .artifacts
                .iter()
                .enumerate()
                .map(|(at, artifact)| {
                    let found = checked.and_then(|checked| checked.get(at));
                    let mut value = artifact_json(artifact);
                    value["name"] = json!(artifact.name());
                    value["resolved"] = json!(found.map(|found| &found.resolved));
                    value["exists"] = json!(found.map(|found| found.exists));
                    value
                })
                .collect();
            json!({
                "id": node.path,
                "kind": "step",
                "parent": parent,
                "title": step.label(),
                "status": state.status.name(),
                "reason": reason,
                "detail": state.detail,
                "blocked": state.is_blocked(),
                "last_line": state.last_line(),
                "check_output": state.check_output,
                "exit_code": state.exit.flatten(),
                "exited": state.exit.is_some(),
                "restarts": state.restarts,
                "health_failures": state.health_failures,
                "elapsed_ms": elapsed_ms,
                "location": active.location(index).map(ToString::to_string),
                "terminal": active.terminal(index).map(|view| view.read(cx).run_name().map(str::to_owned)),
                "artifacts": artifacts,
                "artifacts_error": state.artifacts.as_ref().and_then(|checked| checked.as_ref().err()),
            })
        })
        .collect();
    Ok(json!({
        "run": active.id,
        "workflow": found.name,
        "outcome": active.run.outcome().name(),
        "inputs": active.inputs.to_json(),
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
        // A `path` to a workflow file anywhere on this machine is listed
        // from now on.
        if let Some(path) = request["path"].as_str().map(std::path::Path::new)
            && path.is_absolute()
            && path.is_file()
        {
            let path = path.to_path_buf();
            workflows.update(cx, |workflows, _| workflows.remember_file(path));
        }
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
                if command == "workflow_schema" {
                    let schema: Value =
                        serde_json::from_str(workflow::SCHEMA).map_err(|err| err.to_string())?;
                    return Ok(json!({
                        "schema": schema,
                        "file": workflow::SCHEMA_FILE,
                        "modeline": workflow::SCHEMA_MODELINE,
                    }));
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
                    "workflow_convert" => {
                        let converted = workflows.convert(&found, cx)?;
                        Ok(json!({
                            "path": converted.path,
                            "backup": converted.backup,
                            "problems": converted.problems,
                        }))
                    }
                    "workflow_run" => {
                        let inputs = match request.get("inputs") {
                            None | Some(Value::Null) => Default::default(),
                            Some(Value::Object(inputs)) => inputs.clone(),
                            Some(_) => return Err("`inputs` must be an object".into()),
                        };
                        let run = workflows.start(&found, &inputs, cx)?;
                        Ok(json!({ "run": run, "id": found }))
                    }
                    "workflow_status" => status_json(workflows, &found, cx),
                    "workflow_stop" => Ok(json!(workflows.stop(&found, cx))),
                    "workflow_retry" => match request.get("step") {
                        Some(_) => {
                            let step = step(&request)?;
                            Ok(json!(workflows.retry_step(&found, step, cx)))
                        }
                        None => Ok(json!(workflows.retry_failed(&found, cx))),
                    },
                    "workflow_restart" => {
                        let step = step(&request)?;
                        Ok(json!(workflows.restart_step(&found, step, cx)))
                    }
                    "workflow_check_artifacts" => {
                        let step = step(&request)?;
                        Ok(json!(workflows.check_artifacts(&found, step, cx)))
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
