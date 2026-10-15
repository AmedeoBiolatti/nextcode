//! Run configurations: the model, the files they come from, `${…}`
//! variables, and the command a configuration runs.
//!
//! Both files have VS Code's `launch.json` shape, `{ "configurations":
//! [ … ] }`, read as JSONC (comments, trailing commas):
//!
//! - `.vscode/launch.json`, read for compatibility: `python`/`debugpy`
//!   (launch with `program` or `module`, attach), `lldb` (CodeLLDB: `cargo`
//!   sections or `program`; attach), `cppdbg`/`cppvsdbg` launches (run as
//!   native programs under LLDB). Other types are listed, disabled, with
//!   the reason.
//! - `.next_code/run.json`, next_code's own: the types above plus
//!   `pytest` (`targets`, `args`, `python`), `cargo` (`command`: `run` |
//!   `test`, `package`, `bin` | `example` | `test` | `bench` | `lib: true`,
//!   `features`, `cargoArgs`, `args`), `uv` (`script`, `args`) and `shell`
//!   (`command`).
//!
//! Every entry takes `name`, `cwd` (absolute, or relative to the project
//! folder) and `env`. Keys a type does not use are passed to the debug
//! adapter as they are (`justMyCode`, `stopOnEntry`, …).

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::shell;

pub const LAUNCH_JSON: &str = ".vscode/launch.json";
pub const RUN_JSON: &str = ".next_code/run.json";

/// Where a configuration comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Derived from the project (pyproject, cargo metadata).
    Automatic,
    LaunchJson,
    RunJson,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::LaunchJson => LAUNCH_JSON,
            Self::RunJson => RUN_JSON,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Configuration {
    pub name: String,
    pub source: Source,
    pub kind: Kind,
    /// Working directory as written (`None`: the project folder).
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    /// Why it cannot be used (unsupported type, invalid entry); still
    /// listed so the user sees it was read.
    pub disabled: Option<String>,
    /// Parts of the entry that are ignored (`preLaunchTask`, `envFile`).
    pub notes: Vec<String>,
    /// Remaining keys, passed to the debug adapter.
    pub debug_options: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Kind {
    Python {
        target: PythonTarget,
        args: Vec<String>,
        /// Interpreter path (`python`); default: the project's (`uv run`).
        interpreter: Option<String>,
    },
    Pytest {
        /// Paths or node ids; empty for every test.
        targets: Vec<String>,
        args: Vec<String>,
        interpreter: Option<String>,
    },
    /// A `[project.scripts]` entry point, run with `uv run`.
    UvScript {
        script: String,
        args: Vec<String>,
    },
    Cargo {
        command: CargoCommand,
        package: Option<String>,
        target: Option<CargoTarget>,
        features: Vec<String>,
        /// Options for cargo itself (`--release`, …).
        cargo_args: Vec<String>,
        /// After `--`: the program's arguments, or libtest's (filters).
        args: Vec<String>,
    },
    /// A native executable (debugged with LLDB).
    Program {
        program: String,
        args: Vec<String>,
    },
    Shell {
        command: String,
    },
    /// Attach the debugger to a running process (debug only).
    Attach {
        adapter: Adapter,
        arguments: Map<String, Value>,
    },
    /// Placeholder of a disabled entry.
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PythonTarget {
    Script(String),
    Module(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CargoCommand {
    Run,
    Test,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CargoTarget {
    Lib,
    Bin(String),
    Test(String),
    Example(String),
    Bench(String),
}

impl CargoTarget {
    /// From cargo metadata's target kind (`lib`, `rlib`, `bin`, …).
    pub fn from_kind(kind: &str, name: &str) -> Option<Self> {
        Some(match kind {
            "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro" => Self::Lib,
            "bin" => Self::Bin(name.to_owned()),
            "test" => Self::Test(name.to_owned()),
            "example" => Self::Example(name.to_owned()),
            "bench" => Self::Bench(name.to_owned()),
            _ => return None,
        })
    }

    /// Cargo's target selection flags.
    pub fn args(&self) -> Vec<String> {
        let flag = |flag: &str, name: &str| vec![flag.to_owned(), name.to_owned()];
        match self {
            Self::Lib => vec!["--lib".to_owned()],
            Self::Bin(name) => flag("--bin", name),
            Self::Test(name) => flag("--test", name),
            Self::Example(name) => flag("--example", name),
            Self::Bench(name) => flag("--bench", name),
        }
    }

    /// `lib`, `bin tool`, … for labels.
    pub fn label(&self) -> String {
        match self {
            Self::Lib => "lib".to_owned(),
            Self::Bin(name) => format!("bin {name}"),
            Self::Test(name) => format!("test {name}"),
            Self::Example(name) => format!("example {name}"),
            Self::Bench(name) => format!("bench {name}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Adapter {
    Python,
    Lldb,
}

/// Parses `launch.json` / `run.json` text. Invalid JSON is an error; each
/// bad entry becomes a disabled configuration instead.
pub fn parse_file(text: &str, source: Source) -> Result<Vec<Configuration>, String> {
    let json = devcontainer::jsonc::to_json(text);
    let root: Value = serde_json::from_str(&json).map_err(|err| err.to_string())?;
    let entries = match root.get("configurations") {
        Some(Value::Array(entries)) => entries,
        Some(_) => return Err("`configurations` must be an array".into()),
        None if root.is_object() => return Ok(Vec::new()),
        None => return Err("expected an object with `configurations`".into()),
    };
    Ok(entries
        .iter()
        .enumerate()
        .map(|(index, entry)| parse_entry(index, entry, source))
        .collect())
}

fn parse_entry(index: usize, entry: &Value, source: Source) -> Configuration {
    let mut config = Configuration {
        name: entry
            .get("name")
            .and_then(Value::as_str)
            .map_or_else(|| format!("Configuration {}", index + 1), str::to_owned),
        source,
        kind: Kind::Unsupported,
        cwd: None,
        env: BTreeMap::new(),
        disabled: None,
        notes: Vec::new(),
        debug_options: Map::new(),
    };
    let Some(object) = entry.as_object() else {
        config.disabled = Some("not a JSON object".into());
        return config;
    };
    if let Some(variable) = unsupported_variable(entry) {
        config.disabled = Some(format!("uses {variable}, which next_code does not support"));
        return config;
    }
    let mut rest = object.clone();
    match parse_fields(&mut config, &mut rest) {
        Ok(kind) => {
            config.kind = kind;
            if let Kind::Attach { arguments, .. } = &mut config.kind {
                arguments.append(&mut rest);
            } else {
                config.debug_options = rest;
            }
        }
        Err(reason) => config.disabled = Some(reason),
    }
    config
}

/// The first `${command:…}` / `${input:…}` (interactive VS Code features).
fn unsupported_variable(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => ["${command:", "${input:"].iter().find_map(|prefix| {
            let start = text.find(prefix)?;
            let end = text[start..]
                .find('}')
                .map_or(text.len(), |end| start + end + 1);
            Some(text[start..end].to_owned())
        }),
        Value::Array(items) => items.iter().find_map(unsupported_variable),
        Value::Object(map) => map.values().find_map(unsupported_variable),
        _ => None,
    }
}

/// Keys VS Code uses around the debugger, never for it.
const EDITOR_KEYS: &[&str] = &[
    "name",
    "type",
    "request",
    "presentation",
    "internalConsoleOptions",
    "postDebugTask",
    "purpose",
    "console",
    "windows",
    "linux",
    "osx",
];

fn parse_fields(config: &mut Configuration, rest: &mut Map<String, Value>) -> Result<Kind, String> {
    let ty = take_string(rest, "type")?.ok_or("missing `type`")?;
    let request = take_string(rest, "request")?.unwrap_or_else(|| "launch".into());
    for key in EDITOR_KEYS {
        rest.remove(*key);
    }
    if let Some(task) = rest.remove("preLaunchTask") {
        config.notes.push(format!(
            "preLaunchTask {task} is not run (tasks are not supported)"
        ));
    }
    if rest.remove("envFile").is_some() {
        config.notes.push("envFile is ignored".into());
    }
    config.cwd = take_string(rest, "cwd")?;
    config.env = take_env(rest, "env")?;
    let attach = match request.as_str() {
        "launch" => false,
        "attach" => true,
        other => return Err(format!("unknown request `{other}`")),
    };
    match ty.as_str() {
        "python" | "debugpy" if attach => Ok(Kind::Attach {
            adapter: Adapter::Python,
            arguments: Map::new(),
        }),
        "python" | "debugpy" => {
            let interpreter = take_string(rest, "python")?.or(take_string(rest, "pythonPath")?);
            let target = match (take_string(rest, "program")?, take_string(rest, "module")?) {
                (Some(program), None) => PythonTarget::Script(program),
                (None, Some(module)) => PythonTarget::Module(module),
                (Some(_), Some(_)) => return Err("both `program` and `module` are set".into()),
                (None, None) if rest.contains_key("code") => {
                    return Err("`code` launches are not supported".into());
                }
                (None, None) => return Err("needs `program` or `module`".into()),
            };
            Ok(Kind::Python {
                target,
                args: take_args(rest, "args")?,
                interpreter,
            })
        }
        "lldb" if attach => Ok(Kind::Attach {
            adapter: Adapter::Lldb,
            arguments: Map::new(),
        }),
        "lldb" => {
            let args = take_args(rest, "args")?;
            if let Some(cargo) = rest.remove("cargo") {
                return codelldb_cargo(&cargo, args, &mut config.env);
            }
            let program = take_string(rest, "program")?.ok_or("needs `program` or `cargo`")?;
            Ok(Kind::Program { program, args })
        }
        "cppdbg" | "cppvsdbg" if !attach => {
            let program = take_string(rest, "program")?.ok_or("needs `program`")?;
            let args = take_args(rest, "args")?;
            if let Some(Value::Array(environment)) = rest.remove("environment") {
                for variable in environment {
                    if let (Some(name), Some(value)) = (
                        variable.get("name").and_then(Value::as_str),
                        variable.get("value").and_then(Value::as_str),
                    ) {
                        config.env.insert(name.to_owned(), value.to_owned());
                    }
                }
            }
            // The rest is for gdb/vsdbg; LLDB takes only the entry stop.
            let stop = rest.get("stopAtEntry").cloned();
            rest.clear();
            if let Some(stop) = stop {
                rest.insert("stopOnEntry".into(), stop);
            }
            Ok(Kind::Program { program, args })
        }
        "pytest" => {
            let mut targets = take_args(rest, "targets")?;
            if let Some(target) = take_string(rest, "target")? {
                targets.push(target);
            }
            Ok(Kind::Pytest {
                targets,
                args: take_args(rest, "args")?,
                interpreter: take_string(rest, "python")?,
            })
        }
        "uv" => Ok(Kind::UvScript {
            script: take_string(rest, "script")?.ok_or("needs `script`")?,
            args: take_args(rest, "args")?,
        }),
        "cargo" => {
            let command = match take_string(rest, "command")?.as_deref() {
                None | Some("run") => CargoCommand::Run,
                Some("test") => CargoCommand::Test,
                Some(other) => {
                    return Err(format!(
                        "cargo `command` must be run or test, not `{other}`"
                    ));
                }
            };
            let mut targets = Vec::new();
            if rest
                .remove("lib")
                .is_some_and(|lib| lib == Value::Bool(true))
            {
                targets.push(CargoTarget::Lib);
            }
            for (key, make) in [
                ("bin", CargoTarget::Bin as fn(String) -> CargoTarget),
                ("test", CargoTarget::Test),
                ("example", CargoTarget::Example),
                ("bench", CargoTarget::Bench),
            ] {
                if let Some(name) = take_string(rest, key)? {
                    targets.push(make(name));
                }
            }
            if targets.len() > 1 {
                return Err("set only one of `lib`, `bin`, `test`, `example`, `bench`".into());
            }
            Ok(Kind::Cargo {
                command,
                package: take_string(rest, "package")?,
                target: targets.pop(),
                features: take_features(rest, "features")?,
                cargo_args: take_args(rest, "cargoArgs")?,
                args: take_args(rest, "args")?,
            })
        }
        "shell" => Ok(Kind::Shell {
            command: take_string(rest, "command")?.ok_or("needs `command`")?,
        }),
        "cppdbg" | "cppvsdbg" => Err(format!("`{ty}` {request} is not supported")),
        other => Err(format!(
            "type `{other}` is not supported (python, debugpy, lldb, cppdbg, pytest, cargo, uv, shell)"
        )),
    }
}

/// CodeLLDB's `cargo` section: `["build", "--bin=x", …]` or `{ "args":
/// […], "filter": { "name", "kind" }, "env": {…} }`.
fn codelldb_cargo(
    cargo: &Value,
    args: Vec<String>,
    env: &mut BTreeMap<String, String>,
) -> Result<Kind, String> {
    let (words, filter) = match cargo {
        Value::Array(_) => (cargo.clone(), None),
        Value::Object(object) => {
            let mut object = object.clone();
            env.extend(take_env(&mut object, "env")?);
            (
                object.remove("args").unwrap_or_default(),
                object.remove("filter"),
            )
        }
        _ => return Err("`cargo` must be an array or an object".into()),
    };
    let mut holder = Map::new();
    holder.insert("args".into(), words);
    let words = take_args(&mut holder, "args")?;
    let mut command = None;
    let mut package = None;
    let mut target = None;
    let mut features = Vec::new();
    let mut cargo_args = Vec::new();
    let mut iter = words.into_iter();
    while let Some(word) = iter.next() {
        let (flag, inline) = match word.split_once('=') {
            Some((flag, value)) if word.starts_with("--") => {
                (flag.to_owned(), Some(value.to_owned()))
            }
            _ => (word.clone(), None),
        };
        let value = |iter: &mut std::vec::IntoIter<String>| inline.clone().or_else(|| iter.next());
        match flag.as_str() {
            "build" | "run" if command.is_none() => command = Some(CargoCommand::Run),
            "test" if command.is_none() => command = Some(CargoCommand::Test),
            "--no-run" => {}
            "--message-format" => {
                value(&mut iter);
            }
            "-p" | "--package" => package = value(&mut iter),
            "--lib" => target = Some(CargoTarget::Lib),
            "--bin" => target = value(&mut iter).map(CargoTarget::Bin),
            "--test" => target = value(&mut iter).map(CargoTarget::Test),
            "--example" => target = value(&mut iter).map(CargoTarget::Example),
            "--bench" => target = value(&mut iter).map(CargoTarget::Bench),
            "-F" | "--features" => {
                features.extend(split_features(&value(&mut iter).unwrap_or_default()))
            }
            "--target" | "--profile" | "--manifest-path" | "--target-dir" | "--config" | "-Z"
            | "-j" | "--jobs" => {
                cargo_args.push(flag);
                cargo_args.extend(value(&mut iter));
            }
            _ => cargo_args.push(word),
        }
    }
    let command = command.ok_or("the `cargo` section needs a build or test command")?;
    if target.is_none()
        && let Some(filter) = filter
    {
        let name = filter
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let kind = filter
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        target = CargoTarget::from_kind(kind, name);
    }
    Ok(Kind::Cargo {
        command,
        package,
        target,
        features,
        cargo_args,
        args,
    })
}

fn split_features(text: &str) -> Vec<String> {
    text.split([',', ' '])
        .filter(|feature| !feature.is_empty())
        .map(str::to_owned)
        .collect()
}

fn take_string(rest: &mut Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match rest.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

/// A string array (numbers allowed), or one string split like a shell.
fn take_args(rest: &mut Map<String, Value>, key: &str) -> Result<Vec<String>, String> {
    match rest.remove(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(shell::split(&text)),
        Some(Value::Array(items)) => items
            .into_iter()
            .map(|item| match item {
                Value::String(text) => Ok(text),
                Value::Number(number) => Ok(number.to_string()),
                Value::Bool(flag) => Ok(flag.to_string()),
                _ => Err(format!("`{key}` must hold strings")),
            })
            .collect(),
        Some(_) => Err(format!("`{key}` must be an array of strings")),
    }
}

fn take_features(rest: &mut Map<String, Value>, key: &str) -> Result<Vec<String>, String> {
    match rest.remove(key) {
        Some(Value::String(text)) => Ok(split_features(&text)),
        Some(value) => {
            let mut holder = Map::new();
            holder.insert(key.to_owned(), value);
            take_args(&mut holder, key)
        }
        None => Ok(Vec::new()),
    }
}

/// `{ "NAME": "value" }`; numbers and booleans are written out, `null`
/// skipped.
fn take_env(rest: &mut Map<String, Value>, key: &str) -> Result<BTreeMap<String, String>, String> {
    match rest.remove(key) {
        None | Some(Value::Null) => Ok(BTreeMap::new()),
        Some(Value::Object(map)) => map
            .into_iter()
            .filter(|(_, value)| !value.is_null())
            .map(|(name, value)| match value {
                Value::String(text) => Ok((name, text)),
                Value::Number(number) => Ok((name, number.to_string())),
                Value::Bool(flag) => Ok((name, flag.to_string())),
                _ => Err(format!("`{key}.{name}` must be a string")),
            })
            .collect(),
        Some(_) => Err(format!("`{key}` must be an object")),
    }
}

/// What `${…}` variables expand to.
pub struct Context<'a> {
    /// The project folder, as the project sees it.
    pub workspace_folder: &'a str,
    /// The active editor's file (absolute, project side).
    pub file: Option<&'a str>,
    /// The active editor's 1-based cursor line.
    pub line: Option<usize>,
    /// `\` for local Windows projects, else `/`.
    pub separator: char,
    /// Environment of the machine where the project lives, if known.
    pub env: &'a dyn Fn(&str) -> Option<String>,
}

impl Context<'_> {
    /// Expands `${…}` in `text`. Unknown variables and file variables
    /// without an open file are errors.
    pub fn substitute(&self, text: &str) -> Result<String, String> {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("${") {
            out.push_str(&rest[..start]);
            let end = rest[start..]
                .find('}')
                .ok_or_else(|| format!("unclosed `${{` in `{text}`"))?;
            let name = &rest[start + 2..start + end];
            out.push_str(&self.value(name)?);
            rest = &rest[start + end + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }

    fn value(&self, name: &str) -> Result<String, String> {
        let folder = self.workspace_folder.trim_end_matches(['/', '\\']);
        if let Some(variable) = name.strip_prefix("env:") {
            return Ok((self.env)(variable).unwrap_or_default());
        }
        let file = || {
            self.file
                .ok_or_else(|| format!("${{{name}}} needs an open file"))
        };
        Ok(match name {
            "workspaceFolder" | "workspaceRoot" | "cwd" => folder.to_owned(),
            "workspaceFolderBasename" => base_name(folder).to_owned(),
            "pathSeparator" | "/" => self.separator.to_string(),
            "file" => file()?.to_owned(),
            "relativeFile" => relative(folder, file()?).to_owned(),
            "relativeFileDirname" => dir_name(relative(folder, file()?)).to_owned(),
            "fileBasename" => base_name(file()?).to_owned(),
            "fileBasenameNoExtension" => {
                let base = base_name(file()?);
                base.rsplit_once('.')
                    .map_or(base, |(stem, _)| stem)
                    .to_owned()
            }
            "fileExtname" => {
                let base = base_name(file()?);
                base.rfind('.').map_or("", |dot| &base[dot..]).to_owned()
            }
            "fileDirname" => dir_name(file()?).to_owned(),
            "fileWorkspaceFolder" => folder.to_owned(),
            "lineNumber" => self
                .line
                .ok_or_else(|| format!("${{{name}}} needs an open file"))?
                .to_string(),
            other => return Err(format!("unsupported variable ${{{other}}}")),
        })
    }

    /// `path` made absolute against the project folder.
    pub fn absolute(&self, path: &str) -> String {
        let bytes = path.as_bytes();
        let absolute = path.starts_with(['/', '\\'])
            || (bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic());
        if absolute {
            return path.to_owned();
        }
        let folder = self.workspace_folder.trim_end_matches(['/', '\\']);
        let path = path.trim_start_matches("./");
        if path.is_empty() || path == "." {
            return folder.to_owned();
        }
        let path = if self.separator == '\\' {
            path.replace('/', "\\")
        } else {
            path.to_owned()
        };
        format!("{folder}{}{path}", self.separator)
    }
}

fn base_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn dir_name(path: &str) -> &str {
    path.rfind(['/', '\\']).map_or("", |end| &path[..end])
}

fn relative<'p>(folder: &str, path: &'p str) -> &'p str {
    path.strip_prefix(folder)
        .filter(|rest| rest.starts_with(['/', '\\']))
        .map_or(path, |rest| &rest[1..])
}

/// A configuration with its variables expanded and its directory
/// absolute.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Resolved {
    pub name: String,
    pub kind: Kind,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub debug_options: Map<String, Value>,
}

/// How Python runs where the project lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PythonRunner {
    /// `uv run` (a uv project with uv installed).
    Uv,
    /// This interpreter.
    Interpreter(String),
}

/// What runs: a program with arguments, or a shell line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandLine {
    Process { program: String, args: Vec<String> },
    Shell(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Command {
    pub line: CommandLine,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
}

impl Configuration {
    /// What it runs, briefly (for pickers): `python ${file}`, `cargo run
    /// --bin tool`, …
    pub fn summary(&self) -> String {
        let with_args = |head: String, args: &[String]| {
            if args.is_empty() {
                head
            } else {
                format!("{head} {}", args.join(" "))
            }
        };
        match &self.kind {
            Kind::Python { target, args, .. } => with_args(
                match target {
                    PythonTarget::Script(program) => format!("python {program}"),
                    PythonTarget::Module(module) => format!("python -m {module}"),
                },
                args,
            ),
            Kind::Pytest { targets, args, .. } => {
                with_args("pytest".into(), &[targets.clone(), args.clone()].concat())
            }
            Kind::UvScript { script, args } => with_args(format!("uv run {script}"), args),
            Kind::Cargo {
                command,
                package,
                target,
                ..
            } => {
                let mut text = match command {
                    CargoCommand::Run => "cargo run".to_owned(),
                    CargoCommand::Test => "cargo test".to_owned(),
                };
                if let Some(package) = package {
                    text.push_str(&format!(" -p {package}"));
                }
                if let Some(target) = target {
                    text.push(' ');
                    text.push_str(&target.args().join(" "));
                }
                text
            }
            Kind::Program { program, args } => with_args(program.clone(), args),
            Kind::Shell { command } => command.clone(),
            Kind::Attach { adapter, .. } => match adapter {
                Adapter::Python => "attach (Python)".into(),
                Adapter::Lldb => "attach (LLDB)".into(),
            },
            Kind::Unsupported => "unsupported".into(),
        }
    }

    pub fn resolve(&self, context: &Context) -> Result<Resolved, String> {
        if let Some(reason) = &self.disabled {
            return Err(format!("{} is disabled: {reason}", self.name));
        }
        let sub = |text: &String| context.substitute(text);
        let subs = |items: &Vec<String>| items.iter().map(sub).collect::<Result<Vec<_>, _>>();
        let kind = match &self.kind {
            Kind::Python {
                target,
                args,
                interpreter,
            } => Kind::Python {
                target: match target {
                    PythonTarget::Script(program) => PythonTarget::Script(sub(program)?),
                    PythonTarget::Module(module) => PythonTarget::Module(sub(module)?),
                },
                args: subs(args)?,
                interpreter: interpreter.as_ref().map(sub).transpose()?,
            },
            Kind::Pytest {
                targets,
                args,
                interpreter,
            } => Kind::Pytest {
                targets: subs(targets)?,
                args: subs(args)?,
                interpreter: interpreter.as_ref().map(sub).transpose()?,
            },
            Kind::UvScript { script, args } => Kind::UvScript {
                script: sub(script)?,
                args: subs(args)?,
            },
            Kind::Cargo {
                command,
                package,
                target,
                features,
                cargo_args,
                args,
            } => Kind::Cargo {
                command: *command,
                package: package.as_ref().map(sub).transpose()?,
                target: target.clone(),
                features: subs(features)?,
                cargo_args: subs(cargo_args)?,
                args: subs(args)?,
            },
            Kind::Program { program, args } => Kind::Program {
                program: sub(program)?,
                args: subs(args)?,
            },
            Kind::Shell { command } => Kind::Shell {
                command: sub(command)?,
            },
            Kind::Attach { adapter, arguments } => Kind::Attach {
                adapter: *adapter,
                arguments: substitute_map(arguments, context)?,
            },
            Kind::Unsupported => return Err(format!("{} is not supported", self.name)),
        };
        let cwd = match &self.cwd {
            Some(cwd) => context.absolute(&context.substitute(cwd)?),
            None => context.absolute(""),
        };
        let env = self
            .env
            .iter()
            .map(|(name, value)| Ok((name.clone(), context.substitute(value)?)))
            .collect::<Result<_, String>>()?;
        Ok(Resolved {
            name: self.name.clone(),
            kind,
            cwd,
            env,
            debug_options: substitute_map(&self.debug_options, context)?,
        })
    }
}

fn substitute_map(
    map: &Map<String, Value>,
    context: &Context,
) -> Result<Map<String, Value>, String> {
    map.iter()
        .map(|(key, value)| Ok((key.clone(), substitute_value(value, context)?)))
        .collect()
}

fn substitute_value(value: &Value, context: &Context) -> Result<Value, String> {
    Ok(match value {
        Value::String(text) => Value::String(context.substitute(text)?),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| substitute_value(item, context))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(map) => Value::Object(substitute_map(map, context)?),
        other => other.clone(),
    })
}

impl Resolved {
    /// The command that runs it (not debugging).
    pub fn command(&self, python: &PythonRunner) -> Result<Command, String> {
        let process = |program: &str, args: Vec<String>| CommandLine::Process {
            program: program.to_owned(),
            args,
        };
        // `uv run python …`, or the interpreter itself.
        let python_line =
            |interpreter: &Option<String>, args: Vec<String>| match (interpreter, python) {
                (Some(interpreter), _) | (None, PythonRunner::Interpreter(interpreter)) => {
                    process(interpreter, args)
                }
                (None, PythonRunner::Uv) => process(
                    "uv",
                    ["run", "python"]
                        .map(str::to_owned)
                        .into_iter()
                        .chain(args)
                        .collect(),
                ),
            };
        let line = match &self.kind {
            Kind::Python {
                target,
                args,
                interpreter,
            } => {
                let mut words = match target {
                    PythonTarget::Script(program) => vec![program.clone()],
                    PythonTarget::Module(module) => vec!["-m".to_owned(), module.clone()],
                };
                words.extend(args.iter().cloned());
                python_line(interpreter, words)
            }
            Kind::Pytest {
                targets,
                args,
                interpreter,
            } => {
                let words: Vec<String> = targets.iter().chain(args).cloned().collect();
                match (interpreter, python) {
                    (None, PythonRunner::Uv) => process(
                        "uv",
                        ["run", "pytest"]
                            .map(str::to_owned)
                            .into_iter()
                            .chain(words)
                            .collect(),
                    ),
                    _ => python_line(
                        interpreter,
                        ["-m", "pytest"]
                            .map(str::to_owned)
                            .into_iter()
                            .chain(words)
                            .collect(),
                    ),
                }
            }
            Kind::UvScript { script, args } => process(
                "uv",
                ["run".to_owned(), script.clone()]
                    .into_iter()
                    .chain(args.iter().cloned())
                    .collect(),
            ),
            Kind::Cargo {
                command: CargoCommand::Run,
                target: Some(CargoTarget::Lib | CargoTarget::Test(_) | CargoTarget::Bench(_)),
                ..
            } => return Err("cargo run needs a binary or an example".into()),
            Kind::Cargo { .. } => process("cargo", self.cargo_args(None)),
            Kind::Program { program, args } => process(program, args.clone()),
            Kind::Shell { command } => CommandLine::Shell(command.clone()),
            Kind::Attach { .. } => {
                return Err("attach configurations can only be debugged".into());
            }
            Kind::Unsupported => return Err(format!("{} is not supported", self.name)),
        };
        Ok(Command {
            line,
            cwd: self.cwd.clone(),
            env: self.env.clone(),
        })
    }

    /// `cargo` arguments of a cargo configuration: `run`/`test`, or
    /// `build_command` (`build`, `test --no-run`) with JSON messages and
    /// without the program's arguments.
    pub fn cargo_args(&self, build_command: Option<&[&str]>) -> Vec<String> {
        let Kind::Cargo {
            command,
            package,
            target,
            features,
            cargo_args,
            args,
        } = &self.kind
        else {
            return Vec::new();
        };
        let mut words: Vec<String> = match (build_command, command) {
            (Some(build), _) => build.iter().map(|&word| word.to_owned()).collect(),
            (None, CargoCommand::Run) => vec!["run".into()],
            (None, CargoCommand::Test) => vec!["test".into()],
        };
        if let Some(package) = package {
            words.extend(["--package".to_owned(), package.clone()]);
        }
        if let Some(target) = target {
            words.extend(target.args());
        }
        if !features.is_empty() {
            words.extend(["--features".to_owned(), features.join(",")]);
        }
        words.extend(cargo_args.iter().cloned());
        if build_command.is_some() {
            words.push("--message-format=json-render-diagnostics".into());
        } else if !args.is_empty() {
            words.push("--".into());
            words.extend(args.iter().cloned());
        }
        words
    }
}

/// What the project offers for automatic configurations.
#[derive(Debug, Clone, Default)]
pub struct ProjectFacts {
    /// A Python project (`pyproject.toml`).
    pub python: bool,
    /// `[project.scripts]` names.
    pub uv_scripts: Vec<String>,
    /// Workspace packages with their runnable binaries (cargo metadata).
    pub cargo: Vec<(String, Vec<String>)>,
}

/// Configurations derived from the project.
pub fn automatic(facts: &ProjectFacts) -> Vec<Configuration> {
    let config = |name: String, kind: Kind| Configuration {
        name,
        source: Source::Automatic,
        kind,
        cwd: None,
        env: BTreeMap::new(),
        disabled: None,
        notes: Vec::new(),
        debug_options: Map::new(),
    };
    let mut configs = Vec::new();
    if facts.python {
        configs.push(config(
            "Python: Current File".into(),
            Kind::Python {
                target: PythonTarget::Script("${file}".into()),
                args: Vec::new(),
                interpreter: None,
            },
        ));
        configs.push(config(
            "pytest: All Tests".into(),
            Kind::Pytest {
                targets: Vec::new(),
                args: Vec::new(),
                interpreter: None,
            },
        ));
        for script in &facts.uv_scripts {
            configs.push(config(
                format!("uv run {script}"),
                Kind::UvScript {
                    script: script.clone(),
                    args: Vec::new(),
                },
            ));
        }
    }
    let several = facts.cargo.len() > 1;
    for (package, bins) in &facts.cargo {
        for bin in bins {
            let name = if several {
                format!("cargo run: {package}/{bin}")
            } else {
                format!("cargo run: {bin}")
            };
            configs.push(config(
                name,
                Kind::Cargo {
                    command: CargoCommand::Run,
                    package: Some(package.clone()),
                    target: Some(CargoTarget::Bin(bin.clone())),
                    features: Vec::new(),
                    cargo_args: Vec::new(),
                    args: Vec::new(),
                },
            ));
        }
    }
    if !facts.cargo.is_empty() {
        configs.push(config(
            "cargo test: All Tests".into(),
            Kind::Cargo {
                command: CargoCommand::Test,
                package: None,
                target: None,
                features: Vec::new(),
                cargo_args: if several {
                    vec!["--workspace".into()]
                } else {
                    Vec::new()
                },
                args: Vec::new(),
            },
        ));
    }
    configs
}

/// The starting `.next_code/run.json`: a commented example plus the
/// automatic configurations as templates.
pub fn template(automatic: &[Configuration]) -> String {
    let mut entries: Vec<String> = automatic
        .iter()
        .filter_map(|config| {
            let mut entry = Map::new();
            entry.insert("name".into(), config.name.clone().into());
            match &config.kind {
                Kind::Python { target, .. } => {
                    entry.insert("type".into(), "python".into());
                    match target {
                        PythonTarget::Script(program) => {
                            entry.insert("program".into(), program.clone().into())
                        }
                        PythonTarget::Module(module) => {
                            entry.insert("module".into(), module.clone().into())
                        }
                    };
                    entry.insert("args".into(), Value::Array(Vec::new()));
                }
                Kind::Pytest { .. } => {
                    entry.insert("type".into(), "pytest".into());
                    entry.insert("targets".into(), Value::Array(Vec::new()));
                    entry.insert("args".into(), Value::Array(Vec::new()));
                }
                Kind::UvScript { script, .. } => {
                    entry.insert("type".into(), "uv".into());
                    entry.insert("script".into(), script.clone().into());
                }
                Kind::Cargo {
                    command,
                    package,
                    target,
                    cargo_args,
                    ..
                } => {
                    entry.insert("type".into(), "cargo".into());
                    entry.insert(
                        "command".into(),
                        match command {
                            CargoCommand::Run => "run",
                            CargoCommand::Test => "test",
                        }
                        .into(),
                    );
                    if let Some(package) = package {
                        entry.insert("package".into(), package.clone().into());
                    }
                    if let Some(CargoTarget::Bin(bin)) = target {
                        entry.insert("bin".into(), bin.clone().into());
                    }
                    if !cargo_args.is_empty() {
                        entry.insert("cargoArgs".into(), cargo_args.clone().into());
                    }
                    entry.insert("args".into(), Value::Array(Vec::new()));
                }
                _ => return None,
            }
            entry.insert("env".into(), Value::Object(Map::new()));
            Some(indent(
                &serde_json::to_string_pretty(&Value::Object(entry)).ok()?,
            ))
        })
        .collect();
    if entries.is_empty() {
        entries.push(indent(
            r#"{
  "name": "Hello",
  "type": "shell",
  "command": "echo hello"
}"#,
        ));
    }
    format!(
        r#"// Run configurations for next_code (Run > Select Configuration…).
// Same shape as VS Code's .vscode/launch.json, which is read too.
//
// Types: "python" (program or module, args, python), "pytest" (targets,
// args), "uv" (script), "cargo" (command: run | test, package, bin | test |
// example | lib, features, cargoArgs, args), "lldb" (program, or a CodeLLDB
// "cargo" section), "shell" (command). Every type takes "cwd" and "env".
// Variables: ${{workspaceFolder}}, ${{file}}, ${{relativeFile}},
// ${{fileDirname}}, ${{fileBasenameNoExtension}}, ${{lineNumber}}, ${{env:NAME}}.
// Other keys go to the debugger as they are (justMyCode, stopOnEntry, …).
//
// Example:
//   {{
//     "name": "Serve (debug)",
//     "type": "python",
//     "module": "myapp.server",
//     "args": ["--port", "8000"],
//     "cwd": "${{workspaceFolder}}",
//     "env": {{ "LOG_LEVEL": "debug" }},
//     "justMyCode": false
//   }}
{{
  "configurations": [
{}
  ]
}}
"#,
        entries.join(",\n")
    )
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context<'a>(file: Option<&'a str>, env: &'a dyn Fn(&str) -> Option<String>) -> Context<'a> {
        Context {
            workspace_folder: "/home/me/proj",
            file,
            line: Some(12),
            separator: '/',
            env,
        }
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    const LAUNCH: &str = r#"{
        // VS Code writes comments
        "version": "0.2.0",
        "configurations": [
            {
                "name": "Current file",
                "type": "debugpy",
                "request": "launch",
                "program": "${file}",
                "console": "integratedTerminal",
                "justMyCode": false,
                "args": "--name 'a b'",
                "env": { "DEBUG": "1", "LEVEL": 3, "SKIP": null },
            },
            {
                "name": "Module",
                "type": "python",
                "module": "app.server",
                "cwd": "${workspaceFolder}/src",
                "preLaunchTask": "build",
                "python": "/usr/bin/python3.12"
            },
            {
                "type": "lldb",
                "request": "launch",
                "name": "Debug executable 'tool'",
                "cargo": {
                    "args": ["build", "--bin=tool", "--package=demo", "--release"],
                    "filter": { "name": "tool", "kind": "bin" }
                },
                "args": ["--verbose"],
                "cwd": "${workspaceFolder}"
            },
            {
                "type": "lldb",
                "request": "launch",
                "name": "Debug unit tests in library 'demo'",
                "cargo": {
                    "args": ["test", "--no-run", "--lib", "--package=demo"],
                    "filter": { "name": "demo", "kind": "lib" }
                },
                "args": []
            },
            { "name": "Attach", "type": "debugpy", "request": "attach",
              "connect": { "host": "localhost", "port": 5678 } },
            { "name": "Pick", "type": "debugpy", "request": "attach", "processId": "${command:pickProcess}" },
            { "name": "Node", "type": "node", "request": "launch", "program": "index.js" },
            { "name": "C++", "type": "cppdbg", "request": "launch", "program": "${workspaceFolder}/a.out",
              "environment": [{ "name": "X", "value": "1" }], "MIMode": "gdb", "stopAtEntry": true },
            { "type": "python", "name": "Broken", "program": 3 },
        ],
        "compounds": []
    }"#;

    #[test]
    fn launch_json_entries_parse_with_unsupported_ones_disabled() {
        let configs = parse_file(LAUNCH, Source::LaunchJson).unwrap();
        let names: Vec<&str> = configs.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Current file",
                "Module",
                "Debug executable 'tool'",
                "Debug unit tests in library 'demo'",
                "Attach",
                "Pick",
                "Node",
                "C++",
                "Broken"
            ]
        );
        let current = &configs[0];
        assert_eq!(current.disabled, None);
        assert_eq!(
            current.kind,
            Kind::Python {
                target: PythonTarget::Script("${file}".into()),
                args: vec!["--name".into(), "a b".into()],
                interpreter: None,
            }
        );
        assert_eq!(current.env.get("LEVEL").map(String::as_str), Some("3"));
        assert!(!current.env.contains_key("SKIP"));
        // `console` is the editor's business; `justMyCode` the adapter's.
        assert_eq!(
            current.debug_options.get("justMyCode"),
            Some(&Value::Bool(false))
        );
        assert!(!current.debug_options.contains_key("console"));

        let module = &configs[1];
        assert_eq!(
            module.notes,
            ["preLaunchTask \"build\" is not run (tasks are not supported)"]
        );
        assert!(
            matches!(&module.kind, Kind::Python { interpreter: Some(python), .. } if python == "/usr/bin/python3.12")
        );

        assert_eq!(
            configs[2].kind,
            Kind::Cargo {
                command: CargoCommand::Run,
                package: Some("demo".into()),
                target: Some(CargoTarget::Bin("tool".into())),
                features: Vec::new(),
                cargo_args: vec!["--release".into()],
                args: vec!["--verbose".into()],
            }
        );
        assert!(matches!(
            &configs[3].kind,
            Kind::Cargo { command: CargoCommand::Test, target: Some(CargoTarget::Lib), cargo_args, .. } if cargo_args.is_empty()
        ));
        match &configs[4].kind {
            Kind::Attach {
                adapter: Adapter::Python,
                arguments,
            } => {
                assert_eq!(arguments["connect"]["port"], 5678)
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            configs[5].disabled.as_deref(),
            Some("uses ${command:pickProcess}, which next_code does not support")
        );
        assert!(
            configs[6]
                .disabled
                .as_deref()
                .unwrap()
                .starts_with("type `node` is not supported")
        );
        let cpp = &configs[7];
        assert_eq!(cpp.disabled, None);
        assert_eq!(cpp.env.get("X").map(String::as_str), Some("1"));
        assert_eq!(
            cpp.debug_options.get("stopOnEntry"),
            Some(&Value::Bool(true))
        );
        assert!(!cpp.debug_options.contains_key("MIMode"));
        assert_eq!(
            configs[8].disabled.as_deref(),
            Some("`program` must be a string")
        );
    }

    #[test]
    fn run_json_types_parse() {
        let text = r#"{ "configurations": [
            { "name": "unit", "type": "pytest", "targets": ["tests/unit"], "args": ["-x"] },
            { "name": "cli", "type": "uv", "script": "demo-cli", "args": ["--help"] },
            { "name": "it", "type": "cargo", "command": "test", "test": "it", "features": "a, b", "args": ["--nocapture"] },
            { "name": "both", "type": "cargo", "bin": "x", "lib": true },
            { "name": "sh", "type": "shell", "command": "make lint" },
        ] }"#;
        let configs = parse_file(text, Source::RunJson).unwrap();
        assert_eq!(
            configs[0].kind,
            Kind::Pytest {
                targets: vec!["tests/unit".into()],
                args: vec!["-x".into()],
                interpreter: None
            }
        );
        assert!(matches!(&configs[1].kind, Kind::UvScript { script, .. } if script == "demo-cli"));
        assert_eq!(
            configs[2].kind,
            Kind::Cargo {
                command: CargoCommand::Test,
                package: None,
                target: Some(CargoTarget::Test("it".into())),
                features: vec!["a".into(), "b".into()],
                cargo_args: Vec::new(),
                args: vec!["--nocapture".into()],
            }
        );
        assert!(configs[3].disabled.is_some());
        assert_eq!(
            configs[4].kind,
            Kind::Shell {
                command: "make lint".into()
            }
        );
        assert!(parse_file("{ \"configurations\": 3 }", Source::RunJson).is_err());
        assert!(parse_file("{ nope", Source::RunJson).is_err());
        assert!(parse_file("{}", Source::RunJson).unwrap().is_empty());
    }

    #[test]
    fn variables_expand_and_missing_files_are_reported() {
        let env = |name: &str| (name == "HOME").then(|| "/home/me".to_owned());
        let with_file = context(Some("/home/me/proj/src/pkg/app.py"), &env);
        assert_eq!(
            with_file
                .substitute("${workspaceFolder}|${workspaceFolderBasename}|${relativeFile}|${relativeFileDirname}|${fileBasename}|${fileBasenameNoExtension}|${fileExtname}|${fileDirname}|${lineNumber}|${env:HOME}|${env:NOPE}|${pathSeparator}")
                .unwrap(),
            "/home/me/proj|proj|src/pkg/app.py|src/pkg|app.py|app|.py|/home/me/proj/src/pkg|12|/home/me||/"
        );
        let without = context(None, &no_env);
        assert_eq!(
            without.substitute("${file}").unwrap_err(),
            "${file} needs an open file"
        );
        assert_eq!(
            without.substitute("${input:port}").unwrap_err(),
            "unsupported variable ${input:port}"
        );
        assert_eq!(without.substitute("plain $HOME").unwrap(), "plain $HOME");
    }

    #[test]
    fn resolution_makes_cwd_absolute_and_expands_env_and_options() {
        let configs = parse_file(LAUNCH, Source::LaunchJson).unwrap();
        let env = no_env;
        let ctx = context(Some("/home/me/proj/main.py"), &env);
        let module = configs[1].resolve(&ctx).unwrap();
        assert_eq!(module.cwd, "/home/me/proj/src");
        let current = configs[0].resolve(&ctx).unwrap();
        assert_eq!(current.cwd, "/home/me/proj");
        assert!(
            matches!(&current.kind, Kind::Python { target: PythonTarget::Script(p), .. } if p == "/home/me/proj/main.py")
        );
        assert!(configs[5].resolve(&ctx).unwrap_err().contains("disabled"));

        let relative = Configuration {
            cwd: Some("sub/dir".into()),
            ..configs[0].clone()
        };
        let windows = Context {
            workspace_folder: r"C:\src\proj\",
            file: Some(r"C:\src\proj\main.py"),
            line: None,
            separator: '\\',
            env: &env,
        };
        assert_eq!(
            relative.resolve(&windows).unwrap().cwd,
            r"C:\src\proj\sub\dir"
        );
        assert_eq!(windows.substitute("${relativeFile}").unwrap(), "main.py");
        assert_eq!(windows.absolute(r"D:\other"), r"D:\other");
    }

    #[test]
    fn commands_use_uv_or_the_interpreter_and_cargo_flags() {
        let env = no_env;
        let ctx = context(Some("/home/me/proj/main.py"), &env);
        let configs = parse_file(LAUNCH, Source::LaunchJson).unwrap();
        let process = |command: Command| match command.line {
            CommandLine::Process { program, args } => std::iter::once(program)
                .chain(args)
                .collect::<Vec<_>>()
                .join(" "),
            CommandLine::Shell(line) => line,
        };
        let current = configs[0].resolve(&ctx).unwrap();
        assert_eq!(
            process(current.command(&PythonRunner::Uv).unwrap()),
            "uv run python /home/me/proj/main.py --name a b"
        );
        assert_eq!(
            process(
                current
                    .command(&PythonRunner::Interpreter(".venv/bin/python".into()))
                    .unwrap()
            ),
            ".venv/bin/python /home/me/proj/main.py --name a b"
        );
        let module = configs[1].resolve(&ctx).unwrap();
        assert_eq!(
            process(module.command(&PythonRunner::Uv).unwrap()),
            "/usr/bin/python3.12 -m app.server"
        );
        let tool = configs[2].resolve(&ctx).unwrap();
        assert_eq!(
            process(tool.command(&PythonRunner::Uv).unwrap()),
            "cargo run --package demo --bin tool --release -- --verbose"
        );
        assert_eq!(
            tool.cargo_args(Some(&["build"])).join(" "),
            "build --package demo --bin tool --release --message-format=json-render-diagnostics"
        );
        let tests = configs[3].resolve(&ctx).unwrap();
        assert_eq!(
            tests.cargo_args(Some(&["test", "--no-run"])).join(" "),
            "test --no-run --package demo --lib --message-format=json-render-diagnostics"
        );
        assert!(
            configs[4]
                .resolve(&ctx)
                .unwrap()
                .command(&PythonRunner::Uv)
                .is_err()
        );

        let auto = automatic(&ProjectFacts {
            python: true,
            uv_scripts: vec!["demo".into()],
            cargo: vec![("demo".into(), vec!["tool".into()])],
        });
        let lines: Vec<String> = auto
            .iter()
            .map(|config| {
                process(
                    config
                        .resolve(&ctx)
                        .unwrap()
                        .command(&PythonRunner::Uv)
                        .unwrap(),
                )
            })
            .collect();
        assert_eq!(
            lines,
            [
                "uv run python /home/me/proj/main.py",
                "uv run pytest",
                "uv run demo",
                "cargo run --package demo --bin tool",
                "cargo test"
            ]
        );
    }

    #[test]
    fn the_template_parses_back_to_the_automatic_configurations() {
        let auto = automatic(&ProjectFacts {
            python: true,
            uv_scripts: vec!["demo".into()],
            cargo: vec![("a".into(), vec!["x".into()]), ("b".into(), Vec::new())],
        });
        let parsed = parse_file(&template(&auto), Source::RunJson).unwrap();
        let strip = |configs: &[Configuration]| {
            configs
                .iter()
                .map(|config| {
                    (
                        config.name.clone(),
                        config.kind.clone(),
                        config.disabled.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(strip(&parsed), strip(&auto));
        assert_eq!(
            parse_file(&template(&[]), Source::RunJson).unwrap()[0].name,
            "Hello"
        );
    }
}
