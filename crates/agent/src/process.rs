//! Processes started for the client: piped (language servers) or in a
//! pseudo-terminal (shells).

use std::{
    io::{self, Read, Write},
    process::{Child, Command, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use proto::{Event, PtySize, SpawnRequest};

use crate::EventSink;

/// How long a cancelled process may take to exit before it is killed.
pub(crate) const GRACE: Duration = Duration::from_secs(2);

pub(crate) trait Process: Send + Sync {
    fn write(&self, data: Vec<u8>);
    /// Ends the input: a piped process then reads end of file.
    fn close_input(&self);
    fn resize(&self, size: PtySize);
    /// Asks the process to end (closes its input), killing it after [`GRACE`].
    fn terminate(&self);
    fn kill(&self);
}

pub(crate) fn spawn(request: SpawnRequest, sink: EventSink) -> io::Result<Arc<dyn Process>> {
    match request.pty {
        #[cfg(target_os = "linux")]
        Some(size) => crate::pty::spawn(request, size, sink),
        #[cfg(not(target_os = "linux"))]
        Some(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "terminals are only supported on Linux",
        )),
        None => spawn_piped(request, sink).map(|process| process as Arc<dyn Process>),
    }
}

/// Forwards written chunks to `writer` on a thread until the sender drops.
pub(crate) fn input_thread(
    mut writer: impl Write + Send + 'static,
) -> io::Result<mpsc::Sender<Vec<u8>>> {
    let (input, chunks) = mpsc::channel::<Vec<u8>>();
    thread::Builder::new()
        .name("agent-stdin".into())
        .spawn(move || {
            for chunk in chunks {
                if writer
                    .write_all(&chunk)
                    .and_then(|()| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
        })?;
    Ok(input)
}

struct Piped {
    input: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    child: Arc<Mutex<Child>>,
}

fn spawn_piped(request: SpawnRequest, sink: EventSink) -> io::Result<Arc<Piped>> {
    let program = request.program.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "a program is required without a terminal",
        )
    })?;
    let mut command = Command::new(&program);
    command
        .args(&request.args)
        .current_dir(&request.cwd)
        .envs(request.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group, so ending it also ends what it started (e.g.
    // rustup's proxy and the real rust-analyzer behind it).
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command
        .spawn()
        .map_err(|err| io::Error::new(err.kind(), format!("cannot start {program}: {err}")))?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let child = Arc::new(Mutex::new(child));
    let process = Arc::new(Piped {
        input: Mutex::new(Some(input_thread(stdin)?)),
        child: child.clone(),
    });

    let sink = Arc::new(sink);
    let out = forward(stdout, sink.clone(), Event::Stdout)?;
    let err = forward(stderr, sink.clone(), Event::Stderr)?;
    thread::Builder::new()
        .name("agent-wait".into())
        .spawn(move || {
            out.join().ok();
            err.join().ok();
            // Output closed: the process is exiting; poll so `kill` never
            // waits behind a blocking `wait`.
            let code = loop {
                match child.lock().try_wait() {
                    Ok(Some(status)) => break status.code(),
                    Ok(None) => {}
                    Err(_) => break None,
                }
                thread::sleep(Duration::from_millis(20));
            };
            sink.finish(Event::Exited { code });
        })?;
    Ok(process)
}

fn forward(
    mut reader: impl Read + Send + 'static,
    sink: Arc<EventSink>,
    event: fn(Vec<u8>) -> Event,
) -> io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("agent-output".into())
        .spawn(move || {
            let mut buffer = vec![0; 64 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => sink.send(event(buffer[..n].to_vec())),
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        })
}

impl Process for Piped {
    fn write(&self, data: Vec<u8>) {
        if let Some(input) = &*self.input.lock() {
            input.send(data).ok();
        }
    }

    fn close_input(&self) {
        self.input.lock().take();
    }

    fn resize(&self, _: PtySize) {}

    fn terminate(&self) {
        self.input.lock().take();
        let child = self.child.clone();
        thread::spawn(move || {
            let deadline = Instant::now() + GRACE;
            while Instant::now() < deadline {
                if !matches!(child.lock().try_wait(), Ok(None)) {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
            kill_group(&mut child.lock());
        });
    }

    fn kill(&self) {
        self.input.lock().take();
        kill_group(&mut self.child.lock());
    }
}

/// Kills a running child and its process group. The caller holds the child's
/// lock, so it cannot be reaped (and its id reused) meanwhile.
fn kill_group(child: &mut Child) {
    if !matches!(child.try_wait(), Ok(None)) {
        return;
    }
    #[cfg(unix)]
    // SAFETY: plain kill(2); the group leader is alive (checked above).
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    child.kill().ok();
}
