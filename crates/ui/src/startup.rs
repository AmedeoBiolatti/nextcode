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
