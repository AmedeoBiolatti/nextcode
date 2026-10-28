//! Local child processes of the editor: each started together with
//! everything it goes on to start (a shell and its command, rustup's proxy
//! and the real rust-analyzer), ended together, and none outliving the
//! editor.
//!
//! On Unix every process gets its own process group; ending it signals
//! the group (`SIGTERM`, then `SIGKILL` after [`GRACE`]), and groups still
//! alive when the editor exits normally are killed from an `atexit` hook.
//! On Windows every process is put into one job object created on first
//! use with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so even an aborting editor
//! (release builds run no `Drop` after a panic) takes them with it; each
//! process also gets a nested job of its own, so one tree can be ended
//! without the others.
//!
//! Waiting never polls: one thread per process blocks in `wait`, and
//! deadlines, graces and cancellation wake up through a condition variable.

use std::{
    io,
    process::{Child, Command, ExitStatus},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use parking_lot::{Condvar, Mutex};

/// How long a process asked to end may take before it is killed.
pub const GRACE: Duration = Duration::from_secs(2);

/// Cancels runs made with it ([`crate::project::Project::run_program_cancellable`]):
/// clone it to whoever may cancel. `cancel` ends the processes (and what
/// they started) of the runs in progress, which return `ErrorKind::Interrupted`;
/// runs started afterwards fail at once.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<CancelShared>);

#[derive(Default)]
struct CancelShared {
    cancelled: AtomicBool,
    /// The runs in progress, woken on cancel.
    waiting: Mutex<Vec<Weak<Exit>>>,
}

// `new` and `cancel` are the test explorer's (its cancellable runs come next).
#[allow(dead_code)]
impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    /// Ends the runs in progress; later ones fail at once. Idempotent.
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
        let waiting = std::mem::take(&mut *self.0.waiting.lock());
        for exit in waiting.iter().filter_map(Weak::upgrade) {
            // Under the state lock: a waiter checks the flag under it and
            // waits atomically, so it either sees the flag or is woken.
            let _state = exit.state.lock();
            exit.changed.notify_all();
        }
    }

    fn watch(&self, exit: &Arc<Exit>) {
        let mut waiting = self.0.waiting.lock();
        waiting.retain(|exit| exit.strong_count() > 0);
        waiting.push(Arc::downgrade(exit));
    }
}

/// Whether a process has exited; waiters sleep on `changed`.
#[derive(Default)]
struct Exit {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default, Clone, Copy)]
enum State {
    #[default]
    Running,
    /// `None` when `wait` itself failed (the process is gone anyway).
    Exited(Option<ExitStatus>),
}

impl Exit {
    /// Whether the process exited within `timeout`.
    fn wait_for(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock();
        loop {
            if matches!(*state, State::Exited(_)) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            self.changed.wait_until(&mut state, deadline);
        }
    }
}

/// How waiting for a process ended.
#[derive(Debug, Clone, Copy)]
pub enum Outcome {
    Exited(Option<ExitStatus>),
    TimedOut,
    Cancelled,
}

/// A process just started: take its standard streams, then [`Spawned::watch`]
/// it. Dropped without that, it is killed.
pub struct Spawned {
    pub child: Option<Child>,
    killer: Killer,
}

/// A running process with everything it started.
pub struct Tree {
    killer: Killer,
    exit: Arc<Exit>,
    /// `terminate`/`kill` ran: `Drop` has nothing left to do.
    ended: bool,
}

impl Tree {
    /// Starts `command` in its own process group (Unix) or jobs (Windows).
    pub fn spawn(command: &mut Command) -> io::Result<Spawned> {
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(command, 0);
        let child = command.spawn()?;
        let killer = Killer::adopt(&child);
        Ok(Spawned {
            child: Some(child),
            killer,
        })
    }

    /// Ends every member, including descendants retaining pipes after the leader exits.
    pub(super) fn kill_remaining(&mut self) {
        self.ended = true;
        #[cfg(unix)]
        // SAFETY: signal only the process group created for this tree.
        unsafe {
            libc::kill(-self.killer.pid, libc::SIGKILL);
        }
        #[cfg(windows)]
        if let Some(job) = &self.killer.job {
            job.terminate();
        }
        // If Windows could not make a job, terminate only a still-live leader.
        #[cfg(windows)]
        if self.killer.job.is_none() && matches!(*self.exit.state.lock(), State::Running) {
            self.killer.kill();
        }
    }

    /// Waits for the exit until `deadline`, or until `cancel` is cancelled.
    /// A timed-out or cancelled process keeps running: [`Tree::terminate`]
    /// ends it.
    pub fn wait_until(&self, deadline: Instant, cancel: Option<&CancelToken>) -> Outcome {
        if let Some(cancel) = cancel {
            cancel.watch(&self.exit);
        }
        let mut state = self.exit.state.lock();
        loop {
            if let State::Exited(status) = *state {
                return Outcome::Exited(status);
            }
            if cancel.is_some_and(CancelToken::is_cancelled) {
                return Outcome::Cancelled;
            }
            if Instant::now() >= deadline {
                return Outcome::TimedOut;
            }
            self.exit.changed.wait_until(&mut state, deadline);
        }
    }

    /// Asks the processes to end (`SIGTERM` on Unix), killing them after
    /// `grace` if they are still there; a zero grace kills them now.
    /// Returns at once; reaping happens on the wait thread.
    pub fn terminate(&mut self, grace: Duration) {
        if self.ended {
            return;
        }
        self.ended = true;
        {
            let state = self.exit.state.lock();
            if !matches!(*state, State::Running) {
                return;
            }
            if grace.is_zero() {
                self.killer.kill();
                return;
            }
            self.killer.ask_to_end();
        }
        let killer = self.killer.clone();
        let exit = self.exit.clone();
        let reaper = thread::Builder::new()
            .name("process-grace".into())
            .spawn(move || {
                if !exit.wait_for(grace) {
                    let state = exit.state.lock();
                    if matches!(*state, State::Running) {
                        killer.kill();
                    }
                }
            });
        if reaper.is_err() {
            // No thread for the grace: end it now.
            let state = self.exit.state.lock();
            if matches!(*state, State::Running) {
                self.killer.kill();
            }
        }
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        self.terminate(GRACE);
    }
}

impl lsp::ServerProcess for Tree {
    fn end(mut self: Box<Self>, grace: Duration) {
        self.terminate(grace);
    }
}

impl Spawned {
    /// Hands the process to a thread that waits for its exit.
    pub fn watch(mut self) -> io::Result<Tree> {
        let child = self.child.take().expect("watched once");
        let exit = Arc::new(Exit::default());
        // Shared with the thread, so a failed spawn hands the child back.
        let slot = Arc::new(Mutex::new(Some(child)));
        let thread_slot = slot.clone();
        let thread_exit = exit.clone();
        let killer = self.killer.clone();
        let spawned = thread::Builder::new()
            .name("process-wait".into())
            .spawn(move || {
                let Some(mut child) = thread_slot.lock().take() else {
                    return;
                };
                let status = child.wait().ok();
                drop(child);
                let mut state = thread_exit.state.lock();
                *state = State::Exited(status);
                thread_exit.changed.notify_all();
                drop(state);
                killer.forget();
            });
        match spawned {
            Ok(_) => Ok(Tree {
                killer: self.killer.clone(),
                exit,
                ended: false,
            }),
            Err(err) => {
                self.child = slot.lock().take();
                Err(err)
            }
        }
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            self.killer.kill();
            child.kill().ok();
            child.wait().ok();
            self.killer.forget();
        }
    }
}

/// Starts a process meant to outlive the editor (a container being
/// stopped as it closes): outside the editor's job on Windows, in its own
/// process group on Unix.
pub fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW};
        command.creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB);
        if let Ok(child) = command.spawn() {
            return Ok(child);
        }
        // The editor itself runs in a job that forbids breaking away.
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    command.spawn()
}

/// Ends a process and what it started, by group (Unix) or job (Windows),
/// without owning the `Child` (the wait thread does).
#[derive(Clone)]
struct Killer {
    #[cfg(unix)]
    pid: libc::pid_t,
    #[cfg(windows)]
    pid: u32,
    /// The process's own nested job; `None` when it could not be made,
    /// then only the process itself can be ended.
    #[cfg(windows)]
    job: Option<Arc<job::Job>>,
}

#[cfg(unix)]
impl Killer {
    fn adopt(child: &Child) -> Self {
        let pid = child.id() as libc::pid_t;
        at_exit::add(pid);
        Self { pid }
    }

    fn ask_to_end(&self) {
        self.signal(libc::SIGTERM);
    }

    fn kill(&self) {
        self.signal(libc::SIGKILL);
    }

    /// The process was reaped: its id may be reused.
    fn forget(&self) {
        at_exit::remove(self.pid);
    }

    fn signal(&self, signal: libc::c_int) {
        // SAFETY: plain kill(2). The group is the child's own; if the
        // child has not called setpgid yet, the process itself is signalled.
        unsafe {
            if libc::kill(-self.pid, signal) != 0 {
                libc::kill(self.pid, signal);
            }
        }
    }
}

#[cfg(windows)]
impl Killer {
    fn adopt(child: &Child) -> Self {
        use std::os::windows::io::AsRawHandle;
        let handle = child.as_raw_handle() as job::HANDLE;
        if let Some(editor) = job::editor_job() {
            editor.assign(handle);
        }
        // Nested under the editor's job (Windows 8+), so ending this tree
        // leaves the others alone.
        let job = job::Job::new(false)
            .filter(|job| job.assign(handle))
            .map(Arc::new);
        Self {
            pid: child.id(),
            job,
        }
    }

    /// Windows has no graceful signal for a process without a console.
    fn ask_to_end(&self) {}

    fn kill(&self) {
        match &self.job {
            Some(job) => job.terminate(),
            None => job::terminate_process(self.pid),
        }
    }

    fn forget(&self) {}
}

#[cfg(unix)]
mod at_exit {
    //! Groups still alive when the editor exits normally are killed; an
    //! abort leaves them (reparented to init), like before.

    use std::sync::Once;

    use parking_lot::Mutex;

    static LIVE: Mutex<Vec<libc::pid_t>> = Mutex::new(Vec::new());
    static HOOK: Once = Once::new();

    pub fn add(pid: libc::pid_t) {
        HOOK.call_once(|| {
            // SAFETY: registers a plain C function; nothing else to uphold.
            unsafe {
                libc::atexit(kill_all);
            }
        });
        LIVE.lock().push(pid);
    }

    pub fn remove(pid: libc::pid_t) {
        LIVE.lock().retain(|live| *live != pid);
    }

    extern "C" fn kill_all() {
        let live = std::mem::take(&mut *LIVE.lock());
        for pid in live {
            // SAFETY: plain kill(2) on groups this process started.
            unsafe {
                if libc::kill(-pid, libc::SIGKILL) != 0 {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
}

#[cfg(windows)]
mod job {
    use std::{mem, ptr, sync::OnceLock};

    pub use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
            },
            Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess},
        },
    };

    /// An owned job object handle.
    pub struct Job(HANDLE);

    // SAFETY: a job handle is used from any thread.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        /// A new anonymous job; `kill_on_close` ends its processes when
        /// the last handle closes (the editor exiting however it does) and
        /// lets children break away when asked to.
        pub fn new(kill_on_close: bool) -> Option<Self> {
            // SAFETY: documented call with no attributes and no name.
            let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
            if handle.is_null() {
                return None;
            }
            let job = Self(handle);
            if kill_on_close {
                // SAFETY: a zeroed limit record is valid; the call reads
                // exactly `size_of` bytes of it.
                let ok = unsafe {
                    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
                    info.BasicLimitInformation.LimitFlags =
                        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
                    SetInformationJobObject(
                        handle,
                        JobObjectExtendedLimitInformation,
                        ptr::from_ref(&info).cast(),
                        mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                    ) != 0
                };
                if !ok {
                    return None;
                }
            }
            Some(job)
        }

        pub fn assign(&self, process: HANDLE) -> bool {
            // SAFETY: both handles are open (the process one is the
            // `Child`'s, alive for the call).
            unsafe { AssignProcessToJobObject(self.0, process) != 0 }
        }

        pub fn terminate(&self) {
            // SAFETY: the handle is open.
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closes a handle this struct owns.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// The job every child of the editor is assigned to; closed, with the
    /// editor, it ends them all. `None` when jobs are unavailable.
    pub fn editor_job() -> Option<&'static Job> {
        static JOB: OnceLock<Option<Job>> = OnceLock::new();
        JOB.get_or_init(|| {
            let job = Job::new(true);
            if job.is_none() {
                eprintln!("next_code: cannot create the job object; child processes may outlive the editor");
            }
            job
        })
        .as_ref()
    }

    pub fn terminate_process(pid: u32) {
        // SAFETY: documented calls; a null handle (the process is gone) is
        // checked before use.
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !handle.is_null() {
                TerminateProcess(handle, 1);
                CloseHandle(handle);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::process::Stdio;

    use super::*;

    fn sleeping_shell() -> Command {
        let mut command = Command::new("sh");
        // A grandchild too: the sleep must die with the shell.
        command
            .args(["-c", "sleep 30 & wait"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    #[test]
    fn cancel_wakes_the_waiter_and_ends_the_group() {
        let tree = Tree::spawn(&mut sleeping_shell()).unwrap().watch().unwrap();
        let cancel = CancelToken::new();
        let waker = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            waker.cancel();
        });
        let started = Instant::now();
        let outcome = tree.wait_until(Instant::now() + Duration::from_secs(20), Some(&cancel));
        assert!(matches!(outcome, Outcome::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(5));
        let mut tree = tree;
        tree.terminate(Duration::from_millis(500));
        assert!(tree.exit.wait_for(Duration::from_secs(5)));
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn already_cancelled_and_timed_out_waits_return_at_once() {
        let mut tree = Tree::spawn(&mut sleeping_shell()).unwrap().watch().unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            tree.wait_until(Instant::now() + Duration::from_secs(20), Some(&cancel)),
            Outcome::Cancelled
        ));
        assert!(matches!(
            tree.wait_until(Instant::now() + Duration::from_millis(20), None),
            Outcome::TimedOut
        ));
        tree.terminate(Duration::ZERO);
        assert!(tree.exit.wait_for(Duration::from_secs(5)));
    }

    #[test]
    fn an_exit_is_reported_with_its_status() {
        let mut command = Command::new("sh");
        command.args(["-c", "exit 3"]).stdout(Stdio::null());
        let tree = Tree::spawn(&mut command).unwrap().watch().unwrap();
        match tree.wait_until(Instant::now() + Duration::from_secs(20), None) {
            Outcome::Exited(Some(status)) => assert_eq!(status.code(), Some(3)),
            other => panic!("unexpected {other:?}"),
        }
    }
}
