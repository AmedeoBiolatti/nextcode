//! The workflow file: YAML (JSON works too: it is YAML),
//! `name`, `inputs`, `env` and `steps`, with steps and inputs as mappings
//! keyed by id like Compose services or GitHub Actions jobs; parsed into
//! [`Workflow`] and validated (inputs, `depends_on`, what each step does
//! and when it is ready, its artifacts, `${steps.…}` and `${inputs.…}`
//! variables). A step with `steps` of its own is a group: its
//! `depends_on` gates all its steps, which inherit its `project`, `cwd`
//! and `env`; `depends_on` names siblings. Every problem is located at
//! the key or value it is about.

use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};

use serde_json::Value;

use crate::{
    expr,
    inputs::{self, Input, InputValues},
    policy::{self, EnvFile, Healthcheck, HttpProbe, Probe, Restart},
    source::{self, Problem, StepSpan},
    tree,
    vars::{self, ArtifactRef, StepField},
    yaml::{self, Data, Entry, Node},
};

/// How long a stopped command gets after Ctrl+C by default.
pub const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(3);
/// The longest `stop_grace_period`.
pub const MAX_STOP_GRACE: Duration = Duration::from_secs(3600);

/// A parsed, valid workflow.
#[derive(Debug, Clone, PartialEq)]
pub struct Workflow {
    pub name: String,
    /// What a run asks for, in order (see [`crate::inputs`]).
    pub inputs: Vec<Input>,
    /// Environment every step gets (its groups' and its own win).
    pub env: BTreeMap<String, String>,
    /// Dotenv files every step reads (its `env` wins over them).
    pub env_file: Vec<EnvFile>,
    pub steps: Vec<Step>,
    /// The run's input values (set by [`Workflow::with_inputs`]), which
    /// `if` reads.
    pub values: InputValues,
}

impl Workflow {
    /// A copy with every `${inputs.<id>}` in its steps (at any depth:
    /// `run`, `check`, `cwd`, `env` values, artifact paths, `project`) and
    /// its `env` replaced by its value; what a run of it does.
    pub fn with_inputs(&self, values: &InputValues) -> Workflow {
        let mut resolved = self.clone();
        if !values.is_empty() {
            let replace = |text: &mut String| {
                if text.contains("${inputs.") {
                    *text = inputs::substitute(text, values);
                }
            };
            resolved.env.values_mut().for_each(replace);
            for file in &mut resolved.env_file {
                replace(&mut file.path);
            }
            for step in &mut resolved.steps {
                step.map_texts(&replace);
            }
        }
        resolved.values = values.clone();
        resolved
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// Its key in `steps`: unique among its siblings, no `/`.
    pub id: String,
    pub title: Option<String>,
    /// The siblings it waits for, in the order written.
    pub depends_on: Vec<Dependency>,
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
    pub timeout: Option<Duration>,
    /// URLs the command prints (until ready) open in the browser.
    pub open_urls: bool,
    pub env: BTreeMap<String, String>,
    /// Relative to the project (or the home folder without a project).
    pub cwd: Option<String>,
    /// How long its command gets after Ctrl+C before it is killed.
    pub stop_grace: Duration,
    /// Files and links it produces.
    pub artifacts: Vec<Artifact>,
    /// Dotenv files its commands read (its `env` wins); a group's apply
    /// to its steps.
    pub env_file: Vec<EnvFile>,
    /// When it starts again by itself.
    pub restart: Restart,
    /// Probed while it is ready.
    pub healthcheck: Option<Healthcheck>,
    /// Its `if` (see [`crate::expr`]); false when it would start: skipped.
    pub condition: Option<String>,
    /// A group's steps (empty for a step that does something).
    pub steps: Vec<Step>,
}

/// One entry of a step's `depends_on`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    /// A sibling's id.
    pub step: String,
    pub condition: Condition,
}

/// What a dependency must have done before the step starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    /// Ready (the default).
    Ready,
    /// Finished: ready or failed (a report after an evaluation).
    Completed,
}

impl Condition {
    pub fn name(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Completed => "completed",
        }
    }
}

impl Step {
    /// The title, else the id.
    pub fn label(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.id)
    }

    /// A group: it has steps of its own.
    pub fn is_group(&self) -> bool {
        !self.steps.is_empty()
    }

    /// A copy without its `steps`.
    pub fn shallow(&self) -> Step {
        Step {
            id: self.id.clone(),
            title: self.title.clone(),
            depends_on: self.depends_on.clone(),
            project: self.project.clone(),
            run: self.run.clone(),
            terminal: self.terminal.clone(),
            check: self.check.clone(),
            on_fail: self.on_fail.clone(),
            ready: self.ready.clone(),
            timeout: self.timeout,
            open_urls: self.open_urls,
            env: self.env.clone(),
            cwd: self.cwd.clone(),
            stop_grace: self.stop_grace,
            artifacts: self.artifacts.clone(),
            env_file: self.env_file.clone(),
            restart: self.restart.clone(),
            healthcheck: self.healthcheck.clone(),
            condition: self.condition.clone(),
            steps: Vec::new(),
        }
    }

    /// The terminal tab's name.
    pub fn terminal_name(&self) -> &str {
        self.terminal.as_deref().unwrap_or_else(|| self.label())
    }

    pub fn stop_grace(&self) -> Duration {
        self.stop_grace
    }

    /// The ids of its dependencies with this condition.
    pub fn depending(&self, condition: Condition) -> impl Iterator<Item = &str> {
        self.depends_on
            .iter()
            .filter(move |dependency| dependency.condition == condition)
            .map(|dependency| dependency.step.as_str())
    }

    /// The ids it waits for, in the order written.
    pub fn upstream(&self) -> impl Iterator<Item = &str> {
        self.depends_on
            .iter()
            .map(|dependency| dependency.step.as_str())
    }

    /// The texts `${…}` variables are expanded in: `run`, `check`, `cwd`,
    /// `env` values, artifact paths/URLs, `env_file` paths, HTTP URLs and
    /// the health check's command.
    pub fn texts(&self) -> impl Iterator<Item = &str> {
        let ready_url = match &self.ready {
            Ready::Http(probe) => Some(probe.url.as_str()),
            _ => None,
        };
        let health = self
            .healthcheck
            .as_ref()
            .and_then(|check| match &check.probe {
                Probe::Command(command) => Some(command.as_str()),
                Probe::Http(probe) => Some(probe.url.as_str()),
                Probe::Port(_) => None,
            });
        [&self.run, &self.check, &self.cwd]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .chain(self.env.values().map(String::as_str))
            .chain(self.artifacts.iter().map(|artifact| artifact.target.text()))
            .chain(self.env_file.iter().map(|file| file.path.as_str()))
            .chain(ready_url)
            .chain(health)
    }

    /// [`Step::texts`] and the `project`'s: where `${inputs.…}` works.
    pub fn input_texts(&self) -> impl Iterator<Item = &str> {
        self.texts()
            .chain(self.project.iter().flat_map(ProjectSpec::texts))
    }

    /// Applies `change` to each of [`Step::input_texts`], in this step and
    /// every step inside it.
    fn map_texts(&mut self, change: &impl Fn(&mut String)) {
        for text in [&mut self.run, &mut self.check, &mut self.cwd]
            .into_iter()
            .flatten()
        {
            change(text);
        }
        self.env.values_mut().for_each(change);
        for artifact in &mut self.artifacts {
            match &mut artifact.target {
                ArtifactTarget::Path(text) | ArtifactTarget::Url(text) => change(text),
            }
        }
        for file in &mut self.env_file {
            change(&mut file.path);
        }
        if let Ready::Http(probe) = &mut self.ready {
            change(&mut probe.url);
        }
        if let Some(check) = &mut self.healthcheck {
            match &mut check.probe {
                Probe::Command(text) | Probe::Http(HttpProbe { url: text, .. }) => change(text),
                Probe::Port(_) => {}
            }
        }
        if let Some(project) = &mut self.project {
            project.map_texts(change);
        }
        for step in &mut self.steps {
            step.map_texts(change);
        }
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
    /// This long after the command started (or the check passed).
    Delay(Duration),
    /// An HTTP(S) URL answers (where the step runs) with an accepted
    /// status.
    Http(HttpProbe),
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

impl ProjectSpec {
    /// Its paths, distro and config.
    pub fn texts(&self) -> Vec<&str> {
        match self {
            Self::Local(path) | Self::Typed(path) => vec![path],
            Self::Wsl { distro, path } => vec![distro, path],
            Self::DevContainer { folder, config } => {
                let mut texts = folder.texts();
                texts.extend(config.as_deref());
                texts
            }
        }
    }

    fn map_texts(&mut self, change: &impl Fn(&mut String)) {
        match self {
            Self::Local(path) | Self::Typed(path) => change(path),
            Self::Wsl { distro, path } => {
                change(distro);
                change(path);
            }
            Self::DevContainer { folder, config } => {
                folder.map_texts(change);
                if let Some(config) = config {
                    change(config);
                }
            }
        }
    }
}

/// The keys at the top of the file (and `x-…` ones, which hold shared
/// parts for YAML anchors, as in Compose files).
pub const TOP_KEYS: &[&str] = &[
    "name",
    "description",
    "inputs",
    "env",
    "env_file",
    "steps",
    "$schema",
];

/// The keys a step may have.
pub const STEP_KEYS: &[&str] = &[
    "title",
    "depends_on",
    "project",
    "run",
    "terminal",
    "check",
    "on_fail",
    "ready",
    "timeout",
    "stop_grace_period",
    "open_urls",
    "env",
    "cwd",
    "artifacts",
    "env_file",
    "restart",
    "healthcheck",
    "if",
    "steps",
];

/// The keys a group (a step with `steps`) may have.
pub const GROUP_KEYS: &[&str] = &[
    "title",
    "depends_on",
    "if",
    "steps",
    "project",
    "env",
    "env_file",
    "cwd",
];

/// Keys of the format that are designed but not supported yet (see the
/// workflows manual): an error says so instead of "unknown key".
const PLANNED_KEYS: &[&str] = &[];

/// The old JSON format's step keys, and what they became.
const RENAMED_KEYS: &[(&str, &str)] = &[
    ("id", "the step's key in `steps`"),
    ("needs", "`depends_on`"),
    ("after", "`depends_on` with `condition: completed`"),
    ("onFail", "`on_fail`"),
    ("openUrls", "`open_urls`"),
    ("timeout_s", "`timeout` (a duration: `10m`, `90s`)"),
    ("stop_grace_s", "`stop_grace_period` (a duration: `30s`)"),
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

/// A step by its position: the index in the top-level `steps`, then in
/// each group's.
type StepPath = Vec<usize>;

/// What a problem found by the graph checks is about; located through the
/// steps' spans.
enum At {
    /// That step's key.
    StepId(StepPath),
    /// One of that step's `depends_on` ids.
    Dependency(StepPath, String),
    /// Where this text first occurs in that step (else in its groups,
    /// innermost first: it may inherit it; else anywhere: the workflow's
    /// `env`).
    Text(StepPath, String),
}

fn problem(message: impl Into<String>, offset: usize) -> Problem {
    Problem {
        message: message.into(),
        offset: Some(offset),
    }
}

/// [`parse`], with each problem located in `text`: YAML syntax errors
/// where the parser stopped, step and input problems at the key or value
/// they are about, unknown dependencies at the id, `${steps.…}` and
/// `${inputs.…}` problems at the variable, cycles at the step's key.
pub fn check(text: &str, fallback_name: &str) -> Result<Workflow, Vec<Problem>> {
    let document = yaml::load(text);
    if !document.errors.is_empty() {
        return Err(document
            .errors
            .into_iter()
            .map(|error| problem(error.message, error.offset))
            .collect());
    }
    let file_problem = |message: &str| {
        vec![Problem {
            message: message.to_owned(),
            offset: None,
        }]
    };
    let Some(root) = document.root else {
        return Err(file_problem("missing `steps`"));
    };
    let Some(entries) = root.as_map() else {
        return Err(vec![problem(
            "a workflow is a mapping with `name` and `steps`",
            root.start,
        )]);
    };
    let mut problems: Vec<Problem> = Vec::new();
    for entry in entries {
        if !TOP_KEYS.contains(&entry.key.as_str()) && !entry.key.starts_with("x-") {
            problems.push(problem(
                format!(
                    "unknown key `{}` (known: {}, and `x-…` keys for shared parts)",
                    entry.key,
                    TOP_KEYS[..6].join(", ")
                ),
                entry.key_start,
            ));
        }
    }
    let name = match root.get("name") {
        None => fallback_name.to_owned(),
        Some(node) => match node.text() {
            Some(name) if !name.trim().is_empty() => name.to_owned(),
            _ => {
                problems.push(problem("`name` must be a non-empty string", node.start));
                fallback_name.to_owned()
            }
        },
    };
    let (inputs, input_ids) = match root.get("inputs") {
        None => (Vec::new(), Vec::new()),
        Some(node) => parse_inputs(node, &mut problems),
    };
    let env = match root.get("env") {
        None => BTreeMap::new(),
        Some(node) => env(node, "`env`").unwrap_or_else(|err| {
            problems.push(err);
            BTreeMap::new()
        }),
    };
    let env_file = match root.get("env_file") {
        None => Vec::new(),
        Some(node) => policy::env_files(node).unwrap_or_else(|(message, offset)| {
            problems.push(problem(format!("`env_file`: {message}"), offset));
            Vec::new()
        }),
    };
    let Some(steps_node) = root.get("steps") else {
        problems.push(Problem {
            message: "missing `steps`".into(),
            offset: None,
        });
        return Err(problems);
    };
    let steps = match &steps_node.data {
        Data::Map(entries) => parse_steps(entries, "", &mut problems),
        Data::Null => Vec::new(),
        Data::List(_) => {
            problems.push(problem(
                "`steps` is a mapping of step ids to steps (`api: {run: ...}`), not a list",
                steps_node.start,
            ));
            return Err(problems);
        }
        _ => {
            problems.push(problem(
                "`steps` must be a mapping of step ids to steps",
                steps_node.start,
            ));
            return Err(problems);
        }
    };
    if problems.is_empty() {
        let spans = source::spans_of(&root).0;
        let mut errors = validate(&steps, &env);
        errors.extend(validate_input_refs(&steps, &input_ids));
        errors.extend(validate_conditions(&steps, &env, &input_ids));
        problems.extend(errors.into_iter().map(|(message, at)| Problem {
            offset: locate(text, &spans, at),
            message,
        }));
        if let Some(found) = env_input_problems(&env, &input_ids) {
            problems.extend(found.into_iter().map(|message| {
                let offset = root.get("env").map(|node| node.start);
                Problem { message, offset }
            }));
        }
    }
    if problems.is_empty() {
        Ok(Workflow {
            name,
            inputs,
            env,
            env_file,
            steps,
            values: InputValues::default(),
        })
    } else {
        Err(problems)
    }
}

/// Where a graph problem is in `text`.
fn locate(text: &str, spans: &[StepSpan], at: At) -> Option<usize> {
    match at {
        At::StepId(path) => source::span_at(spans, &path).map(|span| span.start),
        At::Dependency(path, id) => source::span_at(spans, &path).map(|span| {
            span.depends_on
                .iter()
                .find(|(listed, _)| *listed == id)
                .map_or(span.start, |(_, at)| *at)
        }),
        At::Text(path, needle) => (1..=path.len())
            .rev()
            .filter_map(|depth| source::span_at(spans, &path[..depth]))
            .find_map(|span| {
                text.get(span.start..span.end)
                    .and_then(|inside| inside.find(&needle))
                    .map(|at| span.start + at)
            })
            .or_else(|| text.find(&needle))
            .or_else(|| source::span_at(spans, &path).map(|span| span.start)),
    }
}

/// The `inputs` mapping: the inputs that parse and every declared id;
/// problems located at the key they are about.
fn parse_inputs(node: &Node, problems: &mut Vec<Problem>) -> (Vec<Input>, Vec<String>) {
    let entries = match &node.data {
        Data::Map(entries) => entries,
        Data::Null => return (Vec::new(), Vec::new()),
        _ => {
            problems.push(problem(
                "`inputs` is a mapping of input ids to their settings (`env: {type: choice, ...}`)",
                node.start,
            ));
            return (Vec::new(), Vec::new());
        }
    };
    let declared: Vec<(String, Value)> = entries
        .iter()
        .map(|entry| (entry.key.clone(), input_json(&entry.value)))
        .collect();
    let (inputs, ids, found) = inputs::parse(declared);
    for (index, (message, key)) in found {
        let entry = &entries[index];
        let offset = key
            .and_then(|key| entry.value.entry(key))
            .map_or(entry.key_start, |found| found.key_start);
        problems.push(problem(message, offset));
    }
    (inputs, ids)
}

/// An input's settings as JSON; `default` and the options as written
/// (`default: 1.10` is `1.10` for a string input; numbers and booleans
/// read their text).
fn input_json(node: &Node) -> Value {
    let as_text = |node: &Node| match (&node.data, node.text()) {
        (Data::Number(_) | Data::Bool(_), Some(text)) => Value::String(text.to_owned()),
        _ => node.to_json(),
    };
    let Some(entries) = node.as_map() else {
        return node.to_json();
    };
    Value::Object(
        entries
            .iter()
            .map(|entry| {
                let value = match (entry.key.as_str(), &entry.value.data) {
                    ("default", _) => as_text(&entry.value),
                    ("options", Data::List(items)) => {
                        Value::Array(items.iter().map(as_text).collect())
                    }
                    _ => entry.value.to_json(),
                };
                (entry.key.clone(), value)
            })
            .collect(),
    )
}

/// The steps of a `steps` mapping (of the group `group`, qualified; empty:
/// the top level); the ones with problems are left out (and the problems
/// noted).
fn parse_steps(entries: &[Entry], group: &str, problems: &mut Vec<Problem>) -> Vec<Step> {
    entries
        .iter()
        .filter_map(|entry| {
            let path = match group {
                "" => entry.key.clone(),
                group => format!("{group}/{}", entry.key),
            };
            match parse_step(entry, &path, problems) {
                Ok(step) => Some(step),
                Err(err) => {
                    problems.push(err);
                    None
                }
            }
        })
        .collect()
}

/// The step `entry` (qualified id `path`); problems in a group's steps
/// are noted in `problems`.
fn parse_step(entry: &Entry, path: &str, problems: &mut Vec<Problem>) -> Result<Step, Problem> {
    let id = entry.key.clone();
    let at = |message: String, offset: usize| problem(format!("`{path}`: {message}"), offset);
    if id.trim().is_empty() {
        return Err(at("a step id must not be empty".into(), entry.key_start));
    }
    if id.contains('/') {
        return Err(at(
            "a step id must not contain `/` (it joins a group's id and its steps')".into(),
            entry.key_start,
        ));
    }
    let node = &entry.value;
    let empty = Vec::new();
    let fields: &[Entry] = match &node.data {
        Data::Map(entries) => entries,
        Data::Null => &empty,
        _ => {
            return Err(at(
                format!(
                    "a step is a mapping (`run`, `ready`, …), not {}",
                    node.kind()
                ),
                node.start,
            ));
        }
    };
    for field in fields {
        let key = field.key.as_str();
        if STEP_KEYS.contains(&key) || key.starts_with("x-") {
            continue;
        }
        let message = if PLANNED_KEYS.contains(&key) {
            format!("`{key}` is planned but not supported yet")
        } else if let Some((_, now)) = RENAMED_KEYS.iter().find(|(old, _)| *old == key) {
            format!("`{key}` is from the old JSON format; it is now {now}")
        } else {
            format!("unknown key `{key}` (known: {})", STEP_KEYS.join(", "))
        };
        return Err(at(message, field.key_start));
    }
    let get = |key: &str| fields.iter().find(|field| field.key == key);
    let group = match get("steps") {
        None => None,
        Some(field) => match &field.value.data {
            Data::Map(steps) if !steps.is_empty() => Some(steps),
            Data::Map(_) | Data::Null => {
                return Err(at(
                    "a group needs at least one step in `steps`".into(),
                    field.key_start,
                ));
            }
            _ => {
                return Err(at(
                    "`steps` must be a mapping of step ids to steps".into(),
                    field.value.start,
                ));
            }
        },
    };
    if group.is_some()
        && let Some(field) = fields
            .iter()
            .find(|field| !GROUP_KEYS.contains(&field.key.as_str()) && !field.key.starts_with("x-"))
    {
        return Err(at(
            format!(
                "a group cannot have `{}` (a group has {}; its steps do the work)",
                field.key,
                GROUP_KEYS.join(", ")
            ),
            field.key_start,
        ));
    }
    let string = |key: &str| -> Result<Option<String>, Problem> {
        match get(key) {
            None => Ok(None),
            Some(field) if field.value.is_null() => Ok(None),
            Some(field) => match field.value.text() {
                Some(text) => Ok(Some(text.to_owned())),
                None => Err(at(
                    format!("`{key}` must be a string, not {}", field.value.kind()),
                    field.value.start,
                )),
            },
        }
    };
    let depends_on = match get("depends_on") {
        None => Vec::new(),
        Some(field) => {
            dependencies(&field.value).map_err(|(message, offset)| at(message, offset))?
        }
    };
    let project = match get("project") {
        None => None,
        Some(field) => Some(
            project_spec(&field.value)
                .map_err(|err| at(format!("`project`: {err}"), field.value.start))?,
        ),
    };
    let title = string("title")?;
    let run = string("run")?;
    let terminal = string("terminal")?;
    let check = string("check")?;
    let on_fail = string("on_fail")?;
    let cwd = string("cwd")?;
    let ready = match get("ready") {
        None => None,
        Some(field) => Some(
            ready(&field.value)
                .map_err(|(message, offset)| at(format!("`ready`: {message}"), offset))?,
        ),
    };
    let duration_field = |key: &str| -> Result<Option<(Duration, usize)>, Problem> {
        match get(key) {
            None => Ok(None),
            Some(field) => duration(&field.value)
                .map(|duration| Some((duration, field.value.start)))
                .map_err(|err| at(format!("`{key}`: {err}"), field.value.start)),
        }
    };
    let timeout = match duration_field("timeout")? {
        Some((timeout, offset)) if timeout.is_zero() => {
            return Err(at("`timeout` must be longer than 0".into(), offset));
        }
        other => other.map(|(timeout, _)| timeout),
    };
    let stop_grace = match duration_field("stop_grace_period")? {
        Some((grace, offset)) if grace < Duration::from_secs(1) || grace > MAX_STOP_GRACE => {
            return Err(at(
                "`stop_grace_period` must be from 1s to 1h".into(),
                offset,
            ));
        }
        other => other.map(|(grace, _)| grace),
    };
    let artifacts = match get("artifacts") {
        None => Vec::new(),
        Some(field) => match &field.value.data {
            Data::List(items) => {
                let mut artifacts: Vec<Artifact> = Vec::with_capacity(items.len());
                for item in items {
                    let artifact = artifact(item)
                        .map_err(|err| at(format!("`artifacts`: {err}"), item.start))?;
                    if let Some(label) = &artifact.label
                        && artifacts
                            .iter()
                            .any(|other| other.label.as_ref() == Some(label))
                    {
                        return Err(at(
                            format!("`artifacts`: two are labelled `{label}`"),
                            item.start,
                        ));
                    }
                    artifacts.push(artifact);
                }
                artifacts
            }
            _ => {
                return Err(at("`artifacts` must be a list".into(), field.value.start));
            }
        },
    };
    let open_urls = match get("open_urls") {
        None => false,
        Some(field) => match field.value.data {
            Data::Bool(open) => open,
            _ => {
                return Err(at(
                    "`open_urls` must be true or false".into(),
                    field.value.start,
                ));
            }
        },
    };
    let env = match get("env") {
        None => BTreeMap::new(),
        Some(field) => env(&field.value, &format!("`{path}`: `env`"))?,
    };
    let env_file = match get("env_file") {
        None => Vec::new(),
        Some(field) => policy::env_files(&field.value)
            .map_err(|(message, offset)| at(format!("`env_file`: {message}"), offset))?,
    };
    let restart = match get("restart") {
        None => Restart::default(),
        Some(field) => policy::restart(&field.value)
            .map_err(|(message, offset)| at(format!("`restart`: {message}"), offset))?,
    };
    let healthcheck = match get("healthcheck") {
        None => None,
        Some(field) => Some(
            policy::healthcheck(&field.value)
                .map_err(|(message, offset)| at(format!("`healthcheck`: {message}"), offset))?,
        ),
    };
    let condition = match get("if") {
        None => None,
        Some(field) => {
            let Some(text) = field.value.text() else {
                return Err(at(
                    "`if` is an expression: `inputs.env == 'prod'`".into(),
                    field.value.start,
                ));
            };
            if let Err(error) = expr::parse(text) {
                // Where the expression's text starts: after the quote of a
                // quoted scalar (its span is longer than its text).
                let quoted = field.value.end - field.value.start > text.len();
                let start = field.value.start + usize::from(quoted);
                return Err(at(format!("`if`: {}", error.message), start + error.offset));
            }
            Some(text.to_owned())
        }
    };

    let needs_run = |key: &str, message: &str| -> Result<(), Problem> {
        match get(key) {
            Some(field) if group.is_none() && run.is_none() => {
                Err(at(message.into(), field.key_start))
            }
            _ => Ok(()),
        }
    };
    if group.is_none() && run.is_none() && check.is_none() && ready.is_none() {
        return Err(at(
            "needs `run`, `check` or `ready` (or `steps`: a group)".into(),
            entry.key_start,
        ));
    }
    if run.is_none() && matches!(ready, Some(Ready::Exit | Ready::Output(_))) {
        needs_run("ready", "`ready.exit` and `ready.output` need `run`")?;
    }
    needs_run("open_urls", "`open_urls` needs `run`")?;
    needs_run("terminal", "`terminal` needs `run`")?;
    needs_run("stop_grace_period", "`stop_grace_period` needs `run`")?;
    needs_run(
        "restart",
        "`restart` needs `run` (it starts the command again)",
    )?;
    if check.is_none()
        && let Some(field) = get("on_fail")
    {
        return Err(at("`on_fail` needs `check`".into(), field.key_start));
    }
    let steps = group.map_or_else(Vec::new, |steps| parse_steps(steps, path, problems));
    Ok(Step {
        id,
        title,
        depends_on,
        project,
        run,
        terminal,
        check,
        on_fail,
        ready: ready.unwrap_or(Ready::Exit),
        timeout,
        open_urls,
        env,
        cwd,
        stop_grace: stop_grace.unwrap_or(DEFAULT_STOP_GRACE),
        artifacts,
        env_file,
        restart,
        healthcheck,
        condition,
        steps,
    })
}

/// A step's `depends_on`: `api`, `[api, db]` (each must be ready), or a
/// mapping of ids to a condition (`api: ready`, `eval: completed`, or
/// Compose's `eval: {condition: completed}`); with each problem's offset.
fn dependencies(node: &Node) -> Result<Vec<Dependency>, (String, usize)> {
    const SHAPES: &str = "`depends_on` is a step id, a list of step ids, or a mapping of step \
                          ids to `ready` or `completed`";
    let ready = |item: &Node| match item.text() {
        Some(id) => Ok((
            Dependency {
                step: id.to_owned(),
                condition: Condition::Ready,
            },
            item.start,
        )),
        None => Err((SHAPES.to_owned(), item.start)),
    };
    let found: Vec<(Dependency, usize)> = match &node.data {
        Data::Null => Vec::new(),
        Data::List(items) => items.iter().map(ready).collect::<Result<_, _>>()?,
        Data::Map(entries) => entries
            .iter()
            .map(|entry| {
                let condition_node = match entry.value.get("condition") {
                    Some(condition) if entry.value.as_map().is_some_and(|map| map.len() == 1) => {
                        condition
                    }
                    _ if entry.value.as_map().is_some() => {
                        return Err((
                            format!(
                                "`{}`: a dependency's settings are `{{condition: ready}}` or \
                                 `{{condition: completed}}`",
                                entry.key
                            ),
                            entry.value.start,
                        ));
                    }
                    _ => &entry.value,
                };
                let condition = match condition_node.text() {
                    None if condition_node.is_null() => Condition::Ready,
                    Some("ready") => Condition::Ready,
                    Some("completed") => Condition::Completed,
                    _ => {
                        return Err((
                            format!(
                                "`{}`: unknown condition{} (`ready`: it must be ready; \
                                 `completed`: ready or failed)",
                                entry.key,
                                condition_node
                                    .text()
                                    .map(|text| format!(" `{text}`"))
                                    .unwrap_or_default()
                            ),
                            condition_node.start,
                        ));
                    }
                };
                Ok((
                    Dependency {
                        step: entry.key.clone(),
                        condition,
                    },
                    entry.key_start,
                ))
            })
            .collect::<Result<_, _>>()?,
        _ => vec![ready(node)?],
    };
    let mut dependencies: Vec<Dependency> = Vec::with_capacity(found.len());
    for (dependency, offset) in found {
        if dependencies
            .iter()
            .any(|known| known.step == dependency.step)
        {
            return Err((
                format!("`depends_on` lists `{}` twice", dependency.step),
                offset,
            ));
        }
        dependencies.push(dependency);
    }
    Ok(dependencies)
}

/// A step's, a group's or the workflow's `env`: a mapping of names to
/// strings, numbers or booleans (as written). `what` names it in
/// messages.
fn env(node: &Node, what: &str) -> Result<BTreeMap<String, String>, Problem> {
    match &node.data {
        Data::Null => Ok(BTreeMap::new()),
        Data::Map(entries) => entries
            .iter()
            .map(|entry| match entry.value.text() {
                Some(value) if !matches!(entry.value.data, Data::List(_) | Data::Map(_)) => {
                    Ok((entry.key.clone(), value.to_owned()))
                }
                _ => Err(problem(
                    format!(
                        "{what}: `{}` needs a value (a string, number or boolean)",
                        entry.key
                    ),
                    entry.value.start.max(entry.key_start),
                )),
            })
            .collect(),
        _ => Err(problem(
            format!("{what} must be a mapping of names to values"),
            node.start,
        )),
    }
}

/// A Compose-style duration: `90s`, `10m`, `1h30m`, `500ms`, `1.5s`; a
/// plain number is seconds.
pub fn duration(node: &Node) -> Result<Duration, String> {
    match &node.data {
        Data::Number(number) => match number.as_f64() {
            Some(seconds) if seconds >= 0.0 && seconds.is_finite() => {
                Ok(Duration::from_secs_f64(seconds))
            }
            _ => Err("must not be negative".into()),
        },
        Data::String(text) => parse_duration(text),
        _ => Err(format!(
            "must be a duration (`90s`, `10m`, `1h30m`, `500ms`), not {}",
            node.kind()
        )),
    }
}

/// [`duration`] of a text.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let bad = || format!("`{text}` is not a duration (`90s`, `10m`, `1h30m`, `500ms`)");
    let mut rest = text.trim();
    if rest.is_empty() {
        return Err(bad());
    }
    let mut total = 0.0;
    while !rest.is_empty() {
        let number_end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .ok_or_else(bad)?;
        let number: f64 = rest[..number_end].parse().map_err(|_| bad())?;
        rest = &rest[number_end..];
        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let seconds = match &rest[..unit_end] {
            "h" => 3600.0,
            "m" => 60.0,
            "s" => 1.0,
            "ms" => 0.001,
            "us" | "µs" => 0.000_001,
            _ => return Err(bad()),
        };
        total += number * seconds;
        rest = &rest[unit_end..];
    }
    Ok(Duration::from_secs_f64(total))
}

/// A duration as a workflow file writes it: `1h30m`, `90s` → `1m30s`,
/// `500ms`.
pub fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    match millis {
        0 => return "0s".into(),
        1..1000 => return format!("{millis}ms"),
        _ if !millis.is_multiple_of(1000) && millis < 60_000 => {
            return format!("{}s", millis as f64 / 1000.0);
        }
        _ => {}
    }
    let seconds = duration.as_secs();
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    let mut text = String::new();
    for (value, unit) in [(hours, "h"), (minutes, "m"), (seconds, "s")] {
        if value > 0 {
            text.push_str(&format!("{value}{unit}"));
        }
    }
    text
}

/// `"<path or http(s) URL>"`, `{label: ..., path: ...}` or
/// `{label: ..., url: ...}` (`label` optional).
fn artifact(node: &Node) -> Result<Artifact, String> {
    const SHAPES: &str = "each is a path or http(s) URL, `{label: ..., path: ...}` or \
                          `{label: ..., url: ...}`";
    let is_url = |text: &str| text.starts_with("http://") || text.starts_with("https://");
    let (label, target) = match &node.data {
        Data::Map(entries) => {
            if let Some(unknown) = entries
                .iter()
                .find(|entry| !["label", "path", "url"].contains(&entry.key.as_str()))
            {
                return Err(format!("unknown key `{}` ({SHAPES})", unknown.key));
            }
            let text = |key: &str| -> Result<Option<String>, String> {
                match node.get(key) {
                    None => Ok(None),
                    Some(value) => value
                        .text()
                        .map(|text| Some(text.to_owned()))
                        .ok_or_else(|| format!("`{key}` must be a string")),
                }
            };
            let label = text("label")?;
            let target = match (text("path")?, text("url")?) {
                (Some(path), None) => ArtifactTarget::Path(path),
                (None, Some(url)) if is_url(&url) => ArtifactTarget::Url(url),
                (None, Some(url)) => return Err(format!("`{url}` is not an http(s) URL")),
                _ => return Err(format!("a mapping needs `path` or `url` ({SHAPES})")),
            };
            (label, target)
        }
        _ => match node.text() {
            Some(text) if is_url(text) => (None, ArtifactTarget::Url(text.to_owned())),
            Some(text) => (None, ArtifactTarget::Path(text.to_owned())),
            None => return Err(SHAPES.into()),
        },
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

/// One condition: `{exit: true}`, `{output: <regex>}`, `{port: N}`,
/// `{manual: <prompt>}` or `{delay: <duration>}`; with the problem's
/// offset.
fn ready(node: &Node) -> Result<Ready, (String, usize)> {
    const SHAPES: &str = "one of `exit: true`, `output: <regex>`, `port: <number>`, \
                          `http: <url>`, `manual: <prompt>`, `delay: <duration>`";
    let entries = match &node.data {
        Data::Map(entries) => entries,
        _ => return Err((format!("expected {SHAPES}"), node.start)),
    };
    let [entry] = entries.as_slice() else {
        // `{manual: Log in, then Continue}` reads as two keys.
        let hint = if entries.iter().skip(1).any(|entry| entry.value.is_null()) {
            " (a comma inside `{…}` separates keys: quote text that has one)"
        } else {
            ""
        };
        return Err((
            format!("expected exactly one condition: {SHAPES}{hint}"),
            entries.get(1).map_or(node.start, |entry| entry.key_start),
        ));
    };
    let value = &entry.value;
    let wrong = |what: &str| Err((format!("`{}` {what}", entry.key), value.start));
    match entry.key.as_str() {
        "exit" => match value.data {
            Data::Bool(true) => Ok(Ready::Exit),
            _ => wrong("must be true (the command exits with code 0)"),
        },
        "output" => match value.text() {
            Some(pattern) => match regex::Regex::new(pattern) {
                Ok(_) => Ok(Ready::Output(pattern.to_owned())),
                Err(err) => Err((format!("invalid regex: {err}"), value.start)),
            },
            None => wrong("must be a regex"),
        },
        "port" => match &value.data {
            Data::Number(number) => match number.as_u64() {
                Some(port @ 1..=65535) => Ok(Ready::Port(port as u16)),
                _ => wrong("must be 1-65535"),
            },
            _ => wrong("must be a port number"),
        },
        "manual" => match value.text() {
            Some(prompt) => Ok(Ready::Manual(prompt.to_owned())),
            None => wrong("must be the prompt to show"),
        },
        "delay" => match duration(value) {
            Ok(delay) => Ok(Ready::Delay(delay)),
            Err(err) => wrong(&err),
        },
        "http" => policy::http_probe(value).map(Ready::Http),
        "delay_ms" => Err((
            "`delay_ms` is from the old JSON format; it is now `delay` (a duration: `500ms`)"
                .into(),
            entry.key_start,
        )),
        _ => Err((
            format!("unknown condition `{}`: {SHAPES}", entry.key),
            entry.key_start,
        )),
    }
}

/// `{local: C:/p}`, `{wsl: Ubuntu, path: ~/p}`,
/// `{devcontainer: <folder>, config?: ...}` (folder: a string or one of
/// the first two shapes), a typed location string, or the editor's own
/// `{Local: ...}` / `{Wsl: {...}}` / `{DevContainer: {...}}` shape.
pub fn project_spec(node: &Node) -> Result<ProjectSpec, String> {
    const SHAPES: &str = "expected `{local: <path>}`, `{wsl: <distro>, path: <path>}`, \
                          `{devcontainer: <folder>, config: <path>}` or a location string";
    let non_empty = |text: Option<&str>, what: &str| match text {
        Some(text) if !text.trim().is_empty() => Ok(text.to_owned()),
        Some(_) => Err(format!("`{what}` must not be empty")),
        None => Err(SHAPES.to_owned()),
    };
    let entries = match &node.data {
        Data::Map(entries) => entries,
        _ => return Ok(ProjectSpec::Typed(non_empty(node.text(), "project")?)),
    };
    fn text<'n>(node: &'n Node, key: &str) -> Option<&'n str> {
        node.get(key).and_then(Node::text)
    }
    let mut keys: Vec<&str> = entries.iter().map(|entry| entry.key.as_str()).collect();
    keys.sort_unstable();
    let spec = match keys.as_slice() {
        ["local"] => ProjectSpec::Local(non_empty(text(node, "local"), "local")?),
        ["path", "wsl"] => ProjectSpec::Wsl {
            distro: non_empty(text(node, "wsl"), "wsl")?,
            path: non_empty(text(node, "path"), "path")?,
        },
        ["devcontainer"] | ["config", "devcontainer"] => {
            let folder = project_spec(node.get("devcontainer").expect("listed"))?;
            if matches!(folder, ProjectSpec::DevContainer { .. }) {
                return Err("a dev container's folder cannot be another dev container".into());
            }
            ProjectSpec::DevContainer {
                folder: Box::new(folder),
                config: text(node, "config").map(str::to_owned),
            }
        }
        // The editor's serde shape (`recent.json`, `windows.json`).
        ["Local"] => ProjectSpec::Local(non_empty(text(node, "Local"), "Local")?),
        ["Wsl"] => {
            let wsl = node.get("Wsl").expect("listed");
            ProjectSpec::Wsl {
                distro: text(wsl, "distro")
                    .ok_or("`Wsl` needs `distro`")?
                    .to_owned(),
                path: text(wsl, "path").ok_or("`Wsl` needs `path`")?.to_owned(),
            }
        }
        ["DevContainer"] => {
            let container = node.get("DevContainer").expect("listed");
            ProjectSpec::DevContainer {
                folder: Box::new(project_spec(
                    container
                        .get("folder")
                        .ok_or("`DevContainer` needs `folder`")?,
                )?),
                config: text(container, "config").map(str::to_owned),
            }
        }
        _ => return Err(SHAPES.into()),
    };
    Ok(spec)
}

/// Known dependencies, valid `${steps.…}` variables, no cycles (at every
/// level). Ids are unique: they are keys.
fn validate(steps: &[Step], env: &BTreeMap<String, String>) -> Vec<(String, At)> {
    let mut errors = Vec::new();
    check_ids(steps, &mut Vec::new(), None, &mut errors);
    if !errors.is_empty() {
        return errors;
    }
    errors.extend(validate_variables(steps, env));
    check_cycles(steps, &mut Vec::new(), None, &mut errors);
    errors
}

/// ` in group `api`` for the steps of group `api` (qualified), else
/// nothing.
fn in_group(group: Option<&str>) -> String {
    group.map_or_else(String::new, |group| format!(" in group `{group}`"))
}

fn child_path(path: &[usize], index: usize) -> StepPath {
    let mut child = path.to_vec();
    child.push(index);
    child
}

/// Known `depends_on` ids among `steps` (at `path`, in `group`), then the
/// same in each group.
fn check_ids(
    steps: &[Step],
    path: &mut Vec<usize>,
    group: Option<&str>,
    errors: &mut Vec<(String, At)>,
) {
    let in_group = in_group(group);
    for (i, step) in steps.iter().enumerate() {
        for dependency in &step.depends_on {
            if !steps.iter().any(|other| other.id == dependency.step) {
                errors.push((
                    format!(
                        "`{}` depends on unknown step `{}`{in_group}",
                        step.id, dependency.step
                    ),
                    At::Dependency(child_path(path, i), dependency.step.clone()),
                ));
            }
        }
    }
    for (i, step) in steps.iter().enumerate() {
        if step.is_group() {
            let qualified = match group {
                Some(group) => format!("{group}/{}", step.id),
                None => step.id.clone(),
            };
            path.push(i);
            check_ids(&step.steps, path, Some(&qualified), errors);
            path.pop();
        }
    }
}

/// The first dependency cycle among `steps` (ids known), then in each
/// group.
fn check_cycles(
    steps: &[Step],
    path: &mut Vec<usize>,
    group: Option<&str>,
    errors: &mut Vec<(String, At)>,
) {
    let index: HashMap<&str, usize> = steps
        .iter()
        .enumerate()
        .map(|(i, step)| (step.id.as_str(), i))
        .collect();
    // Depth-first search; `chain` is the current chain for the message.
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
        chain: &mut Vec<usize>,
    ) -> Option<(String, usize)> {
        match marks[at] {
            Mark::Done => return None,
            Mark::Active => {
                let start = chain.iter().position(|&p| p == at).unwrap_or(0);
                let mut cycle: Vec<&str> = chain[start..]
                    .iter()
                    .map(|&p| steps[p].id.as_str())
                    .collect();
                cycle.push(&steps[at].id);
                return Some((format!("dependency cycle: {}", cycle.join(" → ")), at));
            }
            Mark::New => {}
        }
        marks[at] = Mark::Active;
        chain.push(at);
        for need in steps[at].upstream() {
            if let Some(cycle) = visit(index[need], steps, index, marks, chain) {
                return Some(cycle);
            }
        }
        chain.pop();
        marks[at] = Mark::Done;
        None
    }
    let mut marks = vec![Mark::New; steps.len()];
    for at in 0..steps.len() {
        if let Some((cycle, at)) = visit(at, steps, &index, &mut marks, &mut Vec::new()) {
            errors.push((
                format!("{cycle}{}", in_group(group)),
                At::StepId(child_path(path, at)),
            ));
            break;
        }
    }
    for (i, step) in steps.iter().enumerate() {
        if step.is_group() {
            let qualified = match group {
                Some(group) => format!("{group}/{}", step.id),
                None => step.id.clone(),
            };
            path.push(i);
            check_cycles(&step.steps, path, Some(&qualified), errors);
            path.pop();
        }
    }
}

/// `${steps.<id>.…}` variables name a step its `depends_on` lists (or an
/// enclosing group's), a known field (a group only has `status`), and an
/// artifact that step declares. Checked in each step that does something,
/// with what it inherits from its groups and the workflow.
fn validate_variables(steps: &[Step], env: &BTreeMap<String, String>) -> Vec<(String, At)> {
    let nodes = tree::flatten_steps(steps, env, &[]);
    let ids: Vec<&str> = nodes.iter().map(|node| node.step.id.as_str()).collect();
    let mut errors = Vec::new();
    for (at, node) in nodes.iter().enumerate() {
        if node.is_group() {
            continue;
        }
        let scope = tree::scope(&nodes, at);
        let position = || {
            let mut position = Vec::with_capacity(node.depth + 1);
            let mut current = Some(at);
            while let Some(index) = current {
                position.push(nodes[index].index);
                current = nodes[index].parent;
            }
            position.reverse();
            position
        };
        for text in node.step.texts() {
            for found in vars::step_refs(text, &ids) {
                let problem = match found {
                    Err(err) => Some((err, text.find("${steps.").map(|at| &text[at..]))),
                    Ok(found) => {
                        let target = scope
                            .iter()
                            .find(|(id, _)| *id == found.step)
                            .map(|&(_, target)| &nodes[target]);
                        let problem = match (target, &found.field) {
                            (None, _) if node.depth == 0 => Some(format!(
                                "`{}` refers to `{}`, which is not in this step's `depends_on`",
                                found.text, found.step
                            )),
                            (None, _) => Some(format!(
                                "`{}` refers to `{}`, which is not in the `depends_on` of this \
                                 step or of its groups",
                                found.text, found.step
                            )),
                            (Some(target), StepField::ExitCode | StepField::Artifact(_))
                                if target.is_group() =>
                            {
                                Some(format!(
                                    "`{}`: `{}` is a group (a group only has `status`)",
                                    found.text, found.step
                                ))
                            }
                            (Some(target), StepField::Artifact(by)) => {
                                let producer = &target.step;
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
                            }
                            (Some(_), _) => None,
                        };
                        problem.map(|problem| (problem, Some(found.text)))
                    }
                };
                if let Some((problem, needle)) = problem {
                    let at = match needle {
                        Some(needle) => At::Text(position(), needle.to_owned()),
                        None => At::StepId(position()),
                    };
                    errors.push((format!("`{}`: {problem}", node.path), at));
                }
            }
        }
    }
    errors
}

/// `${inputs.<id>}` variables name a declared input; checked in every
/// step and group (each in what it writes itself).
fn validate_input_refs(steps: &[Step], ids: &[String]) -> Vec<(String, At)> {
    fn visit(
        steps: &[Step],
        ids: &[String],
        path: &mut Vec<usize>,
        qualified: &str,
        errors: &mut Vec<(String, At)>,
    ) {
        for (index, step) in steps.iter().enumerate() {
            let name = match qualified {
                "" => step.id.clone(),
                group => format!("{group}/{}", step.id),
            };
            path.push(index);
            for text in step.input_texts() {
                for (problem, needle) in input_ref_problems(text, ids) {
                    let at = match needle {
                        Some(needle) => At::Text(path.clone(), needle),
                        None => At::StepId(path.clone()),
                    };
                    errors.push((format!("`{name}`: {problem}"), at));
                }
            }
            visit(&step.steps, ids, path, &name, errors);
            path.pop();
        }
    }
    let mut errors = Vec::new();
    visit(steps, ids, &mut Vec::new(), "", &mut errors);
    errors
}

/// Each `if` names inputs that exist and steps its `depends_on` lists
/// (or an enclosing group's), with a field they have (`status`, and
/// `exit_code` for a step that does something).
fn validate_conditions(
    steps: &[Step],
    env: &BTreeMap<String, String>,
    input_ids: &[String],
) -> Vec<(String, At)> {
    let nodes = tree::flatten_steps(steps, env, &[]);
    let mut errors = Vec::new();
    for (at, node) in nodes.iter().enumerate() {
        let Some(text) = &node.step.condition else {
            continue;
        };
        let Ok(parsed) = expr::parse(text) else {
            continue;
        };
        let scope = tree::scope(&nodes, at);
        let mut position = Vec::new();
        let mut current = Some(at);
        while let Some(index) = current {
            position.push(nodes[index].index);
            current = nodes[index].parent;
        }
        position.reverse();
        for (path, _) in parsed.refs() {
            let written = path.join(".");
            let problem = match path {
                [root, id] if root == "inputs" => {
                    (!input_ids.contains(id)).then(|| match input_ids {
                        [] => format!("`{written}`: the workflow has no `inputs`"),
                        ids => format!("`{written}`: no input `{id}` (inputs: {})", ids.join(", ")),
                    })
                }
                [root, id, field] if root == "steps" => {
                    match scope.iter().find(|(scoped, _)| scoped == id) {
                        None => Some(format!(
                            "`{written}`: `{id}` is not in the `depends_on` of this step or of its groups"
                        )),
                        Some(&(_, target)) => match field.as_str() {
                            "status" => None,
                            "exit_code" if !nodes[target].is_group() => None,
                            "exit_code" => Some(format!(
                                "`{written}`: `{id}` is a group (a group only has `status`)"
                            )),
                            other => Some(format!(
                                "`{written}`: unknown field `{other}` (status, exit_code)"
                            )),
                        },
                    }
                }
                _ => Some(format!(
                    "`{written}`: an `if` reads `inputs.<id>`, `steps.<id>.status` or `steps.<id>.exit_code`"
                )),
            };
            if let Some(problem) = problem {
                errors.push((
                    format!("`{}`: `if`: {problem}", node.path),
                    At::Text(position.clone(), written),
                ));
            }
        }
    }
    errors
}

/// The problems of the `${inputs.…}` variables in `text`, each with the
/// text to point at.
fn input_ref_problems(text: &str, ids: &[String]) -> Vec<(String, Option<String>)> {
    let mut problems = Vec::new();
    for found in inputs::input_refs(text) {
        match found {
            Err(err) => {
                problems.push((err, text.find("${inputs.").map(|at| text[at..].to_owned())))
            }
            Ok(found) if ids.iter().any(|id| id == found.id) => {}
            Ok(found) => {
                let known = match ids {
                    [] => "the workflow has no `inputs`".to_owned(),
                    ids => format!("inputs: {}", ids.join(", ")),
                };
                problems.push((
                    format!("`{}` names no input `{}` ({known})", found.text, found.id),
                    Some(found.text.to_owned()),
                ));
            }
        }
    }
    problems
}

/// Problems of `${inputs.…}` in the workflow's `env`.
fn env_input_problems(env: &BTreeMap<String, String>, ids: &[String]) -> Option<Vec<String>> {
    let problems: Vec<String> = env
        .values()
        .flat_map(|text| input_ref_problems(text, ids))
        .map(|(problem, _)| format!("`env`: {problem}"))
        .collect();
    (!problems.is_empty()).then_some(problems)
}

/// The format as a JSON Schema (draft-07), for editors with a YAML
/// language server and for agents; [`check`] is the authority, the schema
/// says less (it cannot see dependencies, variables or regexes).
pub const SCHEMA: &str = include_str!("../schema/workflow.schema.json");

/// What the schema's copy beside workflow files is called (a
/// `yaml-language-server` modeline finds it: [`SCHEMA_MODELINE`]).
pub const SCHEMA_FILE: &str = "workflow.schema.json";

/// The first line that points YAML editors at [`SCHEMA_FILE`] in the same
/// folder.
pub const SCHEMA_MODELINE: &str = "# yaml-language-server: $schema=./workflow.schema.json";

/// A new workflow file: every field shown and explained.
pub const TEMPLATE: &str = r#"# yaml-language-server: $schema=./workflow.schema.json
# A workflow starts a development environment in one go: checks, commands
# in the terminals of several projects (opened as needed, dev containers
# included), in order, with readiness conditions, browser links and manual
# steps. Its tab (Run > Open Workflow…) shows this file next to the steps'
# graph, which follows your edits; Run All saves and runs it. The command
# palette has "Run Workflow: <name>" too. The format borrows from Docker
# Compose (depends_on, durations, x- keys with anchors) and GitHub Actions
# (inputs).
name: My environment

# Asked for when it runs (a small form; the last values are remembered):
# string, number (min, max, integer), boolean or choice. ${inputs.<id>}
# puts the value into run, check, cwd, env, artifacts and project.
inputs:
  profile:
    label: Profile
    type: choice
    options: [dev, staging, {value: prod, label: Production}]
    default: dev
    description: Settings the API starts with
  branch:
    type: string
    default: main
    placeholder: branch name
    required: true

# Environment for every step (a group's and a step's own `env` win), and
# dotenv files they read (relative to each step's cwd; `env` wins).
env:
  LOG_LEVEL: info
env_file: [{path: .env, required: false}]

# Shared parts: any `x-` key, reused with YAML anchors (&name, *name, <<).
x-api: &api
  project: {devcontainer: C:/src/api}

steps:
  # The key is the step's id, used by depends_on and ${steps.<id>.…}.
  vpn:
    title: VPN connected
    # Runs as a process where the step runs (no `project`: this machine;
    # PowerShell on Windows, sh elsewhere); exit code 0 passes.
    check: ping -n 1 intranet.example.com
    # With `on_fail` a failed check waits for Retry instead of failing the
    # workflow.
    on_fail: Connect the VPN, then press Retry.

  api:
    <<: *api
    depends_on: [vpn]
    # {local: C:/src/api}, {wsl: Ubuntu, path: ~/api},
    # {devcontainer: C:/src/api} or
    # {devcontainer: {wsl: Ubuntu, path: ~/api}, config: ...}.
    # Runs in a terminal tab named `terminal` (default: the title) of the
    # project's Terminal panel (PowerShell on Windows, the login shell
    # elsewhere).
    run: uv run uvicorn app:app --port 8000
    terminal: API
    # exit: true (default) | output: <regex> | port: N | http: <url>
    # | manual: <prompt> | delay: <duration>.
    ready: {http: 'http://localhost:8000/health'}
    # Durations: 90s, 10m, 1h30m, 500ms.
    timeout: 5m
    # Start it again when its command fails (an exit code, a crash after
    # it was ready, a timeout, unhealthy): on_failure; also after a clean
    # exit: always. Or {policy: on_failure, max: 5, backoff: 10s}.
    restart: on_failure
    # Probed every interval while it is ready: `test` (any command, exit
    # 0 is healthy), `http` or `port`; `retries` failures in a row make it
    # unhealthy (it fails, and restarts with the policy above).
    healthcheck: {test: curl -fs localhost:8000/health, interval: 30s, retries: 3}
    env:
      LOG_LEVEL: debug
      PROFILE: ${inputs.profile}
    # Relative to the project; ${workspaceFolder}, ${userHome} and
    # ${env:NAME} work in run, check, cwd and env.
    cwd: .

  web:
    depends_on: [api]
    project: {wsl: Ubuntu, path: ~/web}
    run: git switch ${inputs.branch} && npm run dev
    ready: {output: 'ready in \d+ ms'}
    # http(s) URLs the command prints until it is ready open in the
    # browser (each once).
    open_urls: true

  login:
    depends_on: [web]
    # Quote text with commas inside {…}: a comma there separates keys.
    ready: {manual: 'Log in in the browser, then press Continue.'}

  tests:
    <<: *api
    depends_on: [api]
    # Runs only when this is true once what it depends on is done (else
    # skipped): inputs.<id>, steps.<id>.status / .exit_code, success(),
    # failure(), always(), ==, !=, <, >, &&, ||, !.
    if: inputs.profile != 'prod'
    run: uv run pytest --junitxml=reports/tests.xml
    # Stop sends Ctrl+C, then kills after this long (default 3s).
    stop_grace_period: 30s
    # Files (relative to `cwd`, else the project) or http(s) links the
    # step produces; its details link to them. Also
    # {label: ..., url: https://...} or a plain string.
    artifacts:
      - {label: junit, path: reports/tests.xml}

  report:
    <<: *api
    # `completed`: starts once `tests` finished, ready or failed (`ready`,
    # the default, wants it ready).
    depends_on:
      tests: completed
    # ${steps.<id>.status}, ${steps.<id>.exit_code} and
    # ${steps.<id>.artifacts.<label or index>} read a step listed in
    # depends_on.
    run: uv run python report.py ${steps.tests.artifacts.junit} ${steps.tests.exit_code}

  # A group: `steps` of its own, drawn as one card with a miniature of them
  # (click it to expand). Its depends_on gates all its steps, which inherit
  # its `project`, `cwd` and `env`; ids inside are its own (elsewhere
  # `services/db`), their depends_on name siblings. Steps outside wait for
  # the whole group.
  services:
    title: Services
    depends_on: [vpn]
    project: {wsl: Ubuntu, path: ~/services}
    steps:
      db:
        run: docker compose up db
        ready: {port: 5432}
      seed:
        depends_on: [db]
        run: ./seed.sh
"#;

#[cfg(test)]
mod tests;
