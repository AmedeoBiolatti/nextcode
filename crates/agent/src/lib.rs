//! `next_code_agent`: serves the [`proto`] protocol on a byte stream and runs
//! the requested project operations (files, watching, search, processes,
//! TCP tunnels for port forwarding) on the machine it runs on. The editor
//! starts it inside a WSL distro or dev container and talks to it over the
//! process's stdin/stdout.

mod ports;
mod process;
#[cfg(target_os = "linux")]
mod pty;
mod tunnel;

use std::{
    collections::HashMap,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use parking_lot::Mutex;
use proto::{
    AgentMessage, ClientMessage, ErrorKind, Event, PROTOCOL_VERSION, RemoteError, Request,
    RequestId, Response, SearchEvent,
};

use crate::process::Process;

/// Serves one client until `input` ends (the client's shutdown) or breaks.
/// Every process started for the client is killed before returning.
pub fn serve(mut input: impl Read, output: impl Write + Send + 'static) -> io::Result<()> {
    let agent = Arc::new(Agent {
        outbox: Outbox {
            writer: Mutex::new(Box::new(output)),
        },
        streams: Mutex::default(),
        initialized: AtomicBool::new(false),
        shutting_down: AtomicBool::new(false),
    });
    agent.outbox.write(proto::banner().as_bytes());
    let result = loop {
        match proto::read_frame::<ClientMessage>(&mut input) {
            Ok(Some(message)) => agent.handle(message),
            Ok(None) => break Ok(()),
            Err(err) => break Err(err),
        }
    };
    agent.shutdown();
    result
}

struct Outbox {
    writer: Mutex<Box<dyn Write + Send>>,
}

impl Outbox {
    fn send(&self, message: &AgentMessage) {
        match proto::encode_frame(message) {
            Ok(frame) => self.write(&frame),
            Err(err) => {
                eprintln!("next_code_agent: cannot encode message: {err}");
                // A request must still get its one response.
                if let AgentMessage::Response { id, .. } = message {
                    let fallback = AgentMessage::Response {
                        id: *id,
                        result: Err(RemoteError {
                            kind: ErrorKind::InvalidData,
                            message: format!("the agent could not encode its answer: {err}"),
                        }),
                    };
                    if let Ok(frame) = proto::encode_frame(&fallback) {
                        self.write(&frame);
                    }
                }
            }
        }
    }

    /// A failed write means the client is gone; the read loop ends next.
    fn write(&self, bytes: &[u8]) {
        let mut writer = self.writer.lock();
        writer.write_all(bytes).and_then(|()| writer.flush()).ok();
    }
}

enum Slot {
    /// The request is being set up on its own thread.
    Starting {
        cancelled: bool,
    },
    Running(Stream),
}

enum Stream {
    /// Dropping the watcher stops it.
    Watch(#[allow(dead_code)] watcher::ProjectWatcher),
    /// Dropping the handle cancels the search.
    Search(#[allow(dead_code)] search::SearchHandle),
    /// A process or a tunnel.
    Process(Arc<dyn Process>),
}

impl Stream {
    /// Ends the stream at the client's request.
    fn cancel(self) {
        if let Self::Process(process) = self {
            process.terminate();
        }
    }

    /// Ends the stream immediately because the agent is exiting.
    fn kill(self) {
        if let Self::Process(process) = self {
            process.kill();
        }
    }
}

struct Agent {
    outbox: Outbox,
    streams: Mutex<HashMap<RequestId, Slot>>,
    initialized: AtomicBool,
    shutting_down: AtomicBool,
}

/// Sends a stream's events; the final one also forgets the stream.
pub(crate) struct EventSink {
    id: RequestId,
    agent: Arc<Agent>,
}

impl EventSink {
    pub(crate) fn send(&self, event: Event) {
        self.agent
            .outbox
            .send(&AgentMessage::Event { id: self.id, event });
    }

    pub(crate) fn finish(&self, event: Event) {
        self.send(event);
        self.agent.streams.lock().remove(&self.id);
    }
}

fn is_streaming(request: &Request) -> bool {
    matches!(
        request,
        Request::Watch { .. } | Request::Search { .. } | Request::Spawn(_) | Request::Tunnel { .. }
    )
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn path_string(path: PathBuf) -> Option<String> {
    path.into_os_string().into_string().ok()
}

impl Agent {
    fn handle(self: &Arc<Self>, message: ClientMessage) {
        match message {
            ClientMessage::Request {
                id,
                request: Request::Initialize { protocol, root },
            } => {
                let result = self.initialize(protocol, &root);
                self.respond(id, result);
            }
            ClientMessage::Request { id, request } => {
                if !self.initialized.load(Ordering::Acquire) {
                    self.respond(
                        id,
                        Err(invalid_input("the first request must be Initialize")),
                    );
                    return;
                }
                if is_streaming(&request) {
                    self.streams
                        .lock()
                        .insert(id, Slot::Starting { cancelled: false });
                }
                let agent = self.clone();
                let spawned =
                    thread::Builder::new()
                        .name("agent-request".into())
                        .spawn(move || {
                            let result = agent.run(id, request);
                            if result.is_err() {
                                agent.streams.lock().remove(&id);
                            }
                            agent.respond(id, result);
                        });
                if let Err(err) = spawned {
                    self.streams.lock().remove(&id);
                    self.respond(id, Err(err));
                }
            }
            ClientMessage::Cancel { id } => {
                let mut streams = self.streams.lock();
                match streams.get_mut(&id) {
                    Some(Slot::Starting { cancelled }) => *cancelled = true,
                    // A process stays listed until it exits, so shutdown can
                    // still kill it if it ignores the request to end.
                    Some(Slot::Running(Stream::Process(process))) => {
                        let process = process.clone();
                        drop(streams);
                        process.terminate();
                    }
                    Some(Slot::Running(_)) => {
                        if let Some(Slot::Running(stream)) = streams.remove(&id) {
                            drop(streams);
                            stream.cancel();
                        }
                    }
                    None => {}
                }
            }
            ClientMessage::Input { id, data } => {
                if let Some(process) = self.process(id) {
                    process.write(data);
                }
            }
            ClientMessage::Resize { id, size } => {
                if let Some(process) = self.process(id) {
                    process.resize(size);
                }
            }
            ClientMessage::Credit { id, bytes } => {
                if let Some(process) = self.process(id) {
                    process.credit(bytes);
                }
            }
            ClientMessage::CloseInput { id } => {
                if let Some(process) = self.process(id) {
                    process.close_input();
                }
            }
        }
    }

    fn process(&self, id: RequestId) -> Option<Arc<dyn Process>> {
        match self.streams.lock().get(&id) {
            Some(Slot::Running(Stream::Process(process))) => Some(process.clone()),
            _ => None,
        }
    }

    fn respond(&self, id: RequestId, result: io::Result<Response>) {
        let result = result.map_err(|err| RemoteError::from(&err));
        self.outbox.send(&AgentMessage::Response { id, result });
    }

    fn initialize(&self, protocol: u32, root: &str) -> io::Result<Response> {
        if protocol != PROTOCOL_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "the editor speaks agent protocol {protocol}, this agent speaks {PROTOCOL_VERSION}"
                ),
            ));
        }
        let canonical = fs::canonicalize(root)
            .map_err(|err| io::Error::new(err.kind(), format!("{root}: {err}")))?;
        if !canonical.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{root} is not a directory"),
            ));
        }
        self.initialized.store(true, Ordering::Release);
        Ok(Response::Initialized {
            root: path_string(canonical)
                .ok_or_else(|| invalid_input(format!("{root} is not valid UTF-8")))?,
            home: std::env::var("HOME").unwrap_or_default(),
        })
    }

    fn run(self: &Arc<Self>, id: RequestId, request: Request) -> io::Result<Response> {
        match request {
            Request::Initialize { .. } => Err(invalid_input("already initialized")),
            Request::ReadDir { path } => {
                worktree::read_dir(Path::new(&path)).map(Response::Entries)
            }
            Request::ListFiles { root } => {
                Ok(Response::Files(worktree::list_files(Path::new(&root))))
            }
            Request::ReadFile { path } => fs::read(&path).map(Response::Contents),
            Request::WriteFile { path, data } => {
                worktree::write_file(Path::new(&path), &data).map(|()| Response::Done)
            }
            Request::Stat { path } => worktree::kind(Path::new(&path)).map(Response::Kind),
            Request::FindExecutable { command, root } => Ok(Response::Path(
                worktree::find_executable(&command, Path::new(&root)).and_then(path_string),
            )),
            Request::PythonInterpreter { root } => Ok(Response::Path(
                worktree::python_interpreter(Path::new(&root)).and_then(path_string),
            )),
            Request::Watch { root, debounce_ms } => {
                let (watcher, batches) =
                    watcher::watch(Path::new(&root), Duration::from_millis(debounce_ms))
                        .map_err(|err| io::Error::other(format!("cannot watch {root}: {err}")))?;
                if self.install(id, Stream::Watch(watcher)) {
                    let sink = self.sink(id);
                    thread::Builder::new()
                        .name("agent-watch".into())
                        .spawn(move || {
                            for batch in futures::executor::block_on_stream(batches) {
                                sink.send(Event::Watch(batch));
                            }
                        })?;
                }
                Ok(Response::Done)
            }
            Request::Search {
                root,
                query,
                limits,
            } => {
                let (handle, events) = search::search(Path::new(&root), &query, limits)
                    .map_err(|err| invalid_input(err.to_string()))?;
                if self.install(id, Stream::Search(handle)) {
                    let sink = self.sink(id);
                    thread::Builder::new()
                        .name("agent-search".into())
                        .spawn(move || {
                            for event in futures::executor::block_on_stream(events) {
                                if let SearchEvent::Done { .. } = event {
                                    sink.finish(Event::Search(event));
                                    return;
                                }
                                sink.send(Event::Search(event));
                            }
                        })?;
                }
                Ok(Response::Done)
            }
            Request::Spawn(spawn) => {
                let process = process::spawn(spawn, self.sink(id))?;
                self.install(id, Stream::Process(process));
                Ok(Response::Done)
            }
            Request::Tunnel { host, port } => {
                let tunnel = tunnel::open(&host, port, self.sink(id))?;
                self.install(id, Stream::Process(tunnel));
                Ok(Response::Done)
            }
            Request::ListeningPorts => Ok(Response::Ports(ports::listening())),
        }
    }

    fn sink(self: &Arc<Self>, id: RequestId) -> EventSink {
        EventSink {
            id,
            agent: self.clone(),
        }
    }

    /// Makes a started stream reachable by id; a stream cancelled while it
    /// started (or during shutdown) is ended instead. Returns whether it runs.
    fn install(&self, id: RequestId, stream: Stream) -> bool {
        let mut streams = self.streams.lock();
        let cancelled = matches!(streams.get(&id), Some(Slot::Starting { cancelled: true }));
        if self.shutting_down.load(Ordering::Acquire) {
            streams.remove(&id);
            drop(streams);
            stream.kill();
            false
        } else if cancelled {
            if let Stream::Process(process) = &stream {
                // Listed until it exits, like any cancelled process.
                let process = process.clone();
                streams.insert(id, Slot::Running(stream));
                drop(streams);
                process.terminate();
            } else {
                streams.remove(&id);
                drop(streams);
                stream.cancel();
            }
            false
        } else {
            match streams.get_mut(&id) {
                // A process that already exited has removed its slot.
                Some(slot) => *slot = Slot::Running(stream),
                None => drop(streams),
            }
            true
        }
    }

    fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        let streams: Vec<Slot> = self.streams.lock().drain().map(|(_, slot)| slot).collect();
        for slot in streams {
            if let Slot::Running(stream) = slot {
                stream.kill();
            }
        }
    }
}
