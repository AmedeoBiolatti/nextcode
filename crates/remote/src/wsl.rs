//! Starting `next_code_agent` inside a WSL distro.
//!
//! Everything goes through `wsl.exe` with stdin/stdout pipes: the agent
//! binary is installed by piping it into the distro, and the agent's
//! protocol runs over the same kind of pipes. Windows never touches the
//! distro's files (`\\wsl.localhost`) and the distro needs no network port.

use std::{
    env,
    ffi::OsStr,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
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

#[derive(Debug, PartialEq, Eq)]
struct Probe {
    home: String,
    arch: String,
    installed: bool,
}

fn parse_probe(output: &str) -> Option<Probe> {
    let field = |name: &str| {
        output
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
            .map(str::to_owned)
    };
    Some(Probe {
        home: field("home")?,
        arch: field("arch")?,
        installed: field("installed")? == "yes",
    })
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

/// Installs (if needed) and starts the agent in `distro`, then opens `root`
/// (an absolute Linux path, or one starting with `~`).
pub fn connect(distro: &str, root: &str) -> io::Result<Connection> {
    let installed = distros()?;
    let Some(distro) = installed
        .iter()
        .find(|name| name.eq_ignore_ascii_case(distro))
    else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            if installed.is_empty() {
                format!("WSL distro '{distro}' is not installed, and no distros are.")
            } else {
                format!(
                    "WSL distro '{distro}' is not installed. Installed distros: {}.",
                    installed.join(", ")
                )
            },
        ));
    };

    let binary_path = agent_binary()?;
    let binary = std::fs::read(&binary_path)
        .map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", binary_path.display())))?;
    let name = install_name(&binary);

    let script = format!(
        r#"printf 'home=%s\narch=%s\n' "$HOME" "$(uname -m)"
if [ -x "$HOME/{INSTALL_DIR}/$1" ]; then echo installed=yes; else echo installed=no; fi"#
    );
    let output = run(
        wsl([
            "-d", distro, "--exec", "/bin/sh", "-c", &script, "sh", &name,
        ]),
        None,
        &format!("Starting WSL distro '{distro}'"),
    )?;
    let probe = parse_probe(&output).ok_or_else(|| {
        io::Error::other(format!(
            "WSL distro '{distro}' gave an unexpected answer: {}",
            output.trim()
        ))
    })?;
    if probe.arch != "x86_64" {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "WSL distro '{distro}' runs on {}, but the agent is built for x86_64",
                probe.arch
            ),
        ));
    }
    let root = resolve_root(root, &probe.home)?;

    if !probe.installed {
        // Write to a temporary name and rename, so a half-copied agent is
        // never run; then remove other builds.
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
            wsl([
                "-d", distro, "--exec", "/bin/sh", "-c", &script, "sh", &name,
            ]),
            Some(&binary),
            &format!("Installing the agent into WSL distro '{distro}'"),
        )?;
    }

    // A login shell gives the agent (and the language servers and shells it
    // starts) the user's PATH from ~/.profile.
    let agent = format!("{}/{INSTALL_DIR}/{name}", probe.home.trim_end_matches('/'));
    let mut child = wsl([
        "-d",
        distro,
        "--exec",
        "/bin/sh",
        "-lc",
        r#"exec "$0" serve"#,
        &agent,
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .map_err(|err| not_available(&err))?;

    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
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

    let transport = Transport {
        reader: Box::new(stdout),
        writer: Box::new(stdin),
        child: Some(child),
        stderr: Some(tail),
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
    fn probe_answers_are_parsed_despite_login_noise() {
        let output = "Welcome!\nhome=/home/me\narch=x86_64\ninstalled=no\n";
        assert_eq!(
            parse_probe(output),
            Some(Probe {
                home: "/home/me".into(),
                arch: "x86_64".into(),
                installed: false,
            })
        );
        assert_eq!(parse_probe("home=/home/me\n"), None);
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
