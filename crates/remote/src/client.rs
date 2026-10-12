//! Client side of the agent protocol: request/response correlation, event
//! streams, and disconnect detection over any byte transport.

use std::{
    collections::HashMap,
    io::{self, BufRead, BufReader, Read, Write},
    path::Path,
    process::Child,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc as std_mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use futures::channel::mpsc as async_mpsc;
use lsp::{Incoming, LanguageServer, LogKind, Logger};
use parking_lot::Mutex;
use proto::{
    AgentMessage, Banner, Batch, ClientMessage, DirEntry, Event, FileKind, PROTOCOL_VERSION,
    PtySize, Request, RequestId, Response, SearchEvent, SearchLimits, SearchQuery, SpawnRequest,
};
use terminal::{Palette, PtyOutput, RemotePty, Terminal, TerminalEvent};

/// How long one request may take before it fails.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Text skipped while looking for the banner (login-script noise) is capped.
const MAX_BANNER_SEARCH: usize = 64 * 1024;

/// How the client reaches an agent.
pub struct Transport {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    /// The agent process; reaped (or killed) when the client is dropped.
    pub child: Option<Child>,
    /// Recent stderr of the agent, quoted when it disconnects.
    pub stderr: Option<Arc<Mutex<String>>>,
}

/// What the agent reported on [`Request::Initialize`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Initialized {
    pub root: String,
    pub home: String,
    pub agent_version: String,
}

/// What a process run to completion produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunOutput {
    /// `None` when killed by a signal.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

type Sink = Box<dyn FnMut(Event) + Send>;

struct Shared {
    /// For messages, e.g. "WSL agent (Ubuntu)".
    name: String,
    pending: Mutex<HashMap<RequestId, std_mpsc::Sender<io::Result<Response>>>>,
    streams: Mutex<HashMap<RequestId, Sink>>,
    disconnected: Mutex<Option<String>>,
    listeners: Mutex<Vec<async_mpsc::UnboundedSender<String>>>,
    /// Set when the client shuts down on purpose.
    closing: AtomicBool,
    child: Mutex<Option<Child>>,
    stderr: Option<Arc<Mutex<String>>>,
}

impl Shared {
    fn disconnect(&self, cause: &str) {
        if self.disconnected.lock().is_some() {
            return;
        }
        let mut reason = format!("{} disconnected: {cause}", self.name);
        if let Some(child) = &mut *self.child.lock() {
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        reason.push_str(&format!(" ({status})"));
                        // Let the stderr reader catch the last lines.
                        thread::sleep(Duration::from_millis(100));
                        break;
                    }
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    _ => break,
                }
            }
        }
        if let Some(stderr) = &self.stderr {
            let tail = stderr.lock();
            let tail = tail.trim();
            if !tail.is_empty() {
                reason.push_str(&format!(". Agent output: {tail}"));
            }
        }
        *self.disconnected.lock() = Some(reason.clone());
        for (_, pending) in self.pending.lock().drain() {
            pending.send(Err(disconnected(&reason))).ok();
        }
        // Dropping the sinks ends every stream's channel.
        self.streams.lock().clear();
        if !self.closing.load(Ordering::Acquire) {
            for listener in self.listeners.lock().drain(..) {
                listener.unbounded_send(reason.clone()).ok();
            }
        }
    }

    fn dispatch(&self, message: AgentMessage) {
        match message {
            AgentMessage::Response { id, result } => {
                if let Some(pending) = self.pending.lock().remove(&id) {
                    pending.send(result.map_err(io::Error::from)).ok();
                }
            }
            AgentMessage::Event { id, event } => {
                let last = event.is_final();
                let mut streams = self.streams.lock();
                if let Some(sink) = streams.get_mut(&id) {
                    sink(event);
                }
                if last {
                    streams.remove(&id);
                }
            }
        }
    }
}

fn disconnected(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, reason.to_owned())
}

fn unexpected(response: Response) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("unexpected agent response: {response:?}"),
    )
}

/// Reads up to and including the banner line, skipping other output.
fn read_banner(reader: &mut impl BufRead) -> io::Result<Banner> {
    let mut skipped = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            let output = String::from_utf8_lossy(&skipped);
            let output = output.trim();
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                if output.is_empty() {
                    "the agent exited before it started".to_owned()
                } else {
                    format!("the agent exited before it started; it printed: {output}")
                },
            ));
        }
        let text = String::from_utf8_lossy(&line);
        if let Some(banner) = proto::parse_banner(text.trim_end_matches('\n')) {
            return Ok(banner);
        }
        skipped.extend_from_slice(&line);
        if skipped.len() > MAX_BANNER_SEARCH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the agent printed too much output before its banner",
            ));
        }
    }
}

/// A connected agent. Dropping it closes the connection, which shuts the
/// agent down.
pub struct AgentClient {
    shared: Arc<Shared>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    next_id: AtomicU64,
}

impl AgentClient {
    /// Waits up to `timeout` for the agent's banner, checks its protocol
    /// version, and initializes it for the project at `root`.
    pub fn connect(
        transport: Transport,
        name: impl Into<String>,
        root: &str,
        timeout: Duration,
    ) -> io::Result<(Arc<Self>, Initialized)> {
        let shared = Arc::new(Shared {
            name: name.into(),
            pending: Mutex::default(),
            streams: Mutex::default(),
            disconnected: Mutex::default(),
            listeners: Mutex::default(),
            closing: AtomicBool::new(false),
            child: Mutex::new(transport.child),
            stderr: transport.stderr,
        });
        let client = Arc::new(Self {
            shared: shared.clone(),
            writer: Mutex::new(Some(transport.writer)),
            next_id: AtomicU64::new(1),
        });

        let (banner_tx, banner_rx) = std_mpsc::channel();
        let mut reader = BufReader::new(transport.reader);
        thread::Builder::new()
            .name("agent-reader".into())
            .spawn(move || {
                match read_banner(&mut reader) {
                    Ok(banner) => {
                        let compatible = banner.protocol == PROTOCOL_VERSION;
                        banner_tx.send(Ok(banner)).ok();
                        if !compatible {
                            return;
                        }
                    }
                    Err(err) => {
                        shared.disconnect(&err.to_string());
                        banner_tx.send(Err(err)).ok();
                        return;
                    }
                }
                loop {
                    match proto::read_frame::<AgentMessage>(&mut reader) {
                        Ok(Some(message)) => shared.dispatch(message),
                        Ok(None) => {
                            shared.disconnect("the agent closed the connection");
                            return;
                        }
                        Err(err) => {
                            shared.disconnect(&format!("broken connection: {err}"));
                            return;
                        }
                    }
                }
            })?;

        let banner = match banner_rx.recv_timeout(timeout) {
            Ok(banner) => banner?,
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "{} did not start within {} seconds",
                        client.shared.name,
                        timeout.as_secs()
                    ),
                ));
            }
        };
        if banner.protocol != PROTOCOL_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "{} {} speaks agent protocol {}, but this editor needs protocol {}; \
                     install the agent built with this editor",
                    client.shared.name, banner.version, banner.protocol, PROTOCOL_VERSION
                ),
            ));
        }
        let initialized = client.request(Request::Initialize {
            protocol: PROTOCOL_VERSION,
            root: root.to_owned(),
        })?;
        let Response::Initialized { root, home } = initialized else {
            return Err(unexpected(initialized));
        };
        Ok((
            client,
            Initialized {
                root,
                home,
                agent_version: banner.version,
            },
        ))
    }

    /// Why the connection is gone, if it is.
    pub fn disconnect_reason(&self) -> Option<String> {
        self.shared.disconnected.lock().clone()
    }

    /// Receives the reason once if the agent disconnects unexpectedly.
    pub fn disconnects(&self) -> async_mpsc::UnboundedReceiver<String> {
        let (tx, rx) = async_mpsc::unbounded();
        let mut listeners = self.shared.listeners.lock();
        match self.disconnect_reason() {
            Some(reason) => {
                tx.unbounded_send(reason).ok();
            }
            None => listeners.push(tx),
        }
        rx
    }

    fn send(&self, message: &ClientMessage) -> io::Result<()> {
        let mut writer = self.writer.lock();
        let result = match &mut *writer {
            Some(writer) => proto::write_frame(writer, message),
            None => Err(io::ErrorKind::BrokenPipe.into()),
        };
        result.map_err(|err| match self.disconnect_reason() {
            Some(reason) => disconnected(&reason),
            None => io::Error::new(
                err.kind(),
                format!("cannot reach {}: {err}", self.shared.name),
            ),
        })
    }

    fn call(&self, id: RequestId, request: Request) -> io::Result<Response> {
        let (tx, rx) = std_mpsc::channel();
        self.shared.pending.lock().insert(id, tx);
        if let Some(reason) = self.disconnect_reason() {
            self.shared.pending.lock().remove(&id);
            return Err(disconnected(&reason));
        }
        if let Err(err) = self.send(&ClientMessage::Request { id, request }) {
            self.shared.pending.lock().remove(&id);
            return Err(err);
        }
        match rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(result) => result,
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                self.shared.pending.lock().remove(&id);
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "{} did not answer within {} seconds",
                        self.shared.name,
                        REQUEST_TIMEOUT.as_secs()
                    ),
                ))
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                Err(disconnected(&self.disconnect_reason().unwrap_or_default()))
            }
        }
    }

    /// Sends a request and blocks until its response.
    pub fn request(&self, request: Request) -> io::Result<Response> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.call(id, request)
    }

    /// Starts a streaming request; `sink` gets its events on the reader
    /// thread, and is dropped after the final one or on disconnect.
    pub fn start_stream(
        self: &Arc<Self>,
        request: Request,
        sink: impl FnMut(Event) + Send + 'static,
    ) -> io::Result<StreamHandle> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.shared.streams.lock().insert(id, Box::new(sink));
        let handle = StreamHandle {
            id,
            client: self.clone(),
        };
        match self.call(id, request) {
            Ok(Response::Done) => Ok(handle),
            Ok(other) => Err(unexpected(other)),
            Err(err) => {
                self.shared.streams.lock().remove(&id);
                Err(err)
            }
        }
    }

    pub fn read_dir(&self, path: &str) -> io::Result<Vec<DirEntry>> {
        match self.request(Request::ReadDir { path: path.into() })? {
            Response::Entries(entries) => Ok(entries),
            other => Err(unexpected(other)),
        }
    }

    pub fn list_files(&self, root: &str) -> io::Result<Vec<String>> {
        match self.request(Request::ListFiles { root: root.into() })? {
            Response::Files(files) => Ok(files),
            other => Err(unexpected(other)),
        }
    }

    pub fn read_file(&self, path: &str) -> io::Result<Vec<u8>> {
        match self.request(Request::ReadFile { path: path.into() })? {
            Response::Contents(data) => Ok(data),
            other => Err(unexpected(other)),
        }
    }

    pub fn write_file(&self, path: &str, data: Vec<u8>) -> io::Result<()> {
        match self.request(Request::WriteFile {
            path: path.into(),
            data,
        })? {
            Response::Done => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    pub fn stat(&self, path: &str) -> io::Result<Option<FileKind>> {
        match self.request(Request::Stat { path: path.into() })? {
            Response::Kind(kind) => Ok(kind),
            other => Err(unexpected(other)),
        }
    }

    pub fn find_executable(&self, command: &str, root: &str) -> io::Result<Option<String>> {
        match self.request(Request::FindExecutable {
            command: command.into(),
            root: root.into(),
        })? {
            Response::Path(path) => Ok(path),
            other => Err(unexpected(other)),
        }
    }

    pub fn python_interpreter(&self, root: &str) -> io::Result<Option<String>> {
        match self.request(Request::PythonInterpreter { root: root.into() })? {
            Response::Path(path) => Ok(path),
            other => Err(unexpected(other)),
        }
    }

    /// Watches `root`; batches arrive until the handle is dropped.
    pub fn watch(
        self: &Arc<Self>,
        root: &str,
        debounce: Duration,
    ) -> io::Result<(StreamHandle, async_mpsc::UnboundedReceiver<Batch>)> {
        let (tx, rx) = async_mpsc::unbounded();
        let request = Request::Watch {
            root: root.into(),
            debounce_ms: debounce.as_millis() as u64,
        };
        let handle = self.start_stream(request, move |event| {
            if let Event::Watch(batch) = event {
                tx.unbounded_send(batch).ok();
            }
        })?;
        Ok((handle, rx))
    }

    /// Searches `root`; dropping the handle cancels the search.
    pub fn search(
        self: &Arc<Self>,
        root: &str,
        query: &SearchQuery,
        limits: SearchLimits,
    ) -> io::Result<(StreamHandle, async_mpsc::UnboundedReceiver<SearchEvent>)> {
        let (tx, rx) = async_mpsc::unbounded();
        let request = Request::Search {
            root: root.into(),
            query: query.clone(),
            limits,
        };
        let handle = self.start_stream(request, move |event| {
            if let Event::Search(event) = event {
                tx.unbounded_send(event).ok();
            }
        })?;
        Ok((handle, rx))
    }

    /// Starts a language server on the agent's machine, in `cwd`.
    pub fn spawn_language_server(
        self: &Arc<Self>,
        program: &str,
        args: &[String],
        cwd: &str,
        logger: Option<Logger>,
    ) -> io::Result<(LanguageServer, async_mpsc::UnboundedReceiver<Incoming>)> {
        let (stdout_tx, stdout_rx) = std_mpsc::channel();
        let mut stderr_line = String::new();
        let stderr_logger = logger.clone();
        let request = Request::Spawn(SpawnRequest {
            program: Some(program.into()),
            args: args.to_vec(),
            cwd: cwd.into(),
            env: Vec::new(),
            pty: None,
        });
        let handle = self.start_stream(request, move |event| match event {
            Event::Stdout(data) => {
                stdout_tx.send(data).ok();
            }
            Event::Stderr(data) => {
                stderr_line.push_str(&String::from_utf8_lossy(&data));
                while let Some(end) = stderr_line.find('\n') {
                    let line: String = stderr_line.drain(..=end).collect();
                    if let Some(logger) = &stderr_logger {
                        logger(LogKind::Stderr, line.trim_end());
                    }
                }
            }
            Event::Exited { .. } | Event::Watch(_) | Event::Search(_) => {}
        })?;
        let reader = ChannelReader {
            chunks: stdout_rx,
            chunk: Vec::new(),
            position: 0,
        };
        // The LSP threads live until the server's output ends, so the process
        // is ended by a guard the server drops, not by dropping the writer.
        let handle = Arc::new(handle);
        let (server, incoming) = LanguageServer::new(reader, StreamWriter(handle.clone()), logger);
        server.own_remote_process(Box::new(CancelOnDrop(handle)));
        Ok((server, incoming))
    }

    /// Runs `program` in `cwd` on the agent's machine to completion,
    /// killing it after `timeout`.
    pub fn run(
        self: &Arc<Self>,
        program: &str,
        args: &[String],
        cwd: &str,
        env: &[(&str, &str)],
        timeout: Duration,
    ) -> io::Result<RunOutput> {
        let (tx, rx) = std_mpsc::channel();
        let request = Request::Spawn(SpawnRequest {
            program: Some(program.into()),
            args: args.to_vec(),
            cwd: cwd.into(),
            env: env
                .iter()
                .map(|&(key, value)| (key.to_owned(), value.to_owned()))
                .collect(),
            pty: None,
        });
        let handle = self.start_stream(request, move |event| {
            tx.send(event).ok();
        })?;
        let deadline = Instant::now() + timeout;
        let mut output = RunOutput::default();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(Event::Stdout(data)) => output.stdout.extend(data),
                Ok(Event::Stderr(data)) => output.stderr.extend(data),
                Ok(Event::Exited { code }) => {
                    output.code = code;
                    return Ok(output);
                }
                Ok(Event::Watch(_) | Event::Search(_)) => {}
                Err(std_mpsc::RecvTimeoutError::Timeout) => {
                    drop(handle);
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("{program} did not finish within {} s", timeout.as_secs()),
                    ));
                }
                Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(disconnected(&self.disconnect_reason().unwrap_or_default()));
                }
            }
        }
    }

    /// Starts the user's login shell in a pty on the agent's machine.
    pub fn spawn_terminal(
        self: &Arc<Self>,
        cwd: &str,
        columns: u16,
        rows: u16,
        scrollback: usize,
        palette: Palette,
    ) -> io::Result<(Terminal, async_mpsc::UnboundedReceiver<TerminalEvent>)> {
        let (output_tx, output_rx) = std_mpsc::channel();
        let request = Request::Spawn(SpawnRequest {
            program: None,
            args: Vec::new(),
            cwd: cwd.into(),
            env: vec![
                ("TERM".into(), "xterm-256color".into()),
                ("COLORTERM".into(), "truecolor".into()),
            ],
            pty: Some(PtySize { columns, rows }),
        });
        let handle = self.start_stream(request, move |event| match event {
            Event::Stdout(data) | Event::Stderr(data) => {
                output_tx.send(PtyOutput::Data(data)).ok();
            }
            Event::Exited { code } => {
                output_tx.send(PtyOutput::Exited(code)).ok();
            }
            Event::Watch(_) | Event::Search(_) => {}
        })?;
        Terminal::attach(
            columns,
            rows,
            scrollback,
            palette,
            Arc::new(AgentPty(handle)),
            output_rx,
        )
    }
}

impl Drop for AgentClient {
    fn drop(&mut self) {
        self.shared.closing.store(true, Ordering::Release);
        // EOF on its stdin is the agent's shutdown signal.
        self.writer.lock().take();
        if let Some(mut child) = self.shared.child.lock().take() {
            thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(3);
                while Instant::now() < deadline {
                    if !matches!(child.try_wait(), Ok(None)) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                child.kill().ok();
                child.wait().ok();
            });
        }
    }
}

/// A running stream; dropping it cancels the stream on the agent.
pub struct StreamHandle {
    id: RequestId,
    client: Arc<AgentClient>,
}

impl StreamHandle {
    /// Sends bytes to the stream's process.
    pub fn input(&self, data: Vec<u8>) -> io::Result<()> {
        self.client
            .send(&ClientMessage::Input { id: self.id, data })
    }

    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        self.client
            .send(&ClientMessage::Resize { id: self.id, size })
    }

    pub fn cancel(&self) {
        self.client
            .send(&ClientMessage::Cancel { id: self.id })
            .ok();
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// A process's stdout, as delivered by its stream.
struct ChannelReader {
    chunks: std_mpsc::Receiver<Vec<u8>>,
    chunk: Vec<u8>,
    position: usize,
}

impl Read for ChannelReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        while self.position == self.chunk.len() {
            match self.chunks.recv() {
                Ok(chunk) => {
                    self.chunk = chunk;
                    self.position = 0;
                }
                // The process exited or the agent disconnected.
                Err(_) => return Ok(0),
            }
        }
        let n = buffer.len().min(self.chunk.len() - self.position);
        buffer[..n].copy_from_slice(&self.chunk[self.position..self.position + n]);
        self.position += n;
        Ok(n)
    }
}

/// A process's stdin.
struct StreamWriter(Arc<StreamHandle>);

/// Ends a stream's process when dropped, even while other clones live.
struct CancelOnDrop(Arc<StreamHandle>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl Write for StreamWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0.input(data.to_vec())?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct AgentPty(StreamHandle);

impl RemotePty for AgentPty {
    fn write(&self, data: Vec<u8>) {
        self.0.input(data).ok();
    }

    fn resize(&self, columns: u16, rows: u16) {
        self.0.resize(PtySize { columns, rows }).ok();
    }

    fn close(&self) {
        self.0.cancel();
    }
}

/// The agent-side string for a path; agent paths are always UTF-8.
pub fn agent_path(path: &Path) -> io::Result<&str> {
    path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not valid UTF-8", path.display()),
        )
    })
}
