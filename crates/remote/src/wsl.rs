//! Starting `next_code_agent` inside a WSL distro.
//!
//! Everything goes through `wsl.exe` with stdin/stdout pipes: the agent
//! binary is installed by piping it into the distro, and the agent's
//! protocol runs over the same kind of pipes. Windows never touches the
//! distro's files (`\\wsl.localhost`) and the distro needs no network port.

use std::{
    env,
    ffi::OsStr,
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
/// Distro directory, relative to `$HOME`, holding installed agents.
const INSTALL_DIR: &str = ".local/share/next_code";
/// A cold WSL start can take a while.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// Stderr kept for error messages.
const STDERR_TAIL: usize = 4096;
/// Prefixes the launcher's report lines (see [`start`]).
const LAUNCH_MARK: &str = "NEXT_CODE_LAUNCH";
/// Text skipped while looking for the launcher's report is capped.
const MAX_LAUNCH_NOISE: usize = 64 * 1024;

/// A project opened inside a WSL distro.
pub struct Connection {
    pub client: Arc<AgentClient>,
    pub info: Initialized,
}

fn wsl_exe() -> PathBuf {
    env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join("System32").join("wsl.exe"))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("wsl.exe"))
}

fn wsl(args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Command {
    let mut command = Command::new(wsl_exe());
    command.args(args).env("WSL_UTF8", "1");
    #[cfg(windows)]
    {
        // Don't flash a console window from a GUI process.
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command
}

/// `wsl.exe` writes UTF-16LE unless `WSL_UTF8` is honored; accept both.
fn decode_output(bytes: &[u8]) -> String {
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

fn not_available(err: &io::Error) -> io::Error {
    io::Error::new(
        err.kind(),
        format!(
            "WSL is not available ({}: {err}). Install it with `wsl --install`.",
            wsl_exe().display()
        ),
    )
}

/// Runs `wsl.exe` to completion, failing with its output on a non-zero exit.
fn run(mut command: Command, input: Option<&[u8]>, what: &str) -> io::Result<String> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|err| not_available(&err))?;
    let writer = child.stdin.take().map(|mut stdin| {
        let data = input.unwrap_or_default().to_vec();
        thread::spawn(move || stdin.write_all(&data))
    });
    let output = child.wait_with_output()?;
    if let Some(writer) = writer {
        writer
            .join()
            .map_err(|_| io::Error::other("stdin writer panicked"))??;
    }
    let stdout = decode_output(&output.stdout);
    if !output.status.success() {
        let stderr = decode_output(&output.stderr);
        let details = [stderr.trim(), stdout.trim()]
            .into_iter()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        return Err(io::Error::other(format!(
            "{what} failed ({}){}",
            output.status,
            if details.is_empty() {
                String::new()
            } else {
                format!(": {details}")
            }
        )));
    }
    Ok(stdout)
}

/// Names of the installed distros.
pub fn distros() -> io::Result<Vec<String>> {
    let output = run(wsl(["--list", "--quiet"]), None, "Listing WSL distros")?;
    Ok(parse_distros(&output))
}

fn parse_distros(output: &str) -> Vec<String> {
    output
        .lines()
        .map(|line| line.trim().trim_matches('\0'))
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
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

/// What the launcher found in the distro.
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
                        "the WSL login shell printed too much output before starting the agent",
                    ));
                }
            }
        }
    }
}

/// Expands `~` against the distro user's home; other paths must be absolute.
fn resolve_root(root: &str, home: &str) -> io::Result<String> {
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

/// A launcher run: `wsl.exe` with the agent starting behind it, or done
/// after reporting that the agent is missing.
struct Launcher {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_tail: Arc<Mutex<String>>,
    report: Result<Launch, String>,
}

/// Starts the installed agent `name` in one `wsl.exe` call (each costs a
/// few hundred milliseconds): a login shell reports `$HOME` and the machine,
/// then `exec`s the agent if it is installed and can run. A login shell
/// gives the agent (and the language servers and shells it starts) the
/// user's PATH from ~/.profile.
fn start(distro: &str, name: &str) -> io::Result<Launcher> {
    let script = format!(
        r#"a="$HOME/{INSTALL_DIR}/$1"
m=$(uname -m)
printf '{LAUNCH_MARK} home %s\n{LAUNCH_MARK} arch %s\n' "$HOME" "$m"
if [ "$m" = x86_64 ] && [ -x "$a" ]; then echo '{LAUNCH_MARK} start'; exec "$a" serve; fi
echo '{LAUNCH_MARK} missing'"#
    );
    let mut child = wsl([
        "-d", distro, "--exec", "/bin/sh", "-lc", &script, "sh", name,
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .map_err(|err| not_available(&err))?;
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

    // A cold distro start or a slow login script can hang; don't wait forever.
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
                    "WSL distro '{distro}' did not start within {} s",
                    START_TIMEOUT.as_secs()
                ),
            ))
        }
    }
}

/// Why a launcher ended without a report: an unknown distro, or whatever
/// `wsl.exe` and the shell printed.
fn launch_failure(distro: &str, mut launcher: Launcher, output: String) -> io::Error {
    let status = launcher.child.wait();
    if let Ok(installed) = distros()
        && !installed
            .iter()
            .any(|name| name.eq_ignore_ascii_case(distro))
    {
        return io::Error::new(
            io::ErrorKind::NotFound,
            if installed.is_empty() {
                format!("WSL distro '{distro}' is not installed, and no distros are.")
            } else {
                format!(
                    "WSL distro '{distro}' is not installed. Installed distros: {}.",
                    installed.join(", ")
                )
            },
        );
    }
    let stderr = decode_output(launcher.stderr_tail.lock().as_bytes());
    let details = [stderr.trim(), decode_output(output.as_bytes()).trim()]
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let status = status.map_or_else(|err| err.to_string(), |status| status.to_string());
    io::Error::other(format!(
        "Starting WSL distro '{distro}' failed ({status}){}",
        if details.is_empty() {
            String::new()
        } else {
            format!(": {details}")
        }
    ))
}

/// Installs (if needed) and starts the agent in `distro`, then opens `root`
/// (an absolute Linux path, or one starting with `~`).
pub fn connect(distro: &str, root: &str) -> io::Result<Connection> {
    // Reject relative roots before paying for a launch.
    resolve_root(root, "/")?;
    let binary_path = agent_binary()?;
    let binary = std::fs::read(&binary_path)
        .map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", binary_path.display())))?;
    let name = install_name(&binary);

    let mut launcher = start(distro, &name)?;
    let home = match std::mem::replace(&mut launcher.report, Err(String::new())) {
        Ok(Launch::Started { home }) => home,
        Ok(Launch::Missing { arch, .. }) if arch != "x86_64" => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("WSL distro '{distro}' runs on {arch}, but the agent is built for x86_64"),
            ));
        }
        Ok(Launch::Missing { .. }) => {
            launcher.child.wait().ok();
            install(distro, &name, &binary)?;
            launcher = start(distro, &name)?;
            match std::mem::replace(&mut launcher.report, Err(String::new())) {
                Ok(Launch::Started { home }) => home,
                Ok(Launch::Missing { .. }) => {
                    return Err(io::Error::other(format!(
                        "The agent installed into WSL distro '{distro}' does not start"
                    )));
                }
                Err(output) => return Err(launch_failure(distro, launcher, output)),
            }
        }
        Err(output) => return Err(launch_failure(distro, launcher, output)),
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
        format!("WSL agent ({distro})"),
        &root,
        START_TIMEOUT,
    )
    .map_err(|err| match err.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => io::Error::new(
            err.kind(),
            format!("Cannot open {root} in WSL distro '{distro}': {err}"),
        ),
        _ => err,
    })?;
    Ok(Connection { client, info })
}

/// Copies the agent into the distro under `name`. It is written to a
/// temporary name and renamed, so a half-copied agent is never run; other
/// builds are removed.
fn install(distro: &str, name: &str, binary: &[u8]) -> io::Result<()> {
    let script = format!(
        r#"set -e
d="$HOME/{INSTALL_DIR}"
mkdir -p "$d"
t="$d/.$1.$$"
cat > "$t"
chmod 755 "$t"
mv -f "$t" "$d/$1"
for f in "$d"/{AGENT_BINARY_NAME}-*; do [ "$f" = "$d/$1" ] || rm -f "$f"; done"#
    );
    run(
        wsl(["-d", distro, "--exec", "/bin/sh", "-c", &script, "sh", name]),
        Some(binary),
        &format!("Installing the agent into WSL distro '{distro}'"),
    )
    .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsl_output_decodes_from_utf16_and_utf8() {
        let utf16: Vec<u8> = "\u{feff}Ubuntu\r\ndocker-desktop\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(
            parse_distros(&decode_output(&utf16)),
            ["Ubuntu", "docker-desktop"]
        );
        assert_eq!(
            parse_distros(&decode_output(b"Ubuntu-24.04\n\n")),
            ["Ubuntu-24.04"]
        );
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
