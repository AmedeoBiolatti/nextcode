//! Pinpoint on the agent's machine: `next_code_agent pinpoint …` (run by a
//! CLI agent inside WSL or a dev container) reaches the editor's pinpoint
//! control through the agent serving the project.
//!
//! While the editor runs a [`proto::Request::Pinpoint`] stream, the agent
//! listens on a Unix socket `<dir>/<pid>-<n>.sock` and describes it in
//! `<dir>/<pid>-<n>.json` (`{"socket", "roots", "pid", "started"}`),
//! where `<dir>` is [`runtime_dir`], private to the user. A client writes one
//! JSON request line and reads one JSON reply line; the agent forwards the
//! request to the editor as an [`proto::Event::Pinpoint`] and writes back the
//! editor's [`proto::ClientMessage::PinpointReply`].

use std::{
    collections::HashMap,
    env, fs,
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use parking_lot::Mutex;
use serde_json::{Value, json};

/// Overrides [`runtime_dir`] (both for the agent and for clients).
pub const DIR_ENV: &str = "NEXT_CODE_PINPOINT_DIR";
/// Names the socket a client uses, skipping discovery.
pub const SOCKET_ENV: &str = "NEXT_CODE_PINPOINT_SOCKET";
/// A request line longer than this is refused.
const MAX_REQUEST: u64 = 4 * 1024 * 1024;

/// Where agents put their pinpoint sockets: `$XDG_RUNTIME_DIR/next_code`,
/// else `/tmp/next_code-<uid>` (containers rarely have a runtime dir).
pub fn runtime_dir() -> PathBuf {
    runtime_dirs().swap_remove(0)
}

/// [`runtime_dir`] first, then the other place an agent started with
/// another environment (`docker exec` sets no `XDG_RUNTIME_DIR`) used.
fn runtime_dirs() -> Vec<PathBuf> {
    if let Some(dir) = env::var_os(DIR_ENV) {
        return vec![PathBuf::from(dir)];
    }
    let temp = env::temp_dir().join(format!("next_code-{}", uid()));
    match env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        Some(dir) if dir.is_absolute() && dir.is_dir() => vec![dir.join("next_code"), temp],
        _ => vec![temp],
    }
}

/// A socket name no other server of any agent uses: `<pid>-<n>`.
pub(crate) fn unique_name() -> String {
    static SERVERS: AtomicU64 = AtomicU64::new(1);
    format!(
        "{}-{}",
        std::process::id(),
        SERVERS.fetch_add(1, Ordering::Relaxed)
    )
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// Creates `dir` (mode 0700), or checks that an existing one is a real
/// directory of the user's that nobody else may enter: in a shared `/tmp`
/// someone else could have created it first.
fn private_dir(dir: &Path) -> io::Result<()> {
    match fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
    {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => {
            return Err(io::Error::new(
                err.kind(),
                format!("{}: {err}", dir.display()),
            ));
        }
    }
    check_private(dir)
}

fn check_private(dir: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(dir)
        .map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", dir.display())))?;
    if !meta.is_dir() || meta.uid() != uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is not a directory owned by you", dir.display()),
        ));
    }
    if meta.mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The uid of the process at the other end, where the system tells it.
#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    // SAFETY: `ucred` is plain data; getsockopt writes at most `len` bytes.
    unsafe {
        let mut cred: libc::ucred = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let result = libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        );
        (result == 0).then_some(cred.uid)
    }
}

#[cfg(not(target_os = "linux"))]
fn peer_uid(_: &UnixStream) -> Option<u32> {
    Some(uid())
}

/// An error reply in the control commands' shape.
fn error_reply(message: &str) -> String {
    json!({ "ok": false, "error": message }).to_string()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

type Pending = Arc<Mutex<HashMap<u64, mpsc::Sender<String>>>>;

/// A listening pinpoint socket; dropping it stops listening, removes its
/// files and fails the requests still waiting for the editor.
pub struct Server {
    socket: PathBuf,
    info: PathBuf,
    pending: Pending,
    stopped: Arc<AtomicBool>,
}

impl Server {
    /// Listens in `dir` under `name` for clients working in `roots`;
    /// `forward` sends each request (JSON text) to the editor with its call
    /// number, answered later through [`Server::reply`].
    pub fn start(
        dir: &Path,
        name: &str,
        roots: &[String],
        forward: impl Fn(u64, String) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        private_dir(dir)?;
        let socket = dir.join(format!("{name}.sock"));
        let info = dir.join(format!("{name}.json"));
        // Left behind by a killed agent whose pid came round again.
        fs::remove_file(&socket).ok();
        let listener = UnixListener::bind(&socket)
            .map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", socket.display())))?;
        let description = json!({
            "socket": socket.to_string_lossy(),
            "roots": roots,
            "pid": std::process::id(),
            "started": now_ms(),
        });
        // Written whole then renamed, so a client never reads half of it.
        let partial = dir.join(format!(".{name}.json"));
        fs::write(&partial, description.to_string())
            .and_then(|()| fs::rename(&partial, &info))
            .inspect_err(|_| {
                fs::remove_file(&socket).ok();
            })?;

        let server = Self {
            socket,
            info,
            pending: Pending::default(),
            stopped: Arc::new(AtomicBool::new(false)),
        };
        let (pending, stopped) = (server.pending.clone(), server.stopped.clone());
        let forward = Arc::new(forward);
        thread::Builder::new()
            .name("agent-pinpoint".into())
            .spawn(move || {
                let calls = AtomicU64::new(1);
                for stream in listener.incoming() {
                    if stopped.load(Ordering::Acquire) {
                        return;
                    }
                    let Ok(stream) = stream else { continue };
                    let call = calls.fetch_add(1, Ordering::Relaxed);
                    let (pending, stopped, forward) =
                        (pending.clone(), stopped.clone(), forward.clone());
                    thread::Builder::new()
                        .name("agent-pinpoint-call".into())
                        .spawn(move || serve_call(stream, call, &pending, &stopped, &*forward))
                        .ok();
                }
            })?;
        Ok(server)
    }

    /// Delivers the editor's answer to call `call`.
    pub fn reply(&self, call: u64, reply: String) {
        if let Some(waiting) = self.pending.lock().remove(&call) {
            waiting.send(reply).ok();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        // Wakes the accept loop so it sees `stopped`.
        UnixStream::connect(&self.socket).ok();
        fs::remove_file(&self.socket).ok();
        fs::remove_file(&self.info).ok();
        // Waiting calls answer that the editor is gone.
        self.pending.lock().clear();
    }
}

fn serve_call(
    mut stream: UnixStream,
    call: u64,
    pending: &Pending,
    stopped: &AtomicBool,
    forward: &dyn Fn(u64, String),
) {
    // The directory keeps others out; this also refuses them where root
    // created it for everybody. Root may always connect: it can anyway.
    if !matches!(peer_uid(&stream), Some(peer) if peer == uid() || peer == 0) {
        return;
    }
    let mut line = String::new();
    let read = BufReader::new((&stream).take(MAX_REQUEST)).read_line(&mut line);
    let request = line.trim();
    let reply = if let Err(err) = read {
        error_reply(&format!("cannot read the request: {err}"))
    } else if request.is_empty() {
        error_reply("empty request")
    } else if !line.ends_with('\n') && line.len() as u64 >= MAX_REQUEST {
        error_reply("the request is too long")
    } else {
        let (tx, rx) = mpsc::channel();
        pending.lock().insert(call, tx);
        // Stopped meanwhile: the drop that clears `pending` may have passed.
        if stopped.load(Ordering::Acquire) {
            pending.lock().remove(&call);
        } else {
            forward(call, request.to_owned());
        }
        rx.recv()
            .unwrap_or_else(|_| error_reply("the editor stopped serving pinpoint"))
    };
    stream.write_all(reply.as_bytes()).ok();
    stream.write_all(b"\n").ok();
}

/// A running agent's socket, as its description file tells.
#[derive(Debug, Clone, PartialEq)]
struct Instance {
    socket: PathBuf,
    roots: Vec<String>,
    started: u64,
    /// The description file, removed when the socket is dead.
    info: PathBuf,
}

fn instances(dir: &Path) -> Vec<Instance> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            if name.starts_with('.') || !name.ends_with(".json") {
                return None;
            }
            let value: Value = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
            Some(Instance {
                socket: PathBuf::from(value["socket"].as_str()?),
                roots: value["roots"]
                    .as_array()?
                    .iter()
                    .filter_map(|root| root.as_str().map(str::to_owned))
                    .collect(),
                started: value["started"].as_u64().unwrap_or(0),
                info: path,
            })
        })
        .collect()
}

/// The longest of `roots` containing `cwd` (its length), if any does.
fn containing_root(roots: &[String], cwd: &Path) -> Option<usize> {
    roots
        .iter()
        .filter(|root| cwd.starts_with(root))
        .map(String::len)
        .max()
}

/// Instances in the order to try them: those whose project contains `cwd`
/// (the closest folder first), then the others, newest first.
fn by_preference(mut instances: Vec<Instance>, cwd: &Path) -> Vec<Instance> {
    instances.sort_by_key(|instance| {
        std::cmp::Reverse((containing_root(&instance.roots, cwd), instance.started))
    });
    instances
}

/// Connects to the agent serving `cwd` (see [`by_preference`]), or the one
/// [`SOCKET_ENV`] names; dead sockets are cleaned up on the way.
fn connect(cwd: &Path) -> Result<UnixStream, String> {
    if let Some(socket) = env::var_os(SOCKET_ENV) {
        let socket = PathBuf::from(socket);
        return UnixStream::connect(&socket).map_err(|err| {
            format!(
                "cannot connect to {} ({SOCKET_ENV}): {err}",
                socket.display()
            )
        });
    }
    let dirs = runtime_dirs();
    // Another user's (or a world-writable) directory is not trusted.
    let found = dirs
        .iter()
        .filter(|dir| check_private(dir).is_ok())
        .flat_map(|dir| instances(dir))
        .collect();
    for instance in by_preference(found, cwd) {
        match UnixStream::connect(&instance.socket) {
            Ok(stream) => return Ok(stream),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                ) =>
            {
                fs::remove_file(&instance.socket).ok();
                fs::remove_file(&instance.info).ok();
            }
            Err(_) => {}
        }
    }
    let dirs: Vec<_> = dirs.iter().map(|dir| dir.display().to_string()).collect();
    Err(format!(
        "no NextCode editor serves pinpoint on this machine (looked in {}). Open the \
         project in NextCode (WSL or Dev Container) first, as this user; root reaches \
         another user's editor agent by setting {SOCKET_ENV} to its socket",
        dirs.join(", ")
    ))
}

/// Makes relative `file` targets, and the files of code `ref`s
/// (`src/a.rs:3:5`), that exist below `cwd` absolute: the editor knows the
/// project, not the caller's working directory. Others stay as given
/// (project-relative paths, element paths).
fn absolute_files(value: &mut Value, cwd: &Path) {
    match value {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                match value {
                    Value::String(file) if key == "file" && !file.starts_with('/') => {
                        let path = cwd.join(&*file);
                        if path.exists() {
                            *file = path.to_string_lossy().into_owned();
                        }
                    }
                    Value::String(reference) if key == "ref" && !reference.starts_with('/') => {
                        if let Some(absolute) = absolute_reference(reference, cwd) {
                            *reference = absolute;
                        }
                    }
                    _ => absolute_files(value, cwd),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| absolute_files(item, cwd)),
        _ => {}
    }
}

/// `file:line…` (or `file#cell=n:line…`) with `file` an existing file
/// below `cwd`, made absolute; the shortest such file wins.
fn absolute_reference(reference: &str, cwd: &Path) -> Option<String> {
    reference
        .match_indices([':', '#'])
        .map(|(index, _)| reference.split_at(index))
        .find(|(file, rest)| {
            !file.is_empty()
                && (rest.starts_with(':') || rest.starts_with("#cell="))
                && cwd.join(file).is_file()
        })
        .map(|(file, rest)| format!("{}{rest}", cwd.join(file).to_string_lossy()))
}

/// Sends one request to the agent serving `cwd` (see [`by_preference`])
/// and returns the reply.
pub fn call(request: &Value, cwd: &Path) -> Result<Value, String> {
    let mut stream = connect(cwd)?;
    // As long as the editor may take (`wait_pin`'s timeout), and a margin.
    let wait = request["timeout"].as_f64().unwrap_or(0.).clamp(0., 3600.);
    stream
        .set_read_timeout(Some(Duration::from_secs_f64(wait + 30.)))
        .ok();
    let mut line = request.to_string();
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|err| format!("cannot send the request: {err}"))?;
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .map_err(|err| format!("no reply from the editor: {err}"))?;
    if reply.trim().is_empty() {
        return Err("the agent closed the connection without a reply".into());
    }
    serde_json::from_str(&reply).map_err(|err| format!("the reply is not JSON: {err}"))
}

/// `next_code_agent pinpoint <args>`: the same commands and output as
/// `next_code pinpoint`, served by the editor that opened this machine's
/// project.
pub fn cli(args: &[String]) -> ExitCode {
    let usage = pinpoint_cli::USAGE.replacen("next_code pinpoint", "next_code_agent pinpoint", 1);
    let mut request = match pinpoint_cli::parse(args) {
        Ok(request) => request,
        Err(err) => {
            eprintln!("{err}\n\n{usage}");
            return ExitCode::from(2);
        }
    };
    if request["cmd"] == "help" {
        println!("{usage}");
        return ExitCode::SUCCESS;
    }
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    absolute_files(&mut request, &cwd);
    let reply = call(&request, &cwd).unwrap_or_else(|err| json!({ "ok": false, "error": err }));
    ExitCode::from(pinpoint_cli::print_reply(&request, &reply).clamp(0, 255) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("next_code_pinpoint_{name}_{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        dir
    }

    fn instance(roots: &[&str], started: u64) -> Instance {
        Instance {
            socket: PathBuf::from(format!("/s/{started}.sock")),
            roots: roots.iter().map(|root| root.to_string()).collect(),
            started,
            info: PathBuf::new(),
        }
    }

    #[test]
    fn the_closest_project_wins_then_the_newest_agent() {
        let instances = vec![
            instance(&["/home/me"], 1),
            instance(&["/srv/other"], 9),
            instance(&["/tmp", "/home/me/proj"], 2),
            instance(&["/home/me/proj"], 3),
        ];
        let order = |cwd: &str| -> Vec<u64> {
            by_preference(instances.clone(), Path::new(cwd))
                .iter()
                .map(|instance| instance.started)
                .collect()
        };
        assert_eq!(order("/home/me/proj/src"), [3, 2, 1, 9]);
        assert_eq!(order("/home/me/project"), [1, 9, 3, 2]);
        assert_eq!(order("/elsewhere"), [9, 3, 2, 1]);
    }

    #[test]
    fn relative_files_below_the_cwd_become_absolute() {
        let cwd = temp_dir("files");
        fs::create_dir_all(cwd.join("src")).unwrap();
        fs::write(cwd.join("src/main.rs"), "").unwrap();
        let mut request = json!({
            "cmd": "highlight",
            "targets": [
                { "file": "src/main.rs", "line": 2 },
                { "file": "lib/missing.rs", "line": 1 },
                { "file": "/abs/x.rs" },
                { "path": "workspace/file" },
                { "ref": "src/main.rs:3:5-3:9" },
                { "ref": "src/main.rs#cell=2:1" },
                { "ref": "src/other.rs:3" },
                { "ref": "workspace/editor-area/pane:1" },
            ],
        });
        absolute_files(&mut request, &cwd);
        let main = cwd.join("src/main.rs").to_string_lossy().into_owned();
        assert_eq!(
            request["targets"],
            json!([
                { "file": main, "line": 2 },
                { "file": "lib/missing.rs", "line": 1 },
                { "file": "/abs/x.rs" },
                { "path": "workspace/file" },
                { "ref": format!("{main}:3:5-3:9") },
                { "ref": format!("{main}#cell=2:1") },
                { "ref": "src/other.rs:3" },
                { "ref": "workspace/editor-area/pane:1" },
            ])
        );
        fs::remove_dir_all(&cwd).ok();
    }

    #[test]
    fn requests_are_forwarded_and_answered_over_the_socket() {
        let dir = temp_dir("socket");
        let server: Arc<Mutex<Option<Server>>> = Arc::default();
        let echo = server.clone();
        let started = Server::start(&dir, "1-1", &["/proj".into()], move |call, request| {
            // The editor answers later, from another thread.
            let echo = echo.clone();
            thread::spawn(move || {
                let request: Value = serde_json::from_str(&request).unwrap();
                let reply = json!({ "ok": true, "call": call, "echo": request });
                echo.lock().as_ref().unwrap().reply(call, reply.to_string());
            });
        })
        .unwrap();
        *server.lock() = Some(started);
        let mode = fs::metadata(&dir).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700);

        let found = instances(&dir);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].roots, ["/proj"]);

        let mut stream = UnixStream::connect(&found[0].socket).unwrap();
        stream.write_all(b"{\"command\":\"list\"}\n").unwrap();
        let mut reply = String::new();
        BufReader::new(&stream).read_line(&mut reply).unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["echo"], json!({ "command": "list" }));
        assert_eq!(reply["ok"], json!(true));

        // A call still waiting when the editor stops fails instead of hanging.
        let waiting = Arc::new(Mutex::new(Vec::new()));
        let quiet = Server::start(&dir, "1-2", &[], {
            let waiting = waiting.clone();
            move |call, _| waiting.lock().push(call)
        })
        .unwrap();
        let mut stream = UnixStream::connect(dir.join("1-2.sock")).unwrap();
        stream.write_all(b"{\"command\":\"wait_pin\"}\n").unwrap();
        while waiting.lock().is_empty() {
            thread::sleep(std::time::Duration::from_millis(5));
        }
        drop(quiet);
        let mut reply = String::new();
        BufReader::new(&stream).read_line(&mut reply).unwrap();
        assert!(reply.contains("stopped serving"), "{reply}");
        assert!(!dir.join("1-2.sock").exists() && !dir.join("1-2.json").exists());

        server.lock().take();
        assert!(instances(&dir).is_empty());
        fs::remove_dir_all(&dir).ok();
    }
}
