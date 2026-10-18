//! One editor process per user: launches hand their command line to the
//! editor already running (over the `instance` crate's channel) and it
//! opens the window; a launch costs a connection instead of a process
//! start. The editor keeps running after its last window closes
//! (`keep_running` in settings) so the next launch is served the same way.
//!
//! The launcher (`next_code`) sends requests before starting
//! `next_code_editor`; the editor itself also forwards its command line
//! when started directly (`cargo run`) while another editor runs.
//! `--new-instance` skips all of this for a separate process, as tests and
//! `--automation` need.

use std::{
    ffi::OsString,
    path::Path,
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, SystemTime},
};

use futures::StreamExt as _;
use gpui::App;
use instance::{Endpoint, Reply, Request, SendError, ServeError};

use crate::{app_state, settings::KeepRunning, shell, startup, workspace};

/// How long a running editor gets to answer this process's launch.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the server thread waits for the main thread's answer.
const HANDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// `(size, modified)` of the executable when this process started.
static EXE_AT_START: LazyLock<Option<(u64, SystemTime)>> = LazyLock::new(exe_stamp);
/// Started with `--new-instance`: neither serves nor stays resident. Unit
/// tests run that way, so closing their last window quits as it always did.
static STANDALONE: AtomicBool = AtomicBool::new(cfg!(test));
/// Exiting (`--quit`, or a rebuilt executable handing over): launches
/// arriving meanwhile are told to start a new editor.
static QUITTING: AtomicBool = AtomicBool::new(false);
/// How long a new editor waits for the pipe of one that is exiting.
const HANDOVER_TIMEOUT: Duration = Duration::from_secs(5);
/// The server, until the editor stops serving (a restart hand-over).
static SERVER: Mutex<Option<instance::Server>> = Mutex::new(None);

/// Call first thing: remembers the executable as it is now, so a rebuild
/// while running is noticed (`exe_changed`).
pub fn init() {
    LazyLock::force(&EXE_AT_START);
}

fn exe_stamp() -> Option<(u64, SystemTime)> {
    let meta = std::fs::metadata(std::env::current_exe().ok()?).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

/// The executable on disk is not the one this process started from (it
/// was rebuilt or replaced): new launches should start it instead.
pub fn exe_changed() -> bool {
    match *EXE_AT_START {
        Some(stamp) => exe_stamp() != Some(stamp),
        None => false,
    }
}

/// The executable is a cargo build output (inside a target directory,
/// whatever its name: cargo marks it with a `CACHEDIR.TAG` naming cargo):
/// a developer build, rebuilt in place.
pub fn dev_build() -> bool {
    std::env::current_exe().is_ok_and(|exe| under_cargo_target(&exe))
}

/// `path` is inside a cargo target directory.
fn under_cargo_target(path: &Path) -> bool {
    path.ancestors().any(|dir| {
        std::fs::read_to_string(dir.join("CACHEDIR.TAG"))
            .is_ok_and(|tag| tag.to_lowercase().contains("cargo"))
    })
}

fn endpoint() -> Option<Endpoint> {
    crate::layout::state_root().map(|root| Endpoint::for_state_root(&root))
}

/// Marks this process as started with `--new-instance`.
pub fn set_standalone() {
    STANDALONE.store(true, Ordering::Relaxed);
}

/// Hands `args` to the editor already running, if any. `Some(code)`: it
/// took the launch (or refused it: the error was printed), exit with
/// `code`. `None`: no editor is running (or it is exiting for a restart),
/// carry on as the editor.
pub fn forward(args: &[OsString]) -> Option<i32> {
    let endpoint = endpoint()?;
    let request = Request {
        args: args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect(),
        cwd: std::env::current_dir().ok(),
    };
    match instance::send(&endpoint, &request, FORWARD_TIMEOUT) {
        Ok(Reply::Done) => Some(0),
        Ok(Reply::Error { message }) => {
            startup::attach_console();
            eprintln!("next_code: {message}");
            Some(1)
        }
        Ok(Reply::Restart) | Err(SendError::NoInstance) => None,
        Err(err) => {
            // The editor is there but not answering: start a separate one
            // rather than nothing (`--quit`, though, has nothing to do).
            startup::attach_console();
            eprintln!("next_code: {err}; starting a separate editor");
            if request.args.first().is_some_and(|arg| arg == "--quit") {
                Some(1)
            } else {
                None
            }
        }
    }
}

/// Starts answering launches (unless started with `--new-instance`).
/// Requests are carried out on the main thread.
pub fn serve(cx: &mut App) {
    if STANDALONE.load(Ordering::Relaxed) {
        return;
    }
    let Some(endpoint) = endpoint() else {
        return;
    };
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<(Request, mpsc::Sender<Reply>)>();
    let handler = move |request| {
        if QUITTING.load(Ordering::Relaxed) {
            return Reply::Restart;
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        if tx.unbounded_send((request, reply_tx)).is_err() {
            return Reply::Restart;
        }
        reply_rx.recv_timeout(HANDLE_TIMEOUT).unwrap_or(Reply::Error {
            message: "the editor did not answer".into(),
        })
    };
    // The editor this one replaces (rebuilt, or quitting) may still hold
    // the pipe for a moment: keep trying.
    let started = std::time::Instant::now();
    let server = loop {
        match instance::serve(&endpoint, handler.clone()) {
            Err(ServeError::AlreadyRunning) if started.elapsed() < HANDOVER_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(100));
            }
            result => break result,
        }
    };
    match server {
        Ok(server) => *SERVER.lock().unwrap_or_else(|p| p.into_inner()) = Some(server),
        Err(ServeError::AlreadyRunning) => {
            // Lost a race with another editor starting at the same time:
            // this one runs on its own.
            eprintln!("next_code: another editor started first; this one serves no launches");
            return;
        }
        Err(err) => {
            eprintln!("next_code: launches cannot reach this editor: {err}");
            return;
        }
    }
    cx.spawn(async move |cx| {
        while let Some((request, reply)) = rx.next().await {
            let answer = cx
                .update(|cx| handle(request, cx))
                .unwrap_or(Reply::Restart);
            reply.send(answer).ok();
        }
    })
    .detach();
}

/// Stops answering launches: they start a new editor from now on.
fn stop_serving() {
    if let Some(mut server) = SERVER.lock().unwrap_or_else(|p| p.into_inner()).take() {
        server.stop();
    }
}

/// Carries out a launch handed over by another process. The server is
/// not stopped here (its thread waits for this answer): once quitting,
/// it answers `Restart` by itself until the process ends.
fn handle(request: Request, cx: &mut App) -> Reply {
    if request.args.first().is_some_and(|arg| arg == "--quit") {
        QUITTING.store(true, Ordering::Relaxed);
        app_state::quit(cx);
        cx.quit();
        return Reply::Done;
    }
    if exe_changed() && cx.windows().is_empty() {
        // Rebuilt while idle: the new build should serve this launch.
        QUITTING.store(true, Ordering::Relaxed);
        app_state::quit(cx);
        cx.quit();
        return Reply::Restart;
    }
    let args: Vec<OsString> = request.args.iter().map(OsString::from).collect();
    let base = request.cwd.as_deref();
    match crate::launch::resolve(&args, base) {
        Err(err) => Reply::Error {
            message: err.to_string(),
        },
        Ok(Some(location)) => {
            if !shell::activate_existing(&location, None, cx) {
                let preload = workspace::Preload::start(&location);
                shell::open_window(location, Some(preload), cx);
            }
            cx.activate(true);
            Reply::Done
        }
        Ok(None) => {
            if cx.windows().is_empty() {
                let state = app_state::restorable();
                let preload = state.focused_tab().map(workspace::Preload::start);
                shell::restore(state, preload, cx);
            } else {
                shell::activate_focused(cx);
            }
            cx.activate(true);
            Reply::Done
        }
    }
}

/// Whether the editor stays running (ready for the next launch) now that
/// its last window closed.
pub fn stays_resident() -> bool {
    if STANDALONE.load(Ordering::Relaxed) {
        return false;
    }
    let setting = crate::settings::current().keep_running;
    let resident = resident_policy(setting, dev_build(), exe_changed());
    if !resident {
        stop_serving();
    }
    resident
}

/// The keep-alive decision: `Auto` stays resident except for developer
/// builds (a resident editor would hold the executable cargo wants to
/// rewrite); a rebuilt executable never stays (the new build takes over).
pub fn resident_policy(setting: KeepRunning, dev_build: bool, exe_changed: bool) -> bool {
    if exe_changed {
        return false;
    }
    match setting {
        KeepRunning::Always => true,
        KeepRunning::Never => false,
        KeepRunning::Auto => !dev_build,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keep_alive_policy() {
        assert!(resident_policy(KeepRunning::Auto, false, false));
        assert!(!resident_policy(KeepRunning::Auto, true, false), "developer build");
        assert!(resident_policy(KeepRunning::Always, true, false));
        assert!(!resident_policy(KeepRunning::Never, false, false));
        assert!(!resident_policy(KeepRunning::Always, false, true), "rebuilt: hand over");
    }

    #[test]
    fn test_binaries_are_developer_builds() {
        let exe = std::env::current_exe().unwrap();
        assert!(under_cargo_target(&exe), "{}", exe.display());
        assert!(dev_build());
        let dir = std::env::temp_dir().join(format!("next_code_instance_{}", std::process::id()));
        let out = dir.join("my_target").join("release");
        std::fs::create_dir_all(&out).unwrap();
        assert!(!under_cargo_target(&out.join("x.exe")), "no CACHEDIR.TAG");
        std::fs::write(
            dir.join("my_target").join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag created by cargo.\n",
        )
        .unwrap();
        assert!(under_cargo_target(&out.join("x.exe")), "any directory name");
        assert!(!under_cargo_target(&dir.join("bin").join("x.exe")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_executable_is_unchanged_while_running() {
        init();
        assert!(!exe_changed());
    }
}
