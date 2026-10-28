//! Client side of the agent protocol: request/response correlation, event
//! streams, and disconnect detection over any byte transport.

use std::{
    collections::HashMap,
    io::{self, BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpStream},
    path::Path,
    process::Child,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc as std_mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use futures::channel::mpsc as async_mpsc;
use lsp::{Incoming, LanguageServer, LogKind, Logger};
use parking_lot::{Condvar, Mutex};
use proto::{
    AgentMessage, Banner, Batch, ClientMessage, DirEntry, Event, FileKind, HostRequest,
    HostServices, ListeningPort, PROTOCOL_VERSION, PtySize, Request, RequestId, Response,
    SearchEvent, SearchLimits, SearchQuery, SpawnRequest, TUNNEL_WINDOW,
};
use terminal::{OutputTap, Palette, PtyOutput, RemotePty, Terminal, TerminalEvent};

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
        self.start_stream_with_id(request, |_| sink)
    }

    /// [`Self::start_stream`] with a sink made for the stream's id (events
    /// may arrive before this returns).
    fn start_stream_with_id<S: FnMut(Event) + Send + 'static>(
        self: &Arc<Self>,
        request: Request,
        sink: impl FnOnce(RequestId) -> S,
    ) -> io::Result<StreamHandle> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.shared.streams.lock().insert(id, Box::new(sink(id)));
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

    /// Serves `next_code_agent pinpoint` calls made on the agent's machine
    /// by CLI agents working in `roots` (the project's folders): `handler`
    /// gets each request (JSON text) on the reader thread, so it must not
    /// block; it answers through the [`PinpointReply`]. Dropping the handle
    /// stops serving.
    pub fn serve_pinpoint(
        self: &Arc<Self>,
        roots: Vec<String>,
        handler: impl Fn(String, PinpointReply) + Send + 'static,
    ) -> io::Result<StreamHandle> {
        let client = Arc::downgrade(self);
        self.start_stream_with_id(Request::Pinpoint { roots }, move |id| {
            move |event| {
                if let Event::Pinpoint { call, request } = event {
                    let reply = PinpointReply {
                        client: client.clone(),
                        id,
                        call,
                        sent: false,
                    };
                    handler(request, reply);
                }
            }
        })
    }

    /// Lends this machine's git credentials and ssh-agent to the agent's
    /// machine (see [`HostServices`]): `handler` gets each request on the
    /// reader thread, so it must not block; it answers through the
    /// [`HostReply`]. `closed` hears that a forwarded ssh-agent connection
    /// ended. Dropping the handle stops serving.
    pub fn serve_host(
        self: &Arc<Self>,
        services: HostServices,
        handler: impl Fn(HostRequest, HostReply) + Send + 'static,
        closed: impl Fn(u64) + Send + 'static,
    ) -> io::Result<StreamHandle> {
        let client = Arc::downgrade(self);
        self.start_stream_with_id(Request::HostServices(services), move |id| {
            move |event| match event {
                Event::HostCall { call, request } => {
                    let reply = HostReply {
                        client: client.clone(),
                        id,
                        call,
                        sent: false,
                    };
                    handler(request, reply);
                }
                Event::SshAgentClosed { connection } => closed(connection),
                _ => {}
            }
        })
    }

    pub fn read_dir(&self, path: &str) -> io::Result<Vec<DirEntry>> {
        match self.request(Request::ReadDir { path: path.into() })? {
            Response::Entries(entries) => Ok(entries),
            other => Err(unexpected(other)),
        }
    }

    pub fn browse_folders(&self, path: &str) -> io::Result<proto::FolderListing> {
        match self.request(Request::BrowseFolders { path: path.into() })? {
            Response::Folders(listing) => Ok(listing),
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

    /// TCP sockets listening on the agent's machine.
    pub fn listening_ports(&self) -> io::Result<Vec<ListeningPort>> {
        match self.request(Request::ListeningPorts)? {
            Response::Ports(ports) => Ok(ports),
            other => Err(unexpected(other)),
        }
    }

    /// Connects to `host:port` from the agent's machine (blocking until it
    /// connected or failed) and relays that connection to `local` on
    /// threads, both ways with half-closes, under the protocol's credit
    /// windows. `closed` is called once both directions ended, with the
    /// error if the tunnel broke. Dropping the handle closes the tunnel.
    pub fn tunnel(
        self: &Arc<Self>,
        host: &str,
        port: u16,
        local: TcpStream,
        closed: impl FnOnce(Option<String>) + Send + 'static,
    ) -> io::Result<TunnelHandle> {
        enum Down {
            Data(Vec<u8>),
            Eof,
            Closed(Option<String>),
        }
        let credit = Arc::new(Credit::new());
        let (down_tx, down_rx) = std_mpsc::channel();
        let granted = credit.clone();
        let request = Request::Tunnel {
            host: host.to_owned(),
            port,
        };
        let handle = Arc::new(self.start_stream(request, move |event| match event {
            Event::TunnelData(data) => {
                down_tx.send(Down::Data(data)).ok();
            }
            Event::TunnelEof => {
                down_tx.send(Down::Eof).ok();
            }
            Event::TunnelCredit(bytes) => granted.grant(bytes),
            Event::TunnelClosed { error } => {
                down_tx.send(Down::Closed(error)).ok();
            }
            _ => {}
        })?);
        local.set_nodelay(true).ok();
        // One handle shared by both threads (`&TcpStream` reads and writes),
        // never `try_clone`d: on Windows a duplicated socket can consider
        // itself unconnected, and its half-close is then lost.
        local.set_read_timeout(WAKE)?;
        let local = Arc::new(local);
        let closing = Arc::new(AtomicBool::new(false));

        let uplink = {
            let (handle, credit, local) = (handle.clone(), credit.clone(), local.clone());
            let closing = closing.clone();
            thread::Builder::new()
                .name("tunnel-up".into())
                .spawn(move || {
                    let mut buffer = vec![0; 64 * 1024];
                    while let Some(allowed) = credit.wait() {
                        match (&*local).read(&mut buffer[..allowed]) {
                            Ok(0) => {
                                handle.close_input().ok();
                                return;
                            }
                            Ok(n) => {
                                credit.spend(n);
                                if handle.input(buffer[..n].to_vec()).is_err() {
                                    return;
                                }
                            }
                            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                            // `WAKE`: check whether the tunnel closed.
                            Err(err) if is_wake(&err) && !closing.load(Ordering::Acquire) => {}
                            Err(_) => {
                                handle.cancel();
                                return;
                            }
                        }
                    }
                })?
        };
        let client = self.clone();
        let (down_handle, down_local) = (handle.clone(), local.clone());
        thread::Builder::new()
            .name("tunnel-down".into())
            .spawn(move || {
                let handle = down_handle;
                let mut broken = false;
                let error = loop {
                    match down_rx.recv() {
                        Ok(Down::Data(data)) => {
                            // After a failed write, data is dropped until the
                            // agent confirms the close.
                            if broken {
                                continue;
                            }
                            if (&*down_local).write_all(&data).is_err() {
                                broken = true;
                                handle.cancel();
                            } else {
                                handle.credit(data.len() as u32).ok();
                            }
                        }
                        Ok(Down::Eof) => {
                            if !broken && down_local.shutdown(Shutdown::Write).is_err() {
                                broken = true;
                                handle.cancel();
                            }
                        }
                        Ok(Down::Closed(error)) => break error,
                        // The agent disconnected.
                        Err(_) => {
                            break Some(
                                client
                                    .disconnect_reason()
                                    .unwrap_or_else(|| "the agent disconnected".into()),
                            );
                        }
                    }
                };
                down_local.shutdown(Shutdown::Both).ok();
                credit.close();
                uplink.join().ok();
                closed(error);
            })?;
        Ok(TunnelHandle {
            stream: handle,
            local,
            closing,
        })
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
            _ => {}
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

    /// Starts `program` on the agent's machine, in `cwd`, with piped stdio:
    /// its stdout to read, stdin to write, and stderr chunks passed to
    /// `stderr`. The output ends (EOF) when the process exits; dropping
    /// the guard ends the process.
    pub fn spawn_piped(
        self: &Arc<Self>,
        program: &str,
        args: &[String],
        cwd: &str,
        env: &[(String, String)],
        mut stderr: impl FnMut(&[u8]) + Send + 'static,
    ) -> io::Result<PipedProcess> {
        let (stdout_tx, stdout_rx) = std_mpsc::channel();
        let request = Request::Spawn(SpawnRequest {
            program: Some(program.into()),
            args: args.to_vec(),
            cwd: cwd.into(),
            env: env.to_vec(),
            pty: None,
        });
        let handle = self.start_stream(request, move |event| match event {
            Event::Stdout(data) => {
                stdout_tx.send(data).ok();
            }
            Event::Stderr(data) => stderr(&data),
            _ => {}
        })?;
        let handle = Arc::new(handle);
        Ok(PipedProcess {
            stdout: Box::new(ChannelReader {
                chunks: stdout_rx,
                chunk: Vec::new(),
                position: 0,
            }),
            stdin: Box::new(StreamWriter(handle.clone())),
            guard: Box::new(CancelOnDrop(handle)),
        })
    }

    /// Runs `program` in `cwd` on the agent's machine to completion with
    /// `input` on its stdin (then end of file), killing it after `timeout`.
    pub fn run(
        self: &Arc<Self>,
        program: &str,
        args: &[String],
        cwd: &str,
        env: &[(&str, &str)],
        input: Option<&[u8]>,
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
        if let Some(input) = input {
            handle.input(input.to_vec())?;
        }
        handle.close_input()?;
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
                Ok(_) => {}
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

    /// Starts a program in a pty on the agent's machine: `program` with
    /// its arguments, else the user's login shell. `tap` (if any) gets the
    /// output and the exit.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_terminal(
        self: &Arc<Self>,
        cwd: &str,
        program: Option<(&str, &[String])>,
        columns: u16,
        rows: u16,
        scrollback: usize,
        palette: Palette,
        tap: Option<OutputTap>,
    ) -> io::Result<(Terminal, async_mpsc::UnboundedReceiver<TerminalEvent>)> {
        let (output_tx, output_rx) = std_mpsc::channel();
        let request = Request::Spawn(SpawnRequest {
            program: program.map(|(program, _)| program.into()),
            args: program.map(|(_, args)| args.to_vec()).unwrap_or_default(),
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
            _ => {}
        })?;
        Terminal::attach(
            columns,
            rows,
            scrollback,
            palette,
            Arc::new(AgentPty(handle)),
            output_rx,
            tap,
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

/// The answer to one pinpoint call (see [`AgentClient::serve_pinpoint`]);
/// dropped unanswered, it answers with an error so the caller never hangs.
pub struct PinpointReply {
    /// Weak: the stream's sink, which makes replies, lives in the client.
    client: Weak<AgentClient>,
    id: RequestId,
    call: u64,
    sent: bool,
}

impl PinpointReply {
    /// Sends `reply` (JSON text) to the caller.
    pub fn send(mut self, reply: String) {
        self.sent = true;
        self.deliver(reply);
    }

    fn deliver(&self, reply: String) {
        if let Some(client) = self.client.upgrade() {
            client
                .send(&ClientMessage::PinpointReply {
                    id: self.id,
                    call: self.call,
                    reply,
                })
                .ok();
        }
    }
}

impl Drop for PinpointReply {
    fn drop(&mut self) {
        if !self.sent {
            self.deliver(r#"{"ok":false,"error":"the editor dropped the request"}"#.into());
        }
    }
}

/// The answer to one host services call (see [`AgentClient::serve_host`]);
/// dropped unanswered, it answers with an error so the caller never hangs.
pub struct HostReply {
    client: Weak<AgentClient>,
    id: RequestId,
    call: u64,
    sent: bool,
}

impl HostReply {
    /// Sends the credential text or ssh-agent message, or why there is none.
    pub fn send(mut self, reply: Result<Vec<u8>, String>) {
        self.sent = true;
        self.deliver(reply);
    }

    fn deliver(&self, reply: Result<Vec<u8>, String>) {
        if let Some(client) = self.client.upgrade() {
            client
                .send(&ClientMessage::HostReply {
                    id: self.id,
                    call: self.call,
                    reply,
                })
                .ok();
        }
    }
}

impl Drop for HostReply {
    fn drop(&mut self) {
        if !self.sent {
            self.deliver(Err("the editor dropped the request".into()));
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

    /// Ends the process's stdin.
    pub fn close_input(&self) -> io::Result<()> {
        self.client.send(&ClientMessage::CloseInput { id: self.id })
    }

    /// Lets a tunnel's agent send `bytes` more.
    fn credit(&self, bytes: u32) -> io::Result<()> {
        self.client
            .send(&ClientMessage::Credit { id: self.id, bytes })
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

/// A running tunnel (see [`AgentClient::tunnel`]); dropping it closes the
/// connection both ways.
pub struct TunnelHandle {
    stream: Arc<StreamHandle>,
    local: Arc<TcpStream>,
    /// Tells relay threads woken by [`WAKE`] to stop.
    closing: Arc<AtomicBool>,
}

impl TunnelHandle {
    pub fn close(&self) {
        self.closing.store(true, Ordering::Release);
        self.stream.cancel();
        self.local.shutdown(Shutdown::Both).ok();
    }
}

impl Drop for TunnelHandle {
    fn drop(&mut self) {
        self.close();
    }
}

/// How often the uplink thread, blocked reading the local socket, wakes to
/// see whether its tunnel closed. Windows does not wake a blocked `recv` on
/// `shutdown` (Linux does), so an idle application would keep a stopped
/// tunnel's threads alive and its `closed` callback (the forwarder's
/// connection count) pending. A timed-out `recv` consumed nothing.
const WAKE: Option<Duration> = if cfg!(windows) {
    Some(Duration::from_millis(200))
} else {
    None
};

/// A socket timeout (from [`WAKE`]).
fn is_wake(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// How many bytes a tunnel's client may still send; `None` once closed.
struct Credit {
    bytes: Mutex<Option<u64>>,
    changed: Condvar,
}

impl Credit {
    fn new() -> Self {
        Self {
            bytes: Mutex::new(Some(u64::from(TUNNEL_WINDOW))),
            changed: Condvar::new(),
        }
    }

    /// Waits for credit; the most to send next (64 KiB at most), `None`
    /// once closed.
    fn wait(&self) -> Option<usize> {
        let mut bytes = self.bytes.lock();
        loop {
            match *bytes {
                None => return None,
                Some(0) => self.changed.wait(&mut bytes),
                Some(n) => return Some(n.min(64 * 1024) as usize),
            }
        }
    }

    fn spend(&self, n: usize) {
        if let Some(bytes) = &mut *self.bytes.lock() {
            *bytes = bytes.saturating_sub(n as u64);
        }
    }

    fn grant(&self, n: u32) {
        if let Some(bytes) = &mut *self.bytes.lock() {
            *bytes += u64::from(n);
            self.changed.notify_all();
        }
    }

    fn close(&self) {
        *self.bytes.lock() = None;
        self.changed.notify_all();
    }
}

/// A process with piped stdio (see [`AgentClient::spawn_piped`]).
pub struct PipedProcess {
    pub stdout: Box<dyn Read + Send>,
    pub stdin: Box<dyn Write + Send>,
    /// Ends the process when dropped.
    pub guard: Box<dyn Send>,
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
