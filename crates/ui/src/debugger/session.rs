//! One debug session: the adapter connection and what the user sees of
//! the debuggee (state, threads, stack, variables, watches, output,
//! breakpoint verification). A gpui model without a window, shared by the
//! Debug panel, the Debug Console, the editor glue and headless mode.
//!
//! Startup follows the protocol: `initialize`, then `launch`/`attach` (its
//! answer may wait for configuration), and on the adapter's `initialized`
//! event the breakpoints, exception filters and `configurationDone`. A
//! `stopped` event loads threads and the stopped thread's stack, selects
//! the top frame, loads its scopes and the variables of expanded nodes
//! (expansion is remembered by name path across stops), and re-evaluates
//! the watches. Every request has a timeout; an adapter that exits or
//! stops answering ends the session with a reason instead of hanging.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use dap::{Client, Incoming, types::*};
use futures::{
    StreamExt,
    channel::mpsc,
    future::{self, Either},
};
use gpui::{AppContext, BackgroundExecutor, Context, Entity, EventEmitter, SharedString, Task};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::{
    DebugRequest, DebugRequestKind,
    adapters::{self, PrepareError, Prepared},
    store::{DebugStore, DebugStoreEvent},
};
use crate::{project::Project, settings::DebugSettings};

/// Ordinary requests.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// `initialize`: the first run of `uv run --with debugpy` installs it.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(120);
/// `launch`/`attach` (answered after configuration; a build may run).
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(120);
/// Each of `terminate` and `disconnect` when stopping.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
/// Indexed children fetched per page.
pub const PAGE: u32 = 100;
const MAX_OUTPUT_LINES: usize = 10_000;
/// Stack frames fetched per stop.
const STACK_LEVELS: u32 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Finding and starting the adapter, launching.
    Starting,
    Running,
    Stopped,
    /// Terminating / disconnecting.
    Stopping,
    Ended,
}

impl SessionState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopped => "paused",
            Self::Stopping => "stopping",
            Self::Ended => "ended",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputCategory {
    Stdout,
    Stderr,
    /// The adapter's own messages (and next_code's).
    Console,
    Important,
    /// A REPL input echo.
    Input,
    /// A REPL result.
    Result,
}

impl OutputCategory {
    fn from_dap(category: Option<&str>) -> Option<Self> {
        match category.unwrap_or("console") {
            "stdout" => Some(Self::Stdout),
            "stderr" => Some(Self::Stderr),
            "important" => Some(Self::Important),
            "telemetry" => None,
            _ => Some(Self::Console),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::Console => "console",
            Self::Important => "important",
            Self::Input => "input",
            Self::Result => "result",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutputLine {
    pub category: OutputCategory,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Children {
    Loading,
    Loaded {
        variables: Vec<Variable>,
        /// Start of the next page of indexed children, if any is left.
        next_indexed: Option<u32>,
        total_indexed: u32,
    },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum WatchValue {
    Pending,
    Value(EvaluateResponse),
    Error(String),
}

/// What the adapter said about a breakpoint we sent.
#[derive(Debug, Clone, PartialEq)]
pub struct BreakpointStatus {
    /// The requested (0-based) line.
    pub line: usize,
    pub verified: bool,
    pub message: Option<String>,
    id: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Continue,
    Over,
    Into,
    Out,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DebugSessionEvent {
    /// Anything shown changed.
    Changed,
    /// The debuggee paused and its top frame is selected.
    Stopped,
    /// Another frame was selected.
    FrameSelected,
    /// New output (as the adapter sent it; lines may be partial).
    Output {
        category: OutputCategory,
        text: String,
    },
    BreakpointsVerified(PathBuf),
    /// The session is over; `error` says why if it failed.
    Ended {
        error: Option<String>,
    },
    /// No adapter was found; `download` offers to fetch CodeLLDB.
    AdapterMissing {
        message: String,
        download: bool,
    },
}

pub struct DebugSession {
    pub request: DebugRequest,
    project: Project,
    store: Entity<DebugStore>,
    client: Option<Arc<Client>>,
    executor: BackgroundExecutor,
    capabilities: Capabilities,
    state: SessionState,
    /// The adapter sent `initialized`: breakpoint changes go out now.
    initialized: bool,
    threads: Vec<Thread>,
    /// The stopped (or selected) thread.
    thread_id: Option<i64>,
    stop_reason: Option<String>,
    frames: Vec<StackFrame>,
    selected_frame: usize,
    scopes: Vec<Scope>,
    variables: HashMap<i64, Children>,
    /// Name paths of expanded scopes/variables, kept across stops.
    expanded: HashSet<String>,
    /// The first stop expands the first scope.
    expanded_initialized: bool,
    watches: HashMap<String, WatchValue>,
    breakpoint_status: HashMap<PathBuf, Vec<BreakpointStatus>>,
    output: Vec<OutputLine>,
    /// The last output line has no line break yet.
    output_open: bool,
    exit_code: Option<i64>,
    error: Option<String>,
    /// The adapter's last stderr bytes, for crash reports.
    stderr_tail: Arc<Mutex<String>>,
    /// Bumped on every stop and run: late answers about an older stop are
    /// dropped.
    generation: u64,
    tasks: Vec<Task<()>>,
    _incoming: Option<Task<()>>,
    _store_subscription: gpui::Subscription,
}

impl EventEmitter<DebugSessionEvent> for DebugSession {}

/// `future` with a deadline; errors as text.
async fn deadline<T>(
    executor: BackgroundExecutor,
    duration: Duration,
    what: &str,
    future: impl Future<Output = Result<T, dap::Error>>,
) -> Result<T, String> {
    let future = std::pin::pin!(future);
    match future::select(future, executor.timer(duration)).await {
        Either::Left((result, _)) => result.map_err(|err| err.to_string()),
        Either::Right(_) => Err(format!(
            "the debug adapter did not answer `{what}` within {} s",
            duration.as_secs()
        )),
    }
}

impl DebugSession {
    fn new(
        project: Project,
        request: DebugRequest,
        store: Entity<DebugStore>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription =
            cx.subscribe(&store, |this, _, event, cx| this.on_store_event(event, cx));
        Self {
            request,
            project,
            store,
            client: None,
            executor: cx.background_executor().clone(),
            capabilities: Capabilities::default(),
            state: SessionState::Starting,
            initialized: false,
            threads: Vec::new(),
            thread_id: None,
            stop_reason: None,
            frames: Vec::new(),
            selected_frame: 0,
            scopes: Vec::new(),
            variables: HashMap::new(),
            expanded: HashSet::new(),
            expanded_initialized: false,
            watches: HashMap::new(),
            breakpoint_status: HashMap::new(),
            output: Vec::new(),
            output_open: false,
            exit_code: None,
            error: None,
            stderr_tail: Arc::default(),
            generation: 0,
            tasks: Vec::new(),
            _incoming: None,
            _store_subscription: subscription,
        }
    }

    /// Finds the adapter (off the UI thread), starts it where the project
    /// lives and launches `request`.
    pub fn start(
        project: Project,
        request: DebugRequest,
        store: Entity<DebugStore>,
        settings: DebugSettings,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut session = Self::new(project.clone(), request.clone(), store, cx);
        session.push_output(
            OutputCategory::Console,
            &format!("Starting {}…\n", request.name),
            cx,
        );
        let stderr_tail = session.stderr_tail.clone();
        let (stderr_tx, mut stderr_rx) = mpsc::unbounded::<Vec<u8>>();
        let spawned = cx.background_spawn(async move {
            let prepared = adapters::prepare(&project, &request, &settings)?;
            let process = project
                .spawn_piped(&prepared.program, &prepared.args, &[], move |bytes| {
                    stderr_tx.unbounded_send(bytes.to_vec()).ok();
                })
                .map_err(|err| {
                    PrepareError::Failed(format!(
                        "Cannot start the debug adapter {}: {err}",
                        prepared.program
                    ))
                })?;
            Ok::<_, PrepareError>((prepared, process))
        });
        session.tasks.push(cx.spawn(async move |this, cx| {
            match spawned.await {
                Ok((prepared, process)) => {
                    let logger = std::env::var_os("NEXT_CODE_TRACE_DAP").map(|_| {
                        Arc::new(|kind: dap::LogKind, message: &str| {
                            eprintln!("[dap {kind:?}] {message}");
                        }) as dap::Logger
                    });
                    let (client, incoming) = Client::new(process.stdout, process.stdin, logger);
                    client.own_process(process.guard);
                    this.update(cx, |this, cx| {
                        this.connect(Arc::new(client), incoming, prepared, cx)
                    })
                    .ok();
                    // Adapter stderr: shown in the console, kept for crash reports.
                    while let Some(bytes) = stderr_rx.next().await {
                        let text = String::from_utf8_lossy(&bytes).into_owned();
                        {
                            let mut tail = stderr_tail.lock();
                            tail.push_str(&text);
                            if tail.len() > 2000 {
                                let cut = tail.len() - 2000;
                                let cut = (cut..tail.len())
                                    .find(|&i| tail.is_char_boundary(i))
                                    .unwrap_or(tail.len());
                                tail.drain(..cut);
                            }
                        }
                        if this
                            .update(cx, |this, cx| {
                                this.push_output(OutputCategory::Console, &text, cx)
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                Err(err) => {
                    this.update(cx, |this, cx| {
                        if let PrepareError::Missing { message, download } = &err {
                            cx.emit(DebugSessionEvent::AdapterMissing {
                                message: message.clone(),
                                download: *download,
                            });
                        }
                        this.end(Some(err.to_string()), cx);
                    })
                    .ok();
                }
            }
        }));
        session
    }

    /// A session over an existing connection (tests drive it with
    /// [`dap::Client::pair`]).
    #[cfg(test)]
    pub fn connected(
        project: Project,
        request: DebugRequest,
        store: Entity<DebugStore>,
        client: Client,
        incoming: mpsc::UnboundedReceiver<Incoming>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut session = Self::new(project, request.clone(), store, cx);
        let prepared = Prepared {
            program: "test-adapter".into(),
            args: Vec::new(),
            adapter_id: "test".into(),
            configuration: request.configuration.clone(),
            description: "test adapter".into(),
        };
        session.connect(Arc::new(client), incoming, prepared, cx);
        session
    }

    /// Initializes the adapter and launches/attaches.
    fn connect(
        &mut self,
        client: Arc<Client>,
        mut incoming: mpsc::UnboundedReceiver<Incoming>,
        prepared: Prepared,
        cx: &mut Context<Self>,
    ) {
        if self.state != SessionState::Starting {
            // Stopped before the adapter was up.
            return;
        }
        self.push_output(
            OutputCategory::Console,
            &format!("Debug adapter: {}\n", prepared.description),
            cx,
        );
        self.client = Some(client.clone());
        self._incoming = Some(cx.spawn(async move |this, cx| {
            while let Some(message) = incoming.next().await {
                let exited = message == Incoming::Exited;
                if this
                    .update(cx, |this, cx| this.on_incoming(message, cx))
                    .is_err()
                    || exited
                {
                    break;
                }
            }
        }));
        let executor = self.executor.clone();
        let command = match self.request.request {
            DebugRequestKind::Launch => "launch",
            DebugRequestKind::Attach => "attach",
        };
        // Owned: ending the session cancels it (and releases the client).
        let task = cx.spawn(async move |this, cx| {
            let initialize = client.request::<Initialize>(InitializeArguments {
                client_id: "next_code".into(),
                client_name: "next_code".into(),
                adapter_id: prepared.adapter_id.clone(),
                locale: "en-US".into(),
                lines_start_at1: true,
                columns_start_at1: true,
                path_format: "path".into(),
                supports_variable_type: true,
                supports_variable_paging: true,
                supports_run_in_terminal_request: false,
                supports_start_debugging_request: false,
                supports_progress_reporting: false,
                supports_invalidated_event: false,
            });
            let capabilities = match deadline(
                executor.clone(),
                INITIALIZE_TIMEOUT,
                "initialize",
                initialize,
            )
            .await
            {
                Ok(capabilities) => capabilities,
                Err(err) => {
                    let err = this
                        .read_with(cx, |this, _| this.with_stderr(&err))
                        .unwrap_or(err);
                    this.update(cx, |this, cx| {
                        this.end(Some(format!("The debug adapter did not start: {err}")), cx)
                    })
                    .ok();
                    return;
                }
            };
            let launch = this.update(cx, |this, cx| {
                this.capabilities = capabilities;
                cx.emit(DebugSessionEvent::Changed);
                client.request_raw(command, prepared.configuration.clone())
            });
            let Ok(launch) = launch else {
                return;
            };
            let result = deadline(executor, LAUNCH_TIMEOUT, command, launch).await;
            this.update(cx, |this, cx| match result {
                Ok(_) => {
                    if this.state == SessionState::Starting {
                        this.state = SessionState::Running;
                        cx.emit(DebugSessionEvent::Changed);
                    }
                }
                Err(err) if this.state != SessionState::Ended => {
                    let err = this.with_stderr(&err);
                    this.end(Some(format!("Could not {command}: {err}")), cx);
                }
                Err(_) => {}
            })
            .ok();
        });
        self.tasks.push(task);
    }

    fn with_stderr(&self, error: &str) -> String {
        let tail = self.stderr_tail.lock();
        let last = tail.trim().lines().last().unwrap_or_default();
        if last.is_empty() {
            error.to_owned()
        } else {
            format!("{error} ({last})")
        }
    }

    fn on_incoming(&mut self, message: Incoming, cx: &mut Context<Self>) {
        match message {
            Incoming::Event { event, body } => self.on_event(&event, body, cx),
            Incoming::Request {
                seq,
                command,
                arguments,
            } => self.on_reverse_request(seq, &command, arguments, cx),
            Incoming::Exited => {
                if matches!(self.state, SessionState::Stopping | SessionState::Ended) {
                    self.end(None, cx);
                } else {
                    let message = self.with_stderr("the debug adapter exited unexpectedly");
                    self.end(Some(message), cx);
                }
            }
        }
    }

    fn on_event(&mut self, event: &str, body: Value, cx: &mut Context<Self>) {
        fn parse<T: serde::de::DeserializeOwned + Default>(body: Value) -> T {
            serde_json::from_value(body).unwrap_or_default()
        }
        match event {
            "initialized" => self.configure(cx),
            "stopped" => self.on_stopped(parse(body), cx),
            "continued" => {
                let event: ContinuedEvent = parse(body);
                if self.state == SessionState::Stopped
                    && (event.all_threads_continued != Some(false)
                        || Some(event.thread_id) == self.thread_id)
                {
                    self.set_running(cx);
                }
            }
            "thread" => {
                let event: ThreadEvent = parse(body);
                match event.reason.as_str() {
                    "started" if !self.threads.iter().any(|t| t.id == event.thread_id) => {
                        self.threads.push(Thread {
                            id: event.thread_id,
                            name: format!("Thread {}", event.thread_id),
                        });
                        self.refresh_threads(cx);
                    }
                    "exited" => self.threads.retain(|thread| thread.id != event.thread_id),
                    _ => {}
                }
                cx.emit(DebugSessionEvent::Changed);
            }
            "output" => {
                let event: OutputEvent = parse(body);
                if let Some(category) = OutputCategory::from_dap(event.category.as_deref()) {
                    self.push_output(category, &event.output, cx);
                }
            }
            "breakpoint" => {
                let event: BreakpointEvent = parse(body);
                self.on_breakpoint_event(event, cx);
            }
            "exited" => {
                let event: ExitedEvent = parse(body);
                self.exit_code = Some(event.exit_code);
                self.push_output(
                    OutputCategory::Console,
                    &format!("Process exited with code {}\n", event.exit_code),
                    cx,
                );
            }
            "terminated" => self.on_terminated(cx),
            "capabilities" => {
                if let Ok(capabilities) =
                    serde_json::from_value::<Capabilities>(body["capabilities"].clone())
                {
                    // Only the fields we use; adapters send just the changed ones.
                    if !capabilities.exception_breakpoint_filters.is_empty() {
                        self.capabilities.exception_breakpoint_filters =
                            capabilities.exception_breakpoint_filters;
                    }
                    cx.emit(DebugSessionEvent::Changed);
                }
            }
            // module, loadedSource, process, progress*, invalidated, memory.
            _ => {}
        }
    }

    fn on_reverse_request(
        &mut self,
        seq: i64,
        command: &str,
        arguments: Value,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = &self.client else {
            return;
        };
        let reason = match command {
            "runInTerminal" => {
                let arguments: RunInTerminalArguments =
                    serde_json::from_value(arguments).unwrap_or_default();
                format!(
                    "next_code shows program output in the Debug Console; running `{}` in a \
                     terminal is not supported (use \"console\": \"internalConsole\")",
                    arguments.args.join(" ")
                )
            }
            "startDebugging" => {
                "child debug sessions (startDebugging) are not supported yet".to_owned()
            }
            other => format!("`{other}` is not supported"),
        };
        client.respond(seq, command, Err(reason.clone()));
        self.push_output(OutputCategory::Important, &format!("{reason}\n"), cx);
    }

    /// Sends breakpoints, exception filters and `configurationDone`.
    fn configure(&mut self, cx: &mut Context<Self>) {
        if self.initialized {
            return;
        }
        self.initialized = true;
        let files: Vec<PathBuf> = self
            .store
            .read(cx)
            .files()
            .map(|(path, _)| path.clone())
            .collect();
        let sent: Vec<Task<()>> = files
            .iter()
            .map(|path| self.send_breakpoints(path, cx))
            .collect();
        let filters = self.send_exception_filters(cx);
        let Some(client) = self.client.clone() else {
            return;
        };
        let executor = self.executor.clone();
        let configuration_done = self.capabilities.supports_configuration_done_request;
        cx.spawn(async move |this, cx| {
            future::join_all(sent).await;
            if let Some(filters) = filters {
                filters.await;
            }
            if configuration_done {
                let done = client.request::<ConfigurationDone>(Value::Null);
                if let Err(err) =
                    deadline(executor, REQUEST_TIMEOUT, "configurationDone", done).await
                {
                    this.update(cx, |this, cx| this.end(Some(err), cx)).ok();
                    return;
                }
            }
            this.update(cx, |this, cx| this.refresh_threads(cx)).ok();
        })
        .detach();
    }

    fn on_store_event(&mut self, event: &DebugStoreEvent, cx: &mut Context<Self>) {
        if !self.initialized || matches!(self.state, SessionState::Stopping | SessionState::Ended) {
            return;
        }
        match event {
            DebugStoreEvent::Breakpoints(path) => self.send_breakpoints(path, cx).detach(),
            DebugStoreEvent::ExceptionFilters => {
                if let Some(task) = self.send_exception_filters(cx) {
                    task.detach();
                }
            }
            DebugStoreEvent::Watches => {
                if self.state == SessionState::Stopped {
                    self.evaluate_watches(cx);
                }
            }
        }
    }

    /// Sends the file's enabled breakpoints; resolves when the answer is
    /// applied.
    fn send_breakpoints(&mut self, path: &Path, cx: &mut Context<Self>) -> Task<()> {
        let Some(client) = self.client.clone() else {
            return Task::ready(());
        };
        let breakpoints: Vec<_> = self
            .store
            .read(cx)
            .breakpoints(path)
            .iter()
            .filter(|bp| bp.enabled)
            .cloned()
            .collect();
        let lines: Vec<usize> = breakpoints.iter().map(|bp| bp.line).collect();
        let request = client.request::<SetBreakpoints>(SetBreakpointsArguments {
            source: Source {
                name: path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned()),
                path: Some(path.to_string_lossy().into_owned()),
                ..Source::default()
            },
            breakpoints: breakpoints
                .iter()
                .map(|bp| bp.source_breakpoint())
                .collect(),
            source_modified: false,
        });
        let executor = self.executor.clone();
        let path = path.to_path_buf();
        cx.spawn(async move |this, cx| {
            let result = deadline(executor, REQUEST_TIMEOUT, "setBreakpoints", request).await;
            this.update(cx, |this, cx| {
                let statuses = match result {
                    Ok(response) => lines
                        .iter()
                        .enumerate()
                        .map(|(index, &line)| {
                            let answer = response.breakpoints.get(index);
                            BreakpointStatus {
                                line,
                                verified: answer.is_some_and(|bp| bp.verified),
                                message: answer.and_then(|bp| bp.message.clone()),
                                id: answer.and_then(|bp| bp.id),
                            }
                        })
                        .collect(),
                    Err(err) => lines
                        .iter()
                        .map(|&line| BreakpointStatus {
                            line,
                            verified: false,
                            message: Some(err.clone()),
                            id: None,
                        })
                        .collect(),
                };
                this.breakpoint_status.insert(path.clone(), statuses);
                cx.emit(DebugSessionEvent::BreakpointsVerified(path));
            })
            .ok();
        })
    }

    fn send_exception_filters(&mut self, cx: &mut Context<Self>) -> Option<Task<()>> {
        let filters = &self.capabilities.exception_breakpoint_filters;
        if filters.is_empty() {
            return None;
        }
        let client = self.client.clone()?;
        let enabled = self.store.read(cx).enabled_exception_filters(filters);
        let request = client.request::<SetExceptionBreakpoints>(SetExceptionBreakpointsArguments {
            filters: enabled,
        });
        let executor = self.executor.clone();
        Some(cx.spawn(async move |this, cx| {
            if let Err(err) = deadline(
                executor,
                REQUEST_TIMEOUT,
                "setExceptionBreakpoints",
                request,
            )
            .await
            {
                this.update(cx, |this, cx| {
                    this.push_output(
                        OutputCategory::Important,
                        &format!("Exception breakpoints: {err}\n"),
                        cx,
                    )
                })
                .ok();
            }
        }))
    }

    fn on_breakpoint_event(&mut self, event: BreakpointEvent, cx: &mut Context<Self>) {
        let Some(id) = event.breakpoint.id else {
            return;
        };
        for (path, statuses) in &mut self.breakpoint_status {
            if let Some(status) = statuses.iter_mut().find(|status| status.id == Some(id)) {
                status.verified = event.breakpoint.verified && event.reason != "removed";
                status.message = event.breakpoint.message.clone();
                cx.emit(DebugSessionEvent::BreakpointsVerified(path.clone()));
                return;
            }
        }
    }

    fn on_stopped(&mut self, event: StoppedEvent, cx: &mut Context<Self>) {
        if matches!(self.state, SessionState::Stopping | SessionState::Ended) {
            return;
        }
        self.state = SessionState::Stopped;
        self.stop_reason = Some(match &event.description {
            Some(description) => description.clone(),
            None => event.reason.clone(),
        });
        self.thread_id = event
            .thread_id
            .or(self.thread_id)
            .or_else(|| self.threads.first().map(|thread| thread.id));
        self.clear_stack();
        if let Some(text) = &event.text {
            self.push_output(OutputCategory::Important, &format!("{text}\n"), cx);
        }
        self.refresh_threads(cx);
        match self.thread_id {
            Some(thread_id) => self.load_stack(thread_id, cx),
            // Without a thread there is nothing to show but the state.
            None => cx.emit(DebugSessionEvent::Stopped),
        }
        cx.emit(DebugSessionEvent::Changed);
    }

    fn clear_stack(&mut self) {
        self.generation += 1;
        self.frames.clear();
        self.selected_frame = 0;
        self.scopes.clear();
        self.variables.clear();
        self.watches.clear();
    }

    fn set_running(&mut self, cx: &mut Context<Self>) {
        self.state = SessionState::Running;
        self.stop_reason = None;
        self.clear_stack();
        cx.emit(DebugSessionEvent::Changed);
    }

    fn refresh_threads(&mut self, cx: &mut Context<Self>) {
        let Some(client) = &self.client else {
            return;
        };
        let request = client.request::<Threads>(Value::Null);
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            if let Ok(response) = deadline(executor, REQUEST_TIMEOUT, "threads", request).await {
                this.update(cx, |this, cx| {
                    this.threads = response.threads;
                    if this.thread_id.is_none() && this.state == SessionState::Stopped {
                        this.thread_id = this.threads.first().map(|thread| thread.id);
                    }
                    cx.emit(DebugSessionEvent::Changed);
                })
                .ok();
            }
        })
        .detach();
    }

    fn load_stack(&mut self, thread_id: i64, cx: &mut Context<Self>) {
        let Some(client) = &self.client else {
            return;
        };
        let request = client.request::<StackTrace>(StackTraceArguments {
            thread_id,
            start_frame: Some(0),
            levels: Some(STACK_LEVELS),
        });
        let generation = self.generation;
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = deadline(executor, REQUEST_TIMEOUT, "stackTrace", request).await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(response) => {
                        this.frames = response.stack_frames;
                        if !this.frames.is_empty() {
                            this.select_frame(0, cx);
                        }
                    }
                    Err(err) => this.push_output(
                        OutputCategory::Important,
                        &format!("Call stack: {err}\n"),
                        cx,
                    ),
                }
                cx.emit(DebugSessionEvent::Stopped);
                cx.emit(DebugSessionEvent::Changed);
            })
            .ok();
        })
        .detach();
    }

    /// Selects a frame of the stopped thread: its scopes, variables and
    /// watches load.
    pub fn select_frame(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(frame) = self.frames.get(index) else {
            return;
        };
        let frame_id = frame.id;
        let changed = index != self.selected_frame;
        self.selected_frame = index;
        self.scopes.clear();
        self.variables.clear();
        self.evaluate_watches(cx);
        if let Some(client) = &self.client {
            let request = client.request::<Scopes>(ScopesArguments { frame_id });
            let generation = self.generation;
            let executor = self.executor.clone();
            cx.spawn(async move |this, cx| {
                let result = deadline(executor, REQUEST_TIMEOUT, "scopes", request).await;
                this.update(cx, |this, cx| {
                    if this.generation != generation || this.selected_frame != index {
                        return;
                    }
                    match result {
                        Ok(response) => {
                            this.scopes = response.scopes;
                            if !this.expanded_initialized {
                                this.expanded_initialized = true;
                                if let Some(scope) = this.scopes.iter().find(|s| !s.expensive) {
                                    this.expanded.insert(scope.name.clone());
                                }
                            }
                            let expanded: Vec<(i64, String, Option<u32>)> = this
                                .scopes
                                .iter()
                                .filter(|scope| this.expanded.contains(&scope.name))
                                .map(|scope| {
                                    (
                                        scope.variables_reference,
                                        scope.name.clone(),
                                        scope.indexed_variables,
                                    )
                                })
                                .collect();
                            for (reference, path, indexed) in expanded {
                                this.load_variables(reference, path, indexed, cx);
                            }
                        }
                        Err(err) => this.push_output(
                            OutputCategory::Important,
                            &format!("Scopes: {err}\n"),
                            cx,
                        ),
                    }
                    cx.emit(DebugSessionEvent::Changed);
                })
                .ok();
            })
            .detach();
        }
        if changed {
            cx.emit(DebugSessionEvent::FrameSelected);
        }
        cx.emit(DebugSessionEvent::Changed);
    }

    /// Shows another thread's stack.
    pub fn select_thread(&mut self, thread_id: i64, cx: &mut Context<Self>) {
        if self.state != SessionState::Stopped || self.thread_id == Some(thread_id) {
            return;
        }
        self.thread_id = Some(thread_id);
        self.clear_stack();
        self.load_stack(thread_id, cx);
        cx.emit(DebugSessionEvent::Changed);
    }

    /// The child path of `name` under `parent`.
    pub fn child_path(parent: &str, name: &str) -> String {
        format!("{parent}\u{1f}{name}")
    }

    fn load_variables(
        &mut self,
        reference: i64,
        path: String,
        indexed: Option<u32>,
        cx: &mut Context<Self>,
    ) {
        if reference <= 0
            || matches!(
                self.variables.get(&reference),
                Some(Children::Loaded { .. })
            )
        {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        self.variables.insert(reference, Children::Loading);
        let paged = indexed.filter(|&count| count > PAGE);
        let requests = match paged {
            Some(_) => vec![
                client.request::<Variables>(VariablesArguments {
                    variables_reference: reference,
                    filter: Some("named".into()),
                    start: None,
                    count: None,
                }),
                client.request::<Variables>(VariablesArguments {
                    variables_reference: reference,
                    filter: Some("indexed".into()),
                    start: Some(0),
                    count: Some(PAGE),
                }),
            ],
            None => vec![client.request::<Variables>(VariablesArguments {
                variables_reference: reference,
                filter: None,
                start: None,
                count: None,
            })],
        };
        let generation = self.generation;
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let results =
                future::join_all(requests.into_iter().map(|request| {
                    deadline(executor.clone(), REQUEST_TIMEOUT, "variables", request)
                }))
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                let mut variables = Vec::new();
                for result in results {
                    match result {
                        Ok(response) => variables.extend(response.variables),
                        Err(err) => {
                            this.variables.insert(reference, Children::Failed(err));
                            cx.emit(DebugSessionEvent::Changed);
                            return;
                        }
                    }
                }
                let total_indexed = paged.unwrap_or(0);
                let expanded: Vec<(i64, String, Option<u32>)> = variables
                    .iter()
                    .filter(|var| var.variables_reference > 0)
                    .map(|var| (var, Self::child_path(&path, &var.name)))
                    .filter(|(_, child)| this.expanded.contains(child))
                    .map(|(var, child)| (var.variables_reference, child, var.indexed_variables))
                    .collect();
                this.variables.insert(
                    reference,
                    Children::Loaded {
                        variables,
                        next_indexed: paged.map(|_| PAGE),
                        total_indexed,
                    },
                );
                for (reference, path, indexed) in expanded {
                    this.load_variables(reference, path, indexed, cx);
                }
                cx.emit(DebugSessionEvent::Changed);
            })
            .ok();
        })
        .detach();
    }

    /// Expands or collapses a scope/variable (`path` from
    /// [`Self::child_path`]); expanding loads its children.
    pub fn toggle_expanded(
        &mut self,
        path: String,
        reference: i64,
        indexed: Option<u32>,
        cx: &mut Context<Self>,
    ) {
        if self.expanded.remove(&path) {
            cx.emit(DebugSessionEvent::Changed);
            return;
        }
        self.expanded.insert(path.clone());
        self.load_variables(reference, path, indexed, cx);
        cx.emit(DebugSessionEvent::Changed);
    }

    pub fn is_expanded(&self, path: &str) -> bool {
        self.expanded.contains(path)
    }

    /// Fetches the next page of a long list's indexed children.
    pub fn load_more(&mut self, reference: i64, cx: &mut Context<Self>) {
        let Some(Children::Loaded {
            next_indexed: Some(start),
            total_indexed,
            ..
        }) = self.variables.get(&reference)
        else {
            return;
        };
        let (start, total) = (*start, *total_indexed);
        let Some(client) = &self.client else {
            return;
        };
        let request = client.request::<Variables>(VariablesArguments {
            variables_reference: reference,
            filter: Some("indexed".into()),
            start: Some(start),
            count: Some(PAGE.min(total - start)),
        });
        if let Some(Children::Loaded { next_indexed, .. }) = self.variables.get_mut(&reference) {
            *next_indexed = None;
        }
        let generation = self.generation;
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = deadline(executor, REQUEST_TIMEOUT, "variables", request).await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                if let Some(Children::Loaded {
                    variables,
                    next_indexed,
                    ..
                }) = this.variables.get_mut(&reference)
                {
                    match result {
                        Ok(response) => {
                            variables.extend(response.variables);
                            let next = start + PAGE;
                            *next_indexed = (next < total).then_some(next);
                        }
                        Err(_) => *next_indexed = Some(start),
                    }
                }
                cx.emit(DebugSessionEvent::Changed);
            })
            .ok();
        })
        .detach();
    }

    /// Sets a variable's value (`setVariable`); the new value shows when
    /// the adapter accepts it.
    pub fn set_variable(
        &mut self,
        parent: i64,
        name: String,
        value: String,
        cx: &mut Context<Self>,
    ) {
        if !self.capabilities.supports_set_variable || self.state != SessionState::Stopped {
            return;
        }
        let Some(client) = &self.client else {
            return;
        };
        let request = client.request::<SetVariable>(SetVariableArguments {
            variables_reference: parent,
            name: name.clone(),
            value,
        });
        let generation = self.generation;
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = deadline(executor, REQUEST_TIMEOUT, "setVariable", request).await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(response) => {
                        if let Some(Children::Loaded { variables, .. }) =
                            this.variables.get_mut(&parent)
                            && let Some(variable) = variables.iter_mut().find(|v| v.name == name)
                        {
                            variable.value = response.value;
                            if let Some(reference) = response.variables_reference {
                                variable.variables_reference = reference;
                            }
                        }
                        // Watches may depend on it.
                        this.evaluate_watches(cx);
                    }
                    Err(err) => this.push_output(
                        OutputCategory::Stderr,
                        &format!("Cannot set {name}: {err}\n"),
                        cx,
                    ),
                }
                cx.emit(DebugSessionEvent::Changed);
            })
            .ok();
        })
        .detach();
    }

    fn evaluate_watches(&mut self, cx: &mut Context<Self>) {
        let expressions = self.store.read(cx).watches().to_vec();
        self.watches.clear();
        for expression in expressions {
            self.watches.insert(expression.clone(), WatchValue::Pending);
            let task = self.evaluate(expression.clone(), "watch", cx);
            let generation = self.generation;
            let frame = self.selected_frame;
            cx.spawn(async move |this, cx| {
                let result = task.await;
                this.update(cx, |this, cx| {
                    if this.generation != generation || this.selected_frame != frame {
                        return;
                    }
                    let value = match result {
                        Ok(response) => WatchValue::Value(response),
                        Err(err) => WatchValue::Error(err),
                    };
                    this.watches.insert(expression, value);
                    cx.emit(DebugSessionEvent::Changed);
                })
                .ok();
            })
            .detach();
        }
    }

    /// The scopes of stack frame `frame` (an index), asked directly.
    pub fn fetch_scopes(
        &self,
        frame: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<Scope>, String>> {
        let (Some(client), Some(frame)) = (&self.client, self.frames.get(frame)) else {
            return Task::ready(Err("not paused at that frame".into()));
        };
        let request = client.request::<Scopes>(ScopesArguments { frame_id: frame.id });
        let executor = self.executor.clone();
        cx.background_spawn(async move {
            Ok(deadline(executor, REQUEST_TIMEOUT, "scopes", request)
                .await?
                .scopes)
        })
    }

    /// The children of `reference`, asked directly (all of them).
    pub fn fetch_variables(
        &self,
        reference: i64,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<Variable>, String>> {
        let Some(client) = &self.client else {
            return Task::ready(Err("no debug session".into()));
        };
        let request = client.request::<Variables>(VariablesArguments {
            variables_reference: reference,
            filter: None,
            start: None,
            count: None,
        });
        let executor = self.executor.clone();
        cx.background_spawn(async move {
            Ok(deadline(executor, REQUEST_TIMEOUT, "variables", request)
                .await?
                .variables)
        })
    }

    /// Evaluates `expression` in the selected frame (`context`: `watch`,
    /// `repl`, `hover`).
    pub fn evaluate(
        &self,
        expression: String,
        context: &str,
        cx: &mut Context<Self>,
    ) -> Task<Result<EvaluateResponse, String>> {
        let Some(client) = &self.client else {
            return Task::ready(Err("no debug session".into()));
        };
        let request = client.request::<Evaluate>(EvaluateArguments {
            expression,
            frame_id: self.frame().map(|frame| frame.id),
            context: context.into(),
        });
        let executor = self.executor.clone();
        cx.background_spawn(async move {
            deadline(executor, REQUEST_TIMEOUT, "evaluate", request).await
        })
    }

    /// Evaluates REPL input, echoing it and its result in the output.
    pub fn repl(&mut self, expression: String, cx: &mut Context<Self>) {
        let expression = expression.trim().to_owned();
        if expression.is_empty() {
            return;
        }
        self.push_output(OutputCategory::Input, &format!("> {expression}\n"), cx);
        if self.client.is_none() || self.state == SessionState::Ended {
            self.push_output(OutputCategory::Stderr, "No debug session is running\n", cx);
            return;
        }
        let task = self.evaluate(expression, "repl", cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok(response) => this.push_output(
                    OutputCategory::Result,
                    &format!("{}\n", response.result),
                    cx,
                ),
                Err(err) => this.push_output(OutputCategory::Stderr, &format!("{err}\n"), cx),
            })
            .ok();
        })
        .detach();
    }

    /// A value to show when hovering `expression` while paused.
    pub fn evaluate_hover(
        &self,
        expression: String,
        cx: &mut Context<Self>,
    ) -> Task<Option<String>> {
        if self.state != SessionState::Stopped {
            return Task::ready(None);
        }
        let context = if self.capabilities.supports_evaluate_for_hovers {
            "hover"
        } else {
            "watch"
        };
        let task = self.evaluate(expression.clone(), context, cx);
        cx.background_spawn(async move {
            let response = task.await.ok()?;
            Some(match response.type_name {
                Some(type_name) if !type_name.is_empty() => {
                    format!("{expression}: {type_name} = {}", response.result)
                }
                _ => format!("{expression} = {}", response.result),
            })
        })
    }

    /// Continue or step the stopped thread.
    pub fn step(&mut self, step: Step, cx: &mut Context<Self>) {
        if self.state != SessionState::Stopped {
            return;
        }
        let (Some(client), Some(thread_id)) = (self.client.clone(), self.thread_id) else {
            return;
        };
        let command = match step {
            Step::Continue => Continue::COMMAND,
            Step::Over => Next::COMMAND,
            Step::Into => StepIn::COMMAND,
            Step::Out => StepOut::COMMAND,
        };
        let request = client.request_raw(command, json!({ "threadId": thread_id }));
        self.set_running(cx);
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            if let Err(err) = deadline(executor, REQUEST_TIMEOUT, command, request).await {
                this.update(cx, |this, cx| {
                    this.push_output(OutputCategory::Important, &format!("{err}\n"), cx)
                })
                .ok();
            }
        })
        .detach();
    }

    /// Pauses the running debuggee.
    pub fn pause(&mut self, cx: &mut Context<Self>) {
        if self.state != SessionState::Running {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let known = self
            .thread_id
            .or_else(|| self.threads.first().map(|thread| thread.id));
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let thread_id = match known {
                Some(id) => id,
                None => {
                    let threads = client.request::<Threads>(Value::Null);
                    match deadline(executor.clone(), REQUEST_TIMEOUT, "threads", threads).await {
                        Ok(response) if !response.threads.is_empty() => response.threads[0].id,
                        _ => return,
                    }
                }
            };
            let request = client.request::<Pause>(ThreadArguments { thread_id });
            if let Err(err) = deadline(executor, REQUEST_TIMEOUT, "pause", request).await {
                this.update(cx, |this, cx| {
                    this.push_output(OutputCategory::Important, &format!("{err}\n"), cx)
                })
                .ok();
            }
        })
        .detach();
    }

    /// Ends the session: `terminate` (if supported) lets the debuggee
    /// clean up, then `disconnect` ends it and the adapter.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, SessionState::Stopping | SessionState::Ended) {
            return;
        }
        let Some(client) = self.client.clone() else {
            self.end(None, cx);
            return;
        };
        let terminate =
            self.capabilities.supports_terminate_request && self.state != SessionState::Starting;
        self.state = SessionState::Stopping;
        self.clear_stack();
        cx.emit(DebugSessionEvent::Changed);
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            if terminate {
                let request = client.request::<Terminate>(TerminateArguments::default());
                deadline(executor.clone(), STOP_TIMEOUT, "terminate", request)
                    .await
                    .ok();
            }
            let request = client.request::<Disconnect>(DisconnectArguments {
                restart: None,
                terminate_debuggee: Some(true),
            });
            deadline(executor, STOP_TIMEOUT, "disconnect", request)
                .await
                .ok();
            this.update(cx, |this, cx| this.end(None, cx)).ok();
        })
        .detach();
    }

    /// The debuggee ended (`terminated`): disconnect, then end.
    fn on_terminated(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, SessionState::Stopping | SessionState::Ended) {
            return;
        }
        self.state = SessionState::Stopping;
        self.clear_stack();
        cx.emit(DebugSessionEvent::Changed);
        let Some(client) = self.client.clone() else {
            self.end(None, cx);
            return;
        };
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let request = client.request::<Disconnect>(DisconnectArguments::default());
            deadline(executor, STOP_TIMEOUT, "disconnect", request)
                .await
                .ok();
            this.update(cx, |this, cx| this.end(None, cx)).ok();
        })
        .detach();
    }

    /// Ends the session now (dropping the client ends the adapter).
    fn end(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        if self.state == SessionState::Ended {
            return;
        }
        self.state = SessionState::Ended;
        self.clear_stack();
        self.client = None;
        self._incoming = None;
        self.tasks.clear();
        self.breakpoint_status.clear();
        match &error {
            Some(error) => self.push_output(OutputCategory::Stderr, &format!("{error}\n"), cx),
            None => self.push_output(OutputCategory::Console, "Debug session ended\n", cx),
        }
        self.error = error.clone();
        cx.emit(DebugSessionEvent::Ended { error });
        cx.emit(DebugSessionEvent::Changed);
    }

    fn push_output(&mut self, category: OutputCategory, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        cx.emit(DebugSessionEvent::Output {
            category,
            text: text.to_owned(),
        });
        let text = text.replace("\r\n", "\n");
        let mut rest = text.as_str();
        while !rest.is_empty() {
            let (piece, ended) = match rest.find('\n') {
                Some(end) => {
                    let piece = &rest[..end];
                    rest = &rest[end + 1..];
                    (piece, true)
                }
                None => (std::mem::take(&mut rest), false),
            };
            match self.output.last_mut() {
                Some(last) if self.output_open && last.category == category => {
                    last.text.push_str(piece)
                }
                _ => self.output.push(OutputLine {
                    category,
                    text: piece.to_owned(),
                }),
            }
            self.output_open = !ended;
        }
        if self.output.len() > MAX_OUTPUT_LINES {
            let excess = self.output.len() - MAX_OUTPUT_LINES;
            self.output.drain(..excess);
        }
        cx.emit(DebugSessionEvent::Changed);
    }

    pub fn clear_output(&mut self, cx: &mut Context<Self>) {
        self.output.clear();
        self.output_open = false;
        cx.emit(DebugSessionEvent::Changed);
    }

    // Accessors for the views and headless mode.

    pub fn name(&self) -> SharedString {
        self.request.name.clone().into()
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    pub fn is_active(&self) -> bool {
        !matches!(self.state, SessionState::Ended)
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    pub fn threads(&self) -> &[Thread] {
        &self.threads
    }

    pub fn thread_id(&self) -> Option<i64> {
        self.thread_id
    }

    pub fn stop_reason(&self) -> Option<&str> {
        self.stop_reason.as_deref()
    }

    pub fn frames(&self) -> &[StackFrame] {
        &self.frames
    }

    pub fn selected_frame(&self) -> usize {
        self.selected_frame
    }

    pub fn frame(&self) -> Option<&StackFrame> {
        self.frames.get(self.selected_frame)
    }

    pub fn scopes(&self) -> &[Scope] {
        &self.scopes
    }

    pub fn children(&self, reference: i64) -> Option<&Children> {
        self.variables.get(&reference)
    }

    pub fn watch(&self, expression: &str) -> Option<&WatchValue> {
        self.watches.get(expression)
    }

    pub fn output(&self) -> &[OutputLine] {
        &self.output
    }

    pub fn exit_code(&self) -> Option<i64> {
        self.exit_code
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn breakpoint_status(&self, path: &Path) -> &[BreakpointStatus] {
        self.breakpoint_status.get(path).map_or(&[], Vec::as_slice)
    }

    /// The project path of a frame's (or output's) source.
    pub fn source_path(&self, source: Option<&Source>) -> Option<PathBuf> {
        let raw = source?.path.as_deref().filter(|path| !path.is_empty())?;
        Some(project_path(&self.project, raw))
    }

    /// The selected frame's file and 0-based line, if it has a source.
    pub fn frame_location(&self, index: usize) -> Option<(PathBuf, usize)> {
        let frame = self.frames.get(index)?;
        let path = self.source_path(frame.source.as_ref())?;
        Some((path, frame.line.saturating_sub(1) as usize))
    }
}

/// An adapter's path as the editor spells it: on Windows, paths under the
/// project root are rebuilt from the root (adapters may change the drive
/// letter's case or the separators).
pub fn project_path(project: &Project, raw: &str) -> PathBuf {
    if cfg!(windows) && !project.is_remote() {
        let root = project.root().to_string_lossy();
        if raw.len() > root.len()
            && raw.is_char_boundary(root.len())
            && raw[..root.len()].eq_ignore_ascii_case(&root)
            && matches!(raw.as_bytes()[root.len()], b'\\' | b'/')
        {
            return project.path(&raw[root.len() + 1..]);
        }
    }
    project.path(raw)
}
