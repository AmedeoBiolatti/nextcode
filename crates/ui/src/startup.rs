//! Process-level startup concerns: the console on Windows, the
//! `NEXT_CODE_TRACE_STARTUP` timeline, the crash log with its recovery
//! dump of unsaved text, and closing cleanly on termination signals.
//!
//! The binary uses the Windows GUI subsystem, so no console window opens
//! with it. Command-line output (errors, `--headless`) goes to redirected
//! handles as usual; when there are none, [`attach_console`] borrows the
//! console of the shell that started us.

use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex, PoisonError},
    time::{Duration, Instant},
};

/// Open file documents, for the crash recovery dump (see
/// [`register_document`]).
static DOCUMENTS: Mutex<Vec<(PathBuf, text::DocumentHandle)>> = Mutex::new(Vec::new());

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

/// `next_code/recovery` in the local data dir: one directory per crash,
/// named by its unix time and pid, with the unsaved text of every open
/// file (see [`register_document`]).
pub fn recovery_dir() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("next_code").join("recovery"))
}

/// Makes `path`'s document part of the crash recovery dump: a panic
/// writes its unsaved text under [`recovery_dir`] and says so in the
/// crash log. The handle is weak; closed documents are dropped from the
/// list here.
pub fn register_document(path: PathBuf, handle: text::DocumentHandle) {
    let mut documents = DOCUMENTS.lock().unwrap_or_else(PoisonError::into_inner);
    documents.retain(|(_, handle)| handle.is_alive());
    documents.push((path, handle));
}

/// Writes the unsaved text of every open document to `root/<stamp>/`
/// (one `<sanitized path>.txt` each) and returns the pairs of document
/// path and recovery file. Runs inside the panic hook: no gpui, no locks
/// waited for (a document the panicking thread holds is skipped).
fn dump_unsaved(root: &Path, stamp: &str) -> Vec<(PathBuf, PathBuf)> {
    let documents = match DOCUMENTS.try_lock() {
        Ok(documents) => documents,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return Vec::new(),
    };
    let mut written = Vec::new();
    if !documents.iter().any(|(_, handle)| handle.is_alive()) {
        return written;
    }
    let dir = root.join(stamp);
    if std::fs::create_dir_all(&dir).is_err() {
        return written;
    }
    for (path, handle) in documents.iter() {
        if !handle.is_alive() {
            continue;
        }
        let target = unused_name(&dir, &recovery_name(path));
        let Ok(mut file) = std::fs::File::create(&target) else {
            continue;
        };
        let wrote = handle.write_unsaved(&mut file).unwrap_or(false);
        drop(file);
        if wrote {
            written.push((path.clone(), target));
        } else {
            std::fs::remove_file(&target).ok();
        }
    }
    if written.is_empty() {
        std::fs::remove_dir(&dir).ok();
    }
    written
}

/// `<path>.txt` with everything but letters, digits, `.`, `-` and `_`
/// replaced by `_`, keeping the end (the file name) of a long path.
fn recovery_name(path: &Path) -> String {
    const LIMIT: usize = 150;
    let mut name: String = path
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.len() > LIMIT {
        let mut start = name.len() - LIMIT;
        while !name.is_char_boundary(start) {
            start += 1;
        }
        name.drain(..start);
    }
    let name = name.trim_start_matches(['_', '.']);
    let name = if name.is_empty() { "document" } else { name };
    format!("{name}.txt")
}

/// `dir/name`, or `dir/name-2`, `-3`, … when that exists.
fn unused_name(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) => (stem, format!(".{extension}")),
        None => (name, String::new()),
    };
    (2..)
        .map(|n| dir.join(format!("{stem}-{n}{extension}")))
        .find(|candidate| !candidate.exists())
        .unwrap_or(first)
}

/// Appends every panic, with a backtrace, to [`crash_log_path`], after
/// saving the unsaved text of the open files under [`recovery_dir`]
/// (listed in the entry). A GUI process has no console, so without this
/// a panic (which ends the process with code 101 and no Windows error
/// report) leaves no trace. The default hook still prints to stderr.
fn log_panics() {
    const LIMIT: u64 = 1024 * 1024;
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let pid = std::process::id();
        let recovered = recovery_dir()
            .map(|root| dump_unsaved(&root, &format!("{seconds}-{pid}")))
            .unwrap_or_default();
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
        let thread = std::thread::current();
        writeln!(
            file,
            "--- next_code {} panicked at unix time {seconds}, pid {pid}, thread {}\n{info}\n{}",
            env!("CARGO_PKG_VERSION"),
            thread.name().unwrap_or("<unnamed>"),
            std::backtrace::Backtrace::force_capture(),
        )
        .ok();
        for (document, recovery) in &recovered {
            writeln!(
                file,
                "unsaved text of {} written to {}",
                document.display(),
                recovery.display()
            )
            .ok();
        }
        writeln!(file).ok();
    }));
}

/// Closes every window (as their close buttons would, but without asking
/// about unsaved files) and quits on SIGTERM, SIGINT or SIGHUP, so dev
/// containers and language servers are shut down as on a normal exit.
/// Like a quit, every open window reopens next time. A second signal falls
/// back to the default action.
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
                // Also when the editor would stay for the next launch.
                cx.quit();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_names_are_plain_and_keep_the_file_name_of_long_paths() {
        assert_eq!(
            recovery_name(Path::new("/home/me/src/main.rs")),
            "home_me_src_main.rs.txt"
        );
        assert_eq!(
            recovery_name(Path::new(r"C:\Users\me\a b.py")),
            "C__Users_me_a_b.py.txt"
        );
        let long = format!("/{}/main.rs", "x".repeat(300));
        let name = recovery_name(Path::new(&long));
        assert_eq!(name.len(), 154, "{name}");
        assert!(name.ends_with("x_main.rs.txt"), "{name}");
        assert_eq!(recovery_name(Path::new("")), "document.txt");
    }

    #[test]
    fn the_dump_writes_unsaved_documents_only_and_forgets_closed_ones() {
        let root = std::env::temp_dir().join(format!("next_code_recovery_{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let mut dirty = text::Buffer::with_path("one\n", PathBuf::from("/p/dirty.rs"));
        let clean = text::Buffer::with_path("two\n", PathBuf::from("/p/clean.rs"));
        let closed = text::Buffer::with_path("gone\n", PathBuf::from("/p/closed.rs"));
        register_document(PathBuf::from("/p/dirty.rs"), dirty.document_handle());
        register_document(PathBuf::from("/p/clean.rs"), clean.document_handle());
        register_document(PathBuf::from("/p/closed.rs"), closed.document_handle());
        dirty.edit(4..4, "two\n");
        drop(closed);

        // Other tests may have documents of their own open: look at ours.
        let written = dump_unsaved(&root, "1-2");
        let ours: Vec<&(PathBuf, PathBuf)> = written
            .iter()
            .filter(|(path, _)| path.starts_with("/p"))
            .collect();
        assert_eq!(ours.len(), 1, "{written:?}");
        assert_eq!(ours[0].0, PathBuf::from("/p/dirty.rs"));
        assert_eq!(ours[0].1, root.join("1-2").join("p_dirty.rs.txt"));
        assert_eq!(std::fs::read_to_string(&ours[0].1).unwrap(), "one\ntwo\n");
        assert!(!root.join("1-2").join("p_clean.rs.txt").exists());

        register_document(PathBuf::from("/p/again.rs"), clean.document_handle());
        let documents = DOCUMENTS.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(
            !documents
                .iter()
                .any(|(path, _)| path == Path::new("/p/closed.rs")),
            "closed documents are pruned"
        );
        drop(documents);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unused_names_count_up() {
        let dir =
            std::env::temp_dir().join(format!("next_code_recovery_names_{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unused_name(&dir, "a.txt"), dir.join("a.txt"));
        std::fs::write(dir.join("a.txt"), "").unwrap();
        assert_eq!(unused_name(&dir, "a.txt"), dir.join("a-2.txt"));
        std::fs::write(dir.join("a-2.txt"), "").unwrap();
        assert_eq!(unused_name(&dir, "a.txt"), dir.join("a-3.txt"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
