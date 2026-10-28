//! Workflows, a built-in extension: YAML files that start a
//! multi-project development environment (format and scheduling in the
//! `workflow` crate). They are
//! found in the user's config dir (`<config>/next_code/workflows/`) and in
//! each open project's `.next_code/workflows/`; files of the old JSON
//! format are listed with a problem until converted ([`convert_file`]). A [`Workflows`] model per
//! window (and one in headless mode) runs them: it performs what a
//! `workflow::Run` asks for through a [`Host`] (open project tabs, start
//! commands in their Terminal panels, Ctrl+C and kill them when the run
//! says so, look up artifacts, open URLs and files) and feeds back output,
//! exits, check results, artifacts and listening ports. A run replaced by
//! Run All keeps draining (its commands get their stop grace); the new
//! run's steps sharing their terminals wait for them.
//!
//! Each workflow file opens as a window tab, a [`WorkflowView`]: the file
//! in an editor beside a live graph of its steps (`view`), with Run All,
//! Stop All and per-step actions. Run > Workflows > Open Workflow… picks one
//! ([`WorkflowPicker`]); `window` has the window's commands and tabs. A workflow with `inputs` asks for them first in a
//! form over the graph (`form`); a run keeps its values (Retry and
//! Restart reuse them).
//!
//! Commands: `run` steps start in a terminal tab of their project (named
//! by `terminal`, else the title); without a project they run on this
//! machine in a terminal the workflow's tab shows. Checks are plain
//! processes. See `commands` for the shells used.

use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use futures::{StreamExt, channel::mpsc};
use gpui::{
    App, AppContext, Context, Entity, EventEmitter, Global, Hsla, KeyBinding, Window, WindowId,
    actions,
};
use terminal::{Output, OutputTap};
use workflow::{Effect, InputValues, Outcome, ProjectSpec, Run, Status, Workflow};

use crate::{
    builtins::{Builtin, Entry, MenuItem},
    project::{Location, Project},
    terminal_panel::{self, TerminalView},
    theme,
};

mod commands;
mod form;
mod picker;
mod view;
mod window;

pub use picker::{WorkflowPicker, WorkflowPickerEvent};
pub use view::{WorkflowView, WorkflowViewEvent};

actions!(
    workflows,
    [
        OpenWorkflow,
        OpenWorkflowFile,
        AddWorkflowFolder,
        NewWorkflow,
        RunAll,
        StopAll,
        RetryFailed
    ]
);

/// "Run Workflow: <name>" in the command palette.
#[derive(Clone, PartialEq, Debug, gpui::Action)]
#[action(namespace = workflows, no_json)]
pub struct RunWorkflow {
    /// [`Discovered::id`].
    pub id: String,
}

pub const BUILTIN: Builtin = Builtin {
    id: "workflows",
    name: "Workflows",
    description: "Multi-project development environments from YAML files, each in a window tab with a live graph of its steps (Run > Workflows > Open Workflow…).",
    default_enabled: true,
    labels: &[
        ("workflows::OpenWorkflow", "Open Workflow…"),
        ("workflows::OpenWorkflowFile", "Open Workflow File…"),
        ("workflows::AddWorkflowFolder", "Add Workflow Folder…"),
        ("workflows::NewWorkflow", "New Workflow…"),
        ("workflows::RunAll", "Workflow: Run All"),
        ("workflows::StopAll", "Workflow: Stop All"),
        ("workflows::RetryFailed", "Workflow: Retry Failed"),
    ],
    // Listed once per workflow as "Run Workflow: <name>".
    unlisted: &["workflows::RunWorkflow"],
    menu_items: &[MenuItem {
        menu: "Run/Workflows",
        after: "",
        items: &[
            Entry::Command("workflows::OpenWorkflow"),
            Entry::Command("workflows::OpenWorkflowFile"),
            Entry::Command("workflows::AddWorkflowFolder"),
            Entry::Command("workflows::NewWorkflow"),
        ],
    }],
    window_actions: window::actions,
    window_model: Some(window::model),
    palette: window::palette,
    project_opened: window::refresh,
    window_tabs: &[window::TAB],
    ..Builtin::NONE
};

pub fn bind_keys(cx: &mut App) {
    let ctx = Some(view::CONTEXT);
    cx.bind_keys([
        // JetBrains' Run and Stop.
        KeyBinding::new("shift-f10", RunAll, ctx),
        KeyBinding::new("ctrl-f2", StopAll, ctx),
    ]);
    picker::bind_keys(cx);
    form::bind_keys(cx);
}

/// Where workflows live inside a project.
const PROJECT_DIR: &str = ".next_code/workflows";
/// How often running workflows check timeouts, stop graces, ports and
/// opening projects.
const TICK: Duration = Duration::from_millis(250);
/// How often a step's readiness probe (its port, its URL) is tried.
const PORT_POLL: Duration = Duration::from_secs(1);
/// How long one readiness probe may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The user's workflows folder (next to `settings.json`).
pub fn user_dir() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("next_code").join("workflows"))
}

/// Where a workflow file was found.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    User,
    Project(Location),
    /// A folder of `"workflows": {"folders": […]}` (or
    /// `NEXT_CODE_WORKFLOW_FOLDERS`), or the folder of a file opened with
    /// Open Workflow File…; on this machine.
    Folder(PathBuf),
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Self::User => "User".into(),
            Self::Project(location) => location.to_string(),
            Self::Folder(dir) => dir.display().to_string(),
        }
    }
}

/// The environment variable listing more workflow folders (separated as
/// in `PATH`).
pub const FOLDERS_VARIABLE: &str = "NEXT_CODE_WORKFLOW_FOLDERS";

/// The folders listed besides the user's and the projects': the
/// settings' `"workflows": {"folders": […]}`, then
/// `NEXT_CODE_WORKFLOW_FOLDERS`; `~` expanded, each once.
pub fn extra_folders() -> Vec<PathBuf> {
    let (settings, _) = crate::settings::Settings::load();
    let home = dirs::home_dir();
    let expand = |folder: &str| -> PathBuf {
        match (folder.strip_prefix('~'), &home) {
            (Some(rest), Some(home)) => home.join(rest.trim_start_matches(['/', '\\'])),
            _ => PathBuf::from(folder),
        }
    };
    let mut folders: Vec<PathBuf> = settings
        .workflows
        .folders
        .iter()
        .filter(|folder| !folder.trim().is_empty())
        .map(|folder| expand(folder.trim()))
        .collect();
    if let Some(list) = std::env::var_os(FOLDERS_VARIABLE) {
        folders.extend(std::env::split_paths(&list).filter(|dir| !dir.as_os_str().is_empty()));
    }
    let mut unique: Vec<PathBuf> = Vec::new();
    for folder in folders {
        if !unique.contains(&folder) {
            unique.push(folder);
        }
    }
    unique
}

/// A workflow's id ([`Discovered::id`]): the file path, prefixed by the
/// project for project files. Runs are keyed by it.
pub fn workflow_id(source: &Source, path: &Path) -> String {
    match source {
        Source::User | Source::Folder(_) => path.to_string_lossy().into_owned(),
        Source::Project(location) => format!("{location}|{}", path.display()),
    }
}

/// The extensions of workflow files: YAML, and the old JSON format's.
const EXTENSIONS: &[&str] = &[".yaml", ".yml", ".jsonc", ".json"];

/// Whether a file named `name` is a workflow file (of either format).
pub fn is_workflow_file(name: &str) -> bool {
    EXTENSIONS.iter().any(|extension| name.ends_with(extension)) && !name.ends_with(".schema.json")
}

/// Writes the format's JSON Schema into `dir` as [`workflow::SCHEMA_FILE`]
/// unless it is there already, as this version has it: YAML editors find
/// it through the template's first line (`workflow::SCHEMA_MODELINE`).
/// Returns its path.
pub fn write_schema(dir: &Path) -> std::io::Result<PathBuf> {
    let path = dir.join(workflow::SCHEMA_FILE);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(workflow::SCHEMA) {
        std::fs::write(&path, workflow::SCHEMA)?;
    }
    Ok(path)
}

/// A workflow file's name without its extension (a workflow without
/// `name` is called that).
pub fn file_stem(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    EXTENSIONS
        .iter()
        .find_map(|extension| name.strip_suffix(extension))
        .unwrap_or(&name)
        .to_owned()
}

/// What a file of the old JSON format shows instead of its steps.
pub const LEGACY_FORMAT: &str = "This file uses the old JSON workflow format. Convert it to \
                                 YAML: Convert to YAML in its tab (headless: \
                                 `workflow_convert`).";

/// Whether `path` is a file of the old JSON format.
pub fn is_legacy(path: &Path) -> bool {
    workflow::legacy::is_legacy(&path.to_string_lossy())
}

/// The workflow in file `path` with contents `text` ([`LEGACY_FORMAT`]
/// for an old-format file).
pub fn parse_file(path: &Path, text: &str) -> Result<Workflow, String> {
    if is_legacy(path) {
        return Err(LEGACY_FORMAT.into());
    }
    workflow::parse(text, &file_stem(path))
}

/// What [`convert_file`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct Converted {
    /// The new YAML file.
    pub path: PathBuf,
    /// Where the old file went.
    pub backup: PathBuf,
    /// The problems the converted file has (as the old one had them).
    pub problems: Vec<String>,
}

/// Converts the old-format workflow file `path`, whose text is `text`,
/// through `project`: writes `<stem>.yaml` beside it (an error if that
/// exists) and renames the old file to `<name>.bak` (or `.bak2`, …).
/// Blocks.
pub fn convert_file(project: &Project, path: &Path, text: &str) -> Result<Converted, String> {
    let mut yaml = workflow::legacy::convert(text)?;
    let dir = path.parent().unwrap_or(Path::new(""));
    let target = project.join(dir, &format!("{}.yaml", file_stem(path)));
    let exists = |path: &Path| project.kind(path).ok().flatten().is_some();
    // Point YAML editors at the schema when a copy is beside the file.
    if exists(&project.join(dir, workflow::SCHEMA_FILE)) {
        yaml = format!("{}\n{yaml}", workflow::SCHEMA_MODELINE);
    }
    if exists(&target) {
        return Err(format!(
            "{} already exists: rename or remove it first",
            target.display()
        ));
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let backup = (1..)
        .map(|n| match n {
            1 => project.join(dir, &format!("{name}.bak")),
            n => project.join(dir, &format!("{name}.bak{n}")),
        })
        .find(|backup| !exists(backup))
        .expect("a free name");
    let problems = match workflow::parse(&yaml, &file_stem(path)) {
        Ok(_) => Vec::new(),
        Err(problems) => problems.lines().map(str::to_owned).collect(),
    };
    project
        .write(&target, yaml.into_bytes())
        .map_err(|err| format!("cannot write {}: {err}", target.display()))?;
    project
        .rename(path, &backup)
        .map_err(|err| format!("cannot rename {} to .bak: {err}", path.display()))?;
    Ok(Converted {
        path: target,
        backup,
        problems,
    })
}

/// What a workflow's state is called in its tab.
pub fn outcome_text(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Idle => "Idle",
        Outcome::Running => "Running",
        Outcome::Ready => "All ready",
        Outcome::Failed => "Failed",
        Outcome::Stopped => "Stopped",
    }
}

pub fn outcome_color(outcome: Outcome) -> Hsla {
    match outcome {
        Outcome::Idle | Outcome::Stopped => theme::muted(),
        Outcome::Running => theme::accent(),
        Outcome::Ready => theme::git_added(),
        Outcome::Failed => theme::error(),
    }
}

/// A workflow file and what parsing it gave.
#[derive(Debug, Clone)]
pub struct Discovered {
    /// Unique: the file path, prefixed by the project for project files.
    pub id: String,
    /// The file (where its project lives for project files).
    pub path: PathBuf,
    pub source: Source,
    /// The workflow's name, else the file name.
    pub name: String,
    pub parsed: Result<Arc<Workflow>, String>,
}

/// Reads every workflow file: the user's, then each project's. Blocks.
/// The workflow files in `dir` (not in subfolders), sorted.
fn workflow_files(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && is_workflow_file(&path.to_string_lossy()))
        .collect();
    paths.sort();
    paths
}

/// Reads every workflow file: the user's, the extra `folders`', the
/// `opened` files not found otherwise, then each project's. Blocks.
fn discover(
    user_dir: Option<&Path>,
    folders: &[PathBuf],
    opened: &[PathBuf],
    projects: &[(Location, Project)],
) -> Vec<Discovered> {
    let mut found = Vec::new();
    let mut add = |path: PathBuf, source: Source, text: std::io::Result<String>| {
        let stem = file_stem(&path);
        let parsed = text
            .map_err(|err| format!("cannot read the file: {err}"))
            .and_then(|text| parse_file(&path, &text))
            .map(Arc::new);
        let name = parsed
            .as_ref()
            .map_or(stem, |workflow| workflow.name.clone());
        found.push(Discovered {
            id: workflow_id(&source, &path),
            path,
            source,
            name,
            parsed,
        });
    };
    // Each file on this machine once: the user's, the extra folders',
    // those opened by path.
    let mut listed: Vec<PathBuf> = Vec::new();
    let mut list = |path: PathBuf, source: Source| {
        if !listed.contains(&path) {
            listed.push(path.clone());
            let text = std::fs::read_to_string(&path);
            add(path, source, text);
        }
    };
    if let Some(dir) = user_dir {
        for path in workflow_files(dir) {
            list(path, Source::User);
        }
    }
    for dir in folders.iter().filter(|dir| Some(dir.as_path()) != user_dir) {
        for path in workflow_files(dir) {
            list(path, Source::Folder(dir.clone()));
        }
    }
    for path in opened {
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        list(path.clone(), Source::Folder(dir));
    }
    for (location, project) in projects {
        let dir = project.join(&project.join(project.root(), ".next_code"), "workflows");
        // Listed directly: `.next_code` may well be git-ignored.
        let mut names: Vec<String> = if project.is_remote() {
            project
                .run_program(
                    "ls",
                    &["-1A".into(), PROJECT_DIR.into()],
                    &[],
                    None,
                    TICK * 40,
                )
                .map(|output| {
                    String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        } else {
            std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        };
        names.retain(|name| is_workflow_file(name));
        names.sort();
        for name in names {
            let path = project.join(&dir, &name);
            let text = project.read_text(&path);
            add(path, Source::Project(location.clone()), text);
        }
    }
    found
}

/// Opens projects and their terminals for a [`Workflows`] model: the
/// window's project tabs, or headless projects.
pub(crate) trait Host {
    /// The project open at `location`, opening it if needed: `Ok(None)`
    /// while it still opens (asked again later).
    fn project(&mut self, location: &Location, cx: &mut App) -> Result<Option<Project>, String>;
    /// The open projects whose workflows are listed.
    fn projects(&mut self, cx: &mut App) -> Vec<(Location, Project)>;
    /// Starts `program` in a terminal named `title` of the project at
    /// `location`, replacing one of the same title.
    #[allow(clippy::too_many_arguments)]
    fn start(
        &mut self,
        location: &Location,
        project: &Project,
        title: &str,
        program: &str,
        args: &[String],
        tap: OutputTap,
        cx: &mut App,
    ) -> Result<Entity<TerminalView>, String>;
    /// Shows a step's terminal in its project.
    fn show(&mut self, location: &Location, view: &Entity<TerminalView>, cx: &mut App);
    fn open_url(&mut self, url: &str, cx: &mut App);
    /// Opens a step's artifact file in an editor tab of the project at
    /// `location` (`None`: a file on this machine).
    fn open_file(&mut self, location: Option<&Location>, path: &Path, cx: &mut App);
}

/// What a workflow step's project spec names.
pub(crate) fn location_of(spec: &ProjectSpec) -> Result<Location, String> {
    let location = match spec {
        ProjectSpec::Local(path) => Location::Local(PathBuf::from(path)),
        ProjectSpec::Wsl { distro, path } => Location::Wsl {
            distro: distro.clone(),
            path: path.clone(),
        },
        ProjectSpec::DevContainer { folder, config } => {
            let folder = location_of(folder)?;
            if matches!(folder, Location::DevContainer { .. }) {
                return Err("a dev container's folder cannot be another dev container".into());
            }
            Location::DevContainer {
                folder: Box::new(folder),
                config: config.clone(),
            }
        }
        ProjectSpec::Typed(text) => {
            Location::parse(text).ok_or_else(|| format!("not a location: `{text}`"))?
        }
    };
    Ok(location.absolute())
}

/// Per step: where it runs and what it started.
#[derive(Default)]
struct StepRuntime {
    location: Option<Location>,
    project: Option<Project>,
    /// The step's terminal (kept after it exits, for Show Terminal).
    command: Option<Entity<TerminalView>>,
    /// The generation waiting for its project to open.
    opening: Option<u64>,
    last_port_poll: Option<Instant>,
    port_busy: bool,
    /// Bumped on each chunk of output the step's command printed: the
    /// view keeps its joined output lines until this changes.
    output_generation: u64,
}

/// A started workflow.
pub struct ActiveRun {
    pub id: u64,
    /// [`Discovered::id`] of its file.
    pub workflow_id: String,
    pub run: Run,
    /// What it was started with (its steps have them substituted).
    pub inputs: InputValues,
    steps: Vec<StepRuntime>,
    /// The outcome last reported to listeners.
    reported: Outcome,
}

impl ActiveRun {
    /// The terminal of step `index`, if it started one.
    pub fn terminal(&self, index: usize) -> Option<&Entity<TerminalView>> {
        self.steps.get(index)?.command.as_ref()
    }

    /// Where step `index` runs (`None`: this machine).
    pub fn location(&self, index: usize) -> Option<&Location> {
        self.steps.get(index)?.location.as_ref()
    }

    /// Changes whenever step `index`'s command printed more (see
    /// [`StepState::last_lines`]): a key for what was built from its
    /// output.
    pub fn output_generation(&self, index: usize) -> u64 {
        self.steps
            .get(index)
            .map_or(0, |step| step.output_generation)
    }
}

/// Output from a step's terminal, from its reader thread.
enum Input {
    Output {
        run: u64,
        step: usize,
        generation: u64,
        data: Vec<u8>,
    },
    Exited {
        run: u64,
        step: usize,
        generation: u64,
        code: Option<i32>,
    },
}

/// What listeners (headless) hear about.
#[derive(Debug, Clone)]
pub enum WorkflowsEvent {
    Step {
        run: u64,
        workflow: String,
        step: String,
        status: Status,
        detail: String,
    },
    Outcome {
        run: u64,
        workflow: String,
        outcome: Outcome,
    },
    UrlOpened {
        run: u64,
        workflow: String,
        step: String,
        url: String,
    },
}

pub struct Workflows {
    host: Box<dyn Host>,
    user_dir: Option<PathBuf>,
    /// The extra folders to list; `None`: read them ([`extra_folders`])
    /// on each refresh.
    folders: Option<Vec<PathBuf>>,
    /// Files opened by path (Open Workflow File…, headless `path`), listed
    /// for the session.
    opened: Vec<PathBuf>,
    discovered: Vec<Discovered>,
    runs: Vec<ActiveRun>,
    /// Replaced runs whose commands are still stopping (their steps in the
    /// new run wait for them: the same terminals).
    draining: Vec<ActiveRun>,
    next_run: u64,
    inputs: mpsc::UnboundedSender<Input>,
    pump_scheduled: bool,
    ticking: bool,
    /// [`Self::display_version`] when observers were last notified.
    shown: u64,
    /// Feeds terminal output and exits into the runs.
    _inputs: gpui::Task<()>,
    /// Kills the runs' commands when the model goes (Workflows turned off,
    /// its window closed).
    _release: gpui::Subscription,
}

impl EventEmitter<WorkflowsEvent> for Workflows {}

/// Each window's model.
#[derive(Default)]
struct Registry(HashMap<WindowId, gpui::WeakEntity<Workflows>>);

impl Global for Registry {}

/// The model of the window `window` (created on first use); it opens
/// project tabs in that window.
pub fn for_window(window: &Window, cx: &mut App) -> Entity<Workflows> {
    let id = window.window_handle().window_id();
    if let Some(existing) = cx
        .try_global::<Registry>()
        .and_then(|registry| registry.0.get(&id))
        .and_then(|weak| weak.upgrade())
    {
        return existing;
    }
    let handle = window.window_handle();
    let entity = cx.new(|cx| Workflows::new(Box::new(window::WindowHost(handle)), user_dir(), cx));
    let registry = cx.default_global::<Registry>();
    registry.0.retain(|_, weak| weak.upgrade().is_some());
    registry.0.insert(id, entity.downgrade());
    entity
}

impl Workflows {
    pub(crate) fn new(
        host: Box<dyn Host>,
        user_dir: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Self {
        let (inputs, mut received) = mpsc::unbounded::<Input>();
        let task = cx.spawn(async move |this, cx| {
            while let Some(first) = received.next().await {
                let mut batch = vec![first];
                while let Ok(more) = received.try_recv() {
                    batch.push(more);
                }
                let alive = this.update(cx, |this, cx| {
                    for input in batch {
                        this.apply(input);
                    }
                    this.pump(cx);
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let release = cx.on_release(|this, cx| this.kill_commands(cx));
        Self {
            host,
            user_dir,
            // Tests list what they set, not the user's settings.
            folders: cfg!(test).then(Vec::new),
            opened: Vec::new(),
            discovered: Vec::new(),
            runs: Vec::new(),
            draining: Vec::new(),
            next_run: 1,
            inputs,
            pump_scheduled: false,
            ticking: false,
            shown: 0,
            _inputs: task,
            _release: release,
        }
    }

    pub fn discovered(&self) -> &[Discovered] {
        &self.discovered
    }

    pub fn find(&self, id: &str) -> Option<&Discovered> {
        self.discovered.iter().find(|found| found.id == id)
    }

    /// A workflow by name, file path (or its end) or id.
    pub fn lookup(&self, query: &str) -> Option<&Discovered> {
        let query_path = query.replace('\\', "/");
        self.discovered
            .iter()
            .find(|found| found.id == query || found.name == query)
            .or_else(|| {
                self.discovered.iter().find(|found| {
                    let path = found.path.to_string_lossy().replace('\\', "/");
                    path == query_path || path.ends_with(&format!("/{query_path}"))
                })
            })
    }

    /// The latest run of workflow `id`.
    pub fn run_of(&self, id: &str) -> Option<&ActiveRun> {
        self.runs.iter().find(|run| run.workflow_id == id)
    }

    pub(crate) fn user_dir(&self) -> Option<&Path> {
        self.user_dir.as_deref()
    }

    /// Lists the workflow file at `path` from the next refresh on.
    pub fn remember_file(&mut self, path: PathBuf) {
        if !self.opened.contains(&path) {
            self.opened.push(path);
        }
    }

    /// Converts workflow `id`, a file of the old JSON format (see
    /// [`convert_file`]), and reads the files again. Blocks.
    pub fn convert(&mut self, id: &str, cx: &mut Context<Self>) -> Result<Converted, String> {
        let found = self
            .find(id)
            .cloned()
            .ok_or_else(|| format!("no workflow `{id}`"))?;
        if !is_legacy(&found.path) {
            return Err(format!(
                "{} is not in the old JSON format",
                found.path.display()
            ));
        }
        let path = found.path;
        let project = match &found.source {
            Source::User | Source::Folder(_) => {
                Project::local(path.parent().map(Path::to_path_buf).unwrap_or_default())
            }
            Source::Project(location) => self
                .host
                .projects(cx)
                .into_iter()
                .find(|(open, _)| open == location)
                .map(|(_, project)| project)
                .ok_or_else(|| format!("the project {location} is not open"))?,
        };
        let text = project
            .read_text(&path)
            .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
        let converted = convert_file(&project, &path, &text)?;
        self.refresh(cx).detach();
        Ok(converted)
    }

    /// Reads the workflow files again; the task ends once the list is
    /// updated.
    pub fn refresh(&mut self, cx: &mut Context<Self>) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            let Ok((user_dir, folders, opened, projects)) = this.update(cx, |this, cx| {
                (
                    this.user_dir.clone(),
                    this.folders.clone(),
                    this.opened.clone(),
                    this.host.projects(cx),
                )
            }) else {
                return;
            };
            let found = cx
                .background_spawn(async move {
                    // Keeps the schema beside the user's workflows current.
                    if let Some(dir) = user_dir.as_deref().filter(|dir| dir.is_dir()) {
                        write_schema(dir).ok();
                    }
                    let folders = folders.unwrap_or_else(extra_folders);
                    discover(user_dir.as_deref(), &folders, &opened, &projects)
                })
                .await;
            this.update(cx, |this, cx| {
                this.discovered = found;
                cx.notify();
            })
            .ok();
        })
    }

    /// Starts workflow `id` as last read, with `inputs` (JSON by input id;
    /// the others get their defaults); every invalid or missing value is
    /// an error.
    pub fn start(
        &mut self,
        id: &str,
        inputs: &serde_json::Map<String, serde_json::Value>,
        cx: &mut Context<Self>,
    ) -> Result<u64, String> {
        let found = self
            .find(id)
            .ok_or_else(|| format!("no workflow `{id}`"))?
            .clone();
        let workflow = found
            .parsed
            .map_err(|err| format!("{} cannot run: {err}", found.name))?;
        let values = workflow::inputs::resolve(&workflow.inputs, inputs)
            .map_err(|problems| format!("{} cannot run: {}", found.name, problems.join("; ")))?;
        Ok(self.start_parsed(found.id, workflow, values, cx))
    }

    /// Starts `workflow` afresh with `inputs` as the workflow with id
    /// `workflow_id` (see [`workflow_id`]); a run of it already going is
    /// stopped first (with each step's grace) and its terminals reused: a
    /// step whose old command still stops starts once that command is
    /// gone.
    pub fn start_parsed(
        &mut self,
        workflow_id: String,
        workflow: Arc<Workflow>,
        inputs: InputValues,
        cx: &mut Context<Self>,
    ) -> u64 {
        let now = Instant::now();
        let mut run = Run::new(workflow.with_inputs(&inputs));
        // The previous run's terminals are reused (replaced by title).
        let previous = self
            .runs
            .iter()
            .position(|run| run.workflow_id == workflow_id);
        let mut steps: Vec<StepRuntime> = run.nodes().iter().map(|_| Default::default()).collect();
        if let Some(index) = previous {
            let mut old = self.runs.remove(index);
            old.run.drain(now);
            for (index, node) in run.nodes().iter().enumerate() {
                if let Some(at) = old.run.step(&node.path) {
                    steps[index].command = old.steps[at].command.clone();
                }
            }
            self.draining.push(old);
        }
        let id_number = self.next_run;
        self.next_run += 1;
        let held: Vec<usize> = (0..run.nodes().len())
            .filter(|&index| {
                let node = &run.nodes()[index];
                !node.is_group() && self.still_stopping(&workflow_id, &node.path)
            })
            .collect();
        for index in held {
            run.hold(index);
        }
        run.start(now);
        self.runs.push(ActiveRun {
            id: id_number,
            workflow_id,
            run,
            inputs,
            steps,
            reported: Outcome::Idle,
        });
        self.schedule(cx);
        cx.notify();
        id_number
    }

    /// Whether a replaced run of workflow `workflow_id` still stops its
    /// step `step`'s command (`step`: qualified id).
    fn still_stopping(&self, workflow_id: &str, step: &str) -> bool {
        self.draining.iter().any(|old| {
            old.workflow_id == workflow_id
                && old
                    .run
                    .step(step)
                    .is_some_and(|at| old.run.steps()[at].status == Status::Stopping)
        })
    }

    /// Lets held steps start whose old command is gone; true if any did.
    fn release_held(&mut self) -> bool {
        let now = Instant::now();
        let mut released = false;
        for index in 0..self.runs.len() {
            for step in 0..self.runs[index].run.steps().len() {
                let active = &self.runs[index];
                if !active.run.is_held(step)
                    || self.still_stopping(&active.workflow_id, &active.run.nodes()[step].path)
                {
                    continue;
                }
                self.runs[index].run.release(step, now);
                released = true;
            }
        }
        released
    }

    /// A run by id: the latest runs, then replaced ones still stopping.
    fn run_mut(&mut self, id: u64) -> Option<&mut ActiveRun> {
        self.runs
            .iter_mut()
            .chain(&mut self.draining)
            .find(|run| run.id == id)
    }

    /// `change` to the latest run of workflow `workflow_id`; `None` when
    /// it has none.
    fn with_run<R>(
        &mut self,
        workflow_id: &str,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut Run, Instant) -> R,
    ) -> Option<R> {
        let active = self
            .runs
            .iter_mut()
            .find(|run| run.workflow_id == workflow_id)?;
        let changed = change(&mut active.run, Instant::now());
        self.schedule(cx);
        cx.notify();
        Some(changed)
    }

    /// Stop All: Ctrl+C to the commands the workflow started, killed after
    /// their step's grace (at once on a second Stop All); project tabs and
    /// containers stay.
    pub fn stop(&mut self, workflow_id: &str, cx: &mut Context<Self>) -> bool {
        let now = Instant::now();
        // A second Stop All also kills what a replaced run still stops.
        if self
            .run_of(workflow_id)
            .is_some_and(|active| active.run.outcome() == Outcome::Stopped)
        {
            for old in &mut self.draining {
                if old.workflow_id == workflow_id {
                    old.run.stop_all(now);
                }
            }
        }
        self.with_run(workflow_id, cx, |run, now| run.stop_all(now))
            .is_some()
    }

    pub fn restart_step(&mut self, workflow_id: &str, step: usize, cx: &mut Context<Self>) -> bool {
        self.with_run(workflow_id, cx, |run, now| run.restart(step, now))
            .is_some()
    }

    pub fn retry_failed(&mut self, workflow_id: &str, cx: &mut Context<Self>) -> bool {
        self.with_run(workflow_id, cx, |run, now| run.retry_failed(now))
            .is_some()
    }

    /// Retry on step `step`: it starts again if it failed; a group's
    /// failed steps do. False if none had failed.
    pub fn retry_step(&mut self, workflow_id: &str, step: usize, cx: &mut Context<Self>) -> bool {
        self.with_run(workflow_id, cx, |run, now| run.retry(step, now))
            .unwrap_or(false)
    }

    /// Continue on a manual step; false if it was not waiting.
    pub fn continue_step(
        &mut self,
        workflow_id: &str,
        step: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        self.with_run(workflow_id, cx, |run, now| run.continue_step(step, now))
            .unwrap_or(false)
    }

    /// Checks step `step`'s artifacts again; false if it has none to check.
    pub fn check_artifacts(
        &mut self,
        workflow_id: &str,
        step: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        self.with_run(workflow_id, cx, |run, _| run.check_artifacts(step))
            .unwrap_or(false)
    }

    /// Opens artifact `artifact` of step `step`: a file in an editor tab
    /// of the step's project (this machine's without one), a URL in the
    /// browser.
    pub fn open_artifact(
        &mut self,
        workflow_id: &str,
        step: usize,
        artifact: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.run_of(workflow_id) else {
            return;
        };
        let Some(Ok(checked)) = active
            .run
            .steps()
            .get(step)
            .and_then(|s| s.artifacts.as_ref())
        else {
            return;
        };
        let Some(resolved) = checked
            .get(artifact)
            .map(|checked| checked.resolved.clone())
        else {
            return;
        };
        let is_url = matches!(
            active.run.definition(step).artifacts.get(artifact),
            Some(workflow::Artifact {
                target: workflow::ArtifactTarget::Url(_),
                ..
            })
        );
        let location = active.location(step).cloned();
        // Not inside the click's window update: the host updates windows.
        cx.spawn(async move |this, cx| {
            this.update(cx, |this, cx| {
                if is_url {
                    this.host.open_url(&resolved, cx);
                } else {
                    this.host
                        .open_file(location.as_ref(), Path::new(&resolved), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Shows step `step`'s terminal in its project tab.
    pub fn show_terminal(&mut self, workflow_id: &str, step: usize, cx: &mut Context<Self>) {
        let Some(active) = self.run_of(workflow_id) else {
            return;
        };
        let (Some(location), Some(view)) = (
            active.location(step).cloned(),
            active.terminal(step).cloned(),
        ) else {
            return;
        };
        // Not inside the click's window update: the host updates windows.
        cx.spawn(async move |this, cx| {
            this.update(cx, |this, cx| this.host.show(&location, &view, cx))
                .ok();
        })
        .detach();
    }

    /// Performs the runs' effects soon, outside whatever update is going
    /// on (a click inside the window cannot update the window again).
    fn schedule(&mut self, cx: &mut Context<Self>) {
        if self.pump_scheduled {
            return;
        }
        self.pump_scheduled = true;
        cx.spawn(async move |this, cx| {
            this.update(cx, |this, cx| {
                this.pump_scheduled = false;
                this.pump(cx);
            })
            .ok();
        })
        .detach();
    }

    fn apply(&mut self, input: Input) {
        let now = Instant::now();
        match input {
            Input::Output {
                run,
                step,
                generation,
                data,
            } => {
                if let Some(active) = self.run_mut(run) {
                    active.run.output(step, generation, &data, now);
                    if let Some(runtime) = active.steps.get_mut(step) {
                        runtime.output_generation += 1;
                    }
                }
            }
            Input::Exited {
                run,
                step,
                generation,
                code,
            } => {
                if let Some(active) = self.run_mut(run) {
                    active.run.exited(step, generation, code, now);
                }
            }
        }
    }

    /// Performs every pending effect and reports changes.
    fn pump(&mut self, cx: &mut Context<Self>) {
        loop {
            let mut performed = false;
            for index in 0..self.runs.len() {
                loop {
                    let effects = self.runs[index].run.take_effects();
                    if effects.is_empty() {
                        break;
                    }
                    performed = true;
                    for effect in effects {
                        self.perform(index, effect, cx);
                    }
                }
            }
            // Replaced runs only stop their commands.
            for old in &mut self.draining {
                for effect in old.run.take_effects() {
                    performed = true;
                    match effect {
                        Effect::InterruptCommand { step } => {
                            command(&old.steps[step], cx, TerminalView::interrupt)
                        }
                        Effect::KillCommand { step } => {
                            command(&old.steps[step], cx, TerminalView::kill)
                        }
                        _ => {}
                    }
                }
            }
            if !self.release_held() && !performed {
                break;
            }
        }
        for active in self.runs.iter_mut().chain(&mut self.draining) {
            let changes = active.run.take_changes();
            let workflow = active.run.workflow().name.clone();
            for change in changes {
                let detail = active.run.steps()[change.step].detail.clone();
                let step = active.run.nodes()[change.step].path.clone();
                cx.emit(WorkflowsEvent::Step {
                    run: active.id,
                    workflow: workflow.clone(),
                    step,
                    status: change.status,
                    detail,
                });
            }
            let outcome = active.run.outcome();
            if outcome != active.reported {
                active.reported = outcome;
                cx.emit(WorkflowsEvent::Outcome {
                    run: active.id,
                    workflow,
                    outcome,
                });
            }
        }
        self.draining.retain(|old| old.run.is_active());
        self.ensure_ticking(cx);
        // Ticks pump four times a second while a step is active: observers
        // (the workflow tab, with its graph and the step's output) redraw
        // only when something they show changed.
        let version = self.display_version(Instant::now());
        if version != self.shown {
            self.shown = version;
            cx.notify();
        }
    }

    /// What the UI shows of the runs, hashed: each step's status, detail,
    /// exit, check output, artifacts, restarts and health, whether it
    /// awaits the user, is held, has a terminal, its output generation and
    /// the whole seconds of its elapsed time (as `elapsed_text` shows
    /// them). Equal versions look the same.
    fn display_version(&self, now: Instant) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.runs.len().hash(&mut hasher);
        for active in &self.runs {
            active.id.hash(&mut hasher);
            (active.run.outcome() as u8).hash(&mut hasher);
            let steps = active.run.steps().iter().zip(&active.steps).enumerate();
            for (index, (state, runtime)) in steps {
                state.status.name().hash(&mut hasher);
                if let Status::Failed(reason) | Status::Skipped(reason) = &state.status {
                    reason.hash(&mut hasher);
                }
                state.detail.hash(&mut hasher);
                state
                    .elapsed(now)
                    .map(|elapsed| elapsed.as_secs())
                    .hash(&mut hasher);
                state.exit.hash(&mut hasher);
                state.check_output.hash(&mut hasher);
                state.restarts.hash(&mut hasher);
                state.health_failures.hash(&mut hasher);
                state.is_blocked().hash(&mut hasher);
                state.command_live().hash(&mut hasher);
                match &state.artifacts {
                    None => 0u8.hash(&mut hasher),
                    Some(Err(error)) => (1u8, error).hash(&mut hasher),
                    Some(Ok(artifacts)) => {
                        (2u8, artifacts.len()).hash(&mut hasher);
                        for artifact in artifacts {
                            (&artifact.resolved, artifact.exists).hash(&mut hasher);
                        }
                    }
                }
                active.run.awaits_user(index).hash(&mut hasher);
                active.run.is_held(index).hash(&mut hasher);
                runtime.output_generation.hash(&mut hasher);
                runtime
                    .command
                    .as_ref()
                    .map(|view| view.entity_id())
                    .hash(&mut hasher);
                runtime.location.is_some().hash(&mut hasher);
                runtime.project.is_some().hash(&mut hasher);
                runtime.opening.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    fn perform(&mut self, index: usize, effect: Effect, cx: &mut Context<Self>) {
        let now = Instant::now();
        let run_id = self.runs[index].id;
        match effect {
            Effect::OpenProject { step, generation } => {
                let spec = self.runs[index].run.nodes()[step].step.project.clone();
                let location = spec.as_ref().map(location_of).transpose();
                match location {
                    Ok(location) => {
                        self.runs[index].steps[step].location = location;
                        self.runs[index].steps[step].opening = Some(generation);
                        self.try_open(index, step, cx);
                    }
                    Err(error) => self.runs[index]
                        .run
                        .project_failed(step, generation, &error, now),
                }
            }
            Effect::RunCheck { step, generation } => {
                let definition = self.runs[index].run.definition(step).clone();
                let project = self.runs[index].steps[step].project.clone();
                Self::background(
                    cx,
                    run_id,
                    move || commands::run_check(&definition, project.as_ref()),
                    move |active, result| {
                        active
                            .run
                            .check_finished(step, generation, result, Instant::now())
                    },
                );
            }
            Effect::StartCommand { step, generation } => {
                let definition = self.runs[index].run.definition(step).clone();
                let project = self.runs[index].steps[step].project.clone();
                cx.spawn(async move |this, cx| {
                    let background = project.clone();
                    let program = cx
                        .background_spawn(async move {
                            commands::run_program(&definition, background.as_ref())
                                .map(|program| (definition, program))
                        })
                        .await;
                    this.update(cx, |this, cx| {
                        this.start_command(run_id, step, generation, program, cx);
                        this.pump(cx);
                    })
                    .ok();
                })
                .detach();
            }
            Effect::InterruptCommand { step } => {
                command(&self.runs[index].steps[step], cx, TerminalView::interrupt)
            }
            Effect::KillCommand { step } => {
                command(&self.runs[index].steps[step], cx, TerminalView::kill)
            }
            Effect::CheckArtifacts { step, generation } => {
                let definition = self.runs[index].run.definition(step).clone();
                let project = self.runs[index].steps[step].project.clone();
                Self::background(
                    cx,
                    run_id,
                    move || commands::check_artifacts(&definition, project.as_ref()),
                    move |active, result| {
                        active
                            .run
                            .artifacts_checked(step, generation, result, Instant::now())
                    },
                );
            }
            Effect::RunHealthcheck { step, generation } => {
                let definition = self.runs[index].run.definition(step).clone();
                let project = self.runs[index].steps[step].project.clone();
                Self::background(
                    cx,
                    run_id,
                    move || {
                        let Some(check) = definition.healthcheck.clone() else {
                            return Ok(());
                        };
                        commands::probe(&definition, &check.probe, project.as_ref(), check.timeout)
                    },
                    move |active, result| {
                        active
                            .run
                            .health_checked(step, generation, result, Instant::now())
                    },
                );
            }
            Effect::OpenUrl { step, url } => {
                self.host.open_url(&url, cx);
                let active = &self.runs[index];
                cx.emit(WorkflowsEvent::UrlOpened {
                    run: active.id,
                    workflow: active.run.workflow().name.clone(),
                    step: active.run.nodes()[step].path.clone(),
                    url,
                });
            }
        }
    }

    /// Runs `work` off the UI thread, then `done` with its result on run
    /// `run_id` (if it is still there), and goes on with the runs.
    fn background<T: Send + 'static>(
        cx: &mut Context<Self>,
        run_id: u64,
        work: impl FnOnce() -> T + Send + 'static,
        done: impl FnOnce(&mut ActiveRun, T) + 'static,
    ) {
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { work() }).await;
            this.update(cx, |this, cx| {
                if let Some(active) = this.run_mut(run_id) {
                    done(active, result);
                }
                this.pump(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Asks the host for a step's project; finishes the step's opening
    /// once it is there.
    fn try_open(&mut self, index: usize, step: usize, cx: &mut Context<Self>) {
        let active = &self.runs[index];
        let (Some(generation), Some(location)) = (
            active.steps[step].opening,
            active.steps[step].location.clone(),
        ) else {
            return;
        };
        if active.run.steps()[step].generation() != generation {
            self.runs[index].steps[step].opening = None;
            return;
        }
        let now = Instant::now();
        match self.host.project(&location, cx) {
            Ok(Some(project)) => {
                let active = &mut self.runs[index];
                active.steps[step].project = Some(project);
                active.steps[step].opening = None;
                active.run.project_opened(step, generation, now);
            }
            Ok(None) => {}
            Err(error) => {
                let active = &mut self.runs[index];
                active.steps[step].opening = None;
                active.run.project_failed(step, generation, &error, now);
            }
        }
    }

    fn start_command(
        &mut self,
        run_id: u64,
        step: usize,
        generation: u64,
        program: Result<(workflow::Step, (String, Vec<String>)), String>,
        cx: &mut Context<Self>,
    ) {
        let now = Instant::now();
        let Some(active) = self.run_mut(run_id) else {
            return;
        };
        if active.run.steps()[step].generation() != generation {
            // Stopped (or replaced) meanwhile: it will not start.
            active
                .run
                .start_failed(step, generation, "stopped before it started", now);
            return;
        }
        let (definition, (program, args)) = match program {
            Ok(program) => program,
            Err(error) => {
                active.run.start_failed(step, generation, &error, now);
                return;
            }
        };
        let Some(index) = self.runs.iter().position(|run| run.id == run_id) else {
            return;
        };
        let inputs = self.inputs.clone();
        let tap: OutputTap = Arc::new(move |output| {
            let input = match output {
                Output::Data(data) => Input::Output {
                    run: run_id,
                    step,
                    generation,
                    data: data.to_vec(),
                },
                Output::Exited(code) => Input::Exited {
                    run: run_id,
                    step,
                    generation,
                    code,
                },
            };
            inputs.unbounded_send(input).ok();
        });
        let title = definition.terminal_name();
        let runtime = &self.runs[index].steps[step];
        let started = match (runtime.location.clone(), runtime.project.clone()) {
            (Some(location), Some(project)) => self
                .host
                .start(&location, &project, title, &program, &args, tap, cx),
            _ => terminal_panel::spawn_program(None, title, &program, &args, Some(tap), cx)
                .map_err(|err| format!("could not start {program}: {err}")),
        };
        let active = &mut self.runs[index];
        match started {
            // Replacing the previous view ends what still ran there.
            Ok(view) => active.steps[step].command = Some(view),
            Err(error) => active.run.start_failed(step, generation, &error, now),
        }
    }

    /// Stops every run and kills its commands at once (shutdown).
    pub fn kill_all(&mut self, cx: &mut Context<Self>) {
        self.kill_commands(cx);
        cx.notify();
    }

    fn kill_commands(&mut self, cx: &mut App) {
        let now = Instant::now();
        for active in self.runs.iter_mut().chain(&mut self.draining) {
            active.run.stop_all(now);
            active.run.take_effects();
            for runtime in &active.steps {
                command(runtime, cx, TerminalView::kill);
            }
        }
        self.draining.clear();
    }

    fn any_active(&self) -> bool {
        self.runs
            .iter()
            .chain(&self.draining)
            .any(|active| active.run.is_active())
    }

    /// Ticks while some step is active: timeouts, delays, projects being
    /// opened, ports, elapsed times.
    fn ensure_ticking(&mut self, cx: &mut Context<Self>) {
        if self.ticking || !self.any_active() {
            return;
        }
        self.ticking = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let keep = this.update(cx, |this, cx| this.tick(cx)).unwrap_or(false);
                if !keep {
                    break;
                }
            }
        })
        .detach();
    }

    fn tick(&mut self, cx: &mut Context<Self>) -> bool {
        let now = Instant::now();
        for index in 0..self.runs.len() {
            self.runs[index].run.tick(now);
            for step in 0..self.runs[index].steps.len() {
                if self.runs[index].steps[step].opening.is_some() {
                    self.try_open(index, step, cx);
                }
            }
            let run_id = self.runs[index].id;
            for (step, generation, probe) in self.runs[index].run.ready_probes() {
                let definition = self.runs[index].run.definition(step).clone();
                let runtime = &mut self.runs[index].steps[step];
                if runtime.port_busy
                    || runtime
                        .last_port_poll
                        .is_some_and(|at| now.saturating_duration_since(at) < PORT_POLL)
                {
                    continue;
                }
                runtime.port_busy = true;
                runtime.last_port_poll = Some(now);
                let project = runtime.project.clone();
                Self::background(
                    cx,
                    run_id,
                    move || {
                        commands::probe(&definition, &probe, project.as_ref(), PROBE_TIMEOUT)
                            .is_ok()
                    },
                    move |active, passed| {
                        active.steps[step].port_busy = false;
                        active
                            .run
                            .probe_result(step, generation, passed, Instant::now());
                    },
                );
            }
        }
        // Replaced runs: their stop graces.
        for old in &mut self.draining {
            old.run.tick(now);
        }
        // Delays, timeouts and graces may have finished steps.
        self.pump(cx);
        let active = self.any_active();
        if !active {
            self.ticking = false;
        }
        active
    }
}

/// Ctrl+C to (`TerminalView::interrupt`) or kills (`TerminalView::kill`)
/// the step's command, if it started one.
fn command(
    runtime: &StepRuntime,
    cx: &mut App,
    action: fn(&mut TerminalView, &mut Context<TerminalView>),
) {
    if let Some(view) = &runtime.command {
        view.update(cx, action);
    }
}

/// Writes a new commented workflow file in `dir` (`workflow.yaml`, else
/// `workflow-2.yaml`, …), and the schema its first line names, and returns
/// its path.
pub fn create_template(dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = (1..)
        .map(|n| match n {
            1 => dir.join("workflow.yaml"),
            n => dir.join(format!("workflow-{n}.yaml")),
        })
        .find(|path| !path.exists())
        .expect("a free name");
    std::fs::write(&path, workflow::TEMPLATE)?;
    write_schema(dir)?;
    Ok(path)
}

#[cfg(test)]
mod tests;
