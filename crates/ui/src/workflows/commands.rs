//! The processes a workflow step starts: its `run` command in a terminal
//! (PowerShell on Windows, the user's login shell elsewhere) and its
//! `check` as a plain process, with `${…}` variables, `env` and `cwd`
//! applied as run configurations do.

use std::{collections::HashMap, net::TcpStream, path::PathBuf, time::Duration};

use run::{
    CommandLine,
    shell::{self, Shell},
};
use workflow::{CheckResult, Step};

use crate::project::Project;

/// How long a check may run without `timeout_s`.
const CHECK_TIMEOUT: Duration = Duration::from_secs(120);
const ENV_TIMEOUT: Duration = Duration::from_secs(20);

/// Runs `$1` in the user's login shell, interactive so its rc file sets
/// PATH as in a terminal (`$SHELL`, else the passwd entry, else `sh`).
const LOGIN_SHELL: &str = "s=${SHELL:-$(getent passwd \"$(id -un)\" 2>/dev/null | cut -d: -f7)}; \
                           exec \"${s:-/bin/sh}\" -lic \"$1\"";

/// Where a step's processes run, as far as quoting and variables go.
pub(crate) struct Place {
    shell: Shell,
    /// The project root, or the home folder without a project.
    root: String,
    separator: char,
    env: HashMap<String, String>,
    home: Option<String>,
}

impl Place {
    /// Reads what variables need (the environment where a remote project
    /// lives). Blocks.
    pub(crate) fn of(project: Option<&Project>) -> Result<Self, String> {
        let home = dirs::home_dir().map(|home| home.to_string_lossy().into_owned());
        match project {
            Some(project) if project.is_remote() => {
                let output = project
                    .run_program("env", &[], &[], None, ENV_TIMEOUT)
                    .map_err(|err| format!("could not read the environment: {err}"))?;
                let env = parse_env(&String::from_utf8_lossy(&output.stdout));
                Ok(Self {
                    shell: Shell::Posix,
                    root: project.root().to_string_lossy().into_owned(),
                    separator: '/',
                    home: env.get("HOME").cloned(),
                    env,
                })
            }
            _ => Ok(Self {
                shell: if cfg!(windows) {
                    Shell::PowerShell
                } else {
                    Shell::Posix
                },
                root: match project {
                    Some(project) => project.root().to_string_lossy().into_owned(),
                    None => home.clone().unwrap_or_else(|| ".".into()),
                },
                separator: if cfg!(windows) { '\\' } else { '/' },
                env: std::env::vars().collect(),
                home,
            }),
        }
    }

    /// One shell line that changes to the step's `cwd`, exports its `env`
    /// and runs `command`, with variables expanded.
    pub(crate) fn line(&self, step: &Step, command: &str) -> Result<String, String> {
        let lookup = |name: &str| self.env.get(name).cloned();
        let context = run::Context {
            workspace_folder: &self.root,
            file: None,
            line: None,
            separator: self.separator,
            env: &lookup,
            user_home: self.home.as_deref(),
        };
        let cwd = context.absolute(&context.substitute(step.cwd.as_deref().unwrap_or(""))?);
        let env = step
            .env
            .iter()
            .map(|(name, value)| Ok((name.clone(), context.substitute(value)?)))
            .collect::<Result<_, String>>()?;
        let command = run::Command {
            line: CommandLine::Shell(context.substitute(command)?),
            cwd,
            env,
        };
        Ok(shell::terminal_line(&command, &self.root, self.shell))
    }

    /// The program and arguments for a terminal running `line`.
    pub(crate) fn terminal_program(&self, line: String) -> (String, Vec<String>) {
        match self.shell {
            Shell::PowerShell => (
                "powershell.exe".into(),
                vec!["-NoLogo".into(), "-Command".into(), line],
            ),
            Shell::Posix => (
                "/bin/sh".into(),
                vec!["-c".into(), LOGIN_SHELL.into(), "sh".into(), line],
            ),
        }
    }

    /// The program and arguments for a check running `line`.
    fn check_program(&self, line: String) -> (String, Vec<String>) {
        match self.shell {
            Shell::PowerShell => (
                "powershell.exe".into(),
                vec![
                    "-NoLogo".into(),
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    line,
                ],
            ),
            Shell::Posix => ("/bin/sh".into(), vec!["-c".into(), line]),
        }
    }
}

/// `env` output as a map (`NAME=value` lines; continuation lines of
/// multi-line values are skipped).
fn parse_env(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let (name, value) = line.split_once('=')?;
            let valid =
                !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            valid.then(|| (name.to_owned(), value.to_owned()))
        })
        .collect()
}

/// The terminal program for the step's `run`. Blocks (remote environment).
pub(crate) fn run_program(
    step: &Step,
    project: Option<&Project>,
) -> Result<(String, Vec<String>), String> {
    let command = step.run.as_deref().ok_or("the step has no `run`")?;
    let place = Place::of(project)?;
    let line = place.line(step, command)?;
    Ok(place.terminal_program(line))
}

/// Runs the step's check where it runs. Blocks.
pub(crate) fn run_check(step: &Step, project: Option<&Project>) -> CheckResult {
    let Some(check) = step.check.as_deref() else {
        return CheckResult::Error("the step has no `check`".into());
    };
    let result = Place::of(project).and_then(|place| {
        let line = place.line(step, check)?;
        let (program, args) = place.check_program(line);
        let timeout = step.timeout_s.map_or(CHECK_TIMEOUT, Duration::from_secs);
        // Without a project the check runs here, in the home folder.
        let host = Project::local(PathBuf::from(&place.root));
        project
            .unwrap_or(&host)
            .run_program(&program, &args, &[], None, timeout)
            .map_err(|err| err.to_string())
    });
    match result {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            CheckResult::Exited {
                code: output.code,
                output: text,
            }
        }
        Err(error) => CheckResult::Error(error),
    }
}

/// Whether something accepts TCP connections on `port` here (local
/// projects and steps without one). Blocks up to a second.
pub(crate) fn port_open_locally(port: u16) -> bool {
    ["127.0.0.1", "[::1]"].iter().any(|host| {
        format!("{host}:{port}").parse().is_ok_and(|address| {
            TcpStream::connect_timeout(&address, Duration::from_millis(400)).is_ok()
        })
    })
}

/// Whether `port` listens where `project` lives (remote: the agent's
/// listening sockets; else a connection attempt). Blocks.
pub(crate) fn port_listening(project: Option<&Project>, port: u16) -> bool {
    match project.and_then(Project::agent) {
        Some(agent) => agent
            .listening_ports()
            .is_ok_and(|ports| ports.iter().any(|listening| listening.port == port)),
        None => port_open_locally(port),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn step(cwd: Option<&str>, env: &[(&str, &str)]) -> Step {
        Step {
            id: "s".into(),
            title: None,
            needs: Vec::new(),
            project: None,
            run: Some("serve".into()),
            terminal: None,
            check: None,
            on_fail: None,
            ready: workflow::Ready::Exit,
            timeout_s: None,
            open_urls: false,
            env: env
                .iter()
                .map(|&(name, value)| (name.to_owned(), value.to_owned()))
                .collect::<BTreeMap<_, _>>(),
            cwd: cwd.map(Into::into),
        }
    }

    fn place(shell: Shell) -> Place {
        let (root, separator) = match shell {
            Shell::Posix => ("/home/me/app", '/'),
            Shell::PowerShell => (r"C:\src\app", '\\'),
        };
        Place {
            shell,
            root: root.into(),
            separator,
            env: [("TOKEN".to_owned(), "s3cr'et".to_owned())].into(),
            home: Some(
                if separator == '/' {
                    "/home/me"
                } else {
                    r"C:\Users\me"
                }
                .into(),
            ),
        }
    }

    #[test]
    fn lines_change_directory_export_env_and_expand_variables() {
        let posix = place(Shell::Posix);
        assert_eq!(
            posix.line(&step(None, &[]), "make run").unwrap(),
            "make run"
        );
        assert_eq!(
            posix
                .line(
                    &step(
                        Some("web"),
                        &[("API", "${env:TOKEN}"), ("HOME_DIR", "${userHome}")]
                    ),
                    "npm start -- ${workspaceFolder}"
                )
                .unwrap(),
            "cd /home/me/app/web && export API='s3cr'\\''et' && export HOME_DIR=/home/me \
             && npm start -- /home/me/app"
        );
        let windows = place(Shell::PowerShell);
        assert_eq!(
            windows
                .line(&step(Some("web"), &[("A", "1")]), "npm start")
                .unwrap(),
            "Set-Location -LiteralPath C:\\src\\app\\web; $env:A = '1'; npm start"
        );
        assert!(posix.line(&step(None, &[]), "${file}").is_err());
    }

    #[test]
    fn terminals_use_powershell_or_an_interactive_login_shell() {
        let (program, args) = place(Shell::PowerShell).terminal_program("npm start".into());
        assert_eq!(program, "powershell.exe");
        assert_eq!(args, ["-NoLogo", "-Command", "npm start"]);
        let (program, args) = place(Shell::Posix).terminal_program("npm start".into());
        assert_eq!(program, "/bin/sh");
        assert_eq!(args[3], "npm start");
        assert!(args[1].contains("-lic \"$1\""));
    }

    #[test]
    fn env_output_is_parsed_skipping_continuation_lines() {
        let env = parse_env("HOME=/root\nPATH=/usr/bin:/bin\nMULTI=a\nsecond line\n=bad\nX=\n");
        assert_eq!(env["HOME"], "/root");
        assert_eq!(env["PATH"], "/usr/bin:/bin");
        assert_eq!(env["MULTI"], "a");
        assert_eq!(env["X"], "");
        assert_eq!(env.len(), 4);
    }

    #[cfg(unix)]
    #[test]
    fn checks_run_here_and_report_exit_codes_and_output() {
        let mut passing = step(None, &[("WORD", "hello")]);
        passing.check = Some("echo $WORD".into());
        assert_eq!(
            run_check(&passing, None),
            CheckResult::Exited {
                code: Some(0),
                output: "hello\n".into()
            }
        );
        let mut failing = step(None, &[]);
        failing.check = Some("echo nope >&2; exit 4".into());
        assert_eq!(
            run_check(&failing, None),
            CheckResult::Exited {
                code: Some(4),
                output: "nope\n".into()
            }
        );
    }

    #[test]
    fn local_ports_are_probed_by_connecting() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_open_locally(port));
        drop(listener);
        assert!(!port_open_locally(port));
    }
}
