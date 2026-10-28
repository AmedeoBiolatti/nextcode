//! `next_code`: what users run. A few hundred kilobytes with no gpui, it
//! hands its command line to the editor already running (which opens the
//! window in a fraction of the time a process start takes) and starts
//! `next_code_editor` next to it when none is. Tool invocations
//! (`--headless`, `--automation`, `pinpoint`, `context`, `--help`) run the editor
//! binary in place with this process's console; `notes` runs here (it only
//! edits the project's notes file).

// No console window of its own; `attach_console` borrows the parent's for
// output.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use instance::{Endpoint, Reply, Request, SendError};

/// How long a running editor gets to answer a launch.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);

/// The editor binary next to this one.
const EDITOR: &str = if cfg!(windows) {
    "next_code_editor.exe"
} else {
    "next_code_editor"
};

fn main() {
    trace("launcher main");
    let code = run();
    trace("launcher done");
    std::process::exit(code);
}

/// With `NEXT_CODE_TRACE_STARTUP` set: prints `label` with the time since
/// this process was created, like the editor's startup trace. (Integer
/// arithmetic: float formatting alone adds ~200 KB to this executable.)
fn trace(label: &str) {
    if std::env::var_os("NEXT_CODE_TRACE_STARTUP").is_none() {
        return;
    }
    let tenths = process_age_100ns() / 1_000;
    eprintln!("startup {:>5}.{} ms  {label}", tenths / 10, tenths % 10);
}

/// The process's age in 100 ns ticks.
#[cfg(windows)]
fn process_age_100ns() -> u64 {
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn GetProcessTimes(
            process: isize,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        fn GetSystemTimePreciseAsFileTime(time: *mut FileTime);
    }
    let ticks = |time: FileTime| (u64::from(time.high) << 32) | u64::from(time.low);
    let (mut created, mut exited, mut kernel, mut user, mut now) = (
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
    );
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
        return 0;
    }
    // SAFETY: writes one FileTime.
    unsafe { GetSystemTimePreciseAsFileTime(&mut now) };
    ticks(now).saturating_sub(ticks(created))
}

#[cfg(not(windows))]
fn process_age_100ns() -> u64 {
    0
}

/// What a command line asks for.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    /// Open projects (or restore the last session) in the running editor,
    /// else start one.
    Open,
    /// `--new-instance`: a separate editor process, which neither joins
    /// nor becomes the shared one.
    NewInstance,
    /// `--quit`: ask the running editor to exit.
    Quit,
    /// Command-line tools of the editor binary: run it here and wait.
    Tool,
}

fn classify(args: &[OsString]) -> Mode {
    if args.iter().any(|arg| arg == "--new-instance") {
        return Mode::NewInstance;
    }
    match args.first().and_then(|arg| arg.to_str()) {
        Some("--quit") => Mode::Quit,
        Some("pinpoint" | "context" | "--automation" | "--headless" | "--help" | "-h") => {
            Mode::Tool
        }
        _ => Mode::Open,
    }
}

fn run() -> i32 {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "notes") {
        attach_console();
        let args: Vec<String> = args[1..]
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        return notes::cli::run(&args, "next_code");
    }
    let editor = match editor_path() {
        Ok(editor) => editor,
        Err(message) => return fail(&message),
    };
    match classify(&args) {
        Mode::Tool => {
            attach_console();
            match Command::new(&editor).args(&args).status() {
                Ok(status) => status.code().unwrap_or(1),
                Err(err) => fail(&format!("cannot run {}: {err}", editor.display())),
            }
        }
        Mode::NewInstance => start_editor(&editor, &args),
        Mode::Quit => match deliver(&args) {
            Ok(Reply::Done | Reply::Restart) | Err(SendError::NoInstance) => 0,
            Ok(Reply::Error { message }) => fail(&message),
            Err(err) => fail(&err.to_string()),
        },
        Mode::Open => match deliver(&args) {
            Ok(Reply::Done) => {
                trace("running editor answered");
                0
            }
            Ok(Reply::Error { message }) => fail(&message),
            // Nothing runs, or what runs was rebuilt and is exiting.
            Ok(Reply::Restart) | Err(SendError::NoInstance) => start_editor(&editor, &args),
            Err(SendError::Timeout) => {
                fail("the running editor did not answer; `next_code --new-instance` starts another")
            }
            Err(err) => fail(&err.to_string()),
        },
    }
}

/// Hands `args` to the running editor.
fn deliver(args: &[OsString]) -> Result<Reply, SendError> {
    let Some(state_root) = instance::default_state_root() else {
        return Err(SendError::NoInstance);
    };
    let request = Request {
        args: args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect(),
        cwd: std::env::current_dir().ok(),
    };
    instance::send(
        &Endpoint::for_state_root(&state_root),
        &request,
        ANSWER_TIMEOUT,
    )
}

/// Starts the editor with `args`; the editor serves later launches. An
/// editor that exits right away (an argument error, printed to no
/// console) is reported with its exit code.
fn start_editor(editor: &Path, args: &[OsString]) -> i32 {
    let exited = match spawn_detached(editor, args) {
        Ok(exited) => exited,
        Err(err) => return fail(&format!("cannot start {}: {err}", editor.display())),
    };
    match exited {
        Some(code) if code != 0 => fail(&format!(
            "{EDITOR} exited with code {code}; run it directly for details: {} {}",
            editor.display(),
            args.iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        )),
        _ => 0,
    }
}

/// How long to watch a started editor for an early exit.
const EARLY_EXIT_WATCH: Duration = Duration::from_millis(300);

/// Starts the editor without handing it any of this process's handles
/// (standard streams included): a shell or script that started us and
/// waits for our pipes to close would otherwise wait for the editor to
/// exit. Returns the exit code if the editor ended within
/// [`EARLY_EXIT_WATCH`].
#[cfg(windows)]
fn spawn_detached(editor: &Path, args: &[OsString]) -> std::io::Result<Option<i32>> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{
            CreateProcessW, GetExitCodeProcess, PROCESS_INFORMATION, STARTUPINFOW,
            WaitForSingleObject,
        },
    };
    // The command line, quoted as the editor's `std::env::args` unquotes.
    let mut line = String::new();
    for (index, arg) in std::iter::once(editor.as_os_str())
        .chain(args.iter().map(OsString::as_os_str))
        .enumerate()
    {
        if index > 0 {
            line.push(' ');
        }
        let text = arg.to_string_lossy();
        if !text.is_empty() && !text.contains([' ', '\t', '"']) {
            line.push_str(&text);
            continue;
        }
        line.push('"');
        let mut backslashes = 0;
        for c in text.chars() {
            match c {
                '\\' => backslashes += 1,
                '"' => {
                    line.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                    line.push('"');
                    backslashes = 0;
                }
                _ => {
                    line.extend(std::iter::repeat_n('\\', backslashes));
                    line.push(c);
                    backslashes = 0;
                }
            }
        }
        line.extend(std::iter::repeat_n('\\', backslashes * 2));
        line.push('"');
    }
    let mut line: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
    let program: Vec<u16> = editor
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: zeroed structs are valid defaults; `cb` is set below.
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    // SAFETY: zeroed is a valid out-parameter.
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: NUL-terminated strings; `bInheritHandles` false; the
    // structs outlive the call; the handles are closed below.
    let created = unsafe {
        CreateProcessW(
            program.as_ptr(),
            line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            0,
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut process,
        )
    };
    if created == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: valid handles from a successful CreateProcessW.
    let exited = unsafe {
        CloseHandle(process.hThread);
        let mut code = 0u32;
        let exited = WaitForSingleObject(process.hProcess, EARLY_EXIT_WATCH.as_millis() as u32)
            == WAIT_OBJECT_0
            && GetExitCodeProcess(process.hProcess, &mut code) != 0;
        CloseHandle(process.hProcess);
        exited.then_some(code as i32)
    };
    Ok(exited)
}

#[cfg(not(windows))]
fn spawn_detached(editor: &Path, args: &[OsString]) -> std::io::Result<Option<i32>> {
    use std::process::Stdio;
    let mut child = Command::new(editor)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let started = std::time::Instant::now();
    while started.elapsed() < EARLY_EXIT_WATCH {
        std::thread::sleep(Duration::from_millis(10));
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status.code().unwrap_or(1)));
        }
    }
    Ok(None)
}

fn editor_path() -> Result<PathBuf, String> {
    let this = std::env::current_exe().map_err(|err| format!("cannot locate next_code: {err}"))?;
    let editor = this
        .parent()
        .ok_or_else(|| "cannot locate next_code".to_owned())?
        .join(EDITOR);
    if editor.is_file() {
        Ok(editor)
    } else {
        Err(format!("{EDITOR} is missing next to {}", this.display()))
    }
}

/// Prints `message` to the console that started us, if any; exit code 1.
fn fail(message: &str) -> i32 {
    attach_console();
    eprintln!("next_code: {message}");
    1
}

/// Gives this GUI-subsystem process the console of the shell that started
/// it, for standard streams that are not redirected (see the editor's
/// `startup::attach_console`).
#[cfg(windows)]
fn attach_console() {
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
fn attach_console() {}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn command_lines_are_classified() {
        assert_eq!(classify(&args(&[])), Mode::Open);
        assert_eq!(classify(&args(&["C:\\dev\\p"])), Mode::Open);
        assert_eq!(classify(&args(&["--wsl", "Ubuntu", "/p"])), Mode::Open);
        assert_eq!(classify(&args(&["--devcontainer"])), Mode::Open);
        assert_eq!(classify(&args(&["--new-instance", "."])), Mode::NewInstance);
        assert_eq!(classify(&args(&[".", "--new-instance"])), Mode::NewInstance);
        assert_eq!(classify(&args(&["--quit"])), Mode::Quit);
        assert_eq!(classify(&args(&["--headless", "."])), Mode::Tool);
        assert_eq!(classify(&args(&["--automation", "--", "."])), Mode::Tool);
        assert_eq!(classify(&args(&["pinpoint", "list"])), Mode::Tool);
        assert_eq!(classify(&args(&["context", "--around", "5"])), Mode::Tool);
        assert_eq!(classify(&args(&["--help"])), Mode::Tool);
    }
}
