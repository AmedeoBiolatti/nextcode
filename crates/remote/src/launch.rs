//! Installing and starting `next_code_agent` on a Linux target reached by a
//! command (a WSL distro through `wsl.exe`, a container through
//! `docker exec`), then connecting to it over that command's stdin/stdout.
//!
//! One launch normally costs one command: a login shell reports `$HOME` and
//! the machine, then `exec`s the installed agent. Only when that build of the
//! agent is missing is it piped in (a second command) and launched again.

use std::{
    env,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

use parking_lot::Mutex;

use crate::client::{AgentClient, Initialized, Transport};

/// Overrides where the Linux agent binary is taken from.
pub const AGENT_BINARY_ENV: &str = "NEXT_CODE_WSL_AGENT";
/// File name of the Linux agent binary next to the editor executable.
pub const AGENT_BINARY_NAME: &str = "next_code_agent";
/// Directory, relative to `$HOME`, holding installed agents.
const INSTALL_DIR: &str = ".local/share/next_code";
/// A cold WSL or container start can take a while.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// Stderr kept for error messages.
const STDERR_TAIL: usize = 4096;
/// Prefixes the launcher's report lines (see [`start`]).
const LAUNCH_MARK: &str = "NEXT_CODE_LAUNCH";
/// Text skipped while looking for the launcher's report is capped.
const MAX_LAUNCH_NOISE: usize = 64 * 1024;

/// A project opened on a target through the agent.
pub struct Connection {
    pub client: Arc<AgentClient>,
    pub info: Initialized,
}

/// Where the agent runs, and how to run a shell there.
pub trait Target {
    /// Names the target in messages (`WSL distro 'Ubuntu'`).
    fn describe(&self) -> String;

    /// A command running `/bin/sh -c <script> sh <args…>` on the target
    /// (`-lc`, a login shell, when `login`), stdin and stdout passed through.
    fn shell(&self, login: bool, script: &str, args: &[&str]) -> Command;

    /// Turns a failure to start the command itself into a useful message.
    fn spawn_error(&self, err: io::Error) -> io::Error {
        err
    }

    /// Explains a launch that ended without a report (e.g. an unknown
    /// distro); `None` falls back to the command's output.
    fn diagnose(&self) -> Option<io::Error> {
        None
    }
}

/// Keeps background console programs from opening a window, including
/// when headless mode is attached to its caller's console. Redirected
/// standard streams still work without a console.
pub fn no_window(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command
}

/// Decodes command output; `wsl.exe` writes UTF-16LE unless `WSL_UTF8` is
/// honored, so accept both.
pub(crate) fn decode_output(bytes: &[u8]) -> String {
    let (pairs, rest) = bytes.as_chunks::<2>();
    let utf16 = !pairs.is_empty() && rest.is_empty() && pairs.iter().any(|pair| pair[1] == 0);
    let text = if utf16 {
        let units: Vec<u16> = pairs.iter().map(|&pair| u16::from_le_bytes(pair)).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };
    text.trim_start_matches('\u{feff}').replace("\r\n", "\n")
}

/// Runs `command` to completion with `input` on its stdin, failing with its
/// output on a non-zero exit.
pub(crate) fn run(
    mut command: Command,
    input: Option<&[u8]>,
    what: &str,
    spawn_error: impl FnOnce(io::Error) -> io::Error,
) -> io::Result<String> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(spawn_error)?;
    let writer = child.stdin.take().map(|mut stdin| {
        let data = input.unwrap_or_default().to_vec();
        thread::spawn(move || stdin.write_all(&data))
    });
    let output = child.wait_with_output()?;
    let written = writer.map(|writer| {
        writer
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("stdin writer panicked")))
    });
    let stdout = decode_output(&output.stdout);
    // A command that failed early also breaks its stdin: its own output says why.
    if !output.status.success() {
        let stderr = decode_output(&output.stderr);
        return Err(io::Error::other(format!(
            "{what} failed ({}){}",
            output.status,
            details(&[&stderr, &stdout])
        )));
    }
    if let Some(Err(err)) = written {
        return Err(io::Error::new(err.kind(), format!("{what} failed: {err}")));
    }
    Ok(stdout)
}

/// `": <texts>"` for the non-empty texts, or nothing.
fn details(texts: &[&str]) -> String {
    let joined = texts
        .iter()
        .map(|text| text.trim())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if joined.is_empty() {
        String::new()
    } else {
        format!(": {joined}")
    }
}

/// The Linux agent binary shipped with the editor.
pub fn agent_binary() -> io::Result<PathBuf> {
    if let Some(path) = env::var_os(AGENT_BINARY_ENV) {
        let path = PathBuf::from(path);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "{AGENT_BINARY_ENV} points to {}, which does not exist",
                    path.display()
                ),
            ))
        };
    }
    let exe = env::current_exe()?;
    let path = exe
        .parent()
        .unwrap_or(Path::new("."))
        .join(AGENT_BINARY_NAME);
    if path.is_file() {
        Ok(path)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "The Linux agent binary was not found at {}. Build it with \
                 `scripts/build-wsl-agent.ps1`, or set {AGENT_BINARY_ENV} to its path.",
                path.display()
            ),
        ))
    }
}

/// Stable content hash naming an installed agent build (FNV-1a, 64-bit).
fn content_id(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn install_name(binary: &[u8]) -> String {
    format!(
        "{AGENT_BINARY_NAME}-{}-{:016x}",
        proto::PROTOCOL_VERSION,
        content_id(binary)
    )
}

/// What the launcher found on the target.
#[derive(Debug, PartialEq, Eq)]
enum Launch {
    /// The agent is starting; its banner follows.
    Started { home: String },
    /// No agent of this build is installed (or it cannot run on `arch`).
    Missing { home: String, arch: String },
}

/// Reads the launcher's report, skipping login-script output. `Err` holds
/// what was printed instead when the launcher ended without a report.
fn read_launch(reader: &mut impl BufRead) -> io::Result<Result<Launch, String>> {
    let (mut home, mut arch) = (None, None);
    let mut noise = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(Err(noise));
        }
        let text = line.trim_end_matches(['\n', '\r']);
        let report = text
            .strip_prefix(LAUNCH_MARK)
            .and_then(|rest| rest.strip_prefix(' '));
        match report.map(|rest| rest.split_once(' ').unwrap_or((rest, ""))) {
            Some(("home", value)) => home = Some(value.to_owned()),
            Some(("arch", value)) => arch = Some(value.to_owned()),
            Some(("start", _)) => {
                return Ok(Ok(Launch::Started {
                    home: home.unwrap_or_default(),
                }));
            }
            Some(("missing", _)) => {
                return Ok(Ok(Launch::Missing {
                    home: home.unwrap_or_default(),
                    arch: arch.unwrap_or_default(),
                }));
            }
            _ => {
                noise.push_str(&line);
                if noise.len() > MAX_LAUNCH_NOISE {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the login shell printed too much output before starting the agent",
                    ));
                }
            }
        }
    }
}

/// Expands `~` against the target user's home; other paths must be
/// absolute.
pub(crate) fn resolve_root(root: &str, home: &str) -> io::Result<String> {
    let resolved = if root == "~" {
        home.to_owned()
    } else if let Some(rest) = root.strip_prefix("~/") {
        format!("{}/{rest}", home.trim_end_matches('/'))
    } else {
        root.to_owned()
    };
    if resolved.starts_with('/') {
        Ok(resolved)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{root} is not an absolute Linux path (e.g. /home/me/project)"),
        ))
    }
}

/// A launcher run: the target command with the agent starting behind it, or
/// done after reporting that the agent is missing.
struct Launcher {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_tail: Arc<Mutex<String>>,
    report: Result<Launch, String>,
}

/// Starts the installed agent `name` with one target command: a login shell
/// reports `$HOME` and the machine, then `exec`s the agent if it is
/// installed and can run. A login shell gives the agent (and the language
/// servers and shells it starts) the user's PATH from ~/.profile.
fn start(target: &dyn Target, name: &str) -> io::Result<Launcher> {
    let script = format!(
        r#"a="${{HOME:-/tmp}}/{INSTALL_DIR}/$1"
m=$(uname -m)
printf '{LAUNCH_MARK} home %s\n{LAUNCH_MARK} arch %s\n' "${{HOME:-/tmp}}" "$m"
if [ "$m" = x86_64 ] && [ -x "$a" ]; then echo '{LAUNCH_MARK} start'; exec "$a" serve; fi
echo '{LAUNCH_MARK} missing'"#
    );
    let mut child = target
        .shell(true, &script, &[name])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| target.spawn_error(err))?;
    let stdin = child.stdin.take().expect("piped stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut stderr = child.stderr.take().expect("piped stderr");
    let tail = Arc::new(Mutex::new(String::new()));
    let stderr_tail = tail.clone();
    thread::Builder::new()
        .name("agent-stderr".into())
        .spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(n @ 1..) = stderr.read(&mut buffer) {
                let mut tail = stderr_tail.lock();
                tail.push_str(&String::from_utf8_lossy(&buffer[..n]));
                if tail.len() > STDERR_TAIL {
                    let mut cut = tail.len() - STDERR_TAIL;
                    while !tail.is_char_boundary(cut) {
                        cut += 1;
                    }
                    tail.drain(..cut);
                }
            }
        })?;

    // A cold start or a slow login script can hang; don't wait forever.
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("agent-launch".into())
        .spawn(move || {
            let report = read_launch(&mut stdout);
            tx.send((stdout, report)).ok();
        })?;
    match rx.recv_timeout(START_TIMEOUT) {
        Ok((stdout, report)) => Ok(Launcher {
            child,
            stdin,
            stdout,
            stderr_tail: tail,
            report: report?,
        }),
        Err(_) => {
            child.kill().ok();
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "{} did not start within {} s",
                    target.describe(),
                    START_TIMEOUT.as_secs()
                ),
            ))
        }
    }
}

/// Why a launcher ended without a report.
fn launch_failure(target: &dyn Target, mut launcher: Launcher, output: String) -> io::Error {
    let status = launcher.child.wait();
    if let Some(err) = target.diagnose() {
        return err;
    }
    let stderr = decode_output(launcher.stderr_tail.lock().as_bytes());
    let status = status.map_or_else(|err| err.to_string(), |status| status.to_string());
    io::Error::other(format!(
        "Starting the agent in {} failed ({status}){}",
        target.describe(),
        details(&[&stderr, &decode_output(output.as_bytes())])
    ))
}

/// Copies the agent into the target under `name`. It is written to a
/// temporary name and renamed, so a half-copied agent is never run; other
/// builds are removed.
fn install(target: &dyn Target, name: &str, binary: &[u8]) -> io::Result<()> {
    let script = format!(
        r#"set -e
d="${{HOME:-/tmp}}/{INSTALL_DIR}"
mkdir -p "$d"
t="$d/.$1.$$"
cat > "$t"
chmod 755 "$t"
mv -f "$t" "$d/$1"
for f in "$d"/{AGENT_BINARY_NAME}-*; do [ "$f" = "$d/$1" ] || rm -f "$f"; done"#
    );
    run(
        target.shell(false, &script, &[name]),
        Some(binary),
        &format!("Installing the agent into {}", target.describe()),
        |err| target.spawn_error(err),
    )
    .map(drop)
}

/// Installs (if needed) and starts the agent on `target`, then opens `root`
/// (an absolute Linux path, or one starting with `~`).
pub fn connect(target: &dyn Target, root: &str) -> io::Result<Connection> {
    // Reject relative roots before paying for a launch.
    resolve_root(root, "/")?;
    let binary_path = agent_binary()?;
    let binary = std::fs::read(&binary_path)
        .map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", binary_path.display())))?;
    let name = install_name(&binary);

    let mut launcher = start(target, &name)?;
    let home = match std::mem::replace(&mut launcher.report, Err(String::new())) {
        Ok(Launch::Started { home }) => home,
        Ok(Launch::Missing { arch, .. }) if arch != "x86_64" => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "{} runs on {arch}, but the agent is built for x86_64",
                    target.describe()
                ),
            ));
        }
        Ok(Launch::Missing { .. }) => {
            launcher.child.wait().ok();
            install(target, &name, &binary)?;
            launcher = start(target, &name)?;
            match std::mem::replace(&mut launcher.report, Err(String::new())) {
                Ok(Launch::Started { home }) => home,
                Ok(Launch::Missing { .. }) => {
                    return Err(io::Error::other(format!(
                        "The agent installed into {} does not start",
                        target.describe()
                    )));
                }
                Err(output) => return Err(launch_failure(target, launcher, output)),
            }
        }
        Err(output) => return Err(launch_failure(target, launcher, output)),
    };
    let root = resolve_root(root, &home)?;

    let transport = Transport {
        reader: Box::new(launcher.stdout),
        writer: Box::new(launcher.stdin),
        child: Some(launcher.child),
        stderr: Some(launcher.stderr_tail),
    };
    let (client, info) = AgentClient::connect(
        transport,
        format!("agent ({})", target.describe()),
        &root,
        START_TIMEOUT,
    )
    .map_err(|err| match err.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => io::Error::new(
            err.kind(),
            format!("Cannot open {root} in {}: {err}", target.describe()),
        ),
        _ => err,
    })?;
    Ok(Connection { client, info })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn background_child_has_no_console_window_and_keeps_piped_stdio() {
        const CHILD: &str = "NEXT_CODE_TEST_WINDOWLESS_CHILD";
        if std::env::var_os(CHILD).is_some() {
            // A windowless console may still have an input code page.
            // SAFETY: both calls only query the process's console window.
            let window = unsafe { windows_sys::Win32::System::Console::GetConsoleWindow() };
            assert!(
                window.is_null()
                    || unsafe {
                        windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible(window) == 0
                    }
            );
            let mut input = String::new();
            io::Read::read_to_string(&mut io::stdin(), &mut input).unwrap();
            assert_eq!(input, "piped input");
            println!("piped output");
            eprintln!("piped error");
            return;
        }
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "launch::tests::background_child_has_no_console_window_and_keeps_piped_stdio",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = no_window(&mut command).spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"piped input")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("piped output"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("piped error"));
    }

    #[test]
    fn launch_reports_are_read_despite_login_noise() {
        let mut output = io::Cursor::new(
            "Welcome!\nNEXT_CODE_LAUNCH home /home/me two\nNEXT_CODE_LAUNCH arch x86_64\n\
             NEXT_CODE_LAUNCH start\nNEXT_CODE_AGENT 3 0.1.0\n",
        );
        assert_eq!(
            read_launch(&mut output).unwrap(),
            Ok(Launch::Started {
                home: "/home/me two".into()
            })
        );
        // The banner stays unread for the agent client.
        let mut rest = String::new();
        output.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "NEXT_CODE_AGENT 3 0.1.0\n");

        let mut missing = io::Cursor::new(
            "NEXT_CODE_LAUNCH home /root\nNEXT_CODE_LAUNCH arch aarch64\nNEXT_CODE_LAUNCH missing\n",
        );
        assert_eq!(
            read_launch(&mut missing).unwrap(),
            Ok(Launch::Missing {
                home: "/root".into(),
                arch: "aarch64".into()
            })
        );
        let mut failed = io::Cursor::new("There is no distribution with the supplied name.\n");
        assert_eq!(
            read_launch(&mut failed).unwrap(),
            Err("There is no distribution with the supplied name.\n".into())
        );
    }

    #[test]
    fn roots_must_be_absolute_and_tilde_means_home() {
        assert_eq!(resolve_root("~", "/home/me").unwrap(), "/home/me");
        assert_eq!(
            resolve_root("~/src/p", "/home/me/").unwrap(),
            "/home/me/src/p"
        );
        assert_eq!(resolve_root("/srv/p", "/home/me").unwrap(), "/srv/p");
        assert_eq!(
            resolve_root(r"C:\dev\p", "/home/me").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn install_names_change_with_the_binary() {
        assert_eq!(content_id(b""), 0xcbf2_9ce4_8422_2325);
        assert_ne!(install_name(b"build 1"), install_name(b"build 2"));
        assert!(
            install_name(b"x")
                .starts_with(&format!("next_code_agent-{}-", proto::PROTOCOL_VERSION))
        );
    }
}
