//! Help > Check for Updates: the latest GitHub release against this build,
//! installed with that release's own install script (what the README's
//! one-liners run) when this editor runs from the folder the scripts
//! install into.

use std::{
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use remote::RunOutput;

use crate::project::run_local;

/// Where releases are published.
pub const REPOSITORY: &str = "AmedeoBiolatti/nextcode";
/// Asking GitHub for the latest release.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// Downloading the release (about 20 MB) and installing it.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);

/// An install running, so a second one (another window's) waits its turn.
static INSTALLING: AtomicBool = AtomicBool::new(false);

/// The release page of `tag`, with its notes.
pub fn release_page(tag: &str) -> String {
    format!("https://github.com/{REPOSITORY}/releases/tag/{tag}")
}

/// The latest release's tag (`v0.2.0`; pre-releases are never the
/// latest). Blocks.
pub fn latest_tag() -> Result<String, String> {
    let url = format!("https://api.github.com/repos/{REPOSITORY}/releases/latest");
    // curl.exe ships with Windows 10 and later.
    let program = if cfg!(windows) { "curl.exe" } else { "curl" };
    let mut command = Command::new(program);
    command.args(["-fsSL", "-H", "Accept: application/vnd.github+json", &url]);
    let output = run(command, program, CHECK_TIMEOUT)?;
    let release: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("unexpected answer from GitHub: {err}"))?;
    let tag = release["tag_name"]
        .as_str()
        .ok_or("GitHub's answer names no release")?;
    if !valid_tag(tag) {
        return Err(format!("unexpected release tag {tag:?}"));
    }
    Ok(tag.to_owned())
}

/// `tag` names a newer version than `current` (this build's
/// `CARGO_PKG_VERSION`): `v0.2.0` over `0.1.0`, and a release over its
/// own pre-releases (`v0.2.0` over `0.2.0-rc.1`).
pub fn is_newer(tag: &str, current: &str) -> bool {
    match (
        version(tag.strip_prefix('v').unwrap_or(tag)),
        version(current),
    ) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

/// `1.2.3` or `1.2.3-pre` as `([1, 2, 3], is a release)`, which orders a
/// pre-release before its release.
fn version(text: &str) -> Option<([u64; 3], bool)> {
    let (numbers, release) = match text.split_once('-') {
        Some((numbers, _)) => (numbers, false),
        None => (text, true),
    };
    let mut parts = numbers.split('.').map(|part| part.parse().ok());
    let version = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some((version, release))
}

/// A tag safe in a URL and a command line: `v` and a version.
fn valid_tag(tag: &str) -> bool {
    tag.strip_prefix('v').and_then(version).is_some()
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
}

/// Where the install scripts put a release: `~/.local/share/next_code/bin`
/// (under `$XDG_DATA_HOME` when set) or `%LOCALAPPDATA%\next_code\bin`.
/// `None` where no release is built.
pub fn install_dir() -> Option<PathBuf> {
    if !cfg!(all(
        any(target_os = "linux", windows),
        target_arch = "x86_64"
    )) {
        return None;
    }
    Some(dirs::data_local_dir()?.join("next_code").join("bin"))
}

/// This editor runs from [`install_dir`], so installing a release
/// updates it.
pub fn installed() -> bool {
    let running = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent()?.canonicalize().ok());
    let dir = install_dir().and_then(|dir| dir.canonicalize().ok());
    running.is_some() && running == dir
}

/// The installed launcher: started without arguments, it reopens the
/// windows saved on quitting in the build it belongs to.
pub fn installed_launcher() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "next_code.exe"
    } else {
        "next_code"
    };
    Some(install_dir()?.join(name))
}

/// Installs release `tag` into [`install_dir`] with its own install
/// script, as the README's one-liners do: the download is checked against
/// the release's `SHA256SUMS`, and files are replaced by renaming, so
/// this editor runs on unchanged until it restarts. Blocks.
pub fn install(tag: &str) -> Result<(), String> {
    if !valid_tag(tag) {
        return Err(format!("unexpected release tag {tag:?}"));
    }
    if INSTALLING.swap(true, Ordering::AcqRel) {
        return Err("an update is already being installed".into());
    }
    let script =
        format!("https://raw.githubusercontent.com/{REPOSITORY}/{tag}/scripts/install-release");
    let (program, mut command) = if cfg!(windows) {
        ("powershell.exe", Command::new("powershell.exe"))
    } else {
        ("sh", Command::new("/bin/sh"))
    };
    if cfg!(windows) {
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &format!(
                "$ErrorActionPreference = 'Stop'; $env:NEXT_CODE_VERSION = '{tag}'; \
                 Invoke-RestMethod '{script}.ps1' | Invoke-Expression"
            ),
        ]);
    } else {
        // Fetched whole first: a failed download must not run as an empty script.
        command.args([
            "-c",
            r#"script=$(curl -fsSL "$1") && printf '%s\n' "$script" | NEXT_CODE_VERSION="$2" sh"#,
            "sh",
            &format!("{script}.sh"),
            tag,
        ]);
    }
    let result = run(command, program, INSTALL_TIMEOUT).map(drop);
    INSTALLING.store(false, Ordering::Release);
    result
}

/// Runs `command` to completion; a failure is described by what it said.
fn run(command: Command, program: &str, timeout: Duration) -> Result<RunOutput, String> {
    let output =
        run_local(command, program, &std::env::temp_dir(), None, timeout, None).map_err(|err| {
            match err.kind() {
                std::io::ErrorKind::NotFound => format!("{program} is not installed"),
                _ => format!("{program}: {err}"),
            }
        })?;
    if output.code == Some(0) {
        return Ok(output);
    }
    // The first lines: curl's one, or PowerShell's message before its trace.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(3)
        .collect();
    Err(match (message.is_empty(), output.code) {
        (false, _) => message.join("\n"),
        (true, Some(code)) => format!("{program} exited with code {code}"),
        (true, None) => format!("{program} was stopped"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_versions_are_found() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("v0.1.1", "0.1.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("v0.10.0", "0.9.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.2.0"));
        // A release is newer than its pre-releases, not the other way.
        assert!(is_newer("v0.2.0", "0.2.0-rc.1"));
        assert!(!is_newer("v0.2.0-rc.1", "0.2.0"));
        // Anything unexpected is not an update.
        assert!(!is_newer("latest", "0.1.0"));
        assert!(!is_newer("v1.2", "0.1.0"));
        assert!(!is_newer("v1.2.3.4", "0.1.0"));
        assert!(!is_newer("v0.2.0", "dev"));
    }

    #[test]
    fn only_plain_version_tags_reach_a_command_line() {
        assert!(valid_tag("v0.2.0"));
        assert!(valid_tag("v1.0.0-rc.1"));
        assert!(!valid_tag("0.2.0"));
        assert!(!valid_tag("v0.2"));
        assert!(!valid_tag("v0.2.0'; rm -rf ~; '"));
        assert!(!valid_tag("v0.2.0-$(id)"));
        assert!(!valid_tag("v0.2.0 x"));
        assert!(install("v0.2.0'; x").is_err());
    }

    #[test]
    fn releases_install_where_the_scripts_put_them() {
        let Some(dir) = install_dir() else {
            return;
        };
        assert!(dir.ends_with(std::path::Path::new("next_code").join("bin")));
        let launcher = installed_launcher().unwrap();
        assert_eq!(launcher.parent(), Some(dir.as_path()));
        assert_eq!(
            release_page("v0.2.0"),
            "https://github.com/AmedeoBiolatti/nextcode/releases/tag/v0.2.0"
        );
    }
}
