//! Process-level startup concerns: the console on Windows and the
//! `NEXT_CODE_TRACE_STARTUP` timeline.
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

/// Starts the startup timeline; call first thing in `main`.
pub fn init() {
    mark("main");
}

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
