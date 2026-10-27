//! Language Server Protocol client: JSON-RPC 2.0 over any byte stream.
//!
//! The transport is a plain `Read`/`Write` pair, so the same client works for a
//! local child process, a socket, or a stream tunnelled through a remote agent.
//! I/O runs on dedicated threads; requests are executor-agnostic futures.

pub mod codec;
mod uri;

use std::{
    collections::HashMap,
    fmt,
    future::Future,
    io::{self, BufRead, BufReader, Read, Write},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, Ordering},
        mpsc,
    },
    task::{Context, Poll},
    thread,
    time::Duration,
};

use futures::channel::{mpsc as async_mpsc, oneshot};
pub use lsp_types as types;
use lsp_types::{
    NumberOrString,
    notification::{Cancel, Exit, Notification},
    request::{Request, Shutdown},
};
use parking_lot::Mutex;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
pub use uri::{path_to_uri, uri_to_path};

pub type RequestId = NumberOrString;

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// The server answered with a JSON-RPC error.
    Response { code: i64, message: String },
    /// The server's output closed before it answered.
    ServerExited,
    /// The server's answer did not match the expected type.
    Decode(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Response { code, message } => write!(f, "server error {code}: {message}"),
            Self::ServerExited => f.write_str("language server exited"),
            Self::Decode(message) => write!(f, "unexpected server response: {message}"),
        }
    }
}

impl std::error::Error for Error {}

/// Traffic the server initiates.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    /// Must be answered with [`LanguageServer::respond`].
    Request {
        id: RequestId,
        method: String,
        params: Value,
    },
    /// The server's output closed; nothing else will arrive.
    Exited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    Sent,
    Received,
    /// A line the server wrote to stderr.
    Stderr,
}

pub type Logger = Arc<dyn Fn(LogKind, &str) + Send + Sync>;

/// Time a server gets to exit after `shutdown`/`exit` before its process is ended.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// A local server process the client owns (see [`LanguageServer::attach`]).
pub trait ServerProcess: Send {
    /// Lets the process exit on its own for `grace` (it was told to), then
    /// ends it and whatever it started. Returns at once; a zero grace ends
    /// it now.
    fn end(self: Box<Self>, grace: Duration);
}

type Responder = oneshot::Sender<Result<Value, Error>>;

struct Shared {
    outgoing: mpsc::Sender<Vec<u8>>,
    pending: Mutex<HashMap<i32, Responder>>,
    next_id: AtomicI32,
    exited: AtomicBool,
    logger: Option<Logger>,
}

impl Shared {
    fn send(&self, message: &Value) {
        // Logged by the writer thread, where the body is written: the
        // caller (often the UI thread) never waits on the log file.
        // A closed writer means the server is gone; pending requests fail via the reader.
        self.outgoing.send(message.to_string().into_bytes()).ok();
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }
}

pub struct LanguageServer {
    shared: Arc<Shared>,
    /// The local server process, ended on shutdown (after the grace) or drop.
    process: Mutex<Option<Box<dyn ServerProcess>>>,
    /// Ends a server process running elsewhere when dropped.
    remote_process: Mutex<Option<Box<dyn Send>>>,
}

impl LanguageServer {
    /// Starts a client over an existing stream. Without the threads for
    /// it (resource pressure) the client is born exited, see
    /// [`LanguageServer::try_new`].
    pub fn new(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        logger: Option<Logger>,
    ) -> (Self, async_mpsc::UnboundedReceiver<Incoming>) {
        match Self::try_new(reader, writer, logger) {
            Ok(connected) => connected,
            Err(err) => {
                eprintln!("next_code: cannot start the language server client: {err}");
                let (outgoing, _) = mpsc::channel();
                let (incoming, incoming_rx) = async_mpsc::unbounded();
                let shared = Arc::new(Shared {
                    outgoing,
                    pending: Mutex::default(),
                    next_id: AtomicI32::new(1),
                    exited: AtomicBool::new(true),
                    logger: None,
                });
                incoming.unbounded_send(Incoming::Exited).ok();
                (Self::with_shared(shared), incoming_rx)
            }
        }
    }

    /// Starts a client over an existing stream; fails when its I/O threads
    /// cannot be started.
    pub fn try_new(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        logger: Option<Logger>,
    ) -> io::Result<(Self, async_mpsc::UnboundedReceiver<Incoming>)> {
        let (outgoing, outgoing_rx) = mpsc::channel::<Vec<u8>>();
        let (incoming, incoming_rx) = async_mpsc::unbounded();
        let shared = Arc::new(Shared {
            outgoing,
            pending: Mutex::default(),
            next_id: AtomicI32::new(1),
            exited: AtomicBool::new(false),
            logger,
        });

        let writer_logger = shared.logger.clone();
        thread::Builder::new()
            .name("lsp-writer".into())
            .spawn(move || {
                let mut writer = io::BufWriter::new(writer);
                for body in outgoing_rx {
                    if codec::write_message(&mut writer, &body).is_err() {
                        break;
                    }
                    if let Some(logger) = &writer_logger {
                        logger(LogKind::Sent, &String::from_utf8_lossy(&body));
                    }
                }
            })?;

        let reader_shared = shared.clone();
        thread::Builder::new()
            .name("lsp-reader".into())
            .spawn(move || read_loop(BufReader::new(reader), &reader_shared, &incoming))?;

        Ok((Self::with_shared(shared), incoming_rx))
    }

    fn with_shared(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            process: Mutex::new(None),
            remote_process: Mutex::new(None),
        }
    }

    /// Talks to a server process over its `stdin`/`stdout`; the lines it
    /// writes to `stderr` go to the logger. `process` is ended on shutdown
    /// (after [`SHUTDOWN_GRACE`]) and when the client is dropped.
    pub fn attach(
        stdout: impl Read + Send + 'static,
        stdin: impl Write + Send + 'static,
        stderr: impl Read + Send + 'static,
        process: Box<dyn ServerProcess>,
        logger: Option<Logger>,
    ) -> io::Result<(Self, async_mpsc::UnboundedReceiver<Incoming>)> {
        let stderr_logger = logger.clone();
        thread::Builder::new()
            .name("lsp-stderr".into())
            .spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if let Some(logger) = &stderr_logger {
                        logger(LogKind::Stderr, &line);
                    }
                }
            })?;

        let (server, incoming) = Self::try_new(stdout, stdin, logger)?;
        *server.process.lock() = Some(process);
        Ok((server, incoming))
    }

    /// Ties the lifetime of a server process running elsewhere (e.g. behind
    /// an agent) to this client: `guard` is dropped on shutdown or drop, and
    /// dropping it must end the process.
    pub fn own_remote_process(&self, guard: Box<dyn Send>) {
        *self.remote_process.lock() = Some(guard);
    }

    /// Sends a request. Dropping the returned future before it resolves
    /// sends `$/cancelRequest`.
    pub fn request<R: Request>(&self, params: R::Params) -> PendingRequest<R::Result> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (responder, response) = oneshot::channel();
        if self.shared.exited.load(Ordering::Acquire) {
            responder.send(Err(Error::ServerExited)).ok();
        } else {
            self.shared.pending.lock().insert(id, responder);
            self.shared.send(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": R::METHOD,
                "params": params,
            }));
        }
        PendingRequest {
            id,
            response,
            shared: self.shared.clone(),
            done: false,
            _result: std::marker::PhantomData,
        }
    }

    pub fn notify<N: Notification>(&self, params: N::Params) {
        self.shared.notify(N::METHOD, json!(params));
    }

    /// Answers a server-initiated [`Incoming::Request`].
    pub fn respond(&self, id: RequestId, result: Result<Value, lsp_types::LSPAny>) {
        let mut message = json!({ "jsonrpc": "2.0", "id": id });
        match result {
            Ok(result) => message["result"] = result,
            Err(error) => message["error"] = error,
        }
        self.shared.send(&message);
    }

    /// Asks the server to shut down and exit, then ends its process once
    /// it had [`SHUTDOWN_GRACE`] to do so itself.
    pub async fn shutdown(&self) {
        // Servers that ignore `shutdown` still get `exit`; a dead one is just reaped.
        self.request::<Shutdown>(()).await.ok();
        self.notify::<Exit>(());
        self.remote_process.lock().take();
        if let Some(process) = self.process.lock().take() {
            process.end(SHUTDOWN_GRACE);
        }
    }

    pub fn has_exited(&self) -> bool {
        self.shared.exited.load(Ordering::Acquire)
    }
}

impl Drop for LanguageServer {
    fn drop(&mut self) {
        if let Some(process) = self.process.lock().take() {
            process.end(Duration::ZERO);
        }
    }
}

/// A request awaiting its response.
pub struct PendingRequest<T> {
    id: i32,
    response: oneshot::Receiver<Result<Value, Error>>,
    shared: Arc<Shared>,
    done: bool,
    _result: std::marker::PhantomData<fn() -> T>,
}

impl<T: DeserializeOwned> Future for PendingRequest<T> {
    type Output = Result<T, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let result = match Pin::new(&mut self.response).poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(result) => result,
        };
        self.done = true;
        Poll::Ready(match result {
            Ok(Ok(value)) => {
                serde_json::from_value(value).map_err(|err| Error::Decode(err.to_string()))
            }
            Ok(Err(error)) => Err(error),
            // The reader dropped the responder without answering.
            Err(oneshot::Canceled) => Err(Error::ServerExited),
        })
    }
}

impl<T> Drop for PendingRequest<T> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let was_pending = self.shared.pending.lock().remove(&self.id).is_some();
        if was_pending && !self.shared.exited.load(Ordering::Acquire) {
            self.shared.notify(Cancel::METHOD, json!({ "id": self.id }));
        }
    }
}

#[derive(Deserialize)]
struct RawMessage {
    id: Option<RequestId>,
    method: Option<String>,
    #[serde(default)]
    params: Value,
    #[serde(default)]
    result: Value,
    error: Option<RawError>,
}

#[derive(Deserialize)]
struct RawError {
    code: i64,
    message: String,
}

fn read_loop(
    mut reader: impl BufRead,
    shared: &Shared,
    incoming: &async_mpsc::UnboundedSender<Incoming>,
) {
    loop {
        let body = match codec::read_message(&mut reader) {
            Ok(Some(body)) => body,
            Ok(None) => break,
            Err(err) => {
                if let Some(logger) = &shared.logger {
                    logger(LogKind::Stderr, &format!("[client] read error: {err}"));
                }
                break;
            }
        };
        if let Some(logger) = &shared.logger {
            logger(LogKind::Received, &String::from_utf8_lossy(&body));
        }
        let message: RawMessage = match serde_json::from_slice(&body) {
            Ok(message) => message,
            Err(err) => {
                if let Some(logger) = &shared.logger {
                    logger(LogKind::Stderr, &format!("[client] invalid message: {err}"));
                }
                continue;
            }
        };
        match (message.id, message.method) {
            (Some(id), Some(method)) => {
                incoming
                    .unbounded_send(Incoming::Request {
                        id,
                        method,
                        params: message.params,
                    })
                    .ok();
            }
            (None, Some(method)) => {
                incoming
                    .unbounded_send(Incoming::Notification {
                        method,
                        params: message.params,
                    })
                    .ok();
            }
            (Some(NumberOrString::Number(id)), None) => {
                let responder = shared.pending.lock().remove(&id);
                if let Some(responder) = responder {
                    let result = match message.error {
                        Some(error) => Err(Error::Response {
                            code: error.code,
                            message: error.message,
                        }),
                        None => Ok(message.result),
                    };
                    responder.send(result).ok();
                }
            }
            // Responses to ids we never issued are ignored.
            _ => {}
        }
    }

    shared.exited.store(true, Ordering::Release);
    for (_, responder) in shared.pending.lock().drain() {
        responder.send(Err(Error::ServerExited)).ok();
    }
    incoming.unbounded_send(Incoming::Exited).ok();
}

#[cfg(test)]
mod tests {
    use std::io::{PipeReader, PipeWriter};

    use futures::{StreamExt, executor::block_on};
    use lsp_types::{
        Hover, HoverParams,
        request::{HoverRequest, Initialize},
    };

    use super::*;

    /// The server side of an in-memory connection.
    struct FakeServer {
        reader: BufReader<PipeReader>,
        writer: PipeWriter,
    }

    impl FakeServer {
        fn recv(&mut self) -> Value {
            let body = codec::read_message(&mut self.reader).unwrap().unwrap();
            serde_json::from_slice(&body).unwrap()
        }

        fn send(&mut self, message: Value) {
            codec::write_message(&mut self.writer, message.to_string().as_bytes()).unwrap();
        }
    }

    fn connect() -> (
        LanguageServer,
        async_mpsc::UnboundedReceiver<Incoming>,
        FakeServer,
    ) {
        let (client_reader, server_writer) = io::pipe().unwrap();
        let (server_reader, client_writer) = io::pipe().unwrap();
        let (client, incoming) = LanguageServer::new(client_reader, client_writer, None);
        let server = FakeServer {
            reader: BufReader::new(server_reader),
            writer: server_writer,
        };
        (client, incoming, server)
    }

    fn hover_params() -> HoverParams {
        serde_json::from_value(json!({
            "textDocument": { "uri": "file:///a.py" },
            "position": { "line": 0, "character": 0 }
        }))
        .unwrap()
    }

    #[test]
    fn responses_are_matched_to_requests_by_id_in_any_order() {
        let (client, _incoming, mut server) = connect();
        let first = client.request::<HoverRequest>(hover_params());
        let second = client.request::<HoverRequest>(hover_params());
        let first_id = server.recv()["id"].clone();
        let second_id = server.recv()["id"].clone();

        server.send(json!({ "jsonrpc": "2.0", "id": second_id, "result": { "contents": "two" } }));
        server.send(json!({ "jsonrpc": "2.0", "id": first_id, "result": null }));

        let second: Option<Hover> = block_on(second).unwrap();
        assert_eq!(
            second.unwrap().contents,
            lsp_types::HoverContents::Scalar(lsp_types::MarkedString::String("two".into()))
        );
        assert_eq!(block_on(first).unwrap(), None);
    }

    #[test]
    fn error_responses_and_mistyped_results_surface_as_errors() {
        let (client, _incoming, mut server) = connect();
        let failing = client.request::<HoverRequest>(hover_params());
        let mistyped = client
            .request::<Initialize>(serde_json::from_value(json!({ "capabilities": {} })).unwrap());
        let failing_id = server.recv()["id"].clone();
        let mistyped_id = server.recv()["id"].clone();
        server.send(json!({ "jsonrpc": "2.0", "id": failing_id, "error": { "code": -32601, "message": "nope" } }));
        server.send(json!({ "jsonrpc": "2.0", "id": mistyped_id, "result": 42 }));

        assert_eq!(
            block_on(failing),
            Err(Error::Response {
                code: -32601,
                message: "nope".into()
            })
        );
        assert!(matches!(block_on(mistyped), Err(Error::Decode(_))));
    }

    #[test]
    fn server_requests_and_notifications_are_delivered_and_answerable() {
        let (client, mut incoming, mut server) = connect();
        server.send(json!({ "jsonrpc": "2.0", "method": "window/logMessage", "params": { "type": 3, "message": "hi" } }));
        server.send(json!({ "jsonrpc": "2.0", "id": "cfg-1", "method": "workspace/configuration", "params": { "items": [] } }));

        assert_eq!(
            block_on(incoming.next()),
            Some(Incoming::Notification {
                method: "window/logMessage".into(),
                params: json!({ "type": 3, "message": "hi" }),
            })
        );
        let Some(Incoming::Request { id, method, .. }) = block_on(incoming.next()) else {
            panic!("expected a request");
        };
        assert_eq!(method, "workspace/configuration");
        client.respond(id, Ok(json!([])));
        assert_eq!(
            server.recv(),
            json!({ "jsonrpc": "2.0", "id": "cfg-1", "result": [] })
        );
    }

    #[test]
    fn dropping_a_pending_request_cancels_it_on_the_server() {
        let (client, _incoming, mut server) = connect();
        let request = client.request::<HoverRequest>(hover_params());
        let id = server.recv()["id"].clone();
        drop(request);
        assert_eq!(
            server.recv(),
            json!({ "jsonrpc": "2.0", "method": "$/cancelRequest", "params": { "id": id } })
        );
        // A late answer for the cancelled request is ignored.
        server.send(json!({ "jsonrpc": "2.0", "id": id, "result": null }));
    }

    /// Records how it was ended.
    struct FakeProcess(Arc<Mutex<Vec<Duration>>>);

    impl ServerProcess for FakeProcess {
        fn end(self: Box<Self>, grace: Duration) {
            self.0.lock().push(grace);
        }
    }

    #[test]
    fn attached_processes_end_after_shutdown_with_the_grace_and_traffic_is_logged() {
        let log: Arc<Mutex<Vec<(LogKind, String)>>> = Arc::default();
        let logger: Logger = {
            let log = log.clone();
            Arc::new(move |kind, body: &str| log.lock().push((kind, body.to_owned())))
        };
        let (client_reader, server_writer) = io::pipe().unwrap();
        let (server_reader, client_writer) = io::pipe().unwrap();
        let (stderr_reader, mut stderr_writer) = io::pipe().unwrap();
        let ends: Arc<Mutex<Vec<Duration>>> = Arc::default();
        let (client, _incoming) = LanguageServer::attach(
            client_reader,
            client_writer,
            stderr_reader,
            Box::new(FakeProcess(ends.clone())),
            Some(logger),
        )
        .unwrap();
        let mut server = FakeServer {
            reader: BufReader::new(server_reader),
            writer: server_writer,
        };

        let request = client.request::<HoverRequest>(hover_params());
        let id = server.recv()["id"].clone();
        server.send(json!({ "jsonrpc": "2.0", "id": id, "result": null }));
        assert_eq!(block_on(request).unwrap(), None);
        writeln!(stderr_writer, "warning: slow").unwrap();
        drop(stderr_writer);
        // Sent messages are logged by the writer thread, received ones by
        // the reader, stderr lines by their own: wait for all three.
        let logged = |kind: LogKind| {
            for _ in 0..500 {
                if log.lock().iter().any(|(logged, _)| *logged == kind) {
                    return true;
                }
                thread::sleep(Duration::from_millis(10));
            }
            false
        };
        assert!(logged(LogKind::Sent));
        assert!(logged(LogKind::Received));
        assert!(logged(LogKind::Stderr));
        assert!(
            log.lock()
                .iter()
                .any(|(kind, body)| *kind == LogKind::Sent && body.contains("textDocument/hover"))
        );

        // `shutdown` waits for the server's answer: play the server on a thread.
        let answering = thread::spawn(move || {
            let shutdown = server.recv();
            assert_eq!(shutdown["method"], "shutdown");
            server.send(json!({ "jsonrpc": "2.0", "id": shutdown["id"], "result": null }));
            assert_eq!(server.recv()["method"], "exit");
        });
        block_on(client.shutdown());
        answering.join().unwrap();
        assert_eq!(*ends.lock(), [SHUTDOWN_GRACE]);
        // The process was already handed over: dropping the client ends nothing twice.
        drop(client);
        assert_eq!(*ends.lock(), [SHUTDOWN_GRACE]);

        let (client, _incoming) = LanguageServer::attach(
            io::empty(),
            io::sink(),
            io::empty(),
            Box::new(FakeProcess(ends.clone())),
            None,
        )
        .unwrap();
        drop(client);
        assert_eq!(*ends.lock(), [SHUTDOWN_GRACE, Duration::ZERO]);
    }

    #[test]
    fn server_exit_fails_pending_and_later_requests() {
        let (client, mut incoming, mut server) = connect();
        let pending = client.request::<HoverRequest>(hover_params());
        server.recv();
        drop(server);

        assert_eq!(block_on(pending), Err(Error::ServerExited));
        assert_eq!(block_on(incoming.next()), Some(Incoming::Exited));
        assert!(client.has_exited());
        assert_eq!(
            block_on(client.request::<HoverRequest>(hover_params())),
            Err(Error::ServerExited)
        );
    }
}
