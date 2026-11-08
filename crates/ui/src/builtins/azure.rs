//! Optional Azure DevOps built-in: each Azure DevOps project opens in a window tab beside the project tabs
//! (File > Open Azure DevOps), its pull requests and pipelines in a sidebar. Azure CLI always runs on the
//! local desktop, including when Git discovery uses a remote project agent.
//!
//! A tab shows a connection file (`<config>/next_code/azure-devops/<organization>/<project>.json`), so it is
//! restored with the window. The view is drawn with the extension panel widgets (`extension_api::Widget`,
//! rendered by [`ExtensionView`]): `model` turns the state into a widget tree, and the view's events come
//! back to `on_event`. Only the log pane, whose text can be far larger than a panel's, is drawn natively.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use azure_devops::{
    Build, Check, Client, Connection, Pipeline, PrDetails, PullRequest, Record, Runner,
};
use extension_api::{
    AxisFormat, ChartPoint, ChartSeries, ChartSpec, Icon, InputValue, Mark, PanelUpdate, Scale,
    SeriesStyle, UiEvent, UiEventKind, Widget, limits,
};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla, SharedString, Subscription,
    Task, Window, div, prelude::*, px,
};

use super::{Builtin, Entry, MenuItem, WindowTab, WindowTabKind, WindowTabView};
use crate::{
    controls,
    extension_views::{ExtensionView, ExtensionViewEvent},
    pinpoint::{Pin, context::Limits},
    project::{CancelToken, Location, Project},
    selectable::selectable,
    shell::Shell,
    theme,
};

gpui::actions!(azure, [OpenAzureDevOps]);

pub const BUILTIN: Builtin = Builtin {
    id: "azure",
    name: "Azure DevOps",
    description: "Microsoft SSO, open pull requests with their reviews and checks, pipeline runs and logs; each Azure DevOps project in a window tab (File > Open Azure DevOps). Requires local Azure CLI.",
    default_enabled: false,
    labels: &[("azure::OpenAzureDevOps", "Open Azure DevOps")],
    menu_items: &[MenuItem {
        menu: "File",
        after: "app::OpenProjectInNewWindow",
        items: &[Entry::Command("azure::OpenAzureDevOps")],
    }],
    window_actions: |element, cx| element.on_action(cx.listener(open_azure_devops)),
    window_tabs: &[TAB],
    ..Builtin::NONE
};

const TAB: WindowTabKind = WindowTabKind {
    key: "azure",
    icon: "◆",
    waiting: "Opening Azure DevOps…",
    open: open_tab,
};

/// The tab of connection file `path`.
fn open_tab(
    _: &mut Shell,
    path: &Path,
    _: Option<&Location>,
    window: &mut Window,
    cx: &mut Context<Shell>,
) -> Result<Option<WindowTab>, String> {
    let view = cx.new(|cx| AzureView::new(path.to_path_buf(), cx));
    // Loading another organization or project moves the tab to that project's file.
    let moved = cx.subscribe_in(
        &view,
        window,
        |shell, view, event: &AzureEvent, window, cx| match event {
            AzureEvent::Moved(to) => shell.retarget_file_tab(view.entity_id(), to, window, cx),
        },
    );
    Ok(Some(WindowTab::new(view, vec![moved], cx)))
}

/// File > Open Azure DevOps: the tab of the Azure DevOps project the shown project's `origin` points at,
/// else of the one used last, else an empty one to fill in.
fn open_azure_devops(
    shell: &mut Shell,
    _: &OpenAzureDevOps,
    window: &mut Window,
    cx: &mut Context<Shell>,
) {
    let project = shell
        .active_workspace()
        .map(|workspace| workspace.read(cx).project().clone());
    cx.spawn_in(window, async move |this, cx| {
        let detected = match project {
            Some(project) => cx.background_spawn(async move { detect(&project) }).await,
            None => None,
        };
        this.update_in(cx, |shell, window, cx| {
            let branch = detected.as_ref().map(|(_, branch)| branch.clone());
            let Some(path) = tab_file(detected.map(|(connection, _)| connection)) else {
                return shell.report("Azure DevOps needs a configuration folder", cx);
            };
            let view = shell
                .open_file_tab(TAB.key, &path, None, window, cx)
                .and_then(|view| view.downcast::<AzureView>().ok());
            if let (Some(view), Some(branch)) = (view, branch.filter(|b| !b.is_empty())) {
                view.update(cx, |view, cx| view.set_branch(branch, cx));
            }
        })
        .ok();
    })
    .detach();
}

/// The Azure DevOps project of `project`'s `origin` remote, and its current branch.
fn detect(project: &Project) -> Option<(Connection, String)> {
    let git = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        project
            .run_program_cancellable(
                project.root(),
                "git",
                &args,
                &[],
                None,
                Duration::from_secs(10),
                None,
            )
            .ok()
            .filter(|output| output.code == Some(0))
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let connection = Connection::from_remote(&git(&["config", "--get", "remote.origin.url"])?)?;
    Some((
        connection,
        git(&["symbolic-ref", "--short", "HEAD"]).unwrap_or_default(),
    ))
}

/// Where each Azure DevOps project's connection is kept.
fn connections_dir() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("next_code").join("azure-devops"))
}

/// `name` as a file name: characters other than letters, digits, space, `-`, `_` and inner `.` escaped.
fn file_name(name: &str) -> String {
    name.chars()
        .enumerate()
        .map(|(i, c)| {
            if c.is_ascii_alphanumeric() || " -_".contains(c) || (c == '.' && i > 0) {
                c.to_string()
            } else {
                c.to_string().bytes().map(|b| format!("%{b:02X}")).collect()
            }
        })
        .collect()
}

fn connection_file(dir: &Path, connection: &Connection) -> PathBuf {
    dir.join(file_name(&connection.organization))
        .join(format!("{}.json", file_name(&connection.project)))
}

fn read_connection(path: &Path) -> Option<Connection> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_connection(path: &Path, connection: &Connection) -> std::io::Result<()> {
    std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
    std::fs::write(path, serde_json::to_vec_pretty(connection)?)
}

/// The file to open a tab of: `detected`'s (created, or kept with its saved repository and tenant), else the
/// one loaded last, else the empty connection (`new.json`, never written).
fn tab_file(detected: Option<Connection>) -> Option<PathBuf> {
    let dir = connections_dir()?;
    if let Some(connection) = detected {
        let path = connection_file(&dir, &connection);
        if read_connection(&path).is_none() {
            write_connection(&path, &connection).ok();
        }
        return Some(path);
    }
    let latest = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .flat_map(|org| {
            std::fs::read_dir(org.path())
                .into_iter()
                .flatten()
                .flatten()
        })
        .filter(|file| file.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|file| Some((file.metadata().ok()?.modified().ok()?, file.path())))
        .max();
    if let Some((_, path)) = latest {
        return Some(path);
    }
    // The panel of earlier versions kept one connection.
    let legacy = dir.with_file_name("azure-devops.json");
    if let Some(connection) = read_connection(&legacy).filter(|c| c.validate().is_ok()) {
        let path = connection_file(&dir, &connection);
        if write_connection(&path, &connection).is_ok() {
            return Some(path);
        }
    }
    Some(dir.join("new.json"))
}

pub enum AzureEvent {
    /// The tab shows another project's connection file now.
    Moved(PathBuf),
}

struct LocalCli {
    project: Project,
    cancel: CancelToken,
}
impl Runner for LocalCli {
    fn az(&self, args: &[String], timeout: Duration) -> Result<String, String> {
        let program = self
            .project
            .find_executable("az")
            .map_err(|e| e.to_string())?
            .ok_or("Azure CLI was not found. Install it on this computer and restart NextCode.")?;
        let output = self
            .project
            .run_program_cancellable(
                self.project.root(),
                &program.to_string_lossy(),
                args,
                &[("AZURE_CORE_LOGIN_EXPERIENCE_V2", "off")],
                None,
                timeout,
                Some(&self.cancel),
            )
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    "Azure CLI was not found. Install it on this computer and restart NextCode."
                        .into()
                } else {
                    err.to_string()
                }
            })?;
        if output.code != Some(0) {
            return Err(format!(
                "Azure CLI: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

#[derive(Clone)]
enum Job {
    SignIn,
    Refresh,
    Runs(i64),
    Run(i64, String, String),
    Watch(i64),
    Log(i64, i64),
}
enum Reply {
    SignedIn,
    Dashboard(
        Result<Vec<PullRequest>, String>,
        Result<Vec<Pipeline>, String>,
    ),
    Runs(Vec<Build>),
    Queued(Build),
    Watched(Build, Result<Vec<Record>, String>),
    Log(String),
}

/// The text fields, as the panel last saw them (the view owns the editors).
#[derive(Clone, Debug, Default, PartialEq)]
struct Form {
    organization: String,
    project: String,
    repository: String,
    tenant: String,
    branch: String,
    parameters: String,
}
impl Form {
    fn connection(&self) -> Connection {
        Connection {
            organization: self.organization.trim().to_owned(),
            project: self.project.trim().to_owned(),
            repository: self.repository.trim().to_owned(),
            tenant: self.tenant.trim().to_owned(),
        }
    }
    fn set_connection(&mut self, c: Connection) {
        self.organization = c.organization;
        self.project = c.project;
        self.repository = c.repository;
        self.tenant = c.tenant;
    }
    fn read(&mut self, inputs: &BTreeMap<String, InputValue>) {
        for (id, field) in [
            ("organization", &mut self.organization),
            ("project", &mut self.project),
            ("repository", &mut self.repository),
            ("tenant", &mut self.tenant),
            ("branch", &mut self.branch),
            ("parameters", &mut self.parameters),
        ] {
            if let Some(InputValue::Text(value)) = inputs.get(id) {
                *field = value.clone();
            }
        }
    }
}

/// Everything the panel shows; `model` draws it.
#[derive(Clone, Debug, Default)]
struct State {
    form: Form,
    connection: Option<Connection>,
    prs: Vec<PullRequest>,
    pipelines: Vec<Pipeline>,
    runs: Vec<Build>,
    records: Vec<Record>,
    pipeline: Option<i64>,
    build: Option<Build>,
    /// The timeline record whose log is open.
    log_record: Option<String>,
    error: Option<String>,
    notice: String,
    busy: bool,
    /// The selected pipeline's page is shown, not the pull requests.
    pipelines_tab: bool,
    show_connection: bool,
    show_run: bool,
    /// Checks, approvals needed and comments, by PR ID, filled in a batch at a time after the list loads.
    details: BTreeMap<i64, Result<PrDetails, String>>,
    /// How many PRs the details are being read for.
    details_wanted: usize,
}

/// Pull requests whose checks and comments are read (two Azure CLI calls each), and how many at once.
const DETAILS_LIMIT: usize = 100;
const DETAILS_BATCH: usize = 8;

pub struct AzureView {
    /// The connection file this tab shows, and its project's name (`organization / project`).
    path: PathBuf,
    title: String,
    view: Entity<ExtensionView>,
    state: State,
    log: SharedString,
    log_title: String,
    log_target: Option<(i64, i64)>,
    cancel: CancelToken,
    task: Task<()>,
    poll: Task<()>,
    details_cancel: CancelToken,
    details_task: Task<()>,
    _events: Subscription,
}

impl Drop for AzureView {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.details_cancel.cancel();
    }
}
impl Focusable for AzureView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }
}

impl AzureView {
    fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let view = cx.new(|cx| {
            ExtensionView::new("azure".into(), "azure".into(), "Azure DevOps".into(), cx)
        });
        // The window tab already names it.
        view.update(cx, |view, cx| view.set_docked(false, cx));
        let events = cx.subscribe(&view, |this, _, event: &ExtensionViewEvent, cx| {
            this.on_event(event, cx)
        });
        let mut state = State {
            notice: "Enter the organization and project, then Load. Sign in with Microsoft first unless Azure CLI is signed in already."
                .into(),
            show_connection: true,
            ..State::default()
        };
        let saved = read_connection(&path);
        if let Some(saved) = saved.clone() {
            state.form.set_connection(saved);
        }
        let title = match &saved {
            Some(c) if c.validate().is_ok() => format!("{} / {}", c.organization, c.project),
            _ => "Azure DevOps".into(),
        };
        let mut this = Self {
            path,
            title,
            view,
            state,
            log: "".into(),
            log_title: String::new(),
            log_target: None,
            cancel: CancelToken::new(),
            task: Task::ready(()),
            poll: Task::ready(()),
            details_cancel: CancelToken::new(),
            details_task: Task::ready(()),
            _events: events,
        };
        // Opens loaded, as a project does.
        if saved.is_some_and(|c| c.validate().is_ok()) {
            this.start(Job::Refresh, cx);
        } else {
            this.refresh_view(None, cx);
        }
        this
    }

    fn set_branch(&mut self, branch: String, cx: &mut Context<Self>) {
        self.state.form.branch = branch;
        self.refresh_view(None, cx);
    }

    /// Draw the state again. `submitted` is the form the event carried, so text typed since is kept.
    fn refresh_view(
        &mut self,
        submitted: Option<&BTreeMap<String, InputValue>>,
        cx: &mut Context<Self>,
    ) {
        let update = model(&self.state, now());
        self.view.update(cx, |view, cx| {
            view.replace_from(update, submitted, true, cx)
        });
        cx.notify();
    }

    fn on_event(&mut self, event: &ExtensionViewEvent, cx: &mut Context<Self>) {
        let ExtensionViewEvent::Invoke { event, inputs, .. } = event else {
            return;
        };
        self.state.form.read(inputs);
        let UiEvent { widget, kind } = event;
        let state = self.state.clone();
        match (widget.as_str(), kind) {
            ("nav", UiEventKind::SelectRow { .. }) => {
                self.state.pipelines_tab = false;
            }
            (
                "connection",
                UiEventKind::Change {
                    value: InputValue::Bool(open),
                },
            ) => {
                self.state.show_connection = *open;
            }
            (
                "run-section",
                UiEventKind::Change {
                    value: InputValue::Bool(open),
                },
            ) => {
                self.state.show_run = *open;
            }
            ("load", _) => self.start(Job::Refresh, cx),
            ("sign-in", _) => self.start(Job::SignIn, cx),
            ("cancel", _) => {
                self.cancel.cancel();
                self.state.notice = "Cancelling…".into();
            }
            ("prs", UiEventKind::SelectRow { index }) => {
                if let Some(url) = state
                    .prs
                    .get(*index as usize)
                    .and_then(|pr| pr_url(&state, pr))
                {
                    cx.open_url(&url);
                }
            }
            ("pipelines", UiEventKind::SelectRow { index }) => {
                if let Some(id) = state.pipelines.get(*index as usize).map(|p| p.id) {
                    self.state.pipelines_tab = true;
                    self.start(Job::Runs(id), cx);
                }
            }
            ("refresh-runs", _) => {
                if let Some(id) = state.pipeline {
                    self.start(Job::Runs(id), cx);
                }
            }
            ("runs", UiEventKind::SelectRow { index }) => {
                if let Some(id) = state.runs.get(*index as usize).map(|b| b.id) {
                    self.start(Job::Watch(id), cx);
                }
            }
            ("history", UiEventKind::ChartSelect { series, index }) => {
                let picked = history(&state, now())
                    .get(*series as usize)
                    .and_then(|s| s.points.get(*index as usize))
                    .map(|point| point.3);
                if let Some(id) = picked {
                    self.start(Job::Watch(id), cx);
                }
            }
            ("queue-run", _) => {
                if let Some(id) = state.pipeline {
                    let (branch, parameters) =
                        (state.form.branch.clone(), state.form.parameters.clone());
                    self.start(Job::Run(id, branch, parameters), cx);
                }
            }
            ("refresh-timeline", _) => {
                if let Some(id) = state.build.as_ref().map(|b| b.id) {
                    self.start(Job::Watch(id), cx);
                }
            }
            ("open-build", _) => {
                let url = state
                    .build
                    .as_ref()
                    .and_then(|b| state.connection.as_ref()?.web_url("build", b.id).ok());
                if let Some(url) = url {
                    cx.open_url(&url);
                }
            }
            ("timeline", UiEventKind::SelectRow { index }) => {
                let row = timeline(&state).into_iter().nth(*index as usize);
                if let (Some(build), Some((record, _))) = (state.build.as_ref().map(|b| b.id), row)
                    && let Some(log) = record.log.as_ref().map(|l| l.id)
                {
                    self.state.log_record = Some(record.id.clone());
                    self.log_title = record.name.clone();
                    self.log = "".into();
                    self.log_target = Some((build, log));
                    self.start(Job::Log(build, log), cx);
                }
            }
            _ => {}
        }
        self.refresh_view(Some(inputs), cx);
    }

    fn clear_selection(&mut self) {
        let state = &mut self.state;
        state.pipeline = None;
        state.build = None;
        state.runs.clear();
        state.records.clear();
        state.log_record = None;
        self.close_log();
        self.poll = Task::ready(());
    }

    fn close_log(&mut self) {
        self.log = "".into();
        self.log_target = None;
        self.state.log_record = None;
    }

    fn start(&mut self, job: Job, cx: &mut Context<Self>) {
        if self.state.busy {
            return;
        }
        let connection = match job {
            Job::Refresh | Job::SignIn => self.state.form.connection(),
            _ => self
                .state
                .connection
                .clone()
                .unwrap_or_else(|| self.state.form.connection()),
        };
        if matches!(job, Job::Refresh) {
            self.details_cancel.cancel();
            if let Err(error) = connection.validate() {
                self.state.error = Some(error);
                self.refresh_view(None, cx);
                return;
            }
            if self.state.connection.as_ref() != Some(&connection) {
                self.clear_selection();
                self.state.prs.clear();
                self.state.pipelines.clear();
            }
            if let Some(dir) = connections_dir() {
                let target = connection_file(&dir, &connection);
                let saved = write_connection(&target, &connection);
                if target != self.path && saved.is_ok() {
                    // Another project: its own tab, which loads as it opens.
                    cx.emit(AzureEvent::Moved(target));
                    return;
                }
                if let Err(err) = saved {
                    self.state.notice = format!("Could not save the connection: {err}");
                }
            }
            self.state.connection = Some(connection.clone());
            self.state.show_connection = false;
        }
        if let Job::Runs(id) = job {
            if self.state.pipeline != Some(id) {
                self.clear_selection();
            }
            self.state.pipeline = Some(id);
        }
        if let Job::Watch(id) = job
            && self.state.build.as_ref().map(|b| b.id) != Some(id)
        {
            self.state.records.clear();
            self.close_log();
            self.state.build = self.state.runs.iter().find(|b| b.id == id).cloned();
        }
        self.state.busy = true;
        self.state.error = None;
        self.cancel = CancelToken::new();
        let cancel = self.cancel.clone();
        let request = job.clone();
        let local_root = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
        self.task = cx.spawn(async move |this, cx| {
            let reply = cx
                .background_spawn(async move {
                    let client = Client::new(
                        connection,
                        LocalCli {
                            project: Project::local(local_root),
                            cancel,
                        },
                    );
                    match request {
                        Job::SignIn => client.sign_in().map(|_| Reply::SignedIn),
                        Job::Refresh => {
                            Ok(Reply::Dashboard(client.pull_requests(), client.pipelines()))
                        }
                        Job::Runs(id) => client.runs(id).map(Reply::Runs),
                        Job::Run(id, branch, parameters) => {
                            client.run(id, &branch, &parameters).map(Reply::Queued)
                        }
                        Job::Watch(id) => client
                            .build(id)
                            .map(|b| Reply::Watched(b, client.timeline(id))),
                        Job::Log(build, log) => client.log(build, log).map(Reply::Log),
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.finish(&job, reply, cx);
                this.schedule_poll(cx);
                this.refresh_view(None, cx);
            })
            .ok();
        });
        self.refresh_view(None, cx);
    }

    fn finish(&mut self, job: &Job, reply: Result<Reply, String>, cx: &mut Context<Self>) {
        let state = &mut self.state;
        state.busy = false;
        let mut load_details = false;
        match reply {
            Err(error) => {
                state.error = Some(if matches!(job, Job::Run(..)) {
                    format!(
                        "{error}\nIf the request timed out or was cancelled, check run history before queuing again."
                    )
                } else {
                    error
                });
            }
            Ok(Reply::SignedIn) => {
                state.notice = "Signed in. Load the project to view PRs and pipelines.".into();
            }
            Ok(Reply::Dashboard(prs, pipelines)) => {
                let mut errors = vec![];
                match prs {
                    Ok(prs) => {
                        state.prs = prs;
                        load_details = true;
                    }
                    Err(e) => errors.push(e),
                }
                match pipelines {
                    Ok(pipelines) => state.pipelines = pipelines,
                    Err(e) => errors.push(e),
                }
                state.error = (!errors.is_empty()).then(|| errors.join("\n"));
                state.notice = format!(
                    "Loaded {} open pull requests and {} pipelines (up to 100).",
                    state.prs.len(),
                    state.pipelines.len()
                );
            }
            Ok(Reply::Runs(runs)) => state.runs = runs,
            Ok(Reply::Queued(build)) => {
                state.notice = format!("Queued run #{}.", build.id);
                state.runs.insert(0, build.clone());
                state.build = Some(build);
                state.records.clear();
                state.log_record = None;
                self.log = "".into();
                self.log_target = None;
            }
            Ok(Reply::Watched(build, records)) => {
                if let Some(row) = state.runs.iter_mut().find(|b| b.id == build.id) {
                    *row = build.clone();
                }
                state.build = Some(build);
                match records {
                    Ok(records) => state.records = records,
                    Err(e) => state.error = Some(format!("Timeline not available yet: {e}")),
                }
            }
            Ok(Reply::Log(log)) => {
                // Bound rendering and make the truncation visible.
                let lines: Vec<_> = log.lines().collect();
                let start = lines.len().saturating_sub(2000);
                let text = lines[start..].join("\n");
                self.log = if start > 0 {
                    format!("Showing the last 2,000 of {} lines\n{text}", lines.len()).into()
                } else if text.is_empty() {
                    "No log output yet. Refresh to check again.".into()
                } else {
                    text.into()
                };
            }
        }
        if load_details {
            self.load_details(cx);
        }
        cx.notify();
    }

    /// Read the open PRs' checks and comments, a batch at a time, drawing each batch as it arrives.
    /// Separate from `task`: the pipelines stay usable meanwhile.
    fn load_details(&mut self, cx: &mut Context<Self>) {
        self.details_cancel.cancel();
        self.details_cancel = CancelToken::new();
        self.state.details.clear();
        let Some(connection) = self.state.connection.clone() else {
            return;
        };
        let prs: Vec<PullRequest> = self.state.prs.iter().take(DETAILS_LIMIT).cloned().collect();
        self.state.details_wanted = prs.len();
        let cancel = self.details_cancel.clone();
        let home = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
        self.details_task = cx.spawn(async move |this, cx| {
            for batch in prs.chunks(DETAILS_BATCH) {
                let (batch, connection, cancel_batch, home) = (
                    batch.to_vec(),
                    connection.clone(),
                    cancel.clone(),
                    home.clone(),
                );
                let results = cx
                    .background_spawn(async move {
                        std::thread::scope(|scope| {
                            let workers: Vec<_> = batch
                                .iter()
                                .map(|pr| {
                                    let client = Client::new(
                                        connection.clone(),
                                        LocalCli {
                                            project: Project::local(home.clone()),
                                            cancel: cancel_batch.clone(),
                                        },
                                    );
                                    scope.spawn(move || client.pr_details(pr))
                                })
                                .collect();
                            batch
                                .iter()
                                .zip(workers)
                                .map(|(pr, worker)| {
                                    let details = worker.join().unwrap_or_else(|_| {
                                        Err("Reading the checks failed.".into())
                                    });
                                    (pr.pull_request_id, details)
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .await;
                if cancel.is_cancelled() {
                    return;
                }
                let drawn = this.update(cx, |this, cx| {
                    this.state.details.extend(results);
                    this.refresh_view(None, cx);
                });
                if drawn.is_err() {
                    return;
                }
            }
        });
    }

    fn schedule_poll(&mut self, cx: &mut Context<Self>) {
        self.poll = Task::ready(());
        if let Some(id) = self
            .state
            .build
            .as_ref()
            .filter(|b| b.active())
            .map(|b| b.id)
        {
            // Weak entity in cx.spawn: closing the project drops polling and cancels CLI work.
            self.poll = cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_secs(10))
                    .await;
                this.update(cx, |this, cx| this.start(Job::Watch(id), cx))
                    .ok();
            });
        }
    }
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0., |d| d.as_secs_f64())
}

fn pr_url(state: &State, pr: &PullRequest) -> Option<String> {
    let mut connection = state.connection.clone()?;
    connection.repository = pr.repository.name.clone();
    connection.web_url("pr", pr.pull_request_id).ok()
}

// --- the widget tree

/// A status's palette color (green, red, yellow, grey, blue) and its words.
fn status(label: &str) -> (Option<u8>, String) {
    let color = match label {
        "succeeded" => Some(5),
        "failed" => Some(7),
        "partiallySucceeded" | "succeededWithIssues" => Some(3),
        "canceled" | "cancelling" | "skipped" | "abandoned" => Some(11),
        "inProgress" | "running" => Some(0),
        "notStarted" | "pending" | "postponed" => Some(8),
        _ => None,
    };
    // camelCase → "Partially succeeded"
    let mut words = String::new();
    for (i, c) in label.chars().enumerate() {
        if i == 0 {
            words.extend(c.to_uppercase());
        } else if c.is_uppercase() {
            words.push(' ');
            words.extend(c.to_lowercase());
        } else {
            words.push(c);
        }
    }
    (color, words)
}

fn short_duration(seconds: f64) -> String {
    let s = seconds.round() as i64;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
    }
}

fn age(iso: Option<&str>, now: f64) -> String {
    let Some(at) = iso.and_then(azure_devops::unix_seconds) else {
        return String::new();
    };
    let s = (now - at).max(0.) as i64;
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", s / 60),
        3600..86_400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

fn branch(reference: &str) -> &str {
    reference.trim_start_matches("refs/heads/")
}

fn escape(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            let escaped = "\\`*_[]<>#|~".contains(c);
            escaped
                .then_some('\\')
                .into_iter()
                .chain(std::iter::once(c))
        })
        .collect()
}

/// The runs on the history chart, a series an outcome: (color, name, points as (x, y, label, build id)).
struct History {
    color: u8,
    name: &'static str,
    points: Vec<(f64, f64, String, i64)>,
}
fn history(state: &State, now: f64) -> Vec<History> {
    let mut groups: Vec<History> = [
        (5, "Succeeded"),
        (7, "Failed"),
        (3, "Partially succeeded"),
        (11, "Other"),
    ]
    .into_iter()
    .map(|(color, name)| History {
        color,
        name,
        points: Vec::new(),
    })
    .collect();
    // Completed runs only: a running one's duration so far would dwarf the others.
    for build in state.runs.iter().rev().filter(|b| !b.active()) {
        let (Some(at), Some(duration)) = (
            build
                .queue_time
                .as_deref()
                .and_then(azure_devops::unix_seconds),
            build.duration(now),
        ) else {
            continue;
        };
        let group = match build.label() {
            "succeeded" => 0,
            "failed" => 1,
            "partiallySucceeded" => 2,
            _ => 3,
        };
        groups[group].points.push((
            at,
            duration,
            format!("{} · {}", build.build_number, branch(&build.source_branch)),
            build.id,
        ));
    }
    groups.retain(|group| !group.points.is_empty());
    groups
}

/// The timeline in display order, with each record's depth.
fn timeline(state: &State) -> Vec<(&Record, usize)> {
    azure_devops::timeline_rows(&state.records)
        .into_iter()
        .map(|(i, depth)| (&state.records[i], depth))
        .collect()
}

fn text(id: &str, label: &str, value: &str, placeholder: &str) -> Widget {
    Widget::text_input(id, label, value)
        .label_inline()
        .placeholder(placeholder)
}

fn model(state: &State, now: f64) -> PanelUpdate {
    let busy = state.busy;
    let pipeline = state
        .pipeline
        .filter(|_| state.pipelines_tab)
        .and_then(|id| state.pipelines.iter().find(|p| p.id == id));
    let heading = match pipeline {
        Some(pipeline) => format!("### {}", escape(&pipeline.name)),
        None => "### Pull requests".into(),
    };
    let mut bar = vec![Widget::column("heading", [Widget::markdown("page-title", heading)]).grow()];
    if busy {
        bar.push(Widget::label("busy", "Working…"));
        bar.push(Widget::button("cancel", "Cancel"));
    }
    // Refresh reloads what the page shows: the pipeline's runs, or everything.
    bar.push(if pipeline.is_some() {
        Widget::button("refresh-runs", "Refresh").disabled(busy)
    } else {
        Widget::button(
            "load",
            if state.connection.is_some() {
                "Refresh"
            } else {
                "Load"
            },
        )
        .disabled(busy)
    });
    bar.push(Widget::button("sign-in", "Sign in with Microsoft").disabled(busy));
    let message = match &state.error {
        Some(error) => Widget::markdown("message", format!("**Error** · {}", escape(error))),
        None => Widget::label("message", state.notice.clone()),
    };
    let content = if pipeline.is_some() {
        Widget::column("pipeline-detail", pipeline_detail(state, now)).gap(12)
    } else {
        prs_page(state, now)
    };
    PanelUpdate {
        refresh_ms: None,
        id: "azure".into(),
        state: serde_json::Value::Null,
        root: Widget::row(
            "root",
            [
                sidebar(state),
                Widget::column(
                    "main",
                    [
                        Widget::row("bar", bar)
                            .align(extension_api::Align::Center)
                            .gap(10),
                        message,
                        content,
                    ],
                )
                .gap(12)
                .grow(),
            ],
        )
        .gap(20),
    }
}

/// The project's navigation, as a project's file tree is: its pull requests, its pipelines, its connection.
fn sidebar(state: &State) -> Widget {
    let form = &state.form;
    let title = match &state.connection {
        Some(c) => format!(
            "### {}\n\n{} · {}",
            escape(&c.project),
            escape(&c.organization),
            if c.repository.is_empty() {
                "all repositories".into()
            } else {
                escape(&c.repository)
            }
        ),
        None => "### Azure DevOps".into(),
    };
    let pipeline_row = state
        .pipeline
        .filter(|_| state.pipelines_tab)
        .and_then(|id| state.pipelines.iter().position(|p| p.id == id));
    let mut children = vec![
        Widget::markdown("title", title),
        Widget::table(
            "nav",
            vec!["Repos".into(), String::new()],
            vec![vec!["Pull requests".into(), state.prs.len().to_string()]],
        )
        .widths([0, 48])
        .selectable(pipeline_row.is_none().then_some(0)),
    ];
    if state.pipelines.is_empty() {
        children.push(Widget::label(
            "no-pipelines",
            if state.connection.is_some() {
                "No pipelines."
            } else {
                "Pipelines appear once loaded."
            },
        ));
    } else {
        children.push(
            Widget::table(
                "pipelines",
                vec!["Pipelines".into()],
                state
                    .pipelines
                    .iter()
                    .map(|p| vec![p.name.clone()])
                    .collect(),
            )
            .widths([0])
            .selectable(pipeline_row.map(|i| i as u32))
            .max_height(640),
        );
    }
    children.push(Widget::section(
        "connection",
        "Connection",
        state.show_connection,
        [
            text(
                "organization",
                "Organization",
                &form.organization,
                "contoso",
            ),
            text("project", "Project", &form.project, ""),
            text(
                "repository",
                "Repository",
                &form.repository,
                "all repositories",
            ),
            text("tenant", "Tenant", &form.tenant, "optional"),
            Widget::label(
                "connection-help",
                "Load applies these. Another organization or project opens in its own tab.",
            ),
        ],
    ));
    Widget::column("sidebar", children).gap(12).width(260)
}

/// A table cell: icons, then text.
type Cell = (Vec<Mark>, String);

/// What a pull request's row says about its reviews, checks and comments, and the swatch that sums it up.
#[derive(Debug, PartialEq)]
struct PrSummary {
    approvals: Cell,
    checks: Cell,
    comments: Cell,
    color: Option<u8>,
    ready: bool,
    failing: bool,
    unresolved: bool,
}

/// Reviewer icons in a cell; more fold into "+N".
const VOTE_MARKS: usize = 8;

fn pr_summary(pr: &PullRequest, details: Option<&Result<PrDetails, String>>) -> PrSummary {
    let approved = pr.approvals();
    let loaded = details.and_then(|d| d.as_ref().ok());
    let needed = loaded.and_then(|d| d.approvals_needed);
    let met = needed.map_or(approved > 0, |needed| approved >= needed as usize);
    // A tick an approval, a clock a "waiting for the author", a cross a reject, a ring an approval still needed.
    let mut votes: Vec<Mark> = Vec::new();
    for reviewer in pr.reviewers.iter().filter(|r| !r.is_container) {
        let name = &reviewer.display_name;
        votes.push(match reviewer.vote {
            10 => Mark::new(Icon::Check, format!("{name} approved")).color(5),
            5 => Mark::new(Icon::Check, format!("{name} approved with suggestions")).color(5),
            -5 => Mark::new(Icon::Clock, format!("{name} is waiting for the author")).color(3),
            -10 => Mark::new(Icon::Cross, format!("{name} rejected")).color(7),
            _ => continue,
        });
    }
    votes.sort_by_key(|mark| match mark.icon {
        Icon::Check => 0,
        Icon::Clock => 1,
        _ => 2,
    });
    let missing = needed.map_or(0, |needed| (needed as usize).saturating_sub(approved));
    let still = if missing == 1 {
        "1 more approval needed".to_owned()
    } else {
        format!("{missing} more approvals needed")
    };
    votes.extend((0..missing).map(|_| Mark::new(Icon::Circle, still.clone())));
    let approvals = if votes.is_empty() {
        (votes, "–".to_owned())
    } else if votes.len() > VOTE_MARKS {
        let more = votes.len() - VOTE_MARKS;
        votes.truncate(VOTE_MARKS);
        (votes, format!("+{more}"))
    } else {
        (votes, String::new())
    };
    let (failed, pending, passed) = loaded.map_or((0, 0, 0), |d| {
        (
            d.count(Check::Failed),
            d.count(Check::Pending),
            d.count(Check::Passed),
        )
    });
    let plain = |text: &str| (Vec::new(), text.to_owned());
    let (checks, comments) = match details {
        None => (plain("…"), plain("…")),
        Some(Err(error)) => (
            (
                vec![Mark::new(Icon::Warning, error.clone()).color(3)],
                "Unavailable".into(),
            ),
            plain("Unavailable"),
        ),
        Some(Ok(d)) => {
            let names = |outcome: Check| {
                let names: Vec<_> = d
                    .checks
                    .iter()
                    .filter(|(_, c)| *c == outcome)
                    .map(|(name, _)| name.as_str())
                    .collect();
                names.join(", ")
            };
            let checks = if failed > 0 {
                let mark =
                    Mark::new(Icon::Cross, format!("Failed: {}", names(Check::Failed))).color(7);
                let text = if failed == 1 {
                    names(Check::Failed)
                } else {
                    format!("{failed} failed")
                };
                (vec![mark], text)
            } else if pending > 0 {
                let mark =
                    Mark::new(Icon::Clock, format!("Pending: {}", names(Check::Pending))).color(0);
                (vec![mark], format!("{pending} pending"))
            } else if passed > 0 {
                let mark =
                    Mark::new(Icon::Check, format!("Passed: {}", names(Check::Passed))).color(5);
                (vec![mark], format!("{passed} passed"))
            } else {
                plain("–")
            };
            let comments = match (d.active_threads, d.threads) {
                (_, 0) => plain("–"),
                (0, all) => (
                    vec![Mark::new(Icon::Comment, "All threads resolved")],
                    format!("{all}, none open"),
                ),
                (open, all) => (
                    vec![Mark::new(Icon::Comment, format!("{open} unresolved")).color(3)],
                    format!("{open} open of {all}"),
                ),
            };
            (checks, comments)
        }
    };
    let unresolved = loaded.is_some_and(|d| d.active_threads > 0);
    let failing = failed > 0;
    let ready = !pr.is_draft
        && loaded.is_some()
        && met
        && !pr.rejected()
        && !pr.waiting_for_author()
        && !failing
        && pending == 0
        && !unresolved;
    let color = if pr.is_draft {
        None
    } else if pr.rejected() || failing {
        Some(7)
    } else if pr.waiting_for_author() || unresolved {
        Some(3)
    } else if pending > 0 {
        Some(0)
    } else if ready {
        Some(5)
    } else {
        None
    };
    PrSummary {
        approvals,
        checks,
        comments,
        color,
        ready,
        failing,
        unresolved,
    }
}

fn prs_page(state: &State, now: f64) -> Widget {
    if state.prs.is_empty() {
        return Widget::label(
            "no-prs",
            if state.connection.is_some() {
                "No open pull requests."
            } else {
                "Load a project to list its open pull requests."
            },
        );
    }
    let project_wide = state
        .connection
        .as_ref()
        .is_none_or(|c| c.repository.is_empty());
    let drafts = state.prs.iter().filter(|pr| pr.is_draft).count();
    let oldest = state
        .prs
        .iter()
        .filter_map(|pr| azure_devops::unix_seconds(&pr.creation_date))
        .fold(f64::INFINITY, f64::min);
    let summaries: Vec<_> = state
        .prs
        .iter()
        .map(|pr| pr_summary(pr, state.details.get(&pr.pull_request_id)))
        .collect();
    let mut columns: Vec<String> = [
        "#",
        "Title",
        "Author",
        "Source → target",
        "Approvals",
        "Checks",
        "Comments",
        "Opened",
    ]
    .map(String::from)
    .into();
    // The title takes what the fixed columns leave.
    let mut widths = vec![56, 0, 130, 200, 100, 120, 120, 76];
    if project_wide {
        columns.insert(4, "Repository".into());
        widths.insert(4, 100);
    }
    let shown = limits::TABLE_CELLS / columns.len();
    let rows = state
        .prs
        .iter()
        .zip(&summaries)
        .take(shown)
        .map(|(pr, summary)| {
            let mut row = vec![
                pr.pull_request_id.to_string(),
                format!("{}{}", if pr.is_draft { "Draft · " } else { "" }, pr.title),
                pr.created_by.display_name.clone(),
                format!(
                    "{} → {}",
                    branch(&pr.source_ref_name),
                    branch(&pr.target_ref_name)
                ),
                summary.approvals.1.clone(),
                summary.checks.1.clone(),
                summary.comments.1.clone(),
                age(Some(&pr.creation_date), now),
            ];
            if project_wide {
                row.insert(4, pr.repository.name.clone());
            }
            row
        })
        .collect();
    // Icons in the approvals, checks and comments columns.
    let first = if project_wide { 5 } else { 4 };
    let marks = summaries
        .iter()
        .take(shown)
        .map(|summary| {
            let mut cells = vec![Vec::new(); first];
            cells.extend(
                [&summary.approvals, &summary.checks, &summary.comments].map(|cell| cell.0.clone()),
            );
            cells
        })
        .collect();
    // The detail tiles count what has been read so far, and say so until it is all read.
    let read = state.details.len();
    let reading = read < state.details_wanted;
    let tile = |id: &str, label: &str, count: usize, color: u8| {
        // Nothing counted yet is not zero.
        let value = if read == 0 && reading {
            "…".into()
        } else {
            count.to_string()
        };
        let tile = Widget::stat(id, label, value);
        let tile = if count > 0 { tile.color(color) } else { tile };
        if reading {
            tile.detail(format!("{read} of {} read", state.details_wanted))
        } else {
            tile
        }
    };
    let mut help = vec![];
    if reading {
        help.push(format!(
            "Reading checks and comments: {read} of {} pull requests.",
            state.details_wanted
        ));
    }
    let failed: Vec<_> = state
        .details
        .values()
        .filter_map(|d| d.as_ref().err())
        .collect();
    if let Some(error) = failed.first() {
        help.push(format!("Checks unavailable for {}: {error}", failed.len()));
    }
    if state.prs.len() > DETAILS_LIMIT {
        help.push(format!(
            "Checks and comments are read for the first {DETAILS_LIMIT}."
        ));
    }
    if state.prs.len() > shown {
        help.push(format!("Showing {shown} of {}.", state.prs.len()));
    }
    help.push("Click a pull request to review it in the browser.".into());
    Widget::column(
        "prs-page",
        [
            Widget::grid(
                "pr-stats",
                150,
                [
                    Widget::stat("open", "Open", state.prs.len().to_string()).detail(format!(
                        "{drafts} draft{}",
                        if drafts == 1 { "" } else { "s" }
                    )),
                    tile(
                        "ready",
                        "Ready to merge",
                        summaries.iter().filter(|s| s.ready).count(),
                        5,
                    ),
                    tile(
                        "failing",
                        "Failing checks",
                        summaries.iter().filter(|s| s.failing).count(),
                        7,
                    ),
                    tile(
                        "unresolved",
                        "Unresolved comments",
                        summaries.iter().filter(|s| s.unresolved).count(),
                        3,
                    ),
                    Widget::stat(
                        "oldest",
                        "Oldest opened",
                        if oldest.is_finite() {
                            short_age(now - oldest)
                        } else {
                            "–".into()
                        },
                    ),
                ],
            ),
            Widget::table("prs", columns, rows)
                .widths(widths)
                .swatches(summaries.iter().take(shown).map(|s| s.color))
                .marks(marks)
                .selectable(None)
                .max_height(900),
            Widget::label("prs-help", help.join(" ")),
        ],
    )
    .gap(10)
}

fn short_age(seconds: f64) -> String {
    let s = seconds.max(0.) as i64;
    match s {
        0..3600 => format!("{}m ago", s / 60),
        3600..86_400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

fn pipeline_detail(state: &State, now: f64) -> Vec<Widget> {
    let busy = state.busy;
    let form = &state.form;
    let mut out = vec![Widget::section(
        "run-section",
        "Run this pipeline",
        state.show_run,
        [Widget::row(
            "run-form",
            [
                text("branch", "Branch", &form.branch, "main"),
                text(
                    "parameters",
                    "Parameters",
                    &form.parameters,
                    r#"{"environment":"staging"}"#,
                ),
                Widget::button("queue-run", "Run").disabled(busy),
            ],
        )
        .align(extension_api::Align::Center)],
    )];
    let completed: Vec<_> = state.runs.iter().filter(|b| !b.active()).collect();
    let succeeded = completed.iter().filter(|b| b.result == "succeeded").count();
    let mut durations: Vec<f64> = completed.iter().filter_map(|b| b.duration(now)).collect();
    durations.sort_by(f64::total_cmp);
    let median = durations.get(durations.len() / 2).copied();
    if let Some(last) = state.runs.first() {
        let (color, words) = status(last.label());
        let mut tile = Widget::stat("last-run", "Last run", words).detail(format!(
            "{} · {}",
            last.build_number,
            age(last.queue_time.as_deref(), now)
        ));
        if let Some(color) = color {
            tile = tile.color(color);
        }
        out.push(Widget::grid(
            "run-stats",
            160,
            [
                tile,
                Widget::stat(
                    "success",
                    "Success rate",
                    if completed.is_empty() {
                        "–".into()
                    } else {
                        format!("{:.0}%", 100. * succeeded as f64 / completed.len() as f64)
                    },
                )
                .detail(format!("{succeeded} of {} completed", completed.len())),
                Widget::stat(
                    "median",
                    "Median duration",
                    median.map_or("–".into(), short_duration),
                ),
                Widget::stat(
                    "active",
                    "Running",
                    state.runs.iter().filter(|b| b.active()).count().to_string(),
                ),
            ],
        ));
    }
    let groups = history(state, now);
    if !groups.is_empty() {
        out.push(
            Widget::chart(
                "history",
                "Duration of completed runs",
                ChartSpec::scatter(groups.iter().map(|group| {
                    ChartSeries::new(
                        group.name,
                        group
                            .points
                            .iter()
                            .map(|(x, y, label, _)| ChartPoint::labelled(*x, *y, label)),
                    )
                    .color(group.color)
                    .style(SeriesStyle::Points)
                }))
                .formats(AxisFormat::Date, AxisFormat::Duration)
                .scales(Scale::Linear, Scale::Linear),
            )
            .height(190),
        );
    }
    let selected_run = state
        .build
        .as_ref()
        .and_then(|b| state.runs.iter().position(|r| r.id == b.id));
    if state.runs.is_empty() {
        out.push(Widget::label("no-runs", "No runs yet."));
    } else {
        out.push(
            Widget::table(
                "runs",
                [
                    "Run",
                    "Status",
                    "Branch",
                    "Requested by",
                    "Queued",
                    "Duration",
                ]
                .map(String::from)
                .into(),
                state
                    .runs
                    .iter()
                    .map(|b| {
                        vec![
                            b.build_number.clone(),
                            status(b.label()).1,
                            branch(&b.source_branch).to_owned(),
                            b.requested_for.display_name.clone(),
                            age(b.queue_time.as_deref(), now),
                            b.duration(now).map_or(String::new(), short_duration),
                        ]
                    })
                    .collect(),
            )
            .widths([110, 130, 0, 150, 90, 90])
            .swatches(state.runs.iter().map(|b| status(b.label()).0))
            .selectable(selected_run.map(|i| i as u32))
            .max_height(320),
        );
    }
    if let Some(build) = &state.build {
        let (_, words) = status(build.label());
        out.push(
            Widget::row(
                "run-head",
                [
                    Widget::markdown(
                        "run-title",
                        format!(
                            "**Run {}** · {words}{}",
                            escape(&build.build_number),
                            if build.active() {
                                " · refreshing every 10 s"
                            } else {
                                ""
                            }
                        ),
                    ),
                    Widget::button("refresh-timeline", "Refresh").disabled(busy),
                    Widget::button("open-build", "Open in Azure DevOps"),
                ],
            )
            .align(extension_api::Align::Center),
        );
        let rows = timeline(state);
        if rows.is_empty() {
            out.push(Widget::label(
                "no-timeline",
                if build.active() {
                    "Waiting for the run's timeline…"
                } else {
                    "This run has no timeline."
                },
            ));
        } else {
            let shown = limits::TABLE_CELLS / 6;
            let selected = rows
                .iter()
                .position(|(record, _)| state.log_record.as_deref() == Some(record.id.as_str()));
            out.push(
                Widget::table(
                    "timeline",
                    ["Step", "Type", "Status", "Duration", "Issues", "Log"]
                        .map(String::from)
                        .into(),
                    rows.iter()
                        .take(shown)
                        .map(|(record, depth)| {
                            let (_, words) = status(record.label());
                            vec![
                                format!("{}{}", "\u{2003}".repeat((*depth).min(8)), record.name),
                                record.kind.clone(),
                                match record.percent_complete {
                                    Some(p) if record.result.is_none() && p > 0 => {
                                        format!("{words} · {p}%")
                                    }
                                    _ => words,
                                },
                                azure_devops::span(
                                    record.start_time.as_deref(),
                                    record.finish_time.as_deref(),
                                    now,
                                )
                                .map_or(String::new(), short_duration),
                                match (record.error_count, record.warning_count) {
                                    (0, 0) => String::new(),
                                    (e, 0) => format!("{e} errors"),
                                    (0, w) => format!("{w} warnings"),
                                    (e, w) => format!("{e} errors, {w} warnings"),
                                },
                                if record.log.is_some() {
                                    "View log".into()
                                } else {
                                    String::new()
                                },
                            ]
                        })
                        .collect(),
                )
                .widths([0, 80, 150, 90, 200, 80])
                .swatches(
                    rows.iter()
                        .take(shown)
                        .map(|(record, _)| status(record.label()).0),
                )
                .marks(
                    rows.iter()
                        .take(shown)
                        .map(|(record, _)| {
                            let mut issues = Vec::new();
                            if record.error_count > 0 {
                                issues.push(Mark::new(Icon::Cross, "Errors").color(7));
                            }
                            if record.warning_count > 0 {
                                issues.push(Mark::new(Icon::Warning, "Warnings").color(3));
                            }
                            vec![vec![], vec![], vec![], vec![], issues]
                        })
                        .collect(),
                )
                .selectable(selected.map(|i| i as u32))
                .max_height(520),
            );
            out.push(Widget::label(
                "timeline-help",
                "Click a step to open its log below.",
            ));
        }
    }
    out
}

impl EventEmitter<AzureEvent> for AzureView {}

impl WindowTabView for AzureView {
    fn title(&self, _: &App) -> String {
        self.title.clone()
    }

    fn is_dirty(&self, _: &App) -> bool {
        false
    }

    fn icon_color(&self, _: &App) -> Hsla {
        theme::accent()
    }

    fn context(&self, _: Limits, _: &App) -> serde_json::Value {
        let state = &self.state;
        let mut context = serde_json::json!({
            "focus": "azure-devops",
            "loaded": state.connection.is_some(),
            "pull_requests": state.prs.len(),
            "pipelines": state.pipelines.len(),
        });
        if let Some(c) = &state.connection {
            context["organization"] = c.organization.clone().into();
            context["project"] = c.project.clone().into();
            context["repository"] = c.repository.clone().into();
        }
        if let Some(pipeline) = state.pipeline.filter(|_| state.pipelines_tab) {
            context["pipeline"] = pipeline.into();
        }
        if let Some(error) = &state.error {
            context["error"] = error.clone().into();
        }
        context
    }
}

impl Render for AzureView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let weak = cx.weak_entity();
        let log_pane = (!self.log.is_empty() || self.log_target.is_some()).then(|| {
            let (refresh, close) = (weak.clone(), weak);
            let target = self.log_target;
            div()
                .flex_none()
                .h(px(280.))
                .flex()
                .flex_col()
                .border_t_1()
                .border_color(theme::border())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1()
                        .bg(theme::panel())
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child(format!("Log · {}", self.log_title)),
                        )
                        .child(controls::button(
                            "azure-log-refresh",
                            "Refresh",
                            false,
                            None,
                            self.state.busy || target.is_none(),
                            Rc::new(move |_, cx| {
                                refresh
                                    .update(cx, |this, cx| {
                                        if let Some((build, log)) = this.log_target {
                                            this.start(Job::Log(build, log), cx);
                                        }
                                    })
                                    .ok();
                            }),
                        ))
                        .child(controls::button(
                            "azure-log-close",
                            "Close",
                            false,
                            None,
                            false,
                            Rc::new(move |_, cx| {
                                close
                                    .update(cx, |this, cx| {
                                        this.close_log();
                                        this.refresh_view(None, cx);
                                    })
                                    .ok();
                            }),
                        )),
                )
                .child(
                    div()
                        .id("azure-log-output")
                        .flex_1()
                        .min_h_0()
                        .overflow_scroll()
                        .px_3()
                        .py_2()
                        .font_family(theme::MONO_FONT)
                        .text_xs()
                        .child(if self.log.is_empty() {
                            selectable("azure-log-text", "Loading…")
                        } else {
                            selectable("azure-log-text", self.log.clone())
                        }),
                )
                .pin("log")
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::background())
            .text_color(theme::text())
            .text_sm()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(self.view.clone()),
            )
            .children(log_pane)
            .pin("azure-devops")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn loaded() -> State {
        let pr = |id: i64, repo: &str, draft: bool| PullRequest {
            pull_request_id: id,
            title: format!("Pull request {id} with a fairly long title"),
            created_by: azure_devops::Identity {
                display_name: "Ada Lovelace".into(),
            },
            source_ref_name: "refs/heads/feature/x".into(),
            target_ref_name: "refs/heads/main".into(),
            is_draft: draft,
            repository: azure_devops::Repository {
                name: repo.into(),
                ..Default::default()
            },
            creation_date: "2026-11-01T10:00:00Z".into(),
            reviewers: vec![],
        };
        let build = |id: i64, result: &str, status: &str| -> Build {
            serde_json::from_value(json!({"id": id, "buildNumber": format!("2026.{id}"), "status": status,
                "result": result, "sourceBranch": "refs/heads/main", "requestedFor": {"displayName": "Ada"},
                "queueTime": format!("2026-11-0{}T09:00:00Z", id % 9 + 1),
                "startTime": format!("2026-11-0{}T09:01:00Z", id % 9 + 1),
                "finishTime": if status == "completed" { json!(format!("2026-11-0{}T09:0{}:30Z", id % 9 + 1, id % 9 + 2)) } else { json!(null) }}))
            .unwrap()
        };
        let record = |id: &str, parent: Option<&str>, log: Option<i64>| -> Record {
            serde_json::from_value(json!({"id": id, "parentId": parent, "name": format!("Step {id}"), "type": "Task",
                "state": "completed", "result": "succeeded", "order": 1, "log": log.map(|id| json!({"id": id})),
                "startTime": "2026-11-01T09:01:00Z", "finishTime": "2026-11-01T09:02:00Z", "errorCount": 0, "warningCount": 1}))
            .unwrap()
        };
        let runs = vec![
            build(5, "", "inProgress"),
            build(4, "failed", "completed"),
            build(3, "succeeded", "completed"),
            build(2, "partiallySucceeded", "completed"),
        ];
        State {
            connection: Some(Connection {
                organization: "contoso".into(),
                project: "Web".into(),
                ..Connection::default()
            }),
            prs: vec![pr(1, "api", false), pr(2, "web", true)],
            pipelines: vec![Pipeline {
                id: 7,
                name: "CI".into(),
            }],
            pipeline: Some(7),
            build: Some(runs[1].clone()),
            runs,
            records: vec![record("a", None, None), record("b", Some("a"), Some(3))],
            log_record: Some("b".into()),
            notice: "Loaded.".into(),
            // One read, one failed, one still being read.
            details: BTreeMap::from([
                (1, Ok(PrDetails::default())),
                (2, Err("Azure CLI: denied".into())),
            ]),
            details_wanted: 3,
            ..State::default()
        }
    }

    #[test]
    fn every_page_is_a_valid_panel() {
        let now = azure_devops::unix_seconds("2026-11-06T12:00:00Z").unwrap();
        let mut states = vec![State::default(), loaded()];
        let mut pipelines = loaded();
        pipelines.pipelines_tab = true;
        pipelines.busy = true;
        pipelines.error = Some("Azure CLI: *denied* <tenant>".into());
        states.push(pipelines);
        let mut empty_pipeline = loaded();
        empty_pipeline.pipelines_tab = true;
        empty_pipeline.runs.clear();
        empty_pipeline.build = None;
        states.push(empty_pipeline);
        for state in &states {
            let panel = model(state, now);
            extensions::validate_panel_update(&panel).unwrap_or_else(|e| panic!("{e:#}"));
        }
    }

    #[test]
    fn history_groups_runs_by_outcome_and_maps_points_back_to_runs() {
        let now = azure_devops::unix_seconds("2026-11-06T12:00:00Z").unwrap();
        let groups = history(&loaded(), now);
        let names: Vec<_> = groups.iter().map(|g| g.name).collect();
        assert_eq!(names, ["Succeeded", "Failed", "Partially succeeded"]);
        assert_eq!(groups[1].points[0].3, 4);
        assert_eq!(groups[1].points[0].1, 5. * 60. + 30.);
        assert_eq!(
            status("partiallySucceeded"),
            (Some(3), "Partially succeeded".into())
        );
        assert_eq!(short_duration(3725.), "1h 02m");
    }

    #[test]
    fn pr_rows_sum_up_reviews_checks_and_comments() {
        let pr = |votes: &[i32]| PullRequest {
            reviewers: votes
                .iter()
                .enumerate()
                .map(|(i, &vote)| azure_devops::Reviewer {
                    display_name: format!("R{i}"),
                    vote,
                    ..Default::default()
                })
                .collect(),
            ..PullRequest::default()
        };
        let details = |needed: Option<u32>, checks: &[Check], active: usize| {
            Ok(PrDetails {
                approvals_needed: needed,
                checks: checks.iter().map(|&c| ("web-ci".to_owned(), c)).collect(),
                threads: 4,
                active_threads: active,
            })
        };
        let icons = |cell: &Cell| -> Vec<(Icon, Option<u8>)> {
            cell.0.iter().map(|mark| (mark.icon, mark.color)).collect()
        };
        let ready = pr_summary(&pr(&[10, 5]), Some(&details(Some(2), &[Check::Passed], 0)));
        assert_eq!(icons(&ready.approvals), [(Icon::Check, Some(5)); 2]);
        assert_eq!(ready.approvals.0[1].label, "R1 approved with suggestions");
        assert_eq!(ready.checks.1, "1 passed");
        assert_eq!(icons(&ready.checks), [(Icon::Check, Some(5))]);
        assert_eq!(ready.comments.1, "4, none open");
        assert!(ready.ready && ready.color == Some(5));
        // Approvals first, then waits; a ring an approval still needed.
        let waiting = pr_summary(
            &pr(&[-5, 10]),
            Some(&details(Some(3), &[Check::Failed, Check::Passed], 2)),
        );
        assert_eq!(
            icons(&waiting.approvals),
            [
                (Icon::Check, Some(5)),
                (Icon::Clock, Some(3)),
                (Icon::Circle, None),
                (Icon::Circle, None)
            ]
        );
        assert_eq!(waiting.approvals.0[3].label, "2 more approvals needed");
        assert_eq!(waiting.checks.1, "web-ci");
        assert_eq!(waiting.checks.0[0].label, "Failed: web-ci");
        assert_eq!(waiting.comments.1, "2 open of 4");
        assert!(!waiting.ready && waiting.failing && waiting.unresolved);
        assert_eq!(waiting.color, Some(7));
        let rejected = pr_summary(&pr(&[-10]), Some(&details(None, &[Check::Pending], 0)));
        assert_eq!(icons(&rejected.approvals), [(Icon::Cross, Some(7))]);
        assert_eq!(icons(&rejected.checks), [(Icon::Clock, Some(0))]);
        // Many reviewers fold into "+N".
        let crowd = pr_summary(&pr(&[10; 11]), None);
        assert_eq!(
            (crowd.approvals.0.len(), crowd.approvals.1.as_str()),
            (VOTE_MARKS, "+3")
        );
        // Before the details arrive, the votes already show.
        let reading = pr_summary(&pr(&[10]), None);
        assert_eq!(icons(&reading.approvals), [(Icon::Check, Some(5))]);
        assert_eq!(reading.checks, (vec![], "…".to_owned()));
        assert_eq!(
            pr_summary(&pr(&[]), None).approvals,
            (vec![], "–".to_owned())
        );
        assert!(!reading.ready && reading.color.is_none());
    }

    #[test]
    fn connection_files_are_one_a_project_and_safe_names() {
        let dir = Path::new("/c");
        let connection = Connection {
            organization: "contoso".into(),
            project: "My Project/..".into(),
            ..Connection::default()
        };
        assert_eq!(
            connection_file(dir, &connection),
            Path::new("/c/contoso/My Project%2F...json")
        );
        assert_eq!(file_name(".hidden"), "%2Ehidden");
    }

    #[test]
    fn form_reads_the_event_inputs() {
        let mut form = Form::default();
        form.read(&BTreeMap::from([
            ("organization".into(), InputValue::Text(" contoso ".into())),
            ("branch".into(), InputValue::Text("dev".into())),
        ]));
        assert_eq!(form.connection().organization, "contoso");
        assert_eq!(form.branch, "dev");
    }
}
