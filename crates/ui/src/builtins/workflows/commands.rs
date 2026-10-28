//! The processes a workflow step starts: its `run` command in a terminal
//! (PowerShell on Windows, the user's login shell elsewhere) and its
//! `check` as a plain process, with `${…}` variables, `env_file`s, `env`
//! and `cwd` applied as run configurations do; where its artifacts are;
//! and its probes (a health check's command, an HTTP(S) URL, a port),
//! answered where the step runs.

use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use remote::RunOutput;
use run::{
    CommandLine,
    shell::{self, Shell},
};
use workflow::{ArtifactState, ArtifactTarget, CheckResult, Probe, Step};

use crate::project::Project;

/// How long a check may run without `timeout`.
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
    /// The step's `env_file` variables ([`Place::read_env_files`]): in its
    /// commands' environment (its `env` wins) and for `${env:…}`.
    files: Vec<(String, String)>,
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
                    files: Vec::new(),
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
                files: Vec::new(),
            }),
        }
    }

    /// Reads the step's `env_file`s where it runs (relative to its `cwd`),
    /// in order: a later file's variable wins. A missing required file is
    /// an error. Blocks.
    pub(crate) fn read_env_files(
        &mut self,
        step: &Step,
        project: Option<&Project>,
    ) -> Result<(), String> {
        if step.env_file.is_empty() {
            return Ok(());
        }
        let cwd = self.cwd(step)?;
        let project = self.project(project);
        let mut files = Vec::new();
        for file in &step.env_file {
            let path = self.with_context(|context| {
                let base = run::Context {
                    workspace_folder: &cwd,
                    ..*context
                };
                Ok::<_, String>(base.absolute(&context.substitute(&file.path)?))
            })?;
            match project.read_text(Path::new(&path)) {
                Ok(text) => files.extend(
                    workflow::dotenv::parse(&text)
                        .map_err(|err| format!("env_file {path}: {err}"))?,
                ),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound && !file.required => {}
                Err(err) => return Err(format!("env_file {path}: {err}")),
            }
        }
        self.files = files;
        Ok(())
    }

    /// Expands `${…}` variables as they read where the step runs (its
    /// `env_file` variables first).
    fn with_context<R>(&self, then: impl FnOnce(&run::Context) -> R) -> R {
        let lookup = |name: &str| {
            self.files
                .iter()
                .rev()
                .find(|(known, _)| known == name)
                .map(|(_, value)| value.clone())
                .or_else(|| self.env.get(name).cloned())
        };
        then(&run::Context {
            workspace_folder: &self.root,
            file: None,
            line: None,
            separator: self.separator,
            env: &lookup,
            user_home: self.home.as_deref(),
        })
    }

    /// The step's working directory, absolute.
    fn cwd(&self, step: &Step) -> Result<String, String> {
        self.with_context(|context| {
            Ok(context.absolute(&context.substitute(step.cwd.as_deref().unwrap_or(""))?))
        })
    }

    /// One shell line that changes to the step's `cwd`, exports its `env`
    /// and runs `command`, with variables expanded.
    pub(crate) fn line(&self, step: &Step, command: &str) -> Result<String, String> {
        let cwd = self.cwd(step)?;
        self.with_context(|context| {
            // The env files' variables, then `env` (which wins).
            let mut env: std::collections::BTreeMap<String, String> =
                self.files.iter().cloned().collect();
            for (name, value) in &step.env {
                env.insert(name.clone(), context.substitute(value)?);
            }
            let command = run::Command {
                line: CommandLine::Shell(context.substitute(command)?),
                cwd,
                env,
            };
            Ok(shell::terminal_line(&command, &self.root, self.shell))
        })
    }

    /// The step's artifacts with variables expanded, paths made absolute
    /// against its `cwd`.
    fn artifacts(&self, step: &Step) -> Result<Vec<(String, bool)>, String> {
        let cwd = self.cwd(step)?;
        self.with_context(|context| {
            let base = run::Context {
                workspace_folder: &cwd,
                ..*context
            };
            step.artifacts
                .iter()
                .map(|artifact| match &artifact.target {
                    ArtifactTarget::Path(path) => {
                        Ok((base.absolute(&context.substitute(path)?), false))
                    }
                    ArtifactTarget::Url(url) => Ok((context.substitute(url)?, true)),
                })
                .collect()
        })
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

    /// Where the step's files and programs are: its project, else this
    /// machine (in the home folder).
    fn project(&self, project: Option<&Project>) -> Project {
        project
            .cloned()
            .unwrap_or_else(|| Project::local(PathBuf::from(&self.root)))
    }

    /// Runs `line` as a check where the step runs, within `timeout`.
    /// Blocks.
    fn run_check_line(
        &self,
        project: Option<&Project>,
        line: String,
        timeout: Duration,
    ) -> Result<RunOutput, String> {
        let (program, args) = self.check_program(line);
        self.project(project)
            .run_program(&program, &args, &[], None, timeout)
            .map_err(|err| err.to_string())
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
    let mut place = Place::of(project)?;
    place.read_env_files(step, project)?;
    let line = place.line(step, command)?;
    Ok(place.terminal_program(line))
}

/// Runs the step's check where it runs. Blocks.
pub(crate) fn run_check(step: &Step, project: Option<&Project>) -> CheckResult {
    let Some(check) = step.check.as_deref() else {
        return CheckResult::Error("the step has no `check`".into());
    };
    let timeout = step.timeout.unwrap_or(CHECK_TIMEOUT);
    match run_as_check(step, check, project, timeout) {
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

/// Runs `command` as a check: in the step's shell, `cwd`, env files and
/// `env`, where it runs (without a project: here, in the home folder),
/// within `timeout`. Blocks.
fn run_as_check(
    step: &Step,
    command: &str,
    project: Option<&Project>,
    timeout: Duration,
) -> Result<RunOutput, String> {
    let mut place = Place::of(project)?;
    place.read_env_files(step, project)?;
    let line = place.line(step, command)?;
    place.run_check_line(project, line, timeout)
}

/// The step's artifacts as seen where it runs: variables expanded, paths
/// absolute (against its `cwd`) and looked up there. Blocks.
pub(crate) fn check_artifacts(
    step: &Step,
    project: Option<&Project>,
) -> Result<Vec<ArtifactState>, String> {
    let place = Place::of(project)?;
    let resolved = place.artifacts(step)?;
    let project = place.project(project);
    Ok(resolved
        .into_iter()
        .map(|(resolved, url)| ArtifactState {
            exists: url
                || project
                    .kind(Path::new(&resolved))
                    .is_ok_and(|kind| kind.is_some()),
            resolved,
        })
        .collect())
}

/// Probes a step where it runs: a command (in its shell, `cwd`, env files
/// and `env`; exit code 0 passes), an HTTP(S) URL (an accepted status) or
/// a port; `Err` says why it failed. Blocks up to about `timeout`.
pub(crate) fn probe(
    step: &Step,
    probe: &Probe,
    project: Option<&Project>,
    timeout: Duration,
) -> Result<(), String> {
    match probe {
        Probe::Port(port) => {
            if port_listening(project, *port) {
                Ok(())
            } else {
                Err(format!("nothing listens on port {port}"))
            }
        }
        Probe::Command(command) => {
            let output = run_as_check(step, command, project, timeout)?;
            if output.code == Some(0) {
                return Ok(());
            }
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let last = text
                .lines()
                .rev()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or_default()
                .to_owned();
            let code = output
                .code
                .map_or("killed".to_owned(), |code| format!("exit code {code}"));
            Err(if last.is_empty() {
                code
            } else {
                format!("{code}: {last}")
            })
        }
        Probe::Http(http) => {
            let place = Place::of(project)?;
            let url = place.with_context(|context| context.substitute(&http.url))?;
            let status = http_status(project, &place, &url, timeout)?;
            if http.accepts(status) {
                Ok(())
            } else {
                Err(format!("{url} answered {status}"))
            }
        }
    }
}

/// `scheme`, `host`, `port` and the path (with its query) of an http(s)
/// URL.
fn split_url(url: &str) -> Result<(&str, String, u16, String), String> {
    let (scheme, rest) = url
        .split_once("://")
        .filter(|(scheme, _)| matches!(*scheme, "http" | "https"))
        .ok_or_else(|| format!("`{url}` is not an http(s) URL"))?;
    let (authority, path) = match rest.find(['/', '?', '#']) {
        Some(at) if rest[at..].starts_with('/') => (&rest[..at], rest[at..].to_owned()),
        Some(at) => (&rest[..at], format!("/{}", &rest[at..])),
        None => (rest, "/".to_owned()),
    };
    let path = path.split('#').next().unwrap_or("/").to_owned();
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = match authority.strip_prefix('[') {
        // [::1]:8000
        Some(rest) => {
            let (host, after) = rest
                .split_once(']')
                .ok_or_else(|| format!("`{url}`: unclosed `[`"))?;
            (host.to_owned(), after.strip_prefix(':'))
        }
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_owned(), Some(port)),
            None => (authority.to_owned(), None),
        },
    };
    let port = match port {
        Some(port) => port
            .parse()
            .map_err(|_| format!("`{url}`: `{port}` is not a port"))?,
        None if scheme == "https" => 443,
        None => 80,
    };
    Ok((scheme, host, port, path))
}

/// The status of a `GET` on `url` where the step runs: plain HTTP
/// directly (through the agent's tunnel for a remote project); HTTPS with
/// `curl` there (PowerShell on this Windows machine). Blocks up to about
/// `timeout`.
pub(crate) fn http_status(
    project: Option<&Project>,
    place: &Place,
    url: &str,
    timeout: Duration,
) -> Result<u16, String> {
    let (scheme, host, port, path) = split_url(url)?;
    if scheme == "https" {
        return https_status(project, place, url, timeout);
    }
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUser-Agent: next_code\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    let deadline = Instant::now() + timeout;
    let mut stream = match project.and_then(Project::agent) {
        // From inside WSL or the container: a tunnel from the agent.
        Some(agent) => {
            let listener = TcpListener::bind("127.0.0.1:0").map_err(|err| err.to_string())?;
            let mut client =
                TcpStream::connect(listener.local_addr().map_err(|err| err.to_string())?)
                    .map_err(|err| err.to_string())?;
            let (relayed, _) = listener.accept().map_err(|err| err.to_string())?;
            // Open until the answer is read.
            let _tunnel = agent
                .tunnel(&host, port, relayed, |_| {})
                .map_err(|err| format!("{host}:{port}: {err}"))?;
            return exchange(&mut client, &request, deadline);
        }
        None => {
            let addresses: Vec<_> = (host.as_str(), port)
                .to_socket_addrs()
                .map_err(|err| format!("{host}: {err}"))?
                .collect();
            let mut connected = None;
            let mut last_error = format!("{host}:{port}: no address");
            for address in addresses {
                match TcpStream::connect_timeout(&address, timeout.min(Duration::from_secs(3))) {
                    Ok(stream) => {
                        connected = Some(stream);
                        break;
                    }
                    Err(err) => last_error = format!("{host}:{port}: {err}"),
                }
            }
            connected.ok_or(last_error)?
        }
    };
    stream.set_nodelay(true).ok();
    exchange(&mut stream, &request, deadline)
}

/// Sends `request` on `stream` and reads the status line.
fn exchange(stream: &mut TcpStream, request: &str, deadline: Instant) -> Result<u16, String> {
    let left = || {
        deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(50))
    };
    stream.set_write_timeout(Some(left())).ok();
    stream
        .write_all(request.as_bytes())
        .map_err(|err| format!("could not send the request: {err}"))?;
    let mut head = Vec::new();
    let mut buffer = [0; 1024];
    while !head.windows(2).any(|pair| pair == b"\r\n") {
        stream.set_read_timeout(Some(left())).ok();
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => head.extend_from_slice(&buffer[..read]),
            Err(err) => return Err(format!("no answer: {err}")),
        }
        if head.len() > 8192 || Instant::now() >= deadline {
            break;
        }
    }
    let line = String::from_utf8_lossy(&head);
    let status = line
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok());
    status.ok_or_else(|| {
        if head.is_empty() {
            "the connection closed without an answer".into()
        } else {
            "not an HTTP answer".into()
        }
    })
}

/// HTTPS: `curl` where the step runs (PowerShell's `Invoke-WebRequest`
/// on this Windows machine).
fn https_status(
    project: Option<&Project>,
    place: &Place,
    url: &str,
    timeout: Duration,
) -> Result<u16, String> {
    let seconds = timeout.as_secs().max(1);
    let line = match place.shell {
        Shell::PowerShell => format!(
            "try {{ (Invoke-WebRequest -UseBasicParsing -TimeoutSec {seconds} -Uri '{}').StatusCode }} \
             catch {{ if ($_.Exception.Response) {{ [int]$_.Exception.Response.StatusCode }} else {{ 0 }} }}",
            url.replace('\'', "''")
        ),
        Shell::Posix => format!(
            "curl -sk -o /dev/null -w '%{{http_code}}' --max-time {seconds} '{}'",
            url.replace('\'', "'\\''")
        ),
    };
    let output = place.run_check_line(project, line, timeout + Duration::from_secs(5))?;
    if output.code == Some(127) {
        return Err("https checks need curl where the step runs".into());
    }
    match String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u16>()
    {
        Ok(0) | Err(_) => Err(format!("{url}: no answer")),
        Ok(status) => Ok(status),
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
            depends_on: Vec::new(),
            project: None,
            run: Some("serve".into()),
            terminal: None,
            check: None,
            on_fail: None,
            ready: workflow::Ready::Exit,
            timeout: None,
            open_urls: false,
            env: env
                .iter()
                .map(|&(name, value)| (name.to_owned(), value.to_owned()))
                .collect::<BTreeMap<_, _>>(),
            cwd: cwd.map(Into::into),
            stop_grace: workflow::DEFAULT_STOP_GRACE,
            artifacts: Vec::new(),
            env_file: Vec::new(),
            restart: workflow::Restart::default(),
            healthcheck: None,
            condition: None,
            steps: Vec::new(),
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
            files: Vec::new(),
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
    fn artifacts_resolve_against_the_cwd_with_variables() {
        let artifact = |label: Option<&str>, target: ArtifactTarget| workflow::Artifact {
            label: label.map(Into::into),
            target,
        };
        let mut eval = step(Some("runs"), &[]);
        eval.artifacts = vec![
            artifact(None, ArtifactTarget::Path("out/metrics.json".into())),
            artifact(
                Some("abs"),
                ArtifactTarget::Path("${userHome}/log.txt".into()),
            ),
            artifact(
                None,
                ArtifactTarget::Url("https://board/${env:TOKEN}".into()),
            ),
        ];
        assert_eq!(
            place(Shell::Posix).artifacts(&eval).unwrap(),
            [
                ("/home/me/app/runs/out/metrics.json".to_owned(), false),
                ("/home/me/log.txt".to_owned(), false),
                ("https://board/s3cr'et".to_owned(), true),
            ]
        );
        assert_eq!(
            place(Shell::PowerShell).artifacts(&eval).unwrap()[0].0,
            r"C:\src\app\runs\out\metrics.json"
        );
        eval.artifacts = vec![artifact(None, ArtifactTarget::Path("${file}".into()))];
        assert!(place(Shell::Posix).artifacts(&eval).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn artifacts_are_looked_up_where_the_step_runs() {
        let root = std::env::temp_dir().join(format!("nc-artifacts-{}", std::process::id()));
        std::fs::create_dir_all(root.join("out")).unwrap();
        std::fs::write(root.join("out/here.txt"), "x").unwrap();
        let project = Project::local(root.clone());
        let mut eval = step(None, &[]);
        eval.artifacts = vec![
            workflow::Artifact {
                label: None,
                target: ArtifactTarget::Path("out/here.txt".into()),
            },
            workflow::Artifact {
                label: None,
                target: ArtifactTarget::Path("out/missing.txt".into()),
            },
        ];
        let checked = check_artifacts(&eval, Some(&project)).unwrap();
        let exists: Vec<bool> = checked.iter().map(|checked| checked.exists).collect();
        assert_eq!(exists, [true, false]);
        assert_eq!(
            checked[0].resolved,
            root.join("out/here.txt").to_string_lossy()
        );
        std::fs::remove_dir_all(root).ok();
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

    #[test]
    fn urls_split_into_scheme_host_port_and_path() {
        assert_eq!(
            split_url("http://localhost:8000/health?full=1#top").unwrap(),
            ("http", "localhost".into(), 8000, "/health?full=1".into())
        );
        assert_eq!(
            split_url("https://example.com").unwrap(),
            ("https", "example.com".into(), 443, "/".into())
        );
        assert_eq!(
            split_url("http://[::1]:9000/x").unwrap(),
            ("http", "::1".into(), 9000, "/x".into())
        );
        assert_eq!(split_url("http://u:p@host/a").unwrap().1, "host");
        assert!(split_url("ftp://x").is_err());
        assert!(split_url("http://x:port/").is_err());
    }

    /// A one-request HTTP server answering `status`; returns its port.
    fn serve_once(status: u16) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]).into_owned();
            assert!(request.starts_with("GET /health HTTP/1.1\r\n"), "{request}");
            write!(
                stream,
                "HTTP/1.1 {status} Whatever\r\nContent-Length: 0\r\n\r\n"
            )
            .unwrap();
        });
        port
    }

    #[test]
    fn http_probes_read_the_status_where_the_step_runs() {
        let mut step = step(None, &[]);
        let place = Place::of(None).unwrap();
        let port = serve_once(204);
        let url = format!("http://127.0.0.1:{port}/health");
        assert_eq!(
            http_status(None, &place, &url, Duration::from_secs(5)),
            Ok(204)
        );
        let port = serve_once(503);
        let probe = Probe::Http(workflow::HttpProbe {
            url: format!("http://127.0.0.1:{port}/health"),
            status: Vec::new(),
        });
        let failed = super::probe(&step, &probe, None, Duration::from_secs(5)).unwrap_err();
        assert!(failed.ends_with("answered 503"), "{failed}");
        // Nothing listening: why.
        let closed = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let probe = Probe::Http(workflow::HttpProbe {
            url: format!("http://127.0.0.1:{closed}/health"),
            status: Vec::new(),
        });
        assert!(super::probe(&step, &probe, None, Duration::from_secs(2)).is_err());
        step.run = None;
        assert!(super::probe(&step, &Probe::Port(closed), None, Duration::from_secs(1)).is_err());
    }

    #[test]
    fn env_files_are_read_relative_to_the_cwd_and_env_wins() {
        let dir = std::env::temp_dir().join(format!("nc-envfile-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(
            dir.join("sub/.env"),
            "A=from-file\nB='kept # this'\nTOKEN=file\n",
        )
        .unwrap();
        std::fs::write(dir.join("sub/more.env"), "A=later\n").unwrap();
        let mut step = step(Some("sub"), &[("TOKEN", "own ${env:B}")]);
        step.env_file = vec![
            workflow::EnvFile {
                path: ".env".into(),
                required: true,
            },
            workflow::EnvFile {
                path: "more.env".into(),
                required: true,
            },
            workflow::EnvFile {
                path: "missing.env".into(),
                required: false,
            },
        ];
        let mut place = Place {
            root: dir.to_string_lossy().into_owned(),
            separator: std::path::MAIN_SEPARATOR,
            ..place(Shell::Posix)
        };
        place.read_env_files(&step, None).unwrap();
        assert_eq!(
            place.files,
            [
                ("A".to_owned(), "from-file".to_owned()),
                ("B".to_owned(), "kept # this".to_owned()),
                ("TOKEN".to_owned(), "file".to_owned()),
                ("A".to_owned(), "later".to_owned()),
            ]
        );
        let line = place.line(&step, "run").unwrap();
        assert!(
            line.contains("A=later") && !line.contains("A=from-file"),
            "{line}"
        );
        assert!(
            line.contains("TOKEN='own kept # this'"),
            "env wins and reads the files: {line}"
        );
        step.env_file.push(workflow::EnvFile {
            path: "nope.env".into(),
            required: true,
        });
        let error = place.read_env_files(&step, None).unwrap_err();
        assert!(error.contains("nope.env"), "{error}");
        std::fs::remove_dir_all(dir).ok();
    }
}
