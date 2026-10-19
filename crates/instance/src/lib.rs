//! One editor process per user and state directory: a launch hands its
//! command line to the editor already running, which opens the window, so
//! the launch costs a connection instead of a process start.
//!
//! The channel is a named pipe on Windows and a Unix socket elsewhere,
//! placed by the editor's state directory (`Endpoint::for_state_root`), so
//! editors with private state (tests, `--automation`) have channels of
//! their own. One JSON line goes each way: a [`Request`] (the command line
//! and the directory it is relative to) and a [`Reply`].
//!
//! No gpui here: the launcher (`crates/launcher`) is built from this crate
//! alone, so it stays small and starts in a few milliseconds.

use std::{
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};

/// The most of a request or reply line that is read (1 MiB): a local
/// client cannot make the editor allocate without bound. A longer line is
/// cut there and fails to parse.
const MAX_LINE: u64 = 1024 * 1024;

/// A launch, as the editor process receives it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// The command line after the program name (`--quit` asks the editor
    /// to exit).
    pub args: Vec<String>,
    /// The directory relative paths in `args` are relative to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
}

/// The editor's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Reply {
    /// The request was carried out (the window is opening, or the editor
    /// is quitting).
    Done,
    /// The request was refused (an argument error, mostly).
    Error { message: String },
    /// The running editor is exiting instead (its executable was rebuilt):
    /// start a new one with the same command line.
    Restart,
}

/// Where the editor for a state directory listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    #[cfg(windows)]
    pipe: String,
    #[cfg(not(windows))]
    path: PathBuf,
}

impl Endpoint {
    /// The channel of the editor whose state lives in `state_root` (its
    /// `windows.json` directory).
    pub fn for_state_root(state_root: &Path) -> Self {
        #[cfg(windows)]
        {
            // Pipe names are global to the machine: one per user and
            // state directory.
            let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
            let user: String = user
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            let key = fnv(state_root.to_string_lossy().to_lowercase().as_bytes());
            Self {
                pipe: format!(r"\\.\pipe\next_code-{user}-{key:016x}"),
            }
        }
        #[cfg(not(windows))]
        {
            let path = state_root.join("instance.sock");
            // Socket paths are limited to about 100 bytes.
            let path = if path.as_os_str().len() < 100 {
                path
            } else {
                let key = fnv(state_root.as_os_str().as_encoded_bytes());
                std::env::temp_dir().join(format!("next_code-{key:016x}.sock"))
            };
            Self { path }
        }
    }

    /// The pipe name or socket path, for messages.
    pub fn describe(&self) -> String {
        #[cfg(windows)]
        {
            self.pipe.clone()
        }
        #[cfg(not(windows))]
        {
            self.path.display().to_string()
        }
    }
}

/// The editor's state directory (`windows.json` and the rest), as the
/// editor's `layout::state_root` computes it.
pub fn default_state_root() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("next_code"))
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Why a request could not be delivered.
#[derive(Debug)]
pub enum SendError {
    /// No editor listens there.
    NoInstance,
    /// An editor listens but did not answer in time.
    Timeout,
    Io(io::Error),
    /// The answer was not a reply.
    Protocol(String),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoInstance => write!(f, "no running editor"),
            Self::Timeout => write!(f, "the running editor did not answer"),
            Self::Io(err) => write!(f, "{err}"),
            Self::Protocol(text) => write!(f, "unexpected answer: {text}"),
        }
    }
}

impl std::error::Error for SendError {}

/// Delivers `request` to the editor at `endpoint` and returns its reply,
/// giving up after `timeout`. `NoInstance` means none is running there.
pub fn send(endpoint: &Endpoint, request: &Request, timeout: Duration) -> Result<Reply, SendError> {
    let endpoint = endpoint.clone();
    let line = serde_json::to_string(request).map_err(|err| SendError::Protocol(err.to_string()))?;
    let (tx, rx) = mpsc::channel();
    // The exchange runs on a thread so a stuck editor cannot hold the
    // caller past `timeout`: pipes have no read timeout of their own.
    thread::Builder::new()
        .name("instance-send".into())
        .spawn(move || {
            tx.send(exchange(&endpoint, &line, timeout)).ok();
        })
        .map_err(SendError::Io)?;
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => Err(SendError::Timeout),
    }
}

fn exchange(endpoint: &Endpoint, line: &str, timeout: Duration) -> Result<Reply, SendError> {
    let mut stream = connect(endpoint, timeout)?;
    stream.write_all(line.as_bytes()).map_err(SendError::Io)?;
    stream.write_all(b"\n").map_err(SendError::Io)?;
    stream.flush().map_err(SendError::Io)?;
    let mut answer = String::new();
    BufReader::new(stream.take(MAX_LINE))
        .read_line(&mut answer)
        .map_err(SendError::Io)?;
    if answer.trim().is_empty() {
        return Err(SendError::Protocol("the editor closed the connection".into()));
    }
    serde_json::from_str(&answer).map_err(|_| SendError::Protocol(answer.trim().to_owned()))
}

/// Something that handles one request at a time.
type Handler = Box<dyn FnMut(Request) -> Reply + Send>;

/// Why an editor could not start listening.
#[derive(Debug)]
pub enum ServeError {
    /// Another editor already listens at the endpoint.
    AlreadyRunning,
    Io(io::Error),
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning => write!(f, "another editor is already running"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ServeError {}

/// A listening editor; dropping it stops listening.
pub struct Server {
    endpoint: Endpoint,
    stopping: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Server {
    /// Stops accepting requests (the channel disappears for clients).
    pub fn stop(&mut self) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(thread) = self.thread.take() else {
            return;
        };
        // Wake the accept loop with connections of our own (on Windows a
        // spare pipe instance may take the first one), then give it a
        // moment; a thread that will not finish is left behind.
        for _ in 0..4 {
            connect(&self.endpoint, Duration::from_millis(200)).ok();
            for _ in 0..20 {
                if thread.is_finished() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            if thread.is_finished() {
                thread.join().ok();
                break;
            }
        }
        #[cfg(not(windows))]
        std::fs::remove_file(&self.endpoint.path).ok();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts answering requests at `endpoint` with `handler`, on a thread.
/// Fails with `AlreadyRunning` when another editor listens there.
pub fn serve(
    endpoint: &Endpoint,
    handler: impl FnMut(Request) -> Reply + Send + 'static,
) -> Result<Server, ServeError> {
    let stopping = Arc::new(AtomicBool::new(false));
    let handler: Handler = Box::new(handler);
    let mut server = Server {
        endpoint: endpoint.clone(),
        stopping: stopping.clone(),
        thread: None,
    };
    let thread = start_listening(&mut server, handler)?;
    server.thread = Some(thread);
    Ok(server)
}

// ---------------------------------------------------------------- Windows

#[cfg(windows)]
struct WindowsPipe(std::fs::File);

#[cfg(windows)]
fn create_pipe(name: &str, first: bool) -> Result<WindowsPipe, ServeError> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::{
        Foundation::{ERROR_ACCESS_DENIED, GetLastError, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX},
        System::Pipes::{
            CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
            PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
        },
    };
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut open_mode = PIPE_ACCESS_DUPLEX;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    // SAFETY: a plain call with a NUL-terminated name; the handle is
    // checked before it is owned.
    let handle = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            4096,
            4096,
            0,
            std::ptr::null(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        // SAFETY: plain query.
        let code = unsafe { GetLastError() };
        return Err(if first && code == ERROR_ACCESS_DENIED {
            ServeError::AlreadyRunning
        } else {
            ServeError::Io(io::Error::from_raw_os_error(code as i32))
        });
    }
    // SAFETY: the handle is valid and owned by nothing else; `File` closes it.
    Ok(WindowsPipe(unsafe { std::fs::File::from_raw_handle(handle as _) }))
}

#[cfg(windows)]
impl WindowsPipe {
    /// Waits for a client.
    fn accept(&self) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{
            Foundation::{ERROR_PIPE_CONNECTED, GetLastError},
            System::Pipes::ConnectNamedPipe,
        };
        // SAFETY: a blocking call on our own handle.
        let ok = unsafe { ConnectNamedPipe(self.0.as_raw_handle() as _, std::ptr::null_mut()) };
        if ok != 0 {
            return Ok(());
        }
        // SAFETY: plain query.
        let code = unsafe { GetLastError() };
        if code == ERROR_PIPE_CONNECTED {
            return Ok(());
        }
        Err(io::Error::from_raw_os_error(code as i32))
    }

    fn finish(&self) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{Storage::FileSystem::FlushFileBuffers, System::Pipes::DisconnectNamedPipe};
        // SAFETY: plain calls on our own handle.
        unsafe {
            FlushFileBuffers(self.0.as_raw_handle() as _);
            DisconnectNamedPipe(self.0.as_raw_handle() as _);
        }
    }
}

#[cfg(windows)]
fn start_listening(server: &mut Server, mut handler: Handler) -> Result<thread::JoinHandle<()>, ServeError> {
    let name = server.endpoint.pipe.clone();
    // The first instance is created here so "already running" is known
    // before the thread starts.
    let first = create_pipe(&name, true)?;
    let stopping = server.stopping.clone();
    thread::Builder::new()
        .name("instance-server".into())
        .spawn(move || {
            let mut pipe = first;
            loop {
                // A spare instance exists while this one is busy, so a
                // launch arriving meanwhile finds the pipe (else it would
                // start a second editor).
                let spare = create_pipe(&name, false).ok();
                let accepted = pipe.accept().is_ok();
                if stopping.load(Ordering::SeqCst) {
                    pipe.finish();
                    break;
                }
                if accepted {
                    answer(&pipe.0, &mut handler);
                }
                pipe.finish();
                pipe = match spare.map(Ok).unwrap_or_else(|| create_pipe(&name, false)) {
                    Ok(pipe) => pipe,
                    Err(_) => break,
                };
            }
        })
        .map_err(ServeError::Io)
}

#[cfg(windows)]
fn connect(endpoint: &Endpoint, timeout: Duration) -> Result<std::fs::File, SendError> {
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    const ERROR_PIPE_BUSY: i32 = 231;
    let started = std::time::Instant::now();
    loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&endpoint.pipe)
        {
            Ok(file) => return Ok(file),
            Err(err) => match err.raw_os_error() {
                Some(ERROR_FILE_NOT_FOUND) => return Err(SendError::NoInstance),
                // Every instance is busy: the editor is answering another
                // launch; it creates the next instance right after.
                Some(ERROR_PIPE_BUSY) if started.elapsed() < timeout => {
                    thread::sleep(Duration::from_millis(20));
                }
                Some(ERROR_PIPE_BUSY) => return Err(SendError::Timeout),
                _ => return Err(SendError::Io(err)),
            },
        }
    }
}

// ------------------------------------------------------------------- Unix

#[cfg(not(windows))]
fn start_listening(server: &mut Server, mut handler: Handler) -> Result<thread::JoinHandle<()>, ServeError> {
    use std::os::unix::net::UnixListener;
    let path = server.endpoint.path.clone();
    // A socket file left by an editor that died is taken over; one a
    // running editor answers on is not.
    match connect(&server.endpoint, Duration::from_millis(500)) {
        Ok(_) => return Err(ServeError::AlreadyRunning),
        Err(SendError::NoInstance) => {}
        Err(SendError::Timeout) => return Err(ServeError::AlreadyRunning),
        Err(SendError::Io(err)) => return Err(ServeError::Io(err)),
        Err(SendError::Protocol(_)) => return Err(ServeError::AlreadyRunning),
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(ServeError::Io)?;
    }
    std::fs::remove_file(&path).ok();
    let listener = UnixListener::bind(&path).map_err(ServeError::Io)?;
    let stopping = server.stopping.clone();
    thread::Builder::new()
        .name("instance-server".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if stopping.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else {
                    continue;
                };
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
                answer(&stream, &mut handler);
            }
        })
        .map_err(ServeError::Io)
}

#[cfg(not(windows))]
fn connect(endpoint: &Endpoint, timeout: Duration) -> Result<std::os::unix::net::UnixStream, SendError> {
    use std::os::unix::net::UnixStream;
    match UnixStream::connect(&endpoint.path) {
        Ok(stream) => {
            stream.set_read_timeout(Some(timeout)).ok();
            stream.set_write_timeout(Some(timeout)).ok();
            Ok(stream)
        }
        Err(err) => match err.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => Err(SendError::NoInstance),
            _ => Err(SendError::Io(err)),
        },
    }
}

// ------------------------------------------------------------------ shared

/// Reads one request from `stream`, answers it.
fn answer<S>(mut stream: S, handler: &mut Handler)
where
    S: io::Read + io::Write,
{
    let mut line = String::new();
    let mut reader = BufReader::new((&mut stream).take(MAX_LINE));
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    let reply = match serde_json::from_str::<Request>(&line) {
        Ok(request) => handler(request),
        Err(err) => Reply::Error {
            message: format!("bad request: {err}"),
        },
    };
    if let Ok(json) = serde_json::to_string(&reply) {
        stream.write_all(json.as_bytes()).ok();
        stream.write_all(b"\n").ok();
        stream.flush().ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client connection in memory: `input` is what it sends, `output`
    /// what it is answered.
    struct Pipe {
        input: io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl io::Read for Pipe {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl io::Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn an_endless_request_line_is_cut_at_the_limit() {
        let handled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counting = handled.clone();
        let mut handler: Handler = Box::new(move |_| {
            counting.fetch_add(1, Ordering::Relaxed);
            Reply::Done
        });
        // Three times the limit, never a newline: read up to the limit only.
        let mut pipe = Pipe {
            input: io::Cursor::new(vec![b'a'; 3 * MAX_LINE as usize]),
            output: Vec::new(),
        };
        answer(&mut pipe, &mut handler);
        assert_eq!(pipe.input.position(), MAX_LINE);
        let reply = String::from_utf8(pipe.output).unwrap();
        assert!(reply.contains("bad request"), "{reply}");
        assert_eq!(handled.load(Ordering::Relaxed), 0);

        let mut pipe = Pipe {
            input: io::Cursor::new(br#"{"args":["."]}"#.to_vec()),
            output: Vec::new(),
        };
        let mut handler: Handler = Box::new(|_| Reply::Done);
        answer(&mut pipe, &mut handler);
        assert_eq!(
            String::from_utf8(pipe.output).unwrap(),
            "{\"result\":\"done\"}\n"
        );
    }

    fn endpoint(name: &str) -> (Endpoint, PathBuf) {
        let root = std::env::temp_dir().join(format!("next_code-instance-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        (Endpoint::for_state_root(&root), root)
    }

    #[test]
    fn requests_reach_the_server_and_replies_come_back() {
        let (endpoint, root) = endpoint("roundtrip");
        assert!(matches!(
            send(&endpoint, &Request { args: vec![], cwd: None }, Duration::from_secs(2)),
            Err(SendError::NoInstance)
        ));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let mut server = serve(&endpoint, move |request| {
            log.lock().unwrap().push(request.clone());
            if request.args.first().is_some_and(|arg| arg == "bad") {
                Reply::Error {
                    message: "no".into(),
                }
            } else {
                Reply::Done
            }
        })
        .unwrap();
        let request = Request {
            args: vec!["C:\\dev\\p".into(), "--x".into()],
            cwd: Some(PathBuf::from("/home/me")),
        };
        assert_eq!(send(&endpoint, &request, Duration::from_secs(5)).unwrap(), Reply::Done);
        assert_eq!(
            send(
                &endpoint,
                &Request {
                    args: vec!["bad".into()],
                    cwd: None
                },
                Duration::from_secs(5)
            )
            .unwrap(),
            Reply::Error {
                message: "no".into()
            }
        );
        // Several in a row: the next pipe instance is ready each time.
        for _ in 0..3 {
            assert_eq!(send(&endpoint, &request, Duration::from_secs(5)).unwrap(), Reply::Done);
        }
        assert_eq!(seen.lock().unwrap().len(), 5);
        assert_eq!(seen.lock().unwrap()[0], request);
        // A second editor cannot listen there.
        assert!(matches!(
            serve(&endpoint, |_| Reply::Done).map(|_| ()),
            Err(ServeError::AlreadyRunning)
        ));
        server.stop();
        assert!(matches!(
            send(&endpoint, &request, Duration::from_secs(2)),
            Err(SendError::NoInstance)
        ));
        // Listening again works once the first server is gone.
        let server = serve(&endpoint, |_| Reply::Restart).unwrap();
        assert_eq!(send(&endpoint, &request, Duration::from_secs(5)).unwrap(), Reply::Restart);
        drop(server);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn endpoints_differ_per_state_root() {
        let a = Endpoint::for_state_root(Path::new("/tmp/a"));
        let b = Endpoint::for_state_root(Path::new("/tmp/b"));
        assert_ne!(a, b);
        assert_eq!(a, Endpoint::for_state_root(Path::new("/tmp/a")));
    }

    #[test]
    fn replies_serialize_as_tagged_json() {
        assert_eq!(serde_json::to_string(&Reply::Done).unwrap(), r#"{"result":"done"}"#);
        assert_eq!(
            serde_json::from_str::<Reply>(r#"{"result":"error","message":"m"}"#).unwrap(),
            Reply::Error {
                message: "m".into()
            }
        );
        let request: Request = serde_json::from_str(r#"{"args":["."]}"#).unwrap();
        assert_eq!(request.cwd, None);
    }
}
