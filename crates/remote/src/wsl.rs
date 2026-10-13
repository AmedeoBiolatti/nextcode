//! Starting `next_code_agent` inside a WSL distro.
//!
//! Everything goes through `wsl.exe` with stdin/stdout pipes: the agent
//! binary is installed by piping it into the distro, and the agent's
//! protocol runs over the same kind of pipes. Windows never touches the
//! distro's files (`\\wsl.localhost`) and the distro needs no network port.

use std::{env, ffi::OsStr, io, path::PathBuf, process::Command};

use crate::launch::{self, Connection, Target, no_window};

fn wsl_exe() -> PathBuf {
    env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join("System32").join("wsl.exe"))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("wsl.exe"))
}

/// A `wsl.exe` command with UTF-8 output and no console window.
pub(crate) fn wsl(args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Command {
    let mut command = Command::new(wsl_exe());
    command.args(args).env("WSL_UTF8", "1");
    no_window(&mut command);
    command
}

pub(crate) fn not_available(err: io::Error) -> io::Error {
    io::Error::new(
        err.kind(),
        format!(
            "WSL is not available ({}: {err}). Install it with `wsl --install`.",
            wsl_exe().display()
        ),
    )
}

/// Names of the installed distros.
pub fn distros() -> io::Result<Vec<String>> {
    let output = launch::run(
        wsl(["--list", "--quiet"]),
        None,
        "Listing WSL distros",
        not_available,
    )?;
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

/// A WSL distro as an agent target.
pub struct Distro(pub String);

impl Target for Distro {
    fn describe(&self) -> String {
        format!("WSL distro '{}'", self.0)
    }

    fn shell(&self, login: bool, script: &str, args: &[&str]) -> Command {
        let mut command = wsl(["-d", &self.0, "--exec", "/bin/sh"]);
        command
            .arg(if login { "-lc" } else { "-c" })
            .arg(script)
            .arg("sh")
            .args(args);
        command
    }

    fn spawn_error(&self, err: io::Error) -> io::Error {
        not_available(err)
    }

    /// An unknown distro is the usual reason; name the installed ones.
    fn diagnose(&self) -> Option<io::Error> {
        let installed = distros().ok()?;
        if installed
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&self.0))
        {
            return None;
        }
        let distro = &self.0;
        Some(io::Error::new(
            io::ErrorKind::NotFound,
            if installed.is_empty() {
                format!("WSL distro '{distro}' is not installed, and no distros are.")
            } else {
                format!(
                    "WSL distro '{distro}' is not installed. Installed distros: {}.",
                    installed.join(", ")
                )
            },
        ))
    }
}

/// Installs (if needed) and starts the agent in `distro`, then opens `root`
/// (an absolute Linux path, or one starting with `~`).
pub fn connect(distro: &str, root: &str) -> io::Result<Connection> {
    launch::connect(&Distro(distro.to_owned()), root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distro_lists_are_parsed_from_wsl_output() {
        let utf16: Vec<u8> = "\u{feff}Ubuntu\r\ndocker-desktop\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(
            parse_distros(&launch::decode_output(&utf16)),
            ["Ubuntu", "docker-desktop"]
        );
        assert_eq!(
            parse_distros(&launch::decode_output(b"Ubuntu-24.04\n\n")),
            ["Ubuntu-24.04"]
        );
    }
}
