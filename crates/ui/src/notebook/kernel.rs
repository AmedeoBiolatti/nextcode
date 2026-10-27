//! The kernel bridge process: `assets/bridge.py` (see `notebook::protocol`)
//! run with the notebook's interpreter where the project lives, through
//! `Project::spawn_piped` (locally, or inside WSL/a container through the
//! agent). Dropping the [`Kernel`] shuts the kernel down.
//!
//! No orphans: the bridge shuts its kernel down when asked to, when its
//! stdin ends (our process went away) and on errors; when the bridge itself
//! is killed, ipykernel's parent poller (`JPY_PARENT_PID`) ends the kernel
//! within about a second.

use std::{
    collections::HashMap,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use futures::channel::{
    mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
    oneshot,
};
use notebook::protocol::{self, Event, Message, Op, Request};
use parking_lot::Mutex;
use serde_json::Value;

use crate::project::Project;

/// How much of the bridge's stderr is kept for the `Dead` message.
const STDERR_TAIL: usize = 4096;
/// How long a dropped kernel's bridge may take to exit before it is killed.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// How to start a notebook's kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelLaunch {
    /// Command line prefix running Python (`[".venv/bin/python"]`, or
    /// `["uv", "run", "python"]`); the bridge's arguments follow.
    pub python: Vec<String>,
    /// A kernelspec name; `None`: an ipykernel in that interpreter.
    pub kernel_name: Option<String>,
    /// The kernel's working directory (the notebook's folder).
    pub cwd: PathBuf,
}

impl KernelLaunch {
    /// What the toolbar shows: the interpreter (and kernelspec).
    pub fn label(&self) -> String {
        let interpreter = if self.is_uv() {
            "uv run python".to_owned()
        } else {
            self.python
                .first()
                .map(|python| shorten_interpreter(python, &self.cwd))
                .unwrap_or_else(|| "python".to_owned())
        };
        match &self.kernel_name {
            Some(name) => format!("{interpreter} · {name}"),
            None => interpreter,
        }
    }

    /// Whether Python runs through `uv run`.
    fn is_uv(&self) -> bool {
        self.python.get(1).is_some_and(|arg| arg == "run")
            && self.python.first().is_some_and(|program| {
                Path::new(program.rsplit(['/', '\\']).next().unwrap_or(program))
                    .file_stem()
                    .is_some_and(|stem| stem == "uv")
            })
    }
}

/// `<dir>/<venv>/{bin,Scripts}/<exe>` relative to `cwd` or one of its
/// ancestors (a virtualenv of the notebook's project), else `python` as is.
fn shorten_interpreter(python: &str, cwd: &Path) -> String {
    let path = Path::new(python);
    let separator = if python.contains('\\') { "\\" } else { "/" };
    for dir in cwd.ancestors() {
        if dir.parent().is_none() {
            break;
        }
        let Ok(rest) = path.strip_prefix(dir) else {
            continue;
        };
        let parts: Vec<_> = rest
            .components()
            .filter_map(|component| match component {
                Component::Normal(part) => Some(part.to_string_lossy()),
                _ => None,
            })
            .collect();
        if parts.len() == 3 && (parts[1] == "bin" || parts[1] == "Scripts") {
            return parts.join(separator);
        }
    }
    python.to_owned()
}

/// `uv` where the project lives when `root` is a uv project (has a
/// `pyproject.toml`).
fn uv_in(project: &Project, root: &Path) -> Option<String> {
    let pyproject = project.join(root, "pyproject.toml");
    if !matches!(project.kind(&pyproject), Ok(Some(_))) {
        return None;
    }
    project
        .find_executable_in("uv", root)
        .ok()
        .flatten()
        .map(|uv| uv.to_string_lossy().into_owned())
}

/// The interpreter of a notebook at `notebook` (blocks): the virtualenv of
/// its project folder (as terminals and the Python panel use), else
/// `uv run python` in a uv project when `uv` is there, else
/// `python3`/`python`.
pub fn default_launch(project: &Project, notebook: &Path) -> KernelLaunch {
    let root = project.root_for(notebook).path.clone();
    let cwd = notebook
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map_or_else(|| root.clone(), Path::to_path_buf);
    let python = if let Ok(Some(python)) = project.python_interpreter_in(&root) {
        vec![python.to_string_lossy().into_owned()]
    } else if let Some(uv) = uv_in(project, &root) {
        vec![
            uv,
            "run".into(),
            "--project".into(),
            root.to_string_lossy().into_owned(),
            "python".into(),
        ]
    } else if project.is_remote() || !cfg!(windows) {
        vec!["python3".into()]
    } else {
        vec!["python".into()]
    };
    KernelLaunch {
        python,
        kernel_name: None,
        cwd,
    }
}

/// The command that installs ipykernel for `launch` (program, args): `uv
/// add --dev ipykernel` in a uv project, else `python -m pip install
/// ipykernel`.
pub fn install_command(
    project: &Project,
    notebook: &Path,
    launch: &KernelLaunch,
) -> (String, Vec<String>) {
    let root = project.root_for(notebook).path.clone();
    let uv = if launch.is_uv() {
        Some(launch.python[0].clone())
    } else {
        uv_in(project, &root)
    };
    if let Some(uv) = uv {
        let args = [
            "add",
            "--project",
            &root.to_string_lossy(),
            "--dev",
            "ipykernel",
        ];
        return (uv, args.map(str::to_owned).to_vec());
    }
    let (program, prefix) = match launch.python.split_first() {
        Some((program, prefix)) => (program.clone(), prefix.to_vec()),
        None => ("python".to_owned(), Vec::new()),
    };
    let mut args = prefix;
    args.extend(["-m", "pip", "install", "ipykernel"].map(str::to_owned));
    (program, args)
}

type Reply = oneshot::Sender<Result<Value, String>>;

#[derive(Default)]
struct Pending {
    next_id: u64,
    replies: HashMap<u64, Reply>,
    /// Why the bridge is gone (requests fail with it).
    closed: Option<String>,
}

struct Shared {
    stdin: Mutex<Option<Box<dyn Write + Send>>>,
    pending: Mutex<Pending>,
    /// A shutdown was requested: the bridge's end is no `Event::Dead`.
    stopping: AtomicBool,
}

/// A running bridge.
pub struct Kernel {
    shared: Arc<Shared>,
    /// Kills the process when dropped.
    guard: Mutex<Option<Box<dyn Send>>>,
    /// Disconnects when the reader thread saw the bridge's end.
    exited: Mutex<Option<mpsc::Receiver<()>>>,
}

impl Kernel {
    /// Starts the bridge where `project` lives (blocks; call it off the UI
    /// thread). Events arrive on the receiver, which ends with the bridge
    /// (an unexpected end first sends `Event::Dead` with its stderr tail).
    /// The kernel itself starts with `Op::Start`.
    pub fn spawn(
        project: &Project,
        launch: &KernelLaunch,
    ) -> io::Result<(Kernel, UnboundedReceiver<Event>)> {
        let Some((program, prefix)) = launch.python.split_first() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no Python interpreter to run the kernel bridge",
            ));
        };
        let mut args = prefix.to_vec();
        args.extend(["-u", "-c", protocol::BOOTSTRAP].map(str::to_owned));
        let env = [
            ("PYTHONUNBUFFERED", "1"),
            ("PYDEVD_DISABLE_FILE_VALIDATION", "1"),
            // Requests are UTF-8 whatever the locale (Windows code pages).
            ("PYTHONIOENCODING", "utf-8"),
        ]
        .map(|(key, value)| (key.to_owned(), value.to_owned()));
        let tail = Arc::new(Mutex::new(Vec::new()));
        let sink = tail.clone();
        let process = project.spawn_piped(program, &args, &env, move |bytes| {
            let mut tail = sink.lock();
            tail.extend_from_slice(bytes);
            let excess = tail.len().saturating_sub(STDERR_TAIL);
            tail.drain(..excess);
        })?;
        Self::start(process.stdin, process.stdout, process.guard, Some(tail))
    }

    /// A bridge over given pipes (tests' fake and real bridges): writes the
    /// bootstrap line, then reads lines on a thread. `guard` is dropped
    /// (killing the process) once the bridge was asked to stop.
    #[cfg(test)]
    pub fn from_pipes(
        stdin: Box<dyn Write + Send>,
        stdout: Box<dyn Read + Send>,
        guard: Box<dyn Send>,
    ) -> io::Result<(Kernel, UnboundedReceiver<Event>)> {
        Self::start(stdin, stdout, guard, None)
    }

    fn start(
        mut stdin: Box<dyn Write + Send>,
        stdout: Box<dyn Read + Send>,
        guard: Box<dyn Send>,
        stderr_tail: Option<Arc<Mutex<Vec<u8>>>>,
    ) -> io::Result<(Kernel, UnboundedReceiver<Event>)> {
        stdin.write_all(protocol::bootstrap_line().as_bytes())?;
        stdin.flush()?;
        let shared = Arc::new(Shared {
            stdin: Mutex::new(Some(stdin)),
            pending: Mutex::new(Pending {
                next_id: 1,
                ..Pending::default()
            }),
            stopping: AtomicBool::new(false),
        });
        let (events, receiver) = unbounded();
        let (exited_tx, exited) = mpsc::channel();
        let reader = shared.clone();
        thread::Builder::new()
            .name("notebook-bridge".into())
            .spawn(move || {
                read_bridge(&reader, stdout, &events, stderr_tail);
                drop(exited_tx);
            })?;
        let kernel = Kernel {
            shared,
            guard: Mutex::new(Some(guard)),
            exited: Mutex::new(Some(exited)),
        };
        Ok((kernel, receiver))
    }

    /// Sends a request; returns its id (outputs of an `execute` carry it as
    /// `request`) and its reply (`Err` when the bridge ended first).
    pub fn request(&self, op: Op) -> (u64, oneshot::Receiver<Result<Value, String>>) {
        let (reply, receiver) = oneshot::channel();
        let id = {
            let mut pending = self.shared.pending.lock();
            let id = pending.next_id;
            pending.next_id += 1;
            if let Some(error) = &pending.closed {
                reply.send(Err(error.clone())).ok();
                return (id, receiver);
            }
            pending.replies.insert(id, reply);
            id
        };
        if matches!(op, Op::Shutdown) {
            self.shared.stopping.store(true, Ordering::SeqCst);
        }
        let line = Request { id, op }.to_line();
        let written = match &mut *self.shared.stdin.lock() {
            Some(stdin) => stdin
                .write_all(line.as_bytes())
                .and_then(|()| stdin.flush()),
            None => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the kernel bridge is closed",
            )),
        };
        if let Err(err) = written
            && let Some(reply) = self.shared.pending.lock().replies.remove(&id)
        {
            reply
                .send(Err(format!("cannot reach the kernel bridge: {err}")))
                .ok();
        }
        (id, receiver)
    }

    /// Shuts the kernel down and waits (up to `timeout`) until the bridge
    /// exited (it does after its kernel), then kills the bridge if needed.
    /// For exits of our own process, where `Drop`'s background wait would
    /// not finish.
    pub fn shutdown_blocking(self, timeout: Duration) {
        self.stop();
        if let Some(exited) = self.exited.lock().take() {
            exited.recv_timeout(timeout).ok();
        }
        drop(self.guard.lock().take());
    }

    /// Asks the bridge to stop: `Op::Shutdown` and the end of its stdin.
    fn stop(&self) {
        let closed = self.shared.pending.lock().closed.is_some();
        self.shared.stopping.store(true, Ordering::SeqCst);
        if !closed {
            drop(self.request(Op::Shutdown));
        }
        drop(self.shared.stdin.lock().take());
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        let guard = self.guard.lock().take();
        let exited = self.exited.lock().take();
        if guard.is_none() && exited.is_none() {
            return;
        }
        self.stop();
        thread::Builder::new()
            .name("notebook-bridge-exit".into())
            .spawn(move || {
                if let Some(exited) = exited {
                    exited.recv_timeout(EXIT_GRACE).ok();
                }
                drop(guard);
            })
            .ok();
    }
}

/// Reads the bridge's lines until its end: replies resolve their requests,
/// events go to `events`; then pending requests fail and, unless a shutdown
/// was requested, `Event::Dead` reports the end.
fn read_bridge(
    shared: &Shared,
    stdout: Box<dyn Read + Send>,
    events: &UnboundedSender<Event>,
    stderr_tail: Option<Arc<Mutex<Vec<u8>>>>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        match protocol::parse_line(text) {
            Ok(Message::Reply { id, result }) => {
                if let Some(reply) = shared.pending.lock().replies.remove(&id) {
                    reply.send(result).ok();
                }
            }
            Ok(Message::Event(event)) => {
                events.unbounded_send(event).ok();
            }
            Err(error) => {
                let message = if error.contains(text) {
                    error
                } else {
                    format!("{error}: {text}")
                };
                events.unbounded_send(Event::Log { message }).ok();
            }
        }
    }
    let stopping = shared.stopping.load(Ordering::SeqCst);
    let tail = stderr_tail.map(|tail| {
        if !stopping {
            // The stderr thread may still be delivering the last words.
            thread::sleep(Duration::from_millis(100));
        }
        String::from_utf8_lossy(&tail.lock()).trim().to_owned()
    });
    let message = match tail.as_deref() {
        Some(tail) if !tail.is_empty() => format!("kernel bridge exited: {tail}"),
        _ => "kernel bridge exited".to_owned(),
    };
    let replies = {
        let mut pending = shared.pending.lock();
        pending.closed = Some(message.clone());
        std::mem::take(&mut pending.replies)
    };
    for reply in replies.into_values() {
        reply.send(Err(message.clone())).ok();
    }
    if !stopping {
        events.unbounded_send(Event::Dead { message }).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notebook::protocol::KernelState;
    use serde_json::json;
    use std::{
        io::PipeReader,
        process::{Child, Command, Stdio},
        time::Instant,
    };

    const TIMEOUT: Duration = Duration::from_secs(10);

    fn next_event(events: &mut UnboundedReceiver<Event>, timeout: Duration) -> Option<Event> {
        let deadline = Instant::now() + timeout;
        loop {
            match events.try_recv() {
                Ok(event) => return Some(event),
                Err(err) if err.is_closed() => return None,
                Err(_) if Instant::now() >= deadline => panic!("no bridge event in {timeout:?}"),
                Err(_) => thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    fn wait_reply(
        mut reply: oneshot::Receiver<Result<Value, String>>,
        timeout: Duration,
    ) -> Result<Value, String> {
        let deadline = Instant::now() + timeout;
        loop {
            match reply.try_recv() {
                Ok(Some(result)) => return result,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                Ok(None) => panic!("no reply in {timeout:?}"),
                Err(_) => panic!("reply dropped"),
            }
        }
    }

    /// The other end of a [`Kernel::from_pipes`]: reads request lines,
    /// writes bridge lines.
    struct FakeBridge {
        requests: BufReader<PipeReader>,
        lines: Option<io::PipeWriter>,
    }

    impl FakeBridge {
        fn read_line(&mut self) -> String {
            let mut line = String::new();
            self.requests.read_line(&mut line).unwrap();
            line
        }

        fn request(&mut self) -> Value {
            serde_json::from_str(&self.read_line()).unwrap()
        }

        fn send(&mut self, value: Value) {
            let lines = self.lines.as_mut().unwrap();
            writeln!(lines, "{value}").unwrap();
        }
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn fake() -> (
        Kernel,
        UnboundedReceiver<Event>,
        FakeBridge,
        Arc<AtomicBool>,
    ) {
        let (request_reader, request_writer) = io::pipe().unwrap();
        let (line_reader, line_writer) = io::pipe().unwrap();
        let killed = Arc::new(AtomicBool::new(false));
        // The bootstrap line is larger than a pipe's buffer on some systems.
        let bootstrap = thread::spawn(move || {
            let mut requests = BufReader::new(request_reader);
            let mut line = String::new();
            requests.read_line(&mut line).unwrap();
            (requests, line)
        });
        let (kernel, events) = Kernel::from_pipes(
            Box::new(request_writer),
            Box::new(line_reader),
            Box::new(DropFlag(killed.clone())),
        )
        .unwrap();
        let (requests, first) = bootstrap.join().unwrap();
        assert_eq!(first, protocol::bootstrap_line());
        let bridge = FakeBridge {
            requests,
            lines: Some(line_writer),
        };
        (kernel, events, bridge, killed)
    }

    #[test]
    fn requests_are_lines_and_replies_match_out_of_order() {
        let (kernel, mut events, mut bridge, _killed) = fake();
        let (first, first_reply) = kernel.request(Op::Execute {
            code: "1 + 1".into(),
            marimo: None,
        });
        let (second, second_reply) = kernel.request(Op::Complete {
            code: "import os; os.pa".into(),
            cursor_pos: 16,
        });
        assert_ne!(first, second);
        assert_eq!(
            bridge.request(),
            json!({"id": first, "op": "execute", "code": "1 + 1"})
        );
        assert_eq!(
            bridge.request(),
            json!({"id": second, "op": "complete", "code": "import os; os.pa", "cursor_pos": 16})
        );
        bridge.send(json!({"event": "status", "state": "busy", "request": first}));
        bridge.send(json!({"reply": second, "ok": true, "result": {"matches": ["path"]}}));
        bridge.send(json!({
            "event": "output", "request": first, "msg_type": "execute_result",
            "content": {"data": {"text/plain": "2"}}
        }));
        bridge.send(json!({"reply": first, "ok": false, "error": "no kernel"}));
        assert_eq!(
            wait_reply(second_reply, TIMEOUT),
            Ok(json!({"matches": ["path"]}))
        );
        assert_eq!(wait_reply(first_reply, TIMEOUT), Err("no kernel".into()));
        assert_eq!(
            next_event(&mut events, TIMEOUT),
            Some(Event::Status {
                state: KernelState::Busy,
                request: Some(first)
            })
        );
        assert_eq!(
            next_event(&mut events, TIMEOUT),
            Some(Event::Output {
                request: Some(first),
                parent: None,
                msg_type: "execute_result".into(),
                content: json!({"data": {"text/plain": "2"}}),
            })
        );
    }

    #[test]
    fn the_bridge_ending_fails_pending_requests_and_reports_dead() {
        let (kernel, mut events, mut bridge, _killed) = fake();
        let (_, reply) = kernel.request(Op::Interrupt);
        assert_eq!(bridge.request()["op"], "interrupt");
        bridge.send(json!({"event": "hello", "python": "/x/python", "version": "3.12.1"}));
        bridge.lines = None;
        let error = wait_reply(reply, TIMEOUT).unwrap_err();
        assert!(error.starts_with("kernel bridge exited"), "{error}");
        assert!(matches!(
            next_event(&mut events, TIMEOUT),
            Some(Event::Hello { missing, .. }) if missing.is_empty()
        ));
        assert!(matches!(
            next_event(&mut events, TIMEOUT),
            Some(Event::Dead { message }) if message.starts_with("kernel bridge exited")
        ));
        assert_eq!(next_event(&mut events, TIMEOUT), None);
        // Later requests fail at once.
        let (_, reply) = kernel.request(Op::Restart);
        assert!(wait_reply(reply, TIMEOUT).is_err());
    }

    #[test]
    fn dropping_shuts_the_bridge_down_then_drops_the_guard() {
        let (kernel, mut events, mut bridge, killed) = fake();
        drop(kernel);
        let shutdown = bridge.request();
        assert_eq!(shutdown["op"], "shutdown");
        assert!(!killed.load(Ordering::SeqCst));
        // The bridge's stdin ends after the shutdown request.
        assert_eq!(bridge.read_line(), "");
        bridge.send(json!({"reply": shutdown["id"], "ok": true, "result": null}));
        bridge.lines = None;
        // No `Dead` for a requested shutdown.
        assert_eq!(next_event(&mut events, TIMEOUT), None);
        let deadline = Instant::now() + TIMEOUT;
        while !killed.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "the guard was not dropped");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn invalid_lines_become_log_events() {
        let (_kernel, mut events, mut bridge, _killed) = fake();
        bridge
            .lines
            .as_mut()
            .unwrap()
            .write_all(b"Traceback: not json\n")
            .unwrap();
        assert!(matches!(
            next_event(&mut events, TIMEOUT),
            Some(Event::Log { message }) if message.contains("not json")
        ));
    }

    #[test]
    fn labels_shorten_project_virtualenvs() {
        let launch = |python: &[&str], kernel_name: Option<&str>| KernelLaunch {
            python: python.iter().map(|part| part.to_string()).collect(),
            kernel_name: kernel_name.map(str::to_owned),
            cwd: PathBuf::from("/home/me/proj/notebooks"),
        };
        assert_eq!(
            launch(&["/home/me/proj/.venv/bin/python"], None).label(),
            ".venv/bin/python"
        );
        assert_eq!(
            launch(&["/home/me/proj/notebooks/venv/bin/python3"], Some("py312")).label(),
            "venv/bin/python3 · py312"
        );
        assert_eq!(
            launch(&["/usr/bin/python3"], None).label(),
            "/usr/bin/python3"
        );
        assert_eq!(
            launch(
                &["/usr/bin/uv", "run", "--project", "/home/me/proj", "python"],
                None
            )
            .label(),
            "uv run python"
        );
        assert_eq!(launch(&["python3"], None).label(), "python3");
    }

    #[test]
    fn install_uses_uv_in_uv_projects_else_pip() {
        let root = std::env::temp_dir().join(format!("nc-nbk-install-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let project = Project::local(root.clone());
        let notebook = root.join("a.ipynb");
        let launch = KernelLaunch {
            python: vec!["/opt/py/bin/python".into(), "-X".into(), "utf8".into()],
            kernel_name: None,
            cwd: root.clone(),
        };
        assert_eq!(
            install_command(&project, &notebook, &launch),
            (
                "/opt/py/bin/python".into(),
                ["-X", "utf8", "-m", "pip", "install", "ipykernel"]
                    .map(str::to_owned)
                    .to_vec()
            )
        );
        let uv = KernelLaunch {
            python: ["uv", "run", "--project", &root.to_string_lossy(), "python"]
                .map(str::to_owned)
                .to_vec(),
            ..launch
        };
        let (program, args) = install_command(&project, &notebook, &uv);
        assert_eq!(program, "uv");
        assert_eq!(
            args,
            [
                "add",
                "--project",
                &root.to_string_lossy(),
                "--dev",
                "ipykernel"
            ]
            .map(str::to_owned)
            .to_vec()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    // Real bridge: `NEXT_CODE_NOTEBOOK_PYTHON` names an interpreter with
    // ipykernel (marimo and matplotlib for their parts).

    fn python() -> Option<String> {
        let python = std::env::var("NEXT_CODE_NOTEBOOK_PYTHON").ok();
        if python.is_none() {
            println!(
                "skipped: set NEXT_CODE_NOTEBOOK_PYTHON to an interpreter with ipykernel to run the real bridge"
            );
        }
        python
    }

    struct ChildGuard(Arc<Mutex<Child>>);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let mut child = self.0.lock();
            child.kill().ok();
            child.wait().ok();
        }
    }

    struct Real {
        kernel: Kernel,
        events: UnboundedReceiver<Event>,
        /// Killed directly by the Linux orphan test.
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        child: Arc<Mutex<Child>>,
        cwd: PathBuf,
    }

    impl Real {
        fn spawn(python: &str, name: &str) -> Real {
            let cwd = std::env::temp_dir().join(format!("nc-nbk-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&cwd).unwrap();
            let mut child = Command::new(python)
                .args(["-u", "-c", protocol::BOOTSTRAP])
                .env("PYTHONUNBUFFERED", "1")
                .env("PYDEVD_DISABLE_FILE_VALIDATION", "1")
                .env("PYTHONIOENCODING", "utf-8")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let stdin = child.stdin.take().unwrap();
            let stdout = child.stdout.take().unwrap();
            let child = Arc::new(Mutex::new(child));
            let (kernel, mut events) = Kernel::from_pipes(
                Box::new(stdin),
                Box::new(stdout),
                Box::new(ChildGuard(child.clone())),
            )
            .unwrap();
            match next_event(&mut events, Duration::from_secs(30)) {
                Some(Event::Hello { missing, .. }) => assert!(missing.is_empty(), "{missing:?}"),
                other => panic!("expected hello, got {other:?}"),
            }
            let mut real = Real {
                kernel,
                events,
                child,
                cwd,
            };
            let info = real
                .call(Op::Start {
                    kernel_name: None,
                    cwd: real.cwd.to_string_lossy().into_owned(),
                })
                .unwrap();
            assert_eq!(info["language_info"]["name"], "python");
            real
        }

        fn call(&mut self, op: Op) -> Result<Value, String> {
            let (_, reply) = self.kernel.request(op);
            wait_reply(reply, Duration::from_secs(60))
        }

        /// Runs `code`; its reply and `output` events (msg_type, content).
        fn run(
            &mut self,
            code: &str,
            marimo: Option<protocol::MarimoRun>,
        ) -> (Value, Vec<(String, Value)>) {
            let (id, reply) = self.kernel.request(Op::Execute {
                code: code.into(),
                marimo,
            });
            let reply = wait_reply(reply, Duration::from_secs(60)).unwrap();
            let mut outputs = Vec::new();
            while let Ok(event) = self.events.try_recv() {
                if let Event::Output {
                    request: Some(request),
                    msg_type,
                    content,
                    ..
                } = event
                    && request == id
                {
                    outputs.push((msg_type, content));
                }
            }
            (reply, outputs)
        }

        #[cfg(target_os = "linux")]
        fn kernel_pid(&mut self) -> u32 {
            let (_, outputs) = self.run("import os; os.getpid()", None);
            outputs[0].1["data"]["text/plain"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap()
        }
    }

    fn marimo(cell: &str, defs: &[&str], refs: &[&str]) -> Option<protocol::MarimoRun> {
        Some(protocol::MarimoRun {
            cell: cell.into(),
            defs: defs.iter().map(|name| name.to_string()).collect(),
            refs: refs.iter().map(|name| name.to_string()).collect(),
        })
    }

    fn text_plain(outputs: &[(String, Value)]) -> Vec<String> {
        outputs
            .iter()
            .filter_map(|(_, content)| content["data"]["text/plain"].as_str())
            .map(str::to_owned)
            .collect()
    }

    #[cfg(target_os = "linux")]
    fn wait_gone(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // Gone, or a zombie waiting for its new parent to reap it.
            let gone = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(_) => true,
                Ok(stat) => stat
                    .rsplit_once(')')
                    .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
            };
            if gone {
                return;
            }
            assert!(Instant::now() < deadline, "kernel {pid} still running");
            thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn real_bridge_runs_cells() {
        let Some(python) = python() else { return };
        let mut real = Real::spawn(&python, "run");

        let (reply, outputs) = real.run("print('hi')", None);
        assert_eq!(reply["status"], "ok");
        assert_eq!(outputs[0].0, "stream");
        assert_eq!(outputs[0].1["text"], "hi\n");

        let (reply, outputs) = real.run("1+1", None);
        assert_eq!(reply["status"], "ok");
        assert_eq!(outputs[0].0, "execute_result");
        assert_eq!(text_plain(&outputs), ["2"]);

        let (_, outputs) = real.run(
            "import matplotlib.pyplot as plt\nplt.plot([1, 2, 3])\nplt.show()",
            None,
        );
        assert!(
            outputs
                .iter()
                .any(|(msg_type, content)| msg_type == "display_data"
                    && content["data"]["image/png"].is_string()),
            "{outputs:?}"
        );

        let (reply, outputs) = real.run("1/0", None);
        assert_eq!(reply["status"], "error");
        let (msg_type, content) = &outputs[0];
        assert_eq!(msg_type, "error");
        assert_eq!(content["ename"], "ZeroDivisionError");
        let traceback = content["traceback"].to_string();
        assert!(traceback.contains("\\u001b["), "{traceback}");

        let (_, completion) = real.kernel.request(Op::Complete {
            code: "import os; os.pa".into(),
            cursor_pos: 16,
        });
        let completion = wait_reply(completion, TIMEOUT).unwrap();
        assert!(
            completion["matches"]
                .as_array()
                .unwrap()
                .contains(&json!("path")),
            "{completion}"
        );

        let specs = real.call(Op::KernelSpecs).unwrap();
        assert!(!specs.as_object().unwrap().is_empty(), "{specs}");

        // A long cell is interrupted quickly.
        let (_, reply) = real.kernel.request(Op::Execute {
            code: "import time\ntime.sleep(30)".into(),
            marimo: None,
        });
        thread::sleep(Duration::from_millis(500));
        let started = Instant::now();
        real.call(Op::Interrupt).unwrap();
        let reply = wait_reply(reply, TIMEOUT).unwrap();
        assert_eq!(reply["status"], "error");
        assert!(started.elapsed() < Duration::from_secs(5));
        let interrupted = std::iter::from_fn(|| real.events.try_recv().ok()).any(|event| {
            matches!(event, Event::Output { msg_type, content, .. }
                if msg_type == "error" && content["ename"] == "KeyboardInterrupt")
        });
        assert!(interrupted);

        // Restarting forgets the state.
        real.run("state = 42", None);
        real.call(Op::Restart).unwrap();
        let (_, outputs) = real.run("'state' in globals()", None);
        assert_eq!(text_plain(&outputs), ["False"]);
        std::fs::remove_dir_all(&real.cwd).ok();
    }

    #[test]
    fn real_bridge_runs_marimo_cells() {
        let Some(python) = python() else { return };
        let mut real = Real::spawn(&python, "marimo");
        let (reply, _) = real.run("x = 1\n_p = 2", marimo("a", &["x", "_p"], &[]));
        assert_eq!(reply["status"], "ok");
        let (_, outputs) = real.run("x + 1", marimo("b", &[], &["x"]));
        assert_eq!(text_plain(&outputs), ["2"]);
        let (_, outputs) = real.run("'x' in globals(), '_p' in globals()", None);
        assert_eq!(text_plain(&outputs), ["(True, False)"]);

        // Redefining the cell without `x` deletes it.
        real.run("z = 3", marimo("a", &["z"], &[]));
        let (_, outputs) = real.run("'x' in globals(), z", None);
        assert_eq!(text_plain(&outputs), ["(False, 3)"]);

        // Errors show the cell's code, not the helper.
        let (reply, outputs) = real.run("def f():\n    return 1/0\nf()", marimo("c", &["f"], &[]));
        assert_eq!(reply["status"], "error");
        let traceback = outputs[0].1["traceback"].to_string();
        assert!(traceback.contains("<cell c>:2"), "{traceback}");
        assert!(!traceback.contains("__nc_marimo__"), "{traceback}");

        let (_, outputs) = real.run(
            "import marimo as mo\nmo.md('# hi')",
            marimo("d", &["mo"], &[]),
        );
        assert_eq!(outputs[0].1["data"]["text/markdown"], "# hi", "{outputs:?}");

        // Top-level `await` (marimo writes `async def`) on the kernel's
        // loop: statements, `async for` and the trailing expression.
        let (reply, outputs) = real.run(
            "import asyncio\n\
             async def twice(n):\n    await asyncio.sleep(0.01)\n    return 2 * n\n\
             async def count():\n    for i in range(3):\n        yield i\n\
             late = await twice(z)\n\
             seen = [i async for i in count()]\n\
             _loop = asyncio.get_running_loop()\n\
             await twice(late)",
            marimo("e", &["asyncio", "twice", "count", "late", "seen"], &["z"]),
        );
        assert_eq!(reply["status"], "ok", "{reply} {outputs:?}");
        assert_eq!(text_plain(&outputs), ["12"], "{outputs:?}");
        let (_, outputs) = real.run("late, seen", marimo("f", &[], &["late", "seen"]));
        assert_eq!(text_plain(&outputs), ["(6, [0, 1, 2])"]);
        let (reply, outputs) = real.run(
            "import asyncio\nawait asyncio.sleep(0)\nraise ValueError('async')",
            marimo("g", &["asyncio"], &[]),
        );
        assert_eq!(reply["status"], "error");
        let traceback = outputs[0].1["traceback"].to_string();
        assert!(traceback.contains("<cell g>:3"), "{traceback}");
        assert!(!traceback.contains("__nc_marimo__"), "{traceback}");
        // A word `await` in a string runs on the async path unharmed.
        let (_, outputs) = real.run("'await me'", marimo("h", &[], &[]));
        assert_eq!(text_plain(&outputs), ["'await me'"]);

        // Jupyter cells: IPython's autoawait.
        let (reply, outputs) = real.run("import asyncio\nawait asyncio.sleep(0.01)\n7", None);
        assert_eq!(reply["status"], "ok");
        assert_eq!(text_plain(&outputs), ["7"]);
        std::fs::remove_dir_all(&real.cwd).ok();
    }

    #[test]
    fn real_bridge_describes_and_sets_marimo_ui_elements() {
        let Some(python) = python() else { return };
        let mut real = Real::spawn(&python, "ui");
        let ui = |outputs: &[(String, Value)]| {
            outputs[0].1["data"][notebook::outputs::MARIMO_UI_MIME]["parts"].clone()
        };
        let (_, outputs) = real.run(
            "import marimo as mo\nn = mo.ui.slider(1, 10, value=3, label='n')\nn",
            marimo("a", &["mo", "n"], &[]),
        );
        let parts = ui(&outputs);
        let slider = &parts[0]["element"];
        assert_eq!(slider["kind"], "slider", "{parts}");
        assert_eq!(
            (&slider["label"], &slider["value"]),
            (&json!("n"), &json!(3))
        );
        assert_eq!(
            (&slider["args"]["start"], &slider["args"]["stop"]),
            (&json!(1), &json!(10))
        );
        assert_eq!(text_plain(&outputs), ["[slider n: 3]"]);
        let id = slider["id"].as_str().unwrap().to_owned();

        // Elements inside `mo.md`, between Markdown parts.
        let (_, outputs) = real.run(
            "pick = mo.ui.dropdown(['a', 'b'], value='a')\nmo.md(f'**Pick** {pick} now')",
            marimo("b", &["pick"], &["mo"]),
        );
        let parts = ui(&outputs);
        assert_eq!(parts[0], json!({ "markdown": "**Pick** " }), "{parts}");
        assert_eq!(parts[1]["element"]["kind"], "dropdown");
        assert_eq!(parts[1]["element"]["value"], json!(["a"]));
        assert_eq!(parts[1]["element"]["args"]["options"], json!(["a", "b"]));
        assert_eq!(parts[2], json!({ "markdown": " now" }));

        // Setting a value: the names bound to it come back.
        let reply = real
            .call(Op::MarimoSetUi {
                object_id: id,
                value: json!(7),
            })
            .unwrap();
        assert_eq!(reply["names"], json!(["n"]));
        let (_, outputs) = real.run("n.value * 2", marimo("c", &[], &["n"]));
        assert_eq!(text_plain(&outputs), ["14"]);
        let pick = parts[1]["element"]["id"].as_str().unwrap().to_owned();
        real.call(Op::MarimoSetUi {
            object_id: pick,
            value: json!(["b"]),
        })
        .unwrap();
        let (_, outputs) = real.run("pick.value", marimo("d", &[], &["pick"]));
        assert_eq!(text_plain(&outputs), ["'b'"]);
        let missing = real
            .call(Op::MarimoSetUi {
                object_id: "nope".into(),
                value: json!(1),
            })
            .unwrap_err();
        assert!(missing.contains("LookupError"), "{missing}");
        std::fs::remove_dir_all(&real.cwd).ok();
    }

    /// Feeds the bridge's events to `store` (comms, and outputs Output
    /// widgets capture) until `done(store, events seen)` holds; the events.
    fn pump_widgets(
        real: &mut Real,
        store: &mut notebook::widgets::WidgetStore,
        done: impl Fn(&notebook::widgets::WidgetStore, &[Event]) -> bool,
    ) -> Vec<Event> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut seen = Vec::new();
        while !done(store, &seen) {
            assert!(
                Instant::now() < deadline,
                "widgets did not get there: {seen:?}"
            );
            let Some(event) = next_event(&mut real.events, Duration::from_millis(200)) else {
                continue;
            };
            match &event {
                Event::Comm {
                    msg_type,
                    content,
                    buffers,
                    parent,
                    ..
                } => {
                    store.apply(msg_type, content, buffers, parent.as_deref());
                }
                Event::Output {
                    parent: Some(parent),
                    msg_type,
                    content,
                    ..
                } => {
                    if let Some(model) = store.capturing(parent).map(str::to_owned) {
                        store.capture(&model, msg_type, content);
                    }
                }
                _ => {}
            }
            seen.push(event);
        }
        seen
    }

    fn send_comm(real: &mut Real, message: notebook::widgets::CommMessage) {
        real.call(Op::CommMsg {
            comm_id: message.comm_id,
            data: message.data,
            buffers: message.buffers,
            msg_id: message.msg_id,
        })
        .unwrap();
    }

    /// Words the kernel printed to stdout so far.
    fn printed(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Output {
                    msg_type, content, ..
                } if msg_type == "stream" => content["text"].as_str(),
                _ => None,
            })
            .flat_map(str::split_whitespace)
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn real_bridge_forwards_widget_comms_both_ways() {
        use notebook::widgets::WidgetStore;

        let Some(python) = python() else { return };
        let mut real = Real::spawn(&python, "widgets");
        let (probe, _) = real.run("import ipywidgets", None);
        if probe["status"] != "ok" {
            println!("skipped: ipywidgets is not installed for {python}");
            return;
        }
        let mut store = WidgetStore::new();
        let text = |store: &WidgetStore, id: &str| -> String {
            store.get(id).map_or_else(String::new, |model| {
                model
                    .outputs
                    .iter()
                    .map(notebook::outputs::plain_text)
                    .collect()
            })
        };
        let view = |events: &[Event]| {
            events.iter().find_map(|event| match event {
                Event::Output {
                    msg_type, content, ..
                } if msg_type == "display_data" => {
                    let view = &content["data"][notebook::outputs::WIDGET_MIME];
                    view["model_id"].as_str().map(str::to_owned)
                }
                _ => None,
            })
        };

        // interact: a box of an IntSlider and an Output capturing f's print.
        let _interact = real.kernel.request(Op::Execute {
            code: "import ipywidgets as w\n\
                   def f(x):\n    print('x =', x)\n\
                   ui = w.interact(f, x=5)"
                .into(),
            marimo: None,
        });
        let events = pump_widgets(&mut real, &mut store, |store, events| {
            view(events)
                .and_then(|root| store.get(&root).map(|model| model.references("children")))
                .is_some_and(|children| {
                    children.len() == 2 && text(store, children[1]).contains("x = 5")
                })
        });
        let root = view(&events).unwrap();
        let children: Vec<String> = store
            .get(&root)
            .unwrap()
            .references("children")
            .into_iter()
            .map(str::to_owned)
            .collect();
        let (slider, out) = (children[0].as_str(), children[1].as_str());
        assert_eq!(store.get(slider).unwrap().name, "IntSliderModel");
        assert_eq!(store.get(out).unwrap().name, "OutputModel");

        // The slider moves: the kernel echoes our change and re-runs f
        // into the Output (cleared first).
        let message = store.update(slider, "value", json!(7), true).unwrap();
        let msg_id = message.msg_id.clone();
        send_comm(&mut real, message);
        let events = pump_widgets(&mut real, &mut store, |store, _| {
            text(store, out).contains("x = 7")
        });
        assert_eq!(text(&store, out), "x = 7\n");
        assert_eq!(store.get(slider).unwrap().f64("value"), Some(7.));
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Comm { parent: Some(parent), content, .. }
                if *parent == msg_id && content["data"]["method"] == "echo_update"
        )));

        // A button's click runs its handler, which updates a label.
        let _buttons = real.kernel.request(Op::Execute {
            code: "b = w.Button(description='go')\nl = w.Label('0')\n\
                   def on(_):\n    l.value = str(int(l.value) + 1)\n\
                   b.on_click(on)\n\
                   img = w.Image(value=bytes([137, 80, 78, 71]), format='png')\n\
                   print(b.model_id, l.model_id, img.model_id)"
                .into(),
            marimo: None,
        });
        let events = pump_widgets(&mut real, &mut store, |_, events| {
            printed(events).len() == 3
        });
        let ids = printed(&events);
        let (button, label, image) = (ids[0].as_str(), ids[1].as_str(), ids[2].as_str());
        // Binary state arrives as buffers.
        assert_eq!(store.get(image).unwrap().buffer("value"), Some("iVBORw=="));
        let click = store.custom(button, json!({"event": "click"})).unwrap();
        send_comm(&mut real, click);
        pump_widgets(&mut real, &mut store, |store, _| {
            store
                .get(label)
                .is_some_and(|label| label.str("value") == "1")
        });
        std::fs::remove_dir_all(&real.cwd).ok();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn real_kernels_end_with_their_bridge() {
        let Some(python) = python() else { return };
        let mut real = Real::spawn(&python, "drop");
        let pid = real.kernel_pid();
        drop(real.kernel);
        wait_gone(pid);

        let mut real = Real::spawn(&python, "kill");
        let pid = real.kernel_pid();
        real.child.lock().kill().unwrap();
        wait_gone(pid);
        drop(real.kernel);
    }
}
