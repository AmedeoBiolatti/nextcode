//! Process-level startup concerns: the console on Windows, the
//! `NEXT_CODE_TRACE_STARTUP` timeline, the crash log, and closing cleanly
//! on termination signals.
//!
//! The binary uses the Windows GUI subsystem, so no console window opens
//! with it. Command-line output (errors, `--headless`) goes to redirected
//! handles as usual; when there are none, [`attach_console`] borrows the
//! console of the shell that started us.

use std::{
    sync::LazyLock,
    time::{Duration, Instant},
};

/// Set (to anything) to print startup milestones to stderr.
const TRACE_VAR: &str = "NEXT_CODE_TRACE_STARTUP";

/// When `main` started, if tracing.
static TRACE: LazyLock<Option<Instant>> =
    LazyLock::new(|| std::env::var_os(TRACE_VAR).map(|_| Instant::now()));

/// Time from process creation to `main` (loader and runtime start).
static BEFORE_MAIN: LazyLock<Duration> = LazyLock::new(|| match *TRACE {
    Some(start) => process_age().saturating_sub(start.elapsed()),
    None => Duration::ZERO,
});

/// Starts the startup timeline and the crash log; call first thing in
/// `main`.
pub fn init() {
    mark("main");
    log_panics();
}

/// `next_code/crash.log` in the local data dir.
pub fn crash_log_path() -> Option<std::path::PathBuf> {
    Some(dirs::data_local_dir()?.join("next_code").join("crash.log"))
}

/// Appends every panic, with a backtrace, to [`crash_log_path`]. A GUI
/// process has no console, so without this a panic (which ends the
/// process with code 101 and no Windows error report) leaves no trace.
/// The default hook still prints to stderr.
fn log_panics() {
    use std::io::Write;
    const LIMIT: u64 = 1024 * 1024;
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        let Some(path) = crash_log_path() else {
            return;
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        // Keep the newest crashes: start over past the limit.
        let fresh = std::fs::metadata(&path).is_ok_and(|meta| meta.len() > LIMIT);
        let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(!fresh)
            .write(true)
            .truncate(fresh)
            .open(&path)
        else {
            return;
        };
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let thread = std::thread::current();
        writeln!(
            file,
            "--- next_code {} panicked at unix time {seconds}, pid {}, thread {}\n{info}\n{}\n",
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
            thread.name().unwrap_or("<unnamed>"),
            std::backtrace::Backtrace::force_capture(),
        )
        .ok();
    }));
}

/// Closes every window (as their close buttons would, but without asking
/// about unsaved files) on SIGTERM, SIGINT or SIGHUP, so dev containers
/// and language servers are shut down as on a normal exit. Like a quit,
/// every open window reopens next time. A second signal falls back to the
/// default action.
#[cfg(unix)]
pub fn close_on_termination(cx: &mut gpui::App) {
    use futures::StreamExt;
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let Ok(mut signals) = signal_hook::iterator::Signals::new([SIGTERM, SIGINT, SIGHUP]) else {
        return;
    };
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            let mut received = signals.forever();
            if let Some(signal) = received.next() {
                tx.unbounded_send(signal).ok();
            }
            // Closing is under way; a second signal ends the process now.
            if let Some(signal) = received.next() {
                signal_hook::low_level::emulate_default_handler(signal).ok();
            }
        })
        .ok();
    cx.spawn(async move |cx| {
        if rx.next().await.is_some() {
            cx.update(|cx| {
                crate::app_state::quit(cx);
                for window in cx.windows() {
                    window
                        .update(cx, |_, window, _| window.remove_window())
                        .ok();
                }
            })
            .ok();
        }
    })
    .detach();
}

#[cfg(not(unix))]
pub fn close_on_termination(_: &mut gpui::App) {}

/// Prints `label` with the time since the process was created (when
/// tracing).
pub fn mark(label: &str) {
    let Some(start) = *TRACE else {
        return;
    };
    let total = start.elapsed() + *BEFORE_MAIN;
    eprintln!("startup {:>7.1} ms  {label}", total.as_secs_f64() * 1000.0);
}

/// Like [`mark`], for milestones reached repeatedly (renders, refreshes):
/// only the first time counts.
pub fn mark_once(label: &'static str) {
    static SEEN: std::sync::Mutex<Vec<&str>> = std::sync::Mutex::new(Vec::new());
    if TRACE.is_none() {
        return;
    }
    let mut seen = SEEN.lock().unwrap_or_else(|poison| poison.into_inner());
    if !seen.contains(&label) {
        seen.push(label);
        drop(seen);
        mark(label);
    }
}

#[cfg(windows)]
fn process_age() -> Duration {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::{
            SystemInformation::GetSystemTimePreciseAsFileTime,
            Threading::{GetCurrentProcess, GetProcessTimes},
        },
    };
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    let empty = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user, mut now) =
        (empty, empty, empty, empty, empty);
    // SAFETY: all pointers are to live locals; the pseudo handle needs no close.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        ) != 0
    };
    if !ok {
        return Duration::ZERO;
    }
    // SAFETY: writes one FILETIME.
    unsafe { GetSystemTimePreciseAsFileTime(&mut now) };
    // FILETIMEs count 100 ns ticks.
    Duration::from_nanos(ticks(now).saturating_sub(ticks(created)) * 100)
}

#[cfg(not(windows))]
fn process_age() -> Duration {
    Duration::ZERO
}

/// Gives a GUI-subsystem process the console of the shell that started it,
/// for standard streams that are not redirected. Without a parent console
/// (started from Explorer) nothing changes. Only called for command-line
/// output: an attached process is killed when that console closes.
#[cfg(windows)]
pub fn attach_console() {
    use std::{fs::OpenOptions, os::windows::io::IntoRawHandle};
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE,
        STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    let missing = |which: STD_HANDLE| {
        // SAFETY: plain query.
        let handle = unsafe { GetStdHandle(which) };
        handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE
    };
    let streams = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
    if !streams.iter().any(|&which| missing(which)) {
        return;
    }
    // SAFETY: plain call; fails harmlessly without a parent console.
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } == 0 {
        return;
    }
    for which in streams {
        if !missing(which) {
            continue;
        }
        let input = which == STD_INPUT_HANDLE;
        let name = if input { "CONIN$" } else { "CONOUT$" };
        let Ok(file) = OpenOptions::new().read(true).write(true).open(name) else {
            continue;
        };
        // SAFETY: hands the open handle over to the process for its lifetime.
        unsafe { SetStdHandle(which, file.into_raw_handle()) };
    }
}

#[cfg(not(windows))]
pub fn attach_console() {}

/// Gives the editor its own taskbar identity. Without one, Windows may
/// group its windows with the program that started it (a terminal, for
/// instance), showing that program's icon instead of next_code's.
#[cfg(windows)]
pub fn set_app_id() {
    use windows::{Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID, core::w};
    // SAFETY: a static string; only fails on out-of-range input.
    unsafe { SetCurrentProcessExplicitAppUserModelID(w!("next_code.Editor")) }.ok();
}

#[cfg(not(windows))]
pub fn set_app_id() {}

/// Warm-up threads started ahead of gpui; what they created stays alive
/// until the first window is up.
pub struct Prewarm {
    release: Vec<std::sync::mpsc::Sender<()>>,
}

impl Prewarm {
    /// Lets the warm-up threads drop what they created.
    pub fn finish(self) {
        drop(self.release);
    }
}

/// Runs the slow parts of gpui's Windows setup that it would otherwise do
/// one after another on the main thread, on threads of their own while the
/// main thread creates the Direct3D devices:
/// - DirectWrite's system font collection, cached by the shared factory
///   gpui also uses (~100 ms cold);
/// - the shell's drag-and-drop helper (loads its COM server, ~20 ms).
///
/// A warm-up Direct3D device was tried as well: it competes with gpui's
/// own device creation and makes startup slower.
#[cfg(windows)]
pub fn prewarm() -> Prewarm {
    let fonts = keep_until_released("prewarm-fonts", "prewarm: fonts", || {
        use windows::Win32::Graphics::DirectWrite::{
            DWRITE_FACTORY_TYPE_SHARED, DWriteCreateFactory, IDWriteFactory5,
        };
        // SAFETY: plain COM calls on objects owned by this thread.
        unsafe {
            DWriteCreateFactory::<IDWriteFactory5>(DWRITE_FACTORY_TYPE_SHARED).map(|factory| {
                let mut collection = None;
                // The same call as gpui's (no downloadable fonts, check
                // for updates), so its result is the cached one.
                factory
                    .GetSystemFontCollection(false, &mut collection, true)
                    .ok();
                (factory, collection)
            })
        }
    });
    let shell = keep_until_released("prewarm-shell", "prewarm: shell", || {
        use windows::Win32::{
            System::{
                Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
                Ole::{OleInitialize, OleUninitialize},
            },
            UI::Shell::{CLSID_DragDropHelper, IDropTargetHelper},
        };
        /// Releases the helper, then leaves COM, on the creating thread.
        struct Helper(Option<IDropTargetHelper>);
        impl Drop for Helper {
            fn drop(&mut self) {
                if let Some(helper) = self.0.take() {
                    drop(helper);
                    // SAFETY: balances the successful `OleInitialize`.
                    unsafe { OleUninitialize() };
                }
            }
        }
        // SAFETY: COM is initialized for this thread before creating the
        // helper, and left only after releasing it (see `Helper`).
        unsafe {
            if OleInitialize(None).is_err() {
                return Helper(None);
            }
            match CoCreateInstance::<_, IDropTargetHelper>(
                &CLSID_DragDropHelper,
                None,
                CLSCTX_INPROC_SERVER,
            ) {
                Ok(helper) => Helper(Some(helper)),
                Err(_) => {
                    OleUninitialize();
                    Helper(None)
                }
            }
        }
    });
    Prewarm {
        release: [fonts, shell].into_iter().flatten().collect(),
    }
}

/// Runs `create` on a thread named `name` and keeps its result there until
/// the returned sender is dropped (COM objects stay on their thread).
#[cfg(windows)]
fn keep_until_released<T>(
    name: &str,
    label: &'static str,
    create: impl FnOnce() -> T + Send + 'static,
) -> Option<std::sync::mpsc::Sender<()>> {
    let (release, released) = std::sync::mpsc::channel::<()>();
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let warm = create();
            mark(label);
            // Returns once the sender is dropped.
            released.recv().ok();
            drop(warm);
        })
        .ok()?;
    Some(release)
}

#[cfg(not(windows))]
pub fn prewarm() -> Prewarm {
    Prewarm {
        release: Vec::new(),
    }
}
