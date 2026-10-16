//! Workflows: JSONC files that start a multi-project development
//! environment (format and scheduling in the `workflow` crate). They are
//! found in the user's config dir (`<config>/next_code/workflows/`) and in
//! each open project's `.next_code/workflows/`. A [`Workflows`] model per
//! window (and one in headless mode) runs them: it performs what a
//! `workflow::Run` asks for through a [`Host`] (open project tabs, start
//! commands in their Terminal panels, open URLs) and feeds back output,
//! exits, check results and listening ports.
//!
//! Each workflow file opens as a window tab, a [`WorkflowView`]: the file
//! in an editor beside a live graph of its steps (`view`), with Run All,
//! Stop All and per-step actions. Run > Open Workflow… picks one
//! ([`WorkflowPicker`]).
//!
//! Commands: `run` steps start in a terminal tab of their project (named
//! by `terminal`, else the title); without a project they run on this
//! machine in a terminal the workflow's tab shows. Checks are plain
//! processes. See `commands` for the shells used.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use futures::{StreamExt, channel::mpsc};
use gpui::{
    AnyWindowHandle, App, AppContext, Context, Entity, EventEmitter, Global, Hsla, KeyBinding,
    Window, WindowId, actions,
};
use terminal::{Output, OutputTap};
use workflow::{Effect, Outcome, ProjectSpec, Run, Status, Workflow};

use crate::{
    project::{Location, Project},
    shell::Shell,
    terminal_panel::{self, TerminalView},
    theme,
};

mod commands;
mod picker;
mod view;

pub use picker::{WorkflowPicker, WorkflowPickerEvent};
pub use view::WorkflowView;

actions!(
    workflows,
    [OpenWorkflow, NewWorkflow, RunAll, StopAll, RetryFailed]
);

/// "Run Workflow: <name>" in the command palette.
#[derive(Clone, PartialEq, Debug, gpui::Action)]
#[action(namespace = workflows, no_json)]
pub struct RunWorkflow {
    /// [`Discovered::id`].
    pub id: String,
}

pub fn bind_keys(cx: &mut App) {
    let ctx = Some(view::CONTEXT);
    cx.bind_keys([
        // JetBrains' Run and Stop.
        KeyBinding::new("shift-f10", RunAll, ctx),
        KeyBinding::new("ctrl-f2", StopAll, ctx),
    ]);
    picker::bind_keys(cx);
}

/// Marks workflow tabs in the tab bar.
pub const TAB_ICON: &str = "◈";

/// Where workflows live inside a project.
const PROJECT_DIR: &str = ".next_code/workflows";
/// How often running workflows check timeouts, ports and opening projects.
const TICK: Duration = Duration::from_millis(250);
/// How often a step's port is looked up.
const PORT_POLL: Duration = Duration::from_secs(1);
/// Time a stopped command gets after Ctrl+C before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// The user's workflows folder (next to `settings.json`).
pub fn user_dir() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("next_code").join("workflows"))
}

/// Where a workflow file was found.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    User,
    Project(Location),
}

impl Source {
    pub fn label(&self) -> String {
        match self {
            Self::User => "User".into(),
            Self::Project(location) => location.to_string(),
        }
    }
}

/// A workflow's id ([`Discovered::id`]): the file path, prefixed by the
/// project for project files. Runs are keyed by it.
pub fn workflow_id(source: &Source, path: &Path) -> String {
    match source {
        Source::User => path.to_string_lossy().into_owned(),
        Source::Project(location) => format!("{location}|{}", path.display()),
    }
}

/// A workflow file's name without `.json`/`.jsonc` (a workflow without
/// `name` is called that).
pub fn file_stem(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default()
        .trim_end_matches(".jsonc")
        .trim_end_matches(".json")
        .to_owned()
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
fn discover(user_dir: Option<&Path>, projects: &[(Location, Project)]) -> Vec<Discovered> {
    let mut found = Vec::new();
    let is_workflow = |name: &str| name.ends_with(".json") || name.ends_with(".jsonc");
    let mut add = |path: PathBuf, source: Source, text: std::io::Result<String>| {
        let stem = file_stem(&path);
        let parsed = text
            .map_err(|err| format!("cannot read the file: {err}"))
            .and_then(|text| workflow::parse(&text, &stem))
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
    if let Some(dir) = user_dir
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && is_workflow(&path.to_string_lossy()))
            .collect();
        paths.sort();
        for path in paths {
            let text = std::fs::read_to_string(&path);
            add(path, Source::User, text);
        }
    }
    for (location, project) in projects {
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
            std::fs::read_dir(project.root().join(".next_code").join("workflows"))
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        };
        names.retain(|name| is_workflow(name));
        names.sort();
        for name in names {
            let dir = project.join(&project.join(project.root(), ".next_code"), "workflows");
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
}

/// A started workflow.
pub struct ActiveRun {
    pub id: u64,
    /// [`Discovered::id`] of its file.
    pub workflow_id: String,
    pub run: Run,
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
    /// The list of workflows changed.
    Discovered,
}

pub struct Workflows {
    host: Box<dyn Host>,
    user_dir: Option<PathBuf>,
    discovered: Vec<Discovered>,
    runs: Vec<ActiveRun>,
    next_run: u64,
    inputs: mpsc::UnboundedSender<Input>,
    pump_scheduled: bool,
    ticking: bool,
    /// Feeds terminal output and exits into the runs.
    _inputs: gpui::Task<()>,
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
    let entity = cx.new(|cx| Workflows::new(Box::new(WindowHost(handle)), user_dir(), cx));
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
        Self {
            host,
            user_dir,
            discovered: Vec::new(),
            runs: Vec::new(),
            next_run: 1,
            inputs,
            pump_scheduled: false,
            ticking: false,
            _inputs: task,
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

    #[cfg(test)]
    pub fn runs(&self) -> &[ActiveRun] {
        &self.runs
    }

    pub(crate) fn user_dir(&self) -> Option<&Path> {
        self.user_dir.as_deref()
    }

    /// Reads the workflow files again; the task ends once the list is
    /// updated.
    pub fn refresh(&mut self, cx: &mut Context<Self>) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            let Ok((user_dir, projects)) = this.update(cx, |this, cx| {
                (this.user_dir.clone(), this.host.projects(cx))
            }) else {
                return;
            };
            let found = cx
                .background_spawn(async move { discover(user_dir.as_deref(), &projects) })
                .await;
            this.update(cx, |this, cx| {
                this.discovered = found;
                cx.emit(WorkflowsEvent::Discovered);
                cx.notify();
            })
            .ok();
        })
    }

    /// Starts workflow `id` as last read.
    pub fn start(&mut self, id: &str, cx: &mut Context<Self>) -> Result<u64, String> {
        let found = self
            .find(id)
            .ok_or_else(|| format!("no workflow `{id}`"))?
            .clone();
        let workflow = found
            .parsed
            .map_err(|err| format!("{} cannot run: {err}", found.name))?;
        Ok(self.start_parsed(found.id, workflow, cx))
    }

    /// Starts `workflow` afresh as the workflow with id `workflow_id` (see
    /// [`workflow_id`]); a run of it already going is stopped first and
    /// its terminals reused.
    pub fn start_parsed(
        &mut self,
        workflow_id: String,
        workflow: Arc<Workflow>,
        cx: &mut Context<Self>,
    ) -> u64 {
        let now = Instant::now();
        // The previous run's terminals are reused (replaced by title).
        let previous = self
            .runs
            .iter()
            .position(|run| run.workflow_id == workflow_id);
        let mut steps: Vec<StepRuntime> =
            workflow.steps.iter().map(|_| Default::default()).collect();
        if let Some(index) = previous {
            let mut old = self.runs.remove(index);
            old.run.stop_all(now);
            for effect in old.run.take_effects() {
                if let Effect::StopCommand { step } = effect {
                    stop_command(old.steps[step].command.clone(), cx);
                }
            }
            for (index, step) in workflow.steps.iter().enumerate() {
                if let Some(at) = old.run.step(&step.id) {
                    steps[index].command = old.steps[at].command.take();
                }
            }
        }
        let id_number = self.next_run;
        self.next_run += 1;
        let mut run = Run::new((*workflow).clone());
        run.start(now);
        self.runs.push(ActiveRun {
            id: id_number,
            workflow_id,
            run,
            steps,
            reported: Outcome::Idle,
        });
        self.schedule(cx);
        cx.notify();
        id_number
    }

    fn run_mut(&mut self, id: u64) -> Option<&mut ActiveRun> {
        self.runs.iter_mut().find(|run| run.id == id)
    }

    fn with_run(
        &mut self,
        workflow_id: &str,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut Run, Instant),
    ) -> bool {
        let Some(active) = self
            .runs
            .iter_mut()
            .find(|run| run.workflow_id == workflow_id)
        else {
            return false;
        };
        change(&mut active.run, Instant::now());
        self.schedule(cx);
        cx.notify();
        true
    }

    /// Stop All: interrupts (then kills) the commands the workflow started;
    /// project tabs and containers stay.
    pub fn stop(&mut self, workflow_id: &str, cx: &mut Context<Self>) -> bool {
        self.with_run(workflow_id, cx, |run, now| run.stop_all(now))
    }

    pub fn restart_step(&mut self, workflow_id: &str, step: usize, cx: &mut Context<Self>) -> bool {
        self.with_run(workflow_id, cx, |run, now| run.restart(step, now))
    }

    pub fn retry_failed(&mut self, workflow_id: &str, cx: &mut Context<Self>) -> bool {
        self.with_run(workflow_id, cx, |run, now| run.retry_failed(now))
    }

    /// Continue on a manual step; false if it was not waiting.
    pub fn continue_step(
        &mut self,
        workflow_id: &str,
        step: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut continued = false;
        self.with_run(workflow_id, cx, |run, now| {
            continued = run.continue_step(step, now)
        });
        continued
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
        for index in 0..self.runs.len() {
            loop {
                let effects = self.runs[index].run.take_effects();
                if effects.is_empty() {
                    break;
                }
                for effect in effects {
                    self.perform(index, effect, cx);
                }
            }
        }
        for index in 0..self.runs.len() {
            let active = &mut self.runs[index];
            let changes = active.run.take_changes();
            let workflow = active.run.workflow().name.clone();
            for change in changes {
                let detail = active.run.steps()[change.step].detail.clone();
                let step = active.run.workflow().steps[change.step].id.clone();
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
        self.ensure_ticking(cx);
        cx.notify();
    }

    fn perform(&mut self, index: usize, effect: Effect, cx: &mut Context<Self>) {
        let now = Instant::now();
        let run_id = self.runs[index].id;
        match effect {
            Effect::OpenProject { step, generation } => {
                let spec = self.runs[index].run.workflow().steps[step].project.clone();
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
                let definition = self.runs[index].run.workflow().steps[step].clone();
                let project = self.runs[index].steps[step].project.clone();
                cx.spawn(async move |this, cx| {
                    let result = cx
                        .background_spawn(async move {
                            commands::run_check(&definition, project.as_ref())
                        })
                        .await;
                    this.update(cx, |this, cx| {
                        if let Some(active) = this.run_mut(run_id) {
                            active
                                .run
                                .check_finished(step, generation, result, Instant::now());
                        }
                        this.pump(cx);
                    })
                    .ok();
                })
                .detach();
            }
            Effect::StartCommand { step, generation } => {
                let definition = self.runs[index].run.workflow().steps[step].clone();
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
            Effect::StopCommand { step } => {
                stop_command(self.runs[index].steps[step].command.clone(), cx);
            }
            Effect::OpenUrl { step, url } => {
                self.host.open_url(&url, cx);
                let active = &self.runs[index];
                cx.emit(WorkflowsEvent::UrlOpened {
                    run: active.id,
                    workflow: active.run.workflow().name.clone(),
                    step: active.run.workflow().steps[step].id.clone(),
                    url,
                });
            }
        }
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
        let Some(index) = self.runs.iter().position(|run| run.id == run_id) else {
            return;
        };
        if self.runs[index].run.steps()[step].generation() != generation {
            return; // restarted or stopped meanwhile
        }
        let now = Instant::now();
        let (definition, (program, args)) = match program {
            Ok(program) => program,
            Err(error) => {
                self.runs[index]
                    .run
                    .start_failed(step, generation, &error, now);
                return;
            }
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
        let now = Instant::now();
        for active in &mut self.runs {
            active.run.stop_all(now);
            active.run.take_effects();
            for runtime in &active.steps {
                if let Some(view) = &runtime.command {
                    view.update(cx, |view, cx| view.kill(cx));
                }
            }
        }
        cx.notify();
    }

    fn any_active(&self) -> bool {
        self.runs.iter().any(|active| active.run.is_active())
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
            for (step, generation, port) in self.runs[index].run.port_watches() {
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
                cx.spawn(async move |this, cx| {
                    let listening = cx
                        .background_spawn(async move {
                            commands::port_listening(project.as_ref(), port)
                        })
                        .await;
                    this.update(cx, |this, cx| {
                        if let Some(active) = this.run_mut(run_id) {
                            active.steps[step].port_busy = false;
                            active
                                .run
                                .port_state(step, generation, listening, Instant::now());
                        }
                        this.pump(cx);
                    })
                    .ok();
                })
                .detach();
            }
        }
        // Delays and timeouts may have finished steps.
        self.pump(cx);
        let active = self.any_active();
        if !active {
            self.ticking = false;
        }
        active
    }
}

/// Ctrl+C now, then a kill if it still runs after a grace period.
fn stop_command(view: Option<Entity<TerminalView>>, cx: &mut App) {
    let Some(view) = view else {
        return;
    };
    view.update(cx, |view, cx| view.interrupt(cx));
    cx.spawn(async move |cx| {
        cx.background_executor().timer(STOP_GRACE).await;
        view.update(cx, |view, cx| view.kill(cx)).ok();
    })
    .detach();
}

/// The window's project tabs, through its [`Shell`].
struct WindowHost(AnyWindowHandle);

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
            shell.workflow_project(location, window, cx)
        })?
    }

    fn projects(&mut self, cx: &mut App) -> Vec<(Location, Project)> {
        self.update(cx, |shell, _, cx| shell.workflow_projects(cx))
            .unwrap_or_default()
    }

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
            shell.workflow_terminal(location, title, program, args, tap, window, cx)
        })?
    }

    fn show(&mut self, location: &Location, view: &Entity<TerminalView>, cx: &mut App) {
        self.update(cx, |shell, window, cx| {
            shell.workflow_show(location, view, window, cx)
        })
        .ok();
    }

    fn open_url(&mut self, url: &str, cx: &mut App) {
        cx.open_url(url);
    }
}

/// Writes a new commented workflow file in `dir` (`workflow.jsonc`, else
/// `workflow-2.jsonc`, …) and returns its path.
pub fn create_template(dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = (1..)
        .map(|n| match n {
            1 => dir.join("workflow.jsonc"),
            n => dir.join(format!("workflow-{n}.jsonc")),
        })
        .find(|path| !path.exists())
        .expect("a free name");
    std::fs::write(&path, workflow::TEMPLATE)?;
    Ok(path)
}

#[cfg(test)]
mod tests;
