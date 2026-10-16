//! The workflow file: JSON with comments and trailing commas,
//! `{ "name": "...", "steps": [ ... ] }`, parsed into [`Workflow`] and
//! validated (ids, `needs`, what each step does and when it is ready).

use std::collections::{BTreeMap, HashMap};

use serde_json::{Map, Value};

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
];

/// Parses and validates a workflow file; `fallback_name` names it when it
/// has no `name` (the file name). Every problem found is reported, one per
/// line.
pub fn parse(text: &str, fallback_name: &str) -> Result<Workflow, String> {
    let value: Value = serde_json::from_str(&devcontainer::jsonc::to_json(text))
        .map_err(|err| format!("invalid JSON: {err}"))?;
    let Value::Object(mut root) = value else {
        return Err("a workflow is a JSON object with `name` and `steps`".into());
    };
    let mut errors = Vec::new();
    let name = match root.remove("name") {
        None => fallback_name.to_owned(),
        Some(Value::String(name)) if !name.trim().is_empty() => name,
        Some(_) => {
            errors.push("`name` must be a non-empty string".to_owned());
            fallback_name.to_owned()
        }
    };
    let steps = match root.remove("steps") {
        Some(Value::Array(steps)) => steps,
        Some(_) => return Err("`steps` must be an array".into()),
        None => return Err("missing `steps`".into()),
    };
    // `$schema`, `description` and other documentation keys are allowed.
    let steps: Vec<Step> = steps
        .into_iter()
        .enumerate()
        .filter_map(|(index, step)| match parse_step(step) {
            Ok(step) => Some(step),
            Err(err) => {
                errors.push(format!("step {}: {err}", index + 1));
                None
            }
        })
        .collect();
    if errors.is_empty() {
        errors.extend(validate(&steps));
    }
    if errors.is_empty() {
        Ok(Workflow { name, steps })
    } else {
        Err(errors.join("\n"))
    }
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
    let needs = match map.remove("needs") {
        None => Vec::new(),
        Some(Value::String(need)) => vec![need],
        Some(Value::Array(needs)) => needs
            .into_iter()
            .map(|need| match need {
                Value::String(need) => Ok(need),
                _ => Err(at("`needs` lists step ids (strings)".into())),
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(at("`needs` must be an array of step ids".into())),
    };
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
    })
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
fn validate(steps: &[Step]) -> Vec<String> {
    let mut errors = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (i, step) in steps.iter().enumerate() {
        if index.insert(&step.id, i).is_some() {
            errors.push(format!("duplicate step id `{}`", step.id));
        }
    }
    for step in steps {
        for need in &step.needs {
            if !index.contains_key(need.as_str()) {
                errors.push(format!("`{}` needs unknown step `{need}`", step.id));
            }
        }
    }
    if !errors.is_empty() {
        return errors;
    }
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
    ) -> Option<String> {
        match marks[at] {
            Mark::Done => return None,
            Mark::Active => {
                let start = path.iter().position(|&p| p == at).unwrap_or(0);
                let mut cycle: Vec<&str> = path[start..]
                    .iter()
                    .map(|&p| steps[p].id.as_str())
                    .collect();
                cycle.push(&steps[at].id);
                return Some(format!("dependency cycle: {}", cycle.join(" → ")));
            }
            Mark::New => {}
        }
        marks[at] = Mark::Active;
        path.push(at);
        for need in &steps[at].needs {
            if let Some(cycle) = visit(index[need.as_str()], steps, index, marks, path) {
                return Some(cycle);
            }
        }
        path.pop();
        marks[at] = Mark::Done;
        None
    }
    let mut marks = vec![Mark::New; steps.len()];
    for at in 0..steps.len() {
        if let Some(cycle) = visit(at, steps, &index, &mut marks, &mut Vec::new()) {
            errors.push(cycle);
            break;
        }
    }
    errors
}

/// A new workflow file: every field shown and explained.
pub const TEMPLATE: &str = r#"// A workflow starts a development environment in one go: checks, commands
// in the terminals of several projects (opened as needed, dev containers
// included), in order, with readiness conditions, browser links and manual
// steps. Run it from the Workflows panel (View > Workflows) or the command
// palette ("Run Workflow: <name>").
{
  "name": "My environment",
  "steps": [
    {
      // Unique id, used by `needs`.
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
        assert_eq!(workflow.steps.len(), 4);
    }
}
