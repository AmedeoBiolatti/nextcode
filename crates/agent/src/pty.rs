//! Shells in pseudo-terminals, via `alacritty_terminal`'s pty support.

use std::{
    fs::File,
    io::{self, Read},
    os::fd::AsRawFd,
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use alacritty_terminal::{
    event::{OnResize, WindowSize},
    tty::{self, Pty, Shell},
};
use parking_lot::Mutex;
use proto::{Event, PtySize, SpawnRequest};

use crate::{
    EventSink,
    process::{GRACE, Process, input_thread},
};

struct PtyProcess {
    input: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    /// Taken (and dropped, which reaps the child) once the shell exited. While
    /// it is present the pid cannot have been reused, so signals are safe.
    pty: Arc<Mutex<Option<Pty>>>,
    pid: libc::pid_t,
}

fn window_size(size: PtySize) -> WindowSize {
    WindowSize {
        num_lines: size.rows.max(1),
        num_cols: size.columns.max(2),
        cell_width: 1,
        cell_height: 1,
    }
}

pub(crate) fn spawn(
    request: SpawnRequest,
    size: PtySize,
    sink: EventSink,
) -> io::Result<Arc<dyn Process>> {
    let options = tty::Options {
        shell: request
            .program
            .map(|program| Shell::new(program, request.args)),
        working_directory: Some(PathBuf::from(&request.cwd)),
        drain_on_exit: true,
        env: crate::process_env()
            .into_iter()
            .chain(request.env)
            .collect(),
    };
    let pty = tty::new(&options, window_size(size), 0)?;
    let reader = pty.file().try_clone()?;
    // alacritty's pty is non-blocking for its poll loop; these threads block.
    set_blocking(&reader)?;
    let writer = reader.try_clone()?;
    let pid = pty.child().id() as libc::pid_t;
    let pty = Arc::new(Mutex::new(Some(pty)));
    let process = Arc::new(PtyProcess {
        input: Mutex::new(Some(input_thread(writer)?)),
        pty: pty.clone(),
        pid,
    });

    thread::Builder::new()
        .name("agent-pty".into())
        .spawn(move || {
            pump(reader, &sink);
            let code = wait_exit(pid, true).flatten();
            // Dropping the pty reaps the child.
            let finished = pty.lock().take();
            drop(finished);
            sink.finish(Event::Exited { code });
        })?;
    Ok(process)
}

fn pump(mut reader: File, sink: &EventSink) {
    let mut buffer = vec![0; 64 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return,
            Ok(n) => sink.send(Event::Stdout(buffer[..n].to_vec())),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            // EIO once the shell and everything holding the tty exited.
            Err(_) => return,
        }
    }
}

fn set_blocking(file: &File) -> io::Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: fcntl on a file descriptor we own.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags == -1 || libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Waits for `pid` to exit without reaping it. `None`: still running (only
/// when not blocking); `Some(None)`: exited without a code (signal, error).
fn wait_exit(pid: libc::pid_t, block: bool) -> Option<Option<i32>> {
    let mut flags = libc::WEXITED | libc::WNOWAIT;
    if !block {
        flags |= libc::WNOHANG;
    }
    // SAFETY: waitid fills the zeroed siginfo_t we pass.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        loop {
            if libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags) == 0 {
                break;
            }
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return Some(None);
            }
        }
        if info.si_pid() == 0 {
            return None;
        }
        Some((info.si_code == libc::CLD_EXITED).then(|| info.si_status()))
    }
}

impl PtyProcess {
    /// Signals the shell; `group` also reaches its foreground group members.
    fn signal(&self, signal: libc::c_int, group: bool) {
        let pty = self.pty.lock();
        if pty.is_some() {
            let target = if group { -self.pid } else { self.pid };
            // SAFETY: the child is not reaped while `pty` is held (see field doc).
            unsafe { libc::kill(target, signal) };
        }
    }
}

impl Process for PtyProcess {
    fn write(&self, data: Vec<u8>) {
        if let Some(input) = &*self.input.lock() {
            input.send(data).ok();
        }
    }

    fn close_input(&self) {
        self.input.lock().take();
    }

    fn resize(&self, size: PtySize) {
        if let Some(pty) = &mut *self.pty.lock() {
            pty.on_resize(window_size(size));
        }
    }

    fn terminate(&self) {
        self.input.lock().take();
        self.signal(libc::SIGHUP, false);
        let pty = self.pty.clone();
        let pid = self.pid;
        thread::spawn(move || {
            let deadline = Instant::now() + GRACE;
            while Instant::now() < deadline {
                if wait_exit(pid, false).is_some() {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
            if pty.lock().is_some() {
                // SAFETY: as in `PtyProcess::signal`.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
        });
    }

    fn kill(&self) {
        self.input.lock().take();
        self.signal(libc::SIGKILL, true);
    }
}
