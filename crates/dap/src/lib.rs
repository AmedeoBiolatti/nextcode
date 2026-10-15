//! Debug Adapter Protocol client over any byte stream (a local adapter
//! process's stdio, or one tunnelled through the remote agent).
//!
//! Requests carry a `seq` and resolve when the response with that
//! `request_seq` arrives; events and the adapter's own (reverse) requests
//! arrive on the [`Incoming`] channel, which ends with
//! [`Incoming::Exited`] when the adapter's output closes. I/O runs on
//! dedicated threads; requests are executor-agnostic futures. Timeouts are
//! the caller's (it knows its executor's timers).

mod codec;
pub mod types;

use std::{
    collections::HashMap,
    fmt,
    future::Future,
    io::{self, Read, Write},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, Ordering},
        mpsc,
    },
    task::{Context, Poll},
    thread,
};

pub use codec::{FrameDecoder, write_message};
use futures::channel::{mpsc as async_mpsc, oneshot};
use parking_lot::Mutex;
use serde_json::{Value, json};
pub use types::Request;

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// The adapter answered `success: false`.
    Response { command: String, message: String },
    /// The adapter's output closed before it answered.
    AdapterExited,
    /// The answer's body did not match the expected type.
    Decode(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Response { command, message } => write!(f, "{command} failed: {message}"),
            Self::AdapterExited => f.write_str("the debug adapter exited"),
            Self::Decode(message) => write!(f, "unexpected debug adapter response: {message}"),
        }
    }
}

impl std::error::Error for Error {}

/// Traffic the adapter initiates.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Event {
        event: String,
        body: Value,
    },
    /// A reverse request (`runInTerminal`, `startDebugging`); answer it
    /// with [`Client::respond`].
    Request {
        seq: i64,
        command: String,
        arguments: Value,
    },
    /// The adapter's output closed; nothing else will arrive.
    Exited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    Sent,
    Received,
}

pub type Logger = Arc<dyn Fn(LogKind, &str) + Send + Sync>;

type Responder = oneshot::Sender<Result<Value, Error>>;

enum Outgoing {
    /// Framed and written by the writer thread.
    Writer(mpsc::Sender<Vec<u8>>),
    /// Handed to an in-process [`Peer`].
    Memory(async_mpsc::UnboundedSender<Value>),
}

struct Shared {
    outgoing: Outgoing,
    pending: Mutex<HashMap<i64, Responder>>,
    next_seq: AtomicI64,
    exited: AtomicBool,
    incoming: async_mpsc::UnboundedSender<Incoming>,
    logger: Option<Logger>,
}

impl Shared {
    fn new(
        outgoing: Outgoing,
        logger: Option<Logger>,
    ) -> (Arc<Self>, async_mpsc::UnboundedReceiver<Incoming>) {
        let (incoming, incoming_rx) = async_mpsc::unbounded();
        let shared = Arc::new(Self {
            outgoing,
            pending: Mutex::default(),
            next_seq: AtomicI64::new(1),
            exited: AtomicBool::new(false),
            incoming,
            logger,
        });
        (shared, incoming_rx)
    }

    fn next_seq(&self) -> i64 {
        self.next_seq.fetch_add(1, Ordering::Relaxed)
    }

    fn send(&self, message: Value) {
        match &self.outgoing {
            Outgoing::Writer(writer) => {
                let body = message.to_string();
                if let Some(logger) = &self.logger {
                    logger(LogKind::Sent, &body);
                }
                // A closed writer means the adapter is gone; pending
                // requests fail via the reader.
                writer.send(body.into_bytes()).ok();
            }
            Outgoing::Memory(peer) => {
                peer.unbounded_send(message).ok();
            }
        }
    }

    /// Routes one message from the adapter.
    fn dispatch(&self, message: Value) {
        match message["type"].as_str() {
            Some("response") => {
                let Some(request_seq) = message["request_seq"].as_i64() else {
                    return;
                };
                // Responses to requests nobody waits for are ignored.
                let Some(responder) = self.pending.lock().remove(&request_seq) else {
                    return;
                };
                let result = if message["success"].as_bool().unwrap_or(false) {
                    Ok(message.get("body").cloned().unwrap_or(Value::Null))
                } else {
                    Err(Error::Response {
                        command: message["command"].as_str().unwrap_or_default().to_owned(),
                        message: response_error(&message),
                    })
                };
                responder.send(result).ok();
            }
            Some("event") => {
                self.incoming
                    .unbounded_send(Incoming::Event {
                        event: message["event"].as_str().unwrap_or_default().to_owned(),
                        body: message.get("body").cloned().unwrap_or(Value::Null),
                    })
                    .ok();
            }
            Some("request") => {
                self.incoming
                    .unbounded_send(Incoming::Request {
                        seq: message["seq"].as_i64().unwrap_or_default(),
                        command: message["command"].as_str().unwrap_or_default().to_owned(),
                        arguments: message.get("arguments").cloned().unwrap_or(Value::Null),
                    })
                    .ok();
            }
            _ => {}
        }
    }

    /// The adapter's output ended: fail what waits, report the exit once.
    fn close(&self) {
        if self.exited.swap(true, Ordering::AcqRel) {
            return;
        }
        for (_, responder) in self.pending.lock().drain() {
            responder.send(Err(Error::AdapterExited)).ok();
        }
        self.incoming.unbounded_send(Incoming::Exited).ok();
    }
}

/// The human-readable reason of a failed response: the formatted error
/// body when there is one, else `message`.
fn response_error(message: &Value) -> String {
    let body_error = &message["body"]["error"];
    if let Some(format) = body_error["format"].as_str() {
        // `{name}` placeholders filled from `variables`.
        let mut text = format.to_owned();
        if let Some(variables) = body_error["variables"].as_object() {
            for (name, value) in variables {
                if let Some(value) = value.as_str() {
                    text = text.replace(&format!("{{{name}}}"), value);
                }
            }
        }
        return text;
    }
    message["message"]
        .as_str()
        .unwrap_or("request failed")
        .to_owned()
}

pub struct Client {
    shared: Arc<Shared>,
    /// Ends the adapter process when dropped.
    guard: Mutex<Option<Box<dyn Send>>>,
}

impl Client {
    /// Starts a client over an adapter's output (`reader`) and input
    /// (`writer`).
    pub fn new(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        logger: Option<Logger>,
    ) -> (Self, async_mpsc::UnboundedReceiver<Incoming>) {
        let (outgoing, outgoing_rx) = mpsc::channel::<Vec<u8>>();
        let (shared, incoming) = Shared::new(Outgoing::Writer(outgoing), logger);
        thread::Builder::new()
            .name("dap-writer".into())
            .spawn(move || {
                let mut writer = io::BufWriter::new(writer);
                for body in outgoing_rx {
                    if codec::write_message(&mut writer, &body).is_err() {
                        break;
                    }
                }
            })
            .expect("spawning DAP writer thread");
        let reader_shared = shared.clone();
        thread::Builder::new()
            .name("dap-reader".into())
            .spawn(move || read_loop(reader, &reader_shared))
            .expect("spawning DAP reader thread");
        let client = Self {
            shared,
            guard: Mutex::new(None),
        };
        (client, incoming)
    }

    /// A client wired to an in-process [`Peer`] playing the adapter (for
    /// tests: no threads, every step runs on the caller's executor).
    pub fn pair() -> (Self, async_mpsc::UnboundedReceiver<Incoming>, Peer) {
        let (to_peer, from_client) = async_mpsc::unbounded();
        let (shared, incoming) = Shared::new(Outgoing::Memory(to_peer), None);
        let peer = Peer {
            shared: shared.clone(),
            received: from_client,
        };
        let client = Self {
            shared,
            guard: Mutex::new(None),
        };
        (client, incoming, peer)
    }

    /// Ties the adapter process's lifetime to this client: `guard` is
    /// dropped (and must end the process) when the client is.
    pub fn own_process(&self, guard: Box<dyn Send>) {
        *self.guard.lock() = Some(guard);
    }

    /// Sends a typed request.
    pub fn request<R: Request>(
        &self,
        arguments: R::Arguments,
    ) -> impl Future<Output = Result<R::Response, Error>> + use<R> {
        let arguments = serde_json::to_value(arguments).unwrap_or(Value::Null);
        let pending = self.request_raw(R::COMMAND, arguments);
        async move {
            let body = pending.await?;
            // Adapters may omit the body of an empty answer.
            let body = if body.is_null() { json!({}) } else { body };
            serde_json::from_value(body).map_err(|err| Error::Decode(err.to_string()))
        }
    }

    /// Sends `command`; the future resolves to the response body.
    /// Dropping it before then forgets the request.
    pub fn request_raw(&self, command: &str, arguments: Value) -> PendingRequest {
        let seq = self.shared.next_seq();
        let (responder, response) = oneshot::channel();
        if self.shared.exited.load(Ordering::Acquire) {
            responder.send(Err(Error::AdapterExited)).ok();
        } else {
            self.shared.pending.lock().insert(seq, responder);
            let mut message = json!({ "seq": seq, "type": "request", "command": command });
            if !arguments.is_null() {
                message["arguments"] = arguments;
            }
            self.shared.send(message);
        }
        PendingRequest {
            seq,
            response,
            shared: self.shared.clone(),
            done: false,
        }
    }

    /// Answers a reverse request: `Ok(body)` or `Err(message)`.
    pub fn respond(&self, request_seq: i64, command: &str, result: Result<Value, String>) {
        let mut message = json!({
            "seq": self.shared.next_seq(),
            "type": "response",
            "request_seq": request_seq,
            "command": command,
            "success": result.is_ok(),
        });
        match result {
            Ok(body) if !body.is_null() => message["body"] = body,
            Ok(_) => {}
            Err(error) => message["message"] = error.into(),
        }
        self.shared.send(message);
    }

    pub fn has_exited(&self) -> bool {
        self.shared.exited.load(Ordering::Acquire)
    }
}

/// A request awaiting its response.
pub struct PendingRequest {
    seq: i64,
    response: oneshot::Receiver<Result<Value, Error>>,
    shared: Arc<Shared>,
    done: bool,
}

impl Future for PendingRequest {
    type Output = Result<Value, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let result = match Pin::new(&mut self.response).poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(result) => result,
        };
        self.done = true;
        Poll::Ready(result.unwrap_or(Err(Error::AdapterExited)))
    }
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        if !self.done {
            self.shared.pending.lock().remove(&self.seq);
        }
    }
}

fn read_loop(mut reader: impl Read, shared: &Shared) {
    let mut decoder = FrameDecoder::default();
    let mut buffer = vec![0; 16 * 1024];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let Ok(messages) = decoder.push(&buffer[..read]) else {
            // The stream is out of sync; nothing after this can be trusted.
            break;
        };
        for body in messages {
            if let Some(logger) = &shared.logger {
                logger(LogKind::Received, &String::from_utf8_lossy(&body));
            }
            if let Ok(message) = serde_json::from_slice(&body) {
                shared.dispatch(message);
            }
        }
    }
    shared.close();
}

/// The adapter side of [`Client::pair`].
pub struct Peer {
    shared: Arc<Shared>,
    /// What the client sent: requests and answers to reverse requests.
    pub received: async_mpsc::UnboundedReceiver<Value>,
}

impl Peer {
    /// Delivers a raw message to the client.
    pub fn send(&self, message: Value) {
        self.shared.dispatch(message);
    }

    /// Answers `request` successfully with `body`.
    pub fn respond(&self, request: &Value, body: Value) {
        self.send(json!({
            "seq": 0,
            "type": "response",
            "request_seq": request["seq"],
            "command": request["command"],
            "success": true,
            "body": body,
        }));
    }

    /// Answers `request` with a failure.
    pub fn fail(&self, request: &Value, message: &str) {
        self.send(json!({
            "seq": 0,
            "type": "response",
            "request_seq": request["seq"],
            "command": request["command"],
            "success": false,
            "message": message,
        }));
    }

    pub fn event(&self, event: &str, body: Value) {
        self.send(json!({ "seq": 0, "type": "event", "event": event, "body": body }));
    }

    /// A reverse request from the adapter.
    pub fn reverse_request(&self, seq: i64, command: &str, arguments: Value) {
        self.send(
            json!({ "seq": seq, "type": "request", "command": command, "arguments": arguments }),
        );
    }

    /// The adapter exits: its output closes.
    pub fn exit(&self) {
        self.shared.close();
    }
}

#[cfg(test)]
mod tests {
    use std::io::{PipeReader, PipeWriter};

    use futures::{StreamExt, executor::block_on};

    use super::{types::*, *};

    /// The adapter side of a pipe connection.
    struct FakeAdapter {
        decoder: FrameDecoder,
        queued: Vec<Value>,
        reader: PipeReader,
        writer: PipeWriter,
    }

    impl FakeAdapter {
        fn recv(&mut self) -> Value {
            while self.queued.is_empty() {
                let mut buffer = [0; 1024];
                let read = self.reader.read(&mut buffer).unwrap();
                assert!(read > 0, "client closed");
                for body in self.decoder.push(&buffer[..read]).unwrap() {
                    self.queued.push(serde_json::from_slice(&body).unwrap());
                }
            }
            self.queued.remove(0)
        }

        fn send_raw(&mut self, bytes: &[u8]) {
            self.writer.write_all(bytes).unwrap();
            self.writer.flush().unwrap();
        }

        fn send(&mut self, message: Value) {
            write_message(&mut self.writer, message.to_string().as_bytes()).unwrap();
        }
    }

    fn connect() -> (Client, async_mpsc::UnboundedReceiver<Incoming>, FakeAdapter) {
        let (client_reader, adapter_writer) = io::pipe().unwrap();
        let (adapter_reader, client_writer) = io::pipe().unwrap();
        let (client, incoming) = Client::new(client_reader, client_writer, None);
        let adapter = FakeAdapter {
            decoder: FrameDecoder::default(),
            queued: Vec::new(),
            reader: adapter_reader,
            writer: adapter_writer,
        };
        (client, incoming, adapter)
    }

    fn response(request: &Value, body: Value) -> Value {
        json!({ "seq": 100, "type": "response", "request_seq": request["seq"],
                "command": request["command"], "success": true, "body": body })
    }

    #[test]
    fn responses_match_requests_by_seq_in_any_order() {
        let (client, _incoming, mut adapter) = connect();
        let threads = client.request::<Threads>(Value::Null);
        let evaluate = client.request::<Evaluate>(EvaluateArguments {
            expression: "x + 1".into(),
            frame_id: Some(7),
            context: "repl".into(),
        });
        let threads_request = adapter.recv();
        let evaluate_request = adapter.recv();
        assert_eq!(threads_request["command"], "threads");
        assert_eq!(threads_request["type"], "request");
        assert_eq!(
            evaluate_request["arguments"],
            json!({ "expression": "x + 1", "frameId": 7, "context": "repl" })
        );
        assert_ne!(threads_request["seq"], evaluate_request["seq"]);

        // Answered in reverse order, in one write, after an event.
        let mut bytes = Vec::new();
        for message in [
            json!({ "seq": 1, "type": "event", "event": "output", "body": { "output": "hi\n" } }),
            response(
                &evaluate_request,
                json!({ "result": "42", "variablesReference": 0 }),
            ),
            response(
                &threads_request,
                json!({ "threads": [{ "id": 1, "name": "main" }] }),
            ),
        ] {
            write_message(&mut bytes, message.to_string().as_bytes()).unwrap();
        }
        adapter.send_raw(&bytes);

        assert_eq!(block_on(evaluate).unwrap().result, "42");
        assert_eq!(
            block_on(threads).unwrap().threads,
            [Thread {
                id: 1,
                name: "main".into()
            }]
        );
    }

    #[test]
    fn failures_carry_the_adapter_message_and_bad_bodies_are_decode_errors() {
        let (client, _incoming, mut adapter) = connect();
        let failing = client.request::<Evaluate>(EvaluateArguments {
            expression: "nope".into(),
            frame_id: None,
            context: "hover".into(),
        });
        let mistyped = client.request::<StackTrace>(StackTraceArguments {
            thread_id: 1,
            start_frame: None,
            levels: None,
        });
        let formatted = client.request_raw("next", Value::Null);
        let (a, b, c) = (adapter.recv(), adapter.recv(), adapter.recv());
        adapter.send(
            json!({ "seq": 5, "type": "response", "request_seq": a["seq"],
            "command": "evaluate", "success": false, "message": "name 'nope' is not defined" }),
        );
        adapter.send(response(&b, json!({ "stackFrames": 3 })));
        adapter.send(json!({ "seq": 6, "type": "response", "request_seq": c["seq"],
            "command": "next", "success": false, "message": "short",
            "body": { "error": { "id": 1, "format": "Thread {id} is gone", "variables": { "id": "4" } } } }));

        assert_eq!(
            block_on(failing),
            Err(Error::Response {
                command: "evaluate".into(),
                message: "name 'nope' is not defined".into()
            })
        );
        assert!(matches!(block_on(mistyped), Err(Error::Decode(_))));
        assert_eq!(
            block_on(formatted),
            Err(Error::Response {
                command: "next".into(),
                message: "Thread 4 is gone".into()
            })
        );
    }

    #[test]
    fn events_and_reverse_requests_arrive_and_can_be_answered() {
        let (client, mut incoming, mut adapter) = connect();
        adapter.send(json!({ "seq": 1, "type": "event", "event": "initialized" }));
        adapter.send(
            json!({ "seq": 2, "type": "request", "command": "runInTerminal",
            "arguments": { "cwd": "/p", "args": ["python", "a.py"] } }),
        );
        assert_eq!(
            block_on(incoming.next()),
            Some(Incoming::Event {
                event: "initialized".into(),
                body: Value::Null
            })
        );
        let Some(Incoming::Request { seq, command, .. }) = block_on(incoming.next()) else {
            panic!("expected a reverse request");
        };
        assert_eq!((seq, command.as_str()), (2, "runInTerminal"));
        client.respond(seq, &command, Err("not supported".into()));
        let answer = adapter.recv();
        assert_eq!(answer["type"], "response");
        assert_eq!(answer["request_seq"], 2);
        assert_eq!(answer["success"], false);
        assert_eq!(answer["message"], "not supported");
    }

    #[test]
    fn adapter_exit_fails_pending_and_later_requests() {
        let (client, mut incoming, mut adapter) = connect();
        let pending = client.request_raw("continue", json!({ "threadId": 1 }));
        adapter.recv();
        drop(adapter);
        assert_eq!(block_on(pending), Err(Error::AdapterExited));
        assert_eq!(block_on(incoming.next()), Some(Incoming::Exited));
        assert!(client.has_exited());
        assert_eq!(
            block_on(client.request_raw("threads", Value::Null)),
            Err(Error::AdapterExited)
        );
    }

    #[test]
    fn a_peer_plays_the_adapter_in_process() {
        let (client, mut incoming, mut peer) = Client::pair();
        let threads = client.request::<Threads>(Value::Null);
        let request = block_on(peer.received.next()).unwrap();
        peer.respond(&request, json!({ "threads": [] }));
        assert!(block_on(threads).unwrap().threads.is_empty());
        // Dropped requests are forgotten; their late answers are ignored.
        let dropped = client.request_raw("pause", Value::Null);
        let request = block_on(peer.received.next()).unwrap();
        drop(dropped);
        peer.respond(&request, Value::Null);
        peer.exit();
        assert_eq!(block_on(incoming.next()), Some(Incoming::Exited));
    }
}
