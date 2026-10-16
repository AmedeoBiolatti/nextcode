//! Run configurations in the editor: loading them where the project lives
//! (`.next_code/run.json`, `.vscode/launch.json`, automatic ones from
//! `pyproject.toml` and `cargo metadata`; per folder in a multi-root
//! workspace, labelled `name (folder)`), the selected one (remembered per
//! project), the terminal line that runs one, and the [`DebugRequest`] it
//! makes. The model and file formats are in the `run` crate.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io,
    path::PathBuf,
    time::Duration,
};

use gpui::actions;
use run::{
    Adapter, CargoCommand, Command, Configuration, Kind, PythonRunner, PythonTarget, Resolved,
    Source,
    config::{LAUNCH_JSON, ProjectFacts, RUN_JSON},
    shell::Shell,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{
    debugger::{DebugAdapterKind, DebugRequest, DebugRequestKind},
    layout,
    project::{Project, root_label},
};

pub mod picker;

pub use picker::{ConfigurationPicker, ConfigurationPickerEvent};

actions!(
    run,
    [
        /// Runs the selected configuration in a terminal tab.
        RunConfiguration,
        /// Debugs the selected configuration.
        DebugConfiguration,
        SelectConfiguration,
        EditConfigurations,
        RunAllTests,
        RunFailedTests
    ]
);

/// Long enough for a first `cargo build` of a big workspace.
pub const BUILD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const METADATA_TIMEOUT: Duration = Duration::from_secs(120);

/// The editor's place, for `${file}` and `${lineNumber}`.
#[derive(Debug, Clone, Default)]
pub struct Active {
    /// The active editor's file, as the project spells it.
    pub file: Option<String>,
    /// 1-based cursor line.
    pub line: Option<usize>,
}

/// The configurations of a project and the one selected.
#[derive(Debug, Clone, Default)]
pub struct RunConfigs {
    pub configurations: Vec<Configuration>,
    /// Where each configuration (by name) was found; absent: the first
    /// folder, under its own name.
    pub origins: HashMap<String, Origin>,
    /// Unreadable files, failed `cargo metadata`.
    pub problems: Vec<String>,
    /// The chosen configuration's name (else the first usable one).
    pub selected: Option<String>,
    pub loaded: bool,
}

/// The folder a configuration was found in (its `${workspaceFolder}` and
/// default working directory) and its name there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub folder: usize,
    pub name: String,
}

impl RunConfigs {
    pub fn selected(&self) -> Option<&Configuration> {
        self.selected
            .as_ref()
            .and_then(|name| self.find(name))
            .filter(|config| config.disabled.is_none())
            .or_else(|| self.configurations.iter().find(|c| c.disabled.is_none()))
    }

    pub fn find(&self, name: &str) -> Option<&Configuration> {
        self.configurations
            .iter()
            .find(|config| config.name == name)
    }

    /// The folder (root index) the configuration `name` belongs to.
    pub fn folder(&self, name: &str) -> usize {
        self.origins.get(name).map_or(0, |origin| origin.folder)
    }
}

/// Reads every folder's configurations. Blocks (file reads, `cargo
/// metadata`).
pub fn load(project: &Project) -> RunConfigs {
    let multi_root = project.is_multi_root();
    let mut found: Vec<(Configuration, Origin)> = Vec::new();
    let mut problems = Vec::new();
    for (folder, root) in project.roots().iter().enumerate() {
        let mut configurations = Vec::new();
        let mut folder_problems = Vec::new();
        for (file, source) in [
            (RUN_JSON, Source::RunJson),
            (LAUNCH_JSON, Source::LaunchJson),
        ] {
            match project.read_text(&project.resolve_in(folder, file)) {
                Ok(text) => match run::config::parse_file(&text, source) {
                    Ok(parsed) => configurations.extend(parsed),
                    Err(err) => folder_problems.push(format!("{file}: {err}")),
                },
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => folder_problems.push(format!("{file}: {err}")),
            }
        }
        configurations.extend(run::config::automatic(&facts(
            project,
            folder,
            &mut folder_problems,
        )));
        // With several folders, names and problems say which.
        problems.extend(folder_problems.into_iter().map(|problem| match multi_root {
            true => root_label(&root.name, &problem),
            false => problem,
        }));
        found.extend(configurations.into_iter().map(|mut config| {
            let origin = Origin {
                folder,
                name: config.name.clone(),
            };
            if multi_root {
                config.name = format!("{} ({})", config.name, root.name);
            }
            (config, origin)
        }));
    }
    // Names identify configurations: later duplicates say where they are from.
    let mut names = HashSet::new();
    for (config, _) in &mut found {
        if !names.insert(config.name.clone()) {
            let base = format!("{} ({})", config.name, config.source.label());
            let mut name = base.clone();
            let mut counter = 2;
            while names.contains(&name) {
                name = format!("{base} {counter}");
                counter += 1;
            }
            names.insert(name.clone());
            config.name = name;
        }
    }
    let origins = found
        .iter()
        .map(|(config, origin)| (config.name.clone(), origin.clone()))
        .collect();
    RunConfigs {
        configurations: found.into_iter().map(|(config, _)| config).collect(),
        origins,
        problems,
        selected: None,
        loaded: true,
    }
}

/// What folder `folder`'s automatic configurations are derived from.
fn facts(project: &Project, folder: usize, problems: &mut Vec<String>) -> ProjectFacts {
    let mut facts = ProjectFacts::default();
    if let Ok(text) = project.read_text(&project.resolve_in(folder, "pyproject.toml")) {
        facts.python = true;
        match python::PyProject::parse(&text) {
            Ok(pyproject) => facts.uv_scripts = pyproject.scripts.into_keys().collect(),
            Err(err) => problems.push(format!("pyproject.toml: {err}")),
        }
    }
    if matches!(
        project.kind(&project.resolve_in(folder, "Cargo.toml")),
        Ok(Some(_))
    ) {
        match cargo_packages(project, folder) {
            Ok(packages) => {
                facts.cargo = packages
                    .into_iter()
                    .map(|package| {
                        let bins = package
                            .targets
                            .into_iter()
                            .filter_map(|target| match target.target {
                                run::CargoTarget::Bin(name) => Some(name),
                                _ => None,
                            })
                            .collect();
                        (package.name, bins)
                    })
                    .collect();
            }
            Err(err) => problems.push(err),
        }
    }
    facts
}

/// Folder `folder`'s cargo workspace packages (`cargo metadata
/// --no-deps`). Blocks.
pub fn cargo_packages(
    project: &Project,
    folder: usize,
) -> Result<Vec<run::cargo::Package>, String> {
    let args = ["metadata", "--no-deps", "--format-version", "1"].map(str::to_owned);
    let dir = &project.roots()[folder].path;
    let output = project
        .run_program_in(dir, "cargo", &args, &[], None, METADATA_TIMEOUT)
        .map_err(|err| format!("cargo metadata: {err}"))?;
    if output.code != Some(0) {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    run::cargo::parse_metadata(&String::from_utf8_lossy(&output.stdout))
}

/// Expands a configuration's variables for folder `folder` (its
/// `${workspaceFolder}` and default working directory).
pub fn resolve(
    project: &Project,
    folder: usize,
    config: &Configuration,
    active: &Active,
) -> Result<Resolved, String> {
    let folder = project.roots()[folder].path.to_string_lossy().into_owned();
    let local = !project.is_remote();
    // The project's environment is only known for local projects.
    let env = move |name: &str| local.then(|| std::env::var(name).ok()).flatten();
    let home = local
        .then(dirs::home_dir)
        .flatten()
        .map(|home| home.to_string_lossy().into_owned());
    let context = run::Context {
        workspace_folder: &folder,
        file: active.file.as_deref(),
        line: active.line,
        separator: separator(project),
        env: &env,
        user_home: home.as_deref(),
    };
    config.resolve(&context)
}

fn separator(project: &Project) -> char {
    if !project.is_remote() && cfg!(windows) {
        '\\'
    } else {
        '/'
    }
}

/// `uv run` in uv projects, else the folder's interpreter. Blocks.
pub fn python_runner(project: &Project, folder: usize) -> PythonRunner {
    let dir = &project.roots()[folder].path;
    let pyproject = matches!(
        project.kind(&project.resolve_in(folder, "pyproject.toml")),
        Ok(Some(_))
    );
    if pyproject && matches!(project.find_executable_in("uv", dir), Ok(Some(_))) {
        return PythonRunner::Uv;
    }
    match project.python_interpreter_in(dir) {
        Ok(Some(interpreter)) => {
            PythonRunner::Interpreter(interpreter.to_string_lossy().into_owned())
        }
        _ if project.is_remote() || !cfg!(windows) => PythonRunner::Interpreter("python3".into()),
        _ => PythonRunner::Interpreter("python".into()),
    }
}

/// The shell of the project's default terminal.
pub fn shell(project: &Project) -> Shell {
    if !project.is_remote() && cfg!(windows) {
        Shell::PowerShell
    } else {
        Shell::Posix
    }
}

/// The line typed into a fresh terminal (started in folder `folder`) to
/// run `command`.
pub fn terminal_line(project: &Project, folder: usize, command: &Command) -> String {
    let dir = project.roots()[folder].path.to_string_lossy();
    run::shell::terminal_line(command, &dir, shell(project))
}

/// The debug adapter's view of `resolved` (a configuration of folder
/// `folder`). Cargo configurations are built first (`cargo build` /
/// `cargo test --no-run` with JSON messages, in the folder) to find the
/// executable. Blocks.
pub fn debug_request(
    project: &Project,
    folder: usize,
    resolved: &Resolved,
) -> Result<DebugRequest, String> {
    let mut configuration = Map::new();
    let mut request = DebugRequestKind::Launch;
    let adapter = match &resolved.kind {
        Kind::Python {
            target,
            args,
            interpreter,
        } => {
            match target {
                PythonTarget::Script(program) => {
                    configuration.insert("program".into(), json!(program))
                }
                PythonTarget::Module(module) => {
                    configuration.insert("module".into(), json!(module))
                }
            };
            configuration.insert("args".into(), json!(args));
            if let Some(interpreter) = interpreter {
                configuration.insert("python".into(), json!(interpreter));
            }
            DebugAdapterKind::Python
        }
        Kind::Pytest {
            targets,
            args,
            interpreter,
        } => {
            configuration.insert("module".into(), json!("pytest"));
            configuration.insert(
                "args".into(),
                json!([targets.clone(), args.clone()].concat()),
            );
            if let Some(interpreter) = interpreter {
                configuration.insert("python".into(), json!(interpreter));
            }
            DebugAdapterKind::Python
        }
        Kind::UvScript { script, .. } => {
            return Err(format!(
                "`uv run {script}` cannot be debugged directly; debug the module it calls with a \
                 python configuration (\"module\": \"…\") instead"
            ));
        }
        Kind::Cargo { command, args, .. } => {
            let build: &[&str] = match command {
                CargoCommand::Run => &["build"],
                CargoCommand::Test => &["test", "--no-run"],
            };
            let program = cargo_executable(
                project,
                folder,
                &resolved.cargo_args(Some(build)),
                *command == CargoCommand::Test,
            )?;
            configuration.insert("program".into(), json!(program));
            configuration.insert("args".into(), json!(args));
            DebugAdapterKind::Lldb
        }
        Kind::Program { program, args } => {
            configuration.insert("program".into(), json!(program));
            configuration.insert("args".into(), json!(args));
            DebugAdapterKind::Lldb
        }
        Kind::Shell { .. } => return Err("shell configurations cannot be debugged".into()),
        Kind::Attach { adapter, arguments } => {
            request = DebugRequestKind::Attach;
            configuration.extend(arguments.clone());
            match adapter {
                Adapter::Python => DebugAdapterKind::Python,
                Adapter::Lldb => DebugAdapterKind::Lldb,
            }
        }
        Kind::Unsupported => return Err(format!("{} is not supported", resolved.name)),
    };
    configuration.insert("cwd".into(), json!(resolved.cwd));
    if !resolved.env.is_empty() {
        configuration.insert("env".into(), json!(resolved.env));
    }
    for (key, value) in &resolved.debug_options {
        configuration
            .entry(key.clone())
            .or_insert_with(|| value.clone());
    }
    Ok(DebugRequest {
        name: resolved.name.clone(),
        adapter,
        request,
        configuration: Value::Object(configuration),
    })
}

/// Builds with cargo `args` (JSON messages) in folder `folder` and returns
/// the one executable (a test harness when `test`). Blocks.
pub fn cargo_executable(
    project: &Project,
    folder: usize,
    args: &[String],
    test: bool,
) -> Result<String, String> {
    let dir = &project.roots()[folder].path;
    let output = project
        .run_program_in(dir, "cargo", args, &[], None, BUILD_TIMEOUT)
        .map_err(|err| format!("cargo {}: {err}", args.first().map_or("", String::as_str)))?;
    let build = run::cargo::parse_build(&String::from_utf8_lossy(&output.stdout));
    if output.code != Some(0) && build.errors.is_empty() {
        // Diagnostics were rendered to stderr.
        return Err(format!(
            "cargo {} failed:\n{}",
            args.first().map_or("", String::as_str),
            tail(&String::from_utf8_lossy(&output.stderr), 40)
        ));
    }
    build
        .executable(test)
        .map(|artifact| artifact.executable.clone())
}

/// The last `lines` lines of `text`.
pub fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.trim_end().lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Per-project run state, next to the saved layout and session.
#[derive(Debug, Default, Serialize, Deserialize)]
struct RunState {
    selected: Option<String>,
}

fn state_path(project_key: &str) -> Option<PathBuf> {
    Some(layout::project_state_dir(project_key)?.join("run_state.json"))
}

/// The configuration selected when the project was last open.
pub fn load_selected(project_key: &str) -> Option<String> {
    let text = std::fs::read_to_string(state_path(project_key)?).ok()?;
    serde_json::from_str::<RunState>(&text).ok()?.selected
}

pub fn save_selected(project_key: &str, selected: &str) -> io::Result<()> {
    let path = state_path(project_key).ok_or_else(|| io::Error::other("no data directory"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let state = RunState {
        selected: Some(selected.to_owned()),
    };
    std::fs::write(
        path,
        serde_json::to_string_pretty(&state).map_err(io::Error::other)?,
    )
}

/// Text of a new `.next_code/run.json` for folder `folder`: a commented
/// example and its automatic configurations (under their own names) as
/// templates.
pub fn template(configs: &RunConfigs, folder: usize) -> String {
    let automatic: Vec<Configuration> = configs
        .configurations
        .iter()
        .filter(|config| {
            config.source == Source::Automatic && configs.folder(&config.name) == folder
        })
        .map(|config| {
            let mut config = config.clone();
            if let Some(origin) = configs.origins.get(&config.name) {
                config.name = origin.name.clone();
            }
            config
        })
        .collect();
    run::config::template(&automatic)
}

/// A configuration's description for lists: source, what it runs, and
/// why it is disabled.
pub fn describe(config: &Configuration) -> BTreeMap<&'static str, Value> {
    let mut out = BTreeMap::new();
    out.insert("name", json!(config.name));
    out.insert("source", json!(config.source));
    out.insert("summary", json!(config.summary()));
    out.insert("disabled", json!(config.disabled));
    out.insert("notes", json!(config.notes));
    out.insert("kind", json!(config.kind));
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// `folder` with a `.next_code/run.json` holding a shell configuration
    /// named `Serve`, and a `pyproject.toml` with one script.
    fn folder(base: &Path, name: &str) -> PathBuf {
        let dir = base.join(name);
        std::fs::create_dir_all(dir.join(".next_code")).unwrap();
        std::fs::write(
            dir.join(RUN_JSON),
            r#"{ "configurations": [{ "name": "Serve", "type": "shell", "command": "make serve" }] }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("pyproject.toml"),
            "[project]\nname = \"app\"\n[project.scripts]\nserve = \"app:main\"\n",
        )
        .unwrap();
        dir
    }

    fn names(configs: &RunConfigs) -> Vec<&str> {
        configs
            .configurations
            .iter()
            .map(|config| config.name.as_str())
            .collect()
    }

    #[test]
    fn every_folder_contributes_configurations_labelled_with_it() {
        let base = std::env::temp_dir().join(format!("next_code_run_roots_{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let (api, web) = (folder(&base, "api"), folder(&base, "web"));
        let project = Project::local_folders(&[&api, &web]);
        let configs = load(&project);
        assert_eq!(configs.problems, Vec::<String>::new());
        let names = names(&configs);
        // Each folder's, labelled, in folder order; no name clashes.
        assert_eq!(names[0], "Serve (api)");
        let web_serve = names
            .iter()
            .position(|name| *name == "Serve (web)")
            .unwrap();
        assert!(
            names[..web_serve]
                .iter()
                .all(|name| name.ends_with(" (api)"))
        );
        assert!(
            names[web_serve..]
                .iter()
                .all(|name| name.ends_with(" (web)"))
        );
        assert!(
            names.len() > 2,
            "automatic configurations of both: {names:?}"
        );
        assert_eq!(configs.folder("Serve (api)"), 0);
        assert_eq!(configs.folder("Serve (web)"), 1);

        // Resolved in their own folder.
        let resolve_named = |name: &str| {
            let config = configs.find(name).unwrap();
            resolve(&project, configs.folder(name), config, &Active::default()).unwrap()
        };
        assert_eq!(Path::new(&resolve_named("Serve (web)").cwd), web);
        assert_eq!(Path::new(&resolve_named("Serve (api)").cwd), api);

        // A folder's template uses its configurations' own names.
        let template = template(&configs, 1);
        assert!(!template.contains("(web)"), "{template}");
        assert!(!template.contains("(api)"), "{template}");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_single_folder_keeps_plain_names() {
        let base = std::env::temp_dir().join(format!("next_code_run_root_{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let api = folder(&base, "api");
        let project = Project::local(api.clone());
        let configs = load(&project);
        assert_eq!(names(&configs)[0], "Serve");
        assert!(names(&configs).iter().all(|name| !name.contains("(api)")));
        assert_eq!(configs.folder("Serve"), 0);
        std::fs::remove_dir_all(&base).ok();
    }
}
