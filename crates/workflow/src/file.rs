//! The workflow file: JSON with comments and trailing commas,
//! `{ "name": "...", "steps": [ ... ] }`, parsed into [`Workflow`] and
//! validated (ids, `needs` and `after`, what each step does and when it is
//! ready, its artifacts and `${steps.…}` variables).

use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};

use serde_json::{Map, Value};

use crate::{
    source::{self, Problem},
    vars::{self, ArtifactRef, StepField},
};

/// Seconds a stopped command gets after Ctrl+C by default.
pub const DEFAULT_STOP_GRACE_S: u64 = 3;
/// The longest `stop_grace_s`.
pub const MAX_STOP_GRACE_S: u64 = 3600;

/// A parsed, valid workflow.
#[derive(Debug, Clone, PartialEq)]
pub struct Workflow {
    pub name: String,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// Unique within the workflow; `needs` refers to it.
    pub id: String,
    pub title: Option<String>,
    /// Steps that must be ready before this one starts.
    pub needs: Vec<String>,
    /// Steps that must have finished (ready or failed) before this one
    /// starts.
    pub after: Vec<String>,
    /// Where it runs; `None` is the machine the editor runs on.
    pub project: Option<ProjectSpec>,
    /// Command line run in a terminal tab.
    pub run: Option<String>,
    /// Terminal tab name (default: the title, else the id).
    pub terminal: Option<String>,
    /// Command run as a process before `run`; exit code 0 passes.
    pub check: Option<String>,
    /// Shown when the check fails; the step then waits for Retry instead
    /// of failing the workflow.
    pub on_fail: Option<String>,
    pub ready: Ready,
    pub timeout_s: Option<u64>,
    /// URLs the command prints (until ready) open in the browser.
    pub open_urls: bool,
    pub env: BTreeMap<String, String>,
    /// Relative to the project (or the home folder without a project).
    pub cwd: Option<String>,
    /// Seconds its command gets after Ctrl+C before it is killed.
    pub stop_grace_s: u64,
    /// Files and links it produces.
    pub artifacts: Vec<Artifact>,
}

impl Step {
    /// The title, else the id.
    pub fn label(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.id)
    }

    /// The terminal tab's name.
    pub fn terminal_name(&self) -> &str {
        self.terminal.as_deref().unwrap_or_else(|| self.label())
    }

    pub fn stop_grace(&self) -> Duration {
        Duration::from_secs(self.stop_grace_s)
    }

    /// The steps it waits for: `needs`, then `after`.
    pub fn upstream(&self) -> impl Iterator<Item = &str> {
        self.needs.iter().chain(&self.after).map(String::as_str)
    }

    /// The texts `${…}` variables are expanded in: `run`, `check`, `cwd`,
    /// `env` values and artifact paths/URLs.
    pub fn texts(&self) -> impl Iterator<Item = &str> {
        [&self.run, &self.check, &self.cwd]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .chain(self.env.values().map(String::as_str))
            .chain(self.artifacts.iter().map(|artifact| artifact.target.text()))
    }

    /// The artifact a `${steps.<id>.artifacts.…}` variable names.
    pub fn artifact(&self, by: &ArtifactRef) -> Option<usize> {
        match by {
            ArtifactRef::Index(index) => (*index < self.artifacts.len()).then_some(*index),
            ArtifactRef::Label(label) => self
                .artifacts
                .iter()
                .position(|artifact| artifact.label.as_deref() == Some(label)),
        }
    }
}

/// Something a step produces: a file where it runs, or a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub label: Option<String>,
    pub target: ArtifactTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactTarget {
    /// Relative to the step's `cwd` (else its project root, else the home
    /// folder); may hold `${…}` variables.
    Path(String),
    /// An http(s) URL; may hold `${…}` variables.
    Url(String),
}

impl ArtifactTarget {
    pub fn text(&self) -> &str {
        match self {
            Self::Path(text) | Self::Url(text) => text,
        }
    }
}

impl Artifact {
    /// What the UI calls it: the label, else the file name, else the URL.
    pub fn name(&self) -> &str {
        if let Some(label) = &self.label {
            return label;
        }
        match &self.target {
            ArtifactTarget::Path(path) => path
                .trim_end_matches(['/', '\\'])
                .rsplit(['/', '\\'])
                .next()
                .filter(|name| !name.is_empty())
                .unwrap_or(path),
            ArtifactTarget::Url(url) => url,
        }
    }
}

/// When a step counts as done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ready {
    /// `run` exited with code 0 (the default with `run`; without it, the
    /// check passed or the step has nothing to wait for).
    Exit,
    /// A regex matched the command's output (ANSI codes stripped).
    Output(String),
    /// Something listens on this TCP port where the step runs.
    Port(u16),
    /// The user pressed Continue; the text is the prompt.
    Manual(String),
    /// Milliseconds after the command started (or the check passed).
    Delay(u64),
}

/// Where a step runs. The editor maps it to a project location; the
/// shapes are the same as `recent.json`'s, plus shorter ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectSpec {
    /// A folder on the editor's machine.
    Local(String),
    /// A Linux path inside a WSL distro.
    Wsl { distro: String, path: String },
    /// A folder (local or WSL) opened inside its dev container.
    DevContainer {
        folder: Box<ProjectSpec>,
        config: Option<String>,
    },
    /// A location as typed in Open Recent: `C:\src\p`, `Ubuntu:~/p`,
    /// `\\wsl.localhost\Ubuntu\home\me\p`, `devcontainer:<one of those>`.
    Typed(String),
}

/// The keys a step may have.
const STEP_KEYS: &[&str] = &[
    "id",
    "title",
    "needs",
    "after",
    "project",
    "run",
    "terminal",
    "check",
    "onFail",
    "ready",
    "timeout_s",
    "openUrls",
    "env",
    "cwd",
    "stop_grace_s",
    "artifacts",
];

/// Parses and validates a workflow file; `fallback_name` names it when it
/// has no `name` (the file name). Every problem found is reported, one per
/// line.
pub fn parse(text: &str, fallback_name: &str) -> Result<Workflow, String> {
    check(text, fallback_name).map_err(|problems| {
        problems
            .into_iter()
            .map(|problem| problem.message)
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// What a problem found while parsing is about.
enum At {
    File,
    /// The `steps` element at this index.
    Step(usize),
    /// That step's `"id"`.
    StepId(usize),
    /// One of that step's `needs`.
    Need(usize, String),
    /// One of that step's `after`.
    After(usize, String),
    /// Where this text first occurs in that step.
    Text(usize, String),
}

/// [`parse`], with each problem located in `text` where possible: JSON
/// syntax errors where the parser stopped, step problems at the step,
/// unknown `needs`/`after` at the id, `${steps.…}` problems at the
/// variable, duplicates and cycles at the step's `"id"`.
pub fn check(text: &str, fallback_name: &str) -> Result<Workflow, Vec<Problem>> {
    let value: Value = match serde_json::from_str(&devcontainer::jsonc::to_json(text)) {
        Ok(value) => value,
        Err(err) => {
            return Err(vec![Problem {
                message: format!("invalid JSON: {err}"),
                offset: (err.line() > 0).then(|| source::offset_of(text, err.line(), err.column())),
            }]);
        }
    };
    let file_problem = |message: &str| {
        vec![Problem {
            message: message.to_owned(),
            offset: None,
        }]
    };
    let Value::Object(mut root) = value else {
        return Err(file_problem(
            "a workflow is a JSON object with `name` and `steps`",
        ));
    };
    let mut errors: Vec<(String, At)> = Vec::new();
    let name = match root.remove("name") {
        None => fallback_name.to_owned(),
        Some(Value::String(name)) if !name.trim().is_empty() => name,
        Some(_) => {
            errors.push(("`name` must be a non-empty string".to_owned(), At::File));
            fallback_name.to_owned()
        }
    };
    let steps = match root.remove("steps") {
        Some(Value::Array(steps)) => steps,
        Some(_) => return Err(file_problem("`steps` must be an array")),
        None => return Err(file_problem("missing `steps`")),
    };
    // `$schema`, `description` and other documentation keys are allowed.
    let steps: Vec<Step> = steps
        .into_iter()
        .enumerate()
        .filter_map(|(index, step)| match parse_step(step) {
            Ok(step) => Some(step),
            Err(err) => {
                errors.push((format!("step {}: {err}", index + 1), At::Step(index)));
                None
            }
        })
        .collect();
    if errors.is_empty() {
        errors.extend(validate(&steps));
    }
    if errors.is_empty() {
        return Ok(Workflow { name, steps });
    }
    let spans = source::step_spans(text);
    Err(errors
        .into_iter()
        .map(|(message, at)| {
            let offset = match at {
                At::File => None,
                At::Step(index) => spans.get(index).map(|span| span.start),
                At::StepId(index) => spans
                    .get(index)
                    .map(|span| span.id_offset.unwrap_or(span.start)),
                At::Need(index, id) => spans
                    .get(index)
                    .map(|span| listed_at(span, &span.needs, &id)),
                At::After(index, id) => spans
                    .get(index)
                    .map(|span| listed_at(span, &span.after, &id)),
                At::Text(index, needle) => spans.get(index).map(|span| {
                    text.get(span.start..span.end)
                        .and_then(|inside| inside.find(&needle))
                        .map(|at| span.start + at)
                        .or(span.id_offset)
                        .unwrap_or(span.start)
                }),
            };
            Problem { message, offset }
        })
        .collect())
}

/// Where `id` is listed in `listed` (a step's `needs` or `after`), else the
/// step's `"id"`.
fn listed_at(span: &source::StepSpan, listed: &[(String, usize)], id: &str) -> usize {
    listed
        .iter()
        .find(|(listed, _)| listed == id)
        .map(|(_, at)| *at)
        .or(span.id_offset)
        .unwrap_or(span.start)
}

fn parse_step(value: Value) -> Result<Step, String> {
    let Value::Object(mut map) = value else {
        return Err("a step is an object".into());
    };
    if let Some(unknown) = map.keys().find(|key| !STEP_KEYS.contains(&key.as_str())) {
        return Err(format!(
            "unknown key `{unknown}` (known: {})",
            STEP_KEYS.join(", ")
        ));
    }
    let id = string(&mut map, "id")?.ok_or("missing `id`")?;
    if id.trim().is_empty() {
        return Err("`id` must not be empty".into());
    }
    let at = |err: String| format!("`{id}`: {err}");
    let needs = id_list(&mut map, "needs").map_err(at)?;
    let after = id_list(&mut map, "after").map_err(at)?;
    for (i, id) in after.iter().enumerate() {
        if after[..i].contains(id) {
            return Err(at(format!("`after` lists `{id}` twice")));
        }
        if needs.contains(id) {
            return Err(at(format!(
                "`{id}` is in both `needs` and `after` (keep one)"
            )));
        }
    }
    let project = map
        .remove("project")
        .map(|value| project_spec(value).map_err(|err| at(format!("`project`: {err}"))))
        .transpose()?;
    let title = string(&mut map, "title").map_err(at)?;
    let run = string(&mut map, "run").map_err(at)?;
    let terminal = string(&mut map, "terminal").map_err(at)?;
    let check = string(&mut map, "check").map_err(at)?;
    let on_fail = string(&mut map, "onFail").map_err(at)?;
    let cwd = string(&mut map, "cwd").map_err(at)?;
    let ready = map
        .remove("ready")
        .map(|value| ready(value).map_err(|err| at(format!("`ready`: {err}"))))
        .transpose()?;
    let timeout_s = match map.remove("timeout_s") {
        None => None,
        Some(Value::Number(number)) => match number.as_u64() {
            Some(seconds @ 1..) => Some(seconds),
            _ => return Err(at("`timeout_s` must be a positive whole number".into())),
        },
        Some(_) => return Err(at("`timeout_s` must be a number of seconds".into())),
    };
    let stop_grace_s = match map.remove("stop_grace_s") {
        None => None,
        Some(Value::Number(number)) => match number.as_u64() {
            Some(seconds @ 1..=MAX_STOP_GRACE_S) => Some(seconds),
            _ => {
                return Err(at(format!(
                    "`stop_grace_s` must be a whole number of seconds from 1 to {MAX_STOP_GRACE_S}"
                )));
            }
        },
        Some(_) => return Err(at("`stop_grace_s` must be a number of seconds".into())),
    };
    let artifacts = match map.remove("artifacts") {
        None => Vec::new(),
        Some(Value::Array(entries)) => entries
            .into_iter()
            .map(artifact)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| at(format!("`artifacts`: {err}")))?,
        Some(_) => return Err(at("`artifacts` must be an array".into())),
    };
    for (i, artifact) in artifacts.iter().enumerate() {
        if let Some(label) = &artifact.label
            && artifacts[..i]
                .iter()
                .any(|other| other.label.as_ref() == Some(label))
        {
            return Err(at(format!("`artifacts`: two are labelled `{label}`")));
        }
    }
    let open_urls = match map.remove("openUrls") {
        None => false,
        Some(Value::Bool(open)) => open,
        Some(_) => return Err(at("`openUrls` must be true or false".into())),
    };
    let env = match map.remove("env") {
        None => BTreeMap::new(),
        Some(Value::Object(env)) => env
            .into_iter()
            .map(|(name, value)| match value {
                Value::String(value) => Ok((name, value)),
                Value::Number(number) => Ok((name, number.to_string())),
                Value::Bool(flag) => Ok((name, flag.to_string())),
                _ => Err(at(format!("`env.{name}` must be a string"))),
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(at("`env` must be an object".into())),
    };

    let problem = if run.is_none() && check.is_none() && ready.is_none() {
        Some("needs `run`, `check` or `ready`")
    } else if run.is_none() && matches!(ready, Some(Ready::Exit | Ready::Output(_))) {
        Some("`ready.exit` and `ready.output` need `run`")
    } else if run.is_none() && open_urls {
        Some("`openUrls` needs `run`")
    } else if run.is_none() && terminal.is_some() {
        Some("`terminal` needs `run`")
    } else if run.is_none() && stop_grace_s.is_some() {
        Some("`stop_grace_s` needs `run`")
    } else if check.is_none() && on_fail.is_some() {
        Some("`onFail` needs `check`")
    } else {
        None
    };
    if let Some(problem) = problem {
        return Err(at(problem.into()));
    }
    Ok(Step {
        id,
        title,
        needs,
        after,
        project,
        run,
        terminal,
        check,
        on_fail,
        ready: ready.unwrap_or(Ready::Exit),
        timeout_s,
        open_urls,
        env,
        cwd,
        stop_grace_s: stop_grace_s.unwrap_or(DEFAULT_STOP_GRACE_S),
        artifacts,
    })
}

/// A step's `needs` or `after`: one id, or an array of them.
fn id_list(map: &mut Map<String, Value>, key: &str) -> Result<Vec<String>, String> {
    match map.remove(key) {
        None => Ok(Vec::new()),
        Some(Value::String(id)) => Ok(vec![id]),
        Some(Value::Array(ids)) => ids
            .into_iter()
            .map(|id| match id {
                Value::String(id) => Ok(id),
                _ => Err(format!("`{key}` lists step ids (strings)")),
            })
            .collect(),
        Some(_) => Err(format!("`{key}` must be an array of step ids")),
    }
}

/// `"<path or http(s) URL>"`, `{"label": "...", "path": "..."}` or
/// `{"label": "...", "url": "..."}` (`label` optional).
fn artifact(value: Value) -> Result<Artifact, String> {
    const SHAPES: &str = "each is \"<path or http(s) URL>\", {\"label\": \"...\", \"path\": \
                          \"...\"} or {\"label\": \"...\", \"url\": \"...\"}";
    let is_url = |text: &str| text.starts_with("http://") || text.starts_with("https://");
    let (label, target) = match value {
        Value::String(text) if is_url(&text) => (None, ArtifactTarget::Url(text)),
        Value::String(text) => (None, ArtifactTarget::Path(text)),
        Value::Object(mut map) => {
            if let Some(unknown) = map
                .keys()
                .find(|key| !["label", "path", "url"].contains(&key.as_str()))
            {
                return Err(format!("unknown key `{unknown}` ({SHAPES})"));
            }
            let label = string(&mut map, "label")?;
            let target = match (string(&mut map, "path")?, string(&mut map, "url")?) {
                (Some(path), None) => ArtifactTarget::Path(path),
                (None, Some(url)) if is_url(&url) => ArtifactTarget::Url(url),
                (None, Some(url)) => return Err(format!("`{url}` is not an http(s) URL")),
                _ => return Err(format!("an object needs `path` or `url` ({SHAPES})")),
            };
            (label, target)
        }
        _ => return Err(SHAPES.into()),
    };
    if target.text().trim().is_empty() {
        return Err("an artifact's path must not be empty".into());
    }
    if let Some(label) = &label {
        if label.trim().is_empty() {
            return Err("a `label` must not be empty".into());
        }
        if label.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(format!(
                "label `{label}` is a number (numbers address artifacts by position)"
            ));
        }
        if label.contains('}') {
            return Err(format!("label `{label}` must not contain `}}`"));
        }
    }
    Ok(Artifact { label, target })
}

fn string(map: &mut Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match map.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

fn ready(value: Value) -> Result<Ready, String> {
    const SHAPES: &str = "one of {\"exit\": true}, {\"output\": \"<regex>\"}, {\"port\": N}, \
                          {\"manual\": \"<prompt>\"}, {\"delay_ms\": N}";
    let Value::Object(map) = value else {
        return Err(format!("expected {SHAPES}"));
    };
    let mut entries = map.into_iter();
    let (Some((key, value)), None) = (entries.next(), entries.next()) else {
        return Err(format!("expected exactly {SHAPES}"));
    };
    match (key.as_str(), value) {
        ("exit", Value::Bool(true)) => Ok(Ready::Exit),
        ("output", Value::String(pattern)) => match regex::Regex::new(&pattern) {
            Ok(_) => Ok(Ready::Output(pattern)),
            Err(err) => Err(format!("invalid regex: {err}")),
        },
        ("port", Value::Number(number)) => match number.as_u64() {
            Some(port @ 1..=65535) => Ok(Ready::Port(port as u16)),
            _ => Err("`port` must be 1-65535".into()),
        },
        ("manual", Value::String(prompt)) => Ok(Ready::Manual(prompt)),
        ("delay_ms", Value::Number(number)) => number
            .as_u64()
            .map(Ready::Delay)
            .ok_or_else(|| "`delay_ms` must be a whole number".into()),
        _ => Err(format!("expected {SHAPES}")),
    }
}

/// `{"local": "C:/p"}`, `{"wsl": "Ubuntu", "path": "~/p"}`,
/// `{"devcontainer": <folder>, "config"?: "..."}` (folder: a string or one
/// of the first two shapes), a typed location string, or the editor's own
/// `{"Local": ...}` / `{"Wsl": {...}}` / `{"DevContainer": {...}}` shape.
pub fn project_spec(value: Value) -> Result<ProjectSpec, String> {
    const SHAPES: &str = "expected {\"local\": \"<path>\"}, {\"wsl\": \"<distro>\", \"path\": \
                          \"<path>\"}, {\"devcontainer\": <folder>, \"config\"?: \"<path>\"} or a \
                          location string";
    let non_empty = |text: String, what: &str| {
        if text.trim().is_empty() {
            Err(format!("`{what}` must not be empty"))
        } else {
            Ok(text)
        }
    };
    let mut map = match value {
        Value::String(text) => return Ok(ProjectSpec::Typed(non_empty(text, "project")?)),
        Value::Object(map) => map,
        _ => return Err(SHAPES.into()),
    };
    let keys: Vec<&str> = map.keys().map(String::as_str).collect();
    let spec = match keys.as_slice() {
        ["local"] => ProjectSpec::Local(non_empty(
            string(&mut map, "local")?.ok_or(SHAPES)?,
            "local",
        )?),
        ["path", "wsl"] | ["wsl", "path"] => ProjectSpec::Wsl {
            distro: non_empty(string(&mut map, "wsl")?.ok_or(SHAPES)?, "wsl")?,
            path: non_empty(string(&mut map, "path")?.ok_or(SHAPES)?, "path")?,
        },
        ["devcontainer"] | ["devcontainer", "config"] | ["config", "devcontainer"] => {
            let folder = project_spec(map.remove("devcontainer").unwrap_or_default())?;
            if matches!(folder, ProjectSpec::DevContainer { .. }) {
                return Err("a dev container's folder cannot be another dev container".into());
            }
            ProjectSpec::DevContainer {
                folder: Box::new(folder),
                config: string(&mut map, "config")?,
            }
        }
        // The editor's serde shape (`recent.json`, `windows.json`).
        ["Local"] => ProjectSpec::Local(non_empty(
            string(&mut map, "Local")?.ok_or(SHAPES)?,
            "Local",
        )?),
        ["Wsl"] => match map.remove("Wsl") {
            Some(Value::Object(mut wsl)) => ProjectSpec::Wsl {
                distro: string(&mut wsl, "distro")?.ok_or("`Wsl` needs `distro`")?,
                path: string(&mut wsl, "path")?.ok_or("`Wsl` needs `path`")?,
            },
            _ => return Err("`Wsl` is {\"distro\": ..., \"path\": ...}".into()),
        },
        ["DevContainer"] => match map.remove("DevContainer") {
            Some(Value::Object(mut container)) => ProjectSpec::DevContainer {
                folder: Box::new(project_spec(
                    container
                        .remove("folder")
                        .ok_or("`DevContainer` needs `folder`")?,
                )?),
                config: string(&mut container, "config")?,
            },
            _ => return Err("`DevContainer` is {\"folder\": ..., \"config\": ...}".into()),
        },
        _ => return Err(SHAPES.into()),
    };
    Ok(spec)
}

/// Unique ids, known `needs`, no cycles.
fn validate(steps: &[Step]) -> Vec<(String, At)> {
    let mut errors = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (i, step) in steps.iter().enumerate() {
        if index.insert(&step.id, i).is_some() {
            errors.push((format!("duplicate step id `{}`", step.id), At::StepId(i)));
        }
    }
    for (i, step) in steps.iter().enumerate() {
        for need in &step.needs {
            if !index.contains_key(need.as_str()) {
                errors.push((
                    format!("`{}` needs unknown step `{need}`", step.id),
                    At::Need(i, need.clone()),
                ));
            }
        }
        for after in &step.after {
            if !index.contains_key(after.as_str()) {
                errors.push((
                    format!("`{}` runs after unknown step `{after}`", step.id),
                    At::After(i, after.clone()),
                ));
            }
        }
    }
    if !errors.is_empty() {
        return errors;
    }
    errors.extend(validate_variables(steps, &index));
    // Depth-first search; `path` is the current chain for the message.
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        New,
        Active,
        Done,
    }
    fn visit(
        at: usize,
        steps: &[Step],
        index: &HashMap<&str, usize>,
        marks: &mut [Mark],
        path: &mut Vec<usize>,
    ) -> Option<(String, usize)> {
        match marks[at] {
            Mark::Done => return None,
            Mark::Active => {
                let start = path.iter().position(|&p| p == at).unwrap_or(0);
                let mut cycle: Vec<&str> = path[start..]
                    .iter()
                    .map(|&p| steps[p].id.as_str())
                    .collect();
                cycle.push(&steps[at].id);
                return Some((format!("dependency cycle: {}", cycle.join(" → ")), at));
            }
            Mark::New => {}
        }
        marks[at] = Mark::Active;
        path.push(at);
        for need in steps[at].upstream() {
            if let Some(cycle) = visit(index[need], steps, index, marks, path) {
                return Some(cycle);
            }
        }
        path.pop();
        marks[at] = Mark::Done;
        None
    }
    let mut marks = vec![Mark::New; steps.len()];
    for at in 0..steps.len() {
        if let Some((cycle, at)) = visit(at, steps, &index, &mut marks, &mut Vec::new()) {
            errors.push((cycle, At::StepId(at)));
            break;
        }
    }
    errors
}

/// `${steps.<id>.…}` variables name a step in `needs` or `after`, a known
/// field, and an artifact that step declares.
fn validate_variables(steps: &[Step], index: &HashMap<&str, usize>) -> Vec<(String, At)> {
    let mut errors = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let upstream: Vec<&str> = step.upstream().collect();
        let ids: Vec<&str> = index.keys().copied().collect();
        for text in step.texts() {
            for found in vars::step_refs(text, &ids) {
                let problem = match found {
                    Err(err) => Some((err, text.find("${steps.").map(|at| &text[at..]))),
                    Ok(found) => {
                        let problem = if !upstream.contains(&found.step) {
                            Some(format!(
                                "`{}` refers to `{}`, which is not in this step's `needs` or \
                                 `after`",
                                found.text, found.step
                            ))
                        } else if let StepField::Artifact(by) = &found.field {
                            let producer = &steps[index[found.step]];
                            match (producer.artifact(by), by) {
                                (Some(_), _) => None,
                                (None, ArtifactRef::Label(label)) => Some(format!(
                                    "`{}`: `{}` has no artifact labelled `{label}`",
                                    found.text, found.step
                                )),
                                (None, ArtifactRef::Index(_)) => Some(format!(
                                    "`{}`: `{}` has {} artifact(s), numbered from 0",
                                    found.text,
                                    found.step,
                                    producer.artifacts.len()
                                )),
                            }
                        } else {
                            None
                        };
                        problem.map(|problem| (problem, Some(found.text)))
                    }
                };
                if let Some((problem, needle)) = problem {
                    let at = match needle {
                        Some(needle) => At::Text(i, needle.to_owned()),
                        None => At::StepId(i),
                    };
                    errors.push((format!("`{}`: {problem}", step.id), at));
                }
            }
        }
    }
    errors
}

/// A new workflow file: every field shown and explained.
pub const TEMPLATE: &str = r#"// A workflow starts a development environment in one go: checks, commands
// in the terminals of several projects (opened as needed, dev containers
// included), in order, with readiness conditions, browser links and manual
// steps. Its tab (Run > Open Workflow…) shows this file next to the steps'
// graph, which follows your edits; Run All saves and runs it. The command
// palette has "Run Workflow: <name>" too.
{
  "name": "My environment",
  "steps": [
    {
      // Unique id, used by `needs`, `after` and ${steps.<id>.…}.
      "id": "vpn",
      "title": "VPN connected",
      // Runs as a process where the step runs (no `project`: this
      // machine; PowerShell on Windows, sh elsewhere); exit code 0 passes.
      "check": "ping -n 1 intranet.example.com",
      // With `onFail` a failed check waits for Retry instead of failing
      // the workflow.
      "onFail": "Connect the VPN, then press Retry."
    },
    {
      "id": "api",
      "needs": ["vpn"],
      // {"local": "C:/src/api"}, {"wsl": "Ubuntu", "path": "~/api"},
      // {"devcontainer": "C:/src/api"} or
      // {"devcontainer": {"wsl": "Ubuntu", "path": "~/api"}, "config": "..."}.
      "project": { "devcontainer": "C:/src/api" },
      // Runs in a terminal tab named `terminal` (default: the title) of
      // the project's Terminal panel (PowerShell on Windows, the login
      // shell elsewhere).
      "run": "uv run uvicorn app:app --port 8000",
      "terminal": "API",
      // exit (default) | output (regex) | port | manual | delay_ms.
      "ready": { "port": 8000 },
      "timeout_s": 300,
      "env": { "LOG_LEVEL": "debug" },
      // Relative to the project; ${workspaceFolder}, ${userHome} and
      // ${env:NAME} work in run, check, cwd and env.
      "cwd": "."
    },
    {
      "id": "web",
      "needs": ["api"],
      "project": { "wsl": "Ubuntu", "path": "~/web" },
      "run": "npm run dev",
      "ready": { "output": "ready in \\d+ ms" },
      // http(s) URLs the command prints until it is ready open in the
      // browser (each once).
      "openUrls": true
    },
    {
      "id": "login",
      "needs": ["web"],
      "ready": { "manual": "Log in in the browser, then press Continue." }
    },
    {
      "id": "tests",
      "needs": ["api"],
      "project": { "devcontainer": "C:/src/api" },
      "run": "uv run pytest --junitxml=reports/tests.xml",
      // Stop sends Ctrl+C, then kills after this many seconds (default 3).
      "stop_grace_s": 30,
      // Files (relative to `cwd`, else the project) or http(s) links the
      // step produces; its details link to them. Also
      // {"label": "...", "url": "https://..."} or a plain string.
      "artifacts": [{ "label": "junit", "path": "reports/tests.xml" }]
    },
    {
      "id": "report",
      // Starts once `tests` finished, ready or failed (`needs` wants it
      // ready).
      "after": ["tests"],
      "project": { "devcontainer": "C:/src/api" },
      // ${steps.<id>.status}, ${steps.<id>.exit_code} and
      // ${steps.<id>.artifacts.<label or index>} read a step listed in
      // `needs` or `after`.
      "run": "uv run python report.py ${steps.tests.artifacts.junit} ${steps.tests.exit_code}"
    }
  ]
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn steps(text: &str) -> Result<Vec<Step>, String> {
        parse(text, "fallback").map(|workflow| workflow.steps)
    }

    #[test]
    fn parses_comments_trailing_commas_and_every_field() {
        let workflow = parse(
            r#"
            // Comment
            {
              "name": "Env", /* block */
              "steps": [
                {
                  "id": "a",
                  "title": "First",
                  "project": { "local": "C:/src/a" },
                  "run": "serve",
                  "terminal": "Server",
                  "check": "true",
                  "onFail": "fix it",
                  "ready": { "output": "listening on \\d+" },
                  "timeout_s": 30,
                  "openUrls": true,
                  "env": { "A": "1", "N": 2, "B": true, },
                  "cwd": "sub",
                },
                { "id": "b", "needs": ["a"], "ready": { "manual": "Go" } },
                { "id": "c", "needs": "b", "ready": { "delay_ms": 500 } },
                { "id": "d", "check": "x", "ready": { "port": 8080 } },
                { "id": "e", "run": "make", "ready": { "exit": true } },
              ],
            }
            "#,
            "fallback",
        )
        .unwrap();
        assert_eq!(workflow.name, "Env");
        let a = &workflow.steps[0];
        assert_eq!(a.label(), "First");
        assert_eq!(a.terminal_name(), "Server");
        assert_eq!(a.project, Some(ProjectSpec::Local("C:/src/a".into())));
        assert_eq!(a.ready, Ready::Output("listening on \\d+".into()));
        assert_eq!(a.timeout_s, Some(30));
        assert!(a.open_urls);
        assert_eq!(a.env["N"], "2");
        assert_eq!(a.env["B"], "true");
        assert_eq!(a.cwd.as_deref(), Some("sub"));
        assert_eq!(workflow.steps[1].ready, Ready::Manual("Go".into()));
        assert_eq!(workflow.steps[2].needs, vec!["b".to_owned()]);
        assert_eq!(workflow.steps[2].ready, Ready::Delay(500));
        assert_eq!(workflow.steps[3].ready, Ready::Port(8080));
        assert_eq!(workflow.steps[4].ready, Ready::Exit);
        assert_eq!(workflow.steps[4].terminal_name(), "e");
        assert_eq!(parse(r#"{"steps": []}"#, "file").unwrap().name, "file");
    }

    #[test]
    fn parses_every_project_shape() {
        let project = |json: &str| project_spec(serde_json::from_str(json).unwrap());
        assert_eq!(
            project(r#"{"local": "C:/p"}"#),
            Ok(ProjectSpec::Local("C:/p".into()))
        );
        let wsl = ProjectSpec::Wsl {
            distro: "Ubuntu".into(),
            path: "~/x".into(),
        };
        assert_eq!(
            project(r#"{"wsl": "Ubuntu", "path": "~/x"}"#),
            Ok(wsl.clone())
        );
        assert_eq!(
            project(r#"{"path": "~/x", "wsl": "Ubuntu"}"#),
            Ok(wsl.clone())
        );
        assert_eq!(
            project(r#"{"devcontainer": "C:/p"}"#),
            Ok(ProjectSpec::DevContainer {
                folder: Box::new(ProjectSpec::Typed("C:/p".into())),
                config: None,
            })
        );
        assert_eq!(
            project(
                r#"{"devcontainer": {"wsl": "Ubuntu", "path": "~/x"}, "config": "/c/devcontainer.json"}"#
            ),
            Ok(ProjectSpec::DevContainer {
                folder: Box::new(wsl.clone()),
                config: Some("/c/devcontainer.json".into()),
            })
        );
        assert_eq!(
            project(r#""Ubuntu:~/x""#),
            Ok(ProjectSpec::Typed("Ubuntu:~/x".into()))
        );
        // The editor's own serde shape, as in recent.json.
        assert_eq!(
            project(r#"{"Local": "C:\\p"}"#),
            Ok(ProjectSpec::Local("C:\\p".into()))
        );
        assert_eq!(
            project(r#"{"Wsl": {"distro": "Ubuntu", "path": "~/x"}}"#),
            Ok(wsl.clone())
        );
        assert_eq!(
            project(
                r#"{"DevContainer": {"folder": {"Wsl": {"distro": "Ubuntu", "path": "~/x"}}, "config": null}}"#
            ),
            Ok(ProjectSpec::DevContainer {
                folder: Box::new(wsl),
                config: None,
            })
        );
        assert!(project(r#"{"devcontainer": {"devcontainer": "C:/p"}}"#).is_err());
        assert!(project(r#"{"local": ""}"#).is_err());
        assert!(project(r#"{"local": "C:/p", "path": "x"}"#).is_err());
        assert!(project("42").is_err());
    }

    #[test]
    fn rejects_invalid_steps() {
        let error = |text: &str| steps(text).unwrap_err();
        assert!(error(r#"{"steps": [{"run": "x"}]}"#).contains("missing `id`"));
        assert!(error(r#"{"steps": [{"id": "a"}]}"#).contains("needs `run`, `check` or `ready`"));
        assert!(error(r#"{"steps": [{"id": "a", "rum": "x"}]}"#).contains("unknown key `rum`"));
        assert!(
            error(r#"{"steps": [{"id": "a", "check": "x", "ready": {"output": "y"}}]}"#)
                .contains("need `run`")
        );
        assert!(
            error(r#"{"steps": [{"id": "a", "run": "x", "ready": {"output": "("}}]}"#)
                .contains("invalid regex")
        );
        assert!(
            error(r#"{"steps": [{"id": "a", "run": "x", "ready": {"port": 0}}]}"#)
                .contains("1-65535")
        );
        assert!(
            error(r#"{"steps": [{"id": "a", "run": "x", "ready": {"exit": true, "port": 1}}]}"#)
                .contains("exactly")
        );
        assert!(error(r#"{"steps": [{"id": "a", "run": "x", "onFail": "m"}]}"#).contains("onFail"));
        assert!(
            error(r#"{"steps": [{"id": "a", "run": "x", "timeout_s": 0}]}"#).contains("positive")
        );
        assert!(error(r#"{"steps": {}}"#).contains("array"));
        assert!(error("[1]").contains("JSON object"));
        assert!(error("{").contains("invalid JSON"));
    }

    #[test]
    fn reports_duplicates_unknown_needs_and_cycles() {
        let error = |text: &str| steps(text).unwrap_err();
        assert_eq!(
            error(r#"{"steps": [{"id": "a", "run": "x"}, {"id": "a", "run": "y"}]}"#),
            "duplicate step id `a`"
        );
        assert_eq!(
            error(
                r#"{"steps": [{"id": "a", "run": "x", "needs": ["b", "c"]}, {"id": "b", "run": "y"}]}"#
            ),
            "`a` needs unknown step `c`"
        );
        assert_eq!(
            error(
                r#"{"steps": [
                    {"id": "a", "run": "x", "needs": ["c"]},
                    {"id": "b", "run": "y", "needs": ["a"]},
                    {"id": "c", "run": "z", "needs": ["b"]}
                ]}"#
            ),
            "dependency cycle: a → c → b → a"
        );
        assert_eq!(
            error(r#"{"steps": [{"id": "a", "run": "x", "needs": ["a"]}]}"#),
            "dependency cycle: a → a"
        );
        // Several problems at once, one per line.
        let both = error(r#"{"name": 3, "steps": [{"id": "a"}, {"id": "b", "run": 1}]}"#);
        assert_eq!(both.lines().count(), 3, "{both}");
    }

    #[test]
    fn the_template_is_a_valid_workflow() {
        let workflow = parse(TEMPLATE, "template").unwrap();
        assert_eq!(workflow.steps.len(), 6);
        assert_eq!(workflow.steps[5].after, ["tests"]);
        assert_eq!(workflow.steps[4].stop_grace_s, 30);
    }

    #[test]
    fn problems_point_at_where_they_are() {
        let at = |text: &str| -> Vec<(String, Option<(usize, usize)>)> {
            check(text, "f")
                .unwrap_err()
                .into_iter()
                .map(|problem| {
                    let place = problem.line_column(text);
                    (problem.message, place)
                })
                .collect()
        };
        // JSON syntax: where the parser stopped (comments keep positions).
        let broken = "{\n  // c\n  \"steps\": [\n    { \"id\": \"a\" \"run\": \"x\" }\n  ]\n}";
        let problems = at(broken);
        assert!(problems[0].0.starts_with("invalid JSON"), "{problems:?}");
        assert_eq!(problems[0].1.map(|(line, _)| line), Some(4));
        // An unknown `needs` id: at that id.
        let unknown = "{\"steps\": [\n  {\"id\": \"a\", \"run\": \"x\"},\n  {\"id\": \"b\", \"run\": \"y\",\n   \"needs\": [\"a\", \"zz\"]}\n]}";
        assert_eq!(
            at(unknown),
            [("`b` needs unknown step `zz`".to_owned(), Some((4, 19)))]
        );
        // A bad step: at the step; a duplicate: at its second `"id"`.
        let bad = "{\"steps\": [\n  {\"id\": \"a\"},\n  {\"id\": \"a\", \"run\": \"x\"}]}";
        assert_eq!(at(bad)[0].1, Some((2, 3)));
        let duplicate = "{\"steps\": [\n  {\"id\": \"a\", \"run\": \"x\"},\n  {\"id\": \"a\", \"run\": \"x\"}]}";
        assert_eq!(
            at(duplicate),
            [("duplicate step id `a`".to_owned(), Some((3, 4)))]
        );
        // File-level problems have no place.
        assert_eq!(at("{}"), [("missing `steps`".to_owned(), None)]);
    }

    #[test]
    fn parses_after_stop_grace_and_artifacts() {
        let steps = steps(
            r#"{"steps": [
                {"id": "eval", "run": "python eval.py", "stop_grace_s": 60,
                 "cwd": "runs",
                 "artifacts": [
                    "out/metrics.json",
                    "https://dash.example/run/${env:RUN}",
                    {"label": "report", "path": "out/report.html"},
                    {"label": "board", "url": "http://localhost:6006"}
                 ]},
                {"id": "report", "after": "eval", "run": "r ${steps.eval.exit_code}"},
                {"id": "both", "needs": ["report"], "after": ["eval"],
                 "check": "test -f ${steps.eval.artifacts.report}",
                 "env": {"S": "${steps.eval.status}", "M": "${steps.eval.artifacts.0}"}}
            ]}"#,
        )
        .unwrap();
        let eval = &steps[0];
        assert_eq!(eval.stop_grace_s, 60);
        assert_eq!(eval.stop_grace(), Duration::from_secs(60));
        assert_eq!(steps[1].stop_grace_s, DEFAULT_STOP_GRACE_S);
        assert_eq!(
            eval.artifacts,
            [
                Artifact {
                    label: None,
                    target: ArtifactTarget::Path("out/metrics.json".into())
                },
                Artifact {
                    label: None,
                    target: ArtifactTarget::Url("https://dash.example/run/${env:RUN}".into())
                },
                Artifact {
                    label: Some("report".into()),
                    target: ArtifactTarget::Path("out/report.html".into())
                },
                Artifact {
                    label: Some("board".into()),
                    target: ArtifactTarget::Url("http://localhost:6006".into())
                },
            ]
        );
        let names: Vec<&str> = eval.artifacts.iter().map(Artifact::name).collect();
        assert_eq!(
            names,
            [
                "metrics.json",
                "https://dash.example/run/${env:RUN}",
                "report",
                "board"
            ]
        );
        assert_eq!(eval.artifact(&ArtifactRef::Label("report".into())), Some(2));
        assert_eq!(eval.artifact(&ArtifactRef::Index(3)), Some(3));
        assert_eq!(eval.artifact(&ArtifactRef::Index(4)), None);
        assert_eq!(steps[1].after, ["eval"]);
        let upstream: Vec<&str> = steps[2].upstream().collect();
        assert_eq!(upstream, ["report", "eval"]);
    }

    #[test]
    fn rejects_bad_after_stop_grace_and_artifacts() {
        let error = |text: &str| steps(text).unwrap_err();
        let one = |extra: &str| {
            error(&format!(
                r#"{{"steps": [{{"id": "a", "run": "x"}}, {{"id": "b", "run": "y", {extra}}}]}}"#
            ))
        };
        assert_eq!(
            one(r#""after": ["a", "a"]"#),
            "step 2: `b`: `after` lists `a` twice"
        );
        assert_eq!(
            one(r#""needs": "a", "after": ["a"]"#),
            "step 2: `b`: `a` is in both `needs` and `after` (keep one)"
        );
        assert_eq!(
            one(r#""after": ["zz"]"#),
            "`b` runs after unknown step `zz`"
        );
        assert!(one(r#""after": 3"#).contains("`after` must be an array of step ids"));
        for bad in ["0", "3601", "1.5", "-1"] {
            assert!(
                one(&format!(r#""stop_grace_s": {bad}"#)).contains("from 1 to 3600"),
                "{bad}"
            );
        }
        assert!(one(r#""stop_grace_s": "3""#).contains("number of seconds"));
        assert!(
            error(r#"{"steps": [{"id": "a", "check": "x", "stop_grace_s": 5}]}"#)
                .contains("`stop_grace_s` needs `run`")
        );
        assert!(one(r#""artifacts": "x""#).contains("`artifacts` must be an array"));
        assert!(one(r#""artifacts": [3]"#).contains("each is"));
        assert!(one(r#""artifacts": [""]"#).contains("must not be empty"));
        assert!(one(r#""artifacts": [{"label": "l"}]"#).contains("needs `path` or `url`"));
        assert!(
            one(r#""artifacts": [{"path": "p", "url": "http://u"}]"#)
                .contains("needs `path` or `url`")
        );
        assert!(one(r#""artifacts": [{"url": "ftp://u"}]"#).contains("not an http(s) URL"));
        assert!(one(r#""artifacts": [{"path": "p", "name": "n"}]"#).contains("unknown key `name`"));
        assert!(one(r#""artifacts": [{"label": "7", "path": "p"}]"#).contains("is a number"));
        assert!(
            one(r#""artifacts": [{"label": "l", "path": "p"}, {"label": "l", "path": "q"}]"#)
                .contains("two are labelled `l`")
        );
    }

    #[test]
    fn step_variables_must_name_needs_or_after_and_known_artifacts() {
        let error = |text: &str| steps(text).unwrap_err();
        let with = |extra: &str| {
            error(&format!(
                r#"{{"steps": [
                    {{"id": "eval", "run": "x", "artifacts": [{{"label": "log", "path": "l"}}]}},
                    {{"id": "other", "run": "y"}},
                    {{"id": "report", "after": ["eval"], {extra}}}
                ]}}"#
            ))
        };
        assert_eq!(
            with(r#""run": "r ${steps.other.status}""#),
            "`report`: `${steps.other.status}` refers to `other`, which is not in this step's \
             `needs` or `after`"
        );
        assert_eq!(
            with(r#""run": "r", "env": {"X": "${steps.nope.exit_code}"}"#),
            "`report`: `${steps.nope.exit_code}` refers to `nope`, which is not in this step's \
             `needs` or `after`"
        );
        assert_eq!(
            with(r#""run": "r", "cwd": "${steps.eval.artifacts.html}""#),
            "`report`: `${steps.eval.artifacts.html}`: `eval` has no artifact labelled `html`"
        );
        assert_eq!(
            with(r#""check": "${steps.eval.artifacts.1}""#),
            "`report`: `${steps.eval.artifacts.1}`: `eval` has 1 artifact(s), numbered from 0"
        );
        assert!(with(r#""run": "${steps.eval.code}""#).contains("unknown field `code`"));
        assert!(
            with(r#""run": "r", "artifacts": ["${steps.other.artifacts.0}/x"]"#)
                .contains("refers to `other`")
        );
        // At the variable in the text.
        let text = "{\"steps\": [\n  {\"id\": \"a\", \"run\": \"x\"},\n  {\"id\": \"b\",\n   \"run\": \"echo ${steps.a.status}\"}\n]}";
        let problems = check(text, "f").unwrap_err();
        assert_eq!(problems[0].line_column(text), Some((4, 17)));
        // After-cycles, and cycles mixing needs and after, show the path.
        assert_eq!(
            error(
                r#"{"steps": [
                    {"id": "a", "run": "x", "after": ["b"]},
                    {"id": "b", "run": "y", "needs": ["a"]}
                ]}"#
            ),
            "dependency cycle: a → b → a"
        );
        assert_eq!(
            error(r#"{"steps": [{"id": "a", "run": "x", "after": "a"}]}"#),
            "dependency cycle: a → a"
        );
        // An unknown `after` id points at that id.
        let unknown =
            "{\"steps\": [\n  {\"id\": \"b\", \"run\": \"y\",\n   \"after\": [\"zz\"]}\n]}";
        let problems = check(unknown, "f").unwrap_err();
        assert_eq!(problems[0].line_column(unknown), Some((3, 14)));
    }
}
