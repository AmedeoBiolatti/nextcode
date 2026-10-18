//! The editor's side of host services (see [`proto::HostServices`]): git
//! credential calls from the project's machine are answered by this
//! machine's `git credential fill|approve|reject` (on Windows `git.exe`, so
//! Git Credential Manager), and ssh-agent messages are relayed to this
//! machine's ssh-agent (`SSH_AUTH_SOCK`, on Windows the OpenSSH agent's
//! pipe). Only `http`/`https` credentials are lent; anything else, and
//! anything git cannot fill without a terminal, gets an empty answer, so
//! git on the project's machine falls back to its own prompt. Credentials
//! are never logged.

use std::{
    collections::HashMap,
    ffi::OsString,
    io::{self, Read, Write},
    path::PathBuf,
    process::{Command, ExitStatus, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use proto::{
    HostRequest, HostServices,
    credential::{self, Credential, Operation},
};

use crate::{AgentClient, HostReply, StreamHandle};

/// `git credential fill` may show Git Credential Manager's sign-in window.
pub const FILL_TIMEOUT: Duration = Duration::from_secs(180);
/// `approve`/`reject` and config reads.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(30);
/// OpenSSH's limit on an agent message.
const MAX_SSH_MESSAGE: usize = 256 * 1024;
/// Windows OpenSSH's agent.
pub const WINDOWS_SSH_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

/// The git this machine runs: a program and arguments before the
/// subcommand (tests add `-c credential.helper=…`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Git {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl Default for Git {
    fn default() -> Self {
        Self {
            // `git.exe` on Windows (Command adds the extension).
            program: PathBuf::from("git"),
            args: Vec::new(),
        }
    }
}

impl Git {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            // Never a terminal prompt: the editor may have no console, and
            // the project's git prompts itself when nothing is lent.
            .env("GIT_TERMINAL_PROMPT", "0")
            .env(credential::NO_FORWARDING_ENV, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        command
    }

    /// `git credential <fill|approve|reject>` for `operation`.
    pub fn credential_command(&self, operation: Operation) -> Command {
        let mut command = self.command();
        command.args(["credential", operation.git_command()]);
        command
    }

    /// `git config --global --get <key>`.
    fn config_command(&self, key: &str) -> Command {
        let mut command = self.command();
        command.args(["config", "--global", "--get", key]);
        command
    }

    /// This machine's `user.name` and `user.email`; `None` when git does not
    /// run here.
    pub fn identity(&self) -> Option<(Option<String>, Option<String>)> {
        let read = |key: &str| -> io::Result<Option<String>> {
            let (status, out) = run(self.config_command(key), b"", GIT_TIMEOUT)?;
            let value = String::from_utf8_lossy(&out).trim().to_owned();
            Ok((status.success() && !value.is_empty()).then_some(value))
        };
        Some((read("user.name").ok()?, read("user.email").ok()?))
    }
}

/// Runs `command` with `input` on its stdin; killed after `timeout`.
fn run(mut command: Command, input: &[u8], timeout: Duration) -> io::Result<(ExitStatus, Vec<u8>)> {
    let mut child = command.spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut stdout = child.stdout.take().expect("piped stdout");
    // Credentials are small: the write cannot fill the pipe.
    stdin.write_all(input).ok();
    drop(stdin);
    let reader = thread::spawn(move || {
        let mut out = Vec::new();
        (&mut stdout)
            .take(credential::MAX_TEXT as u64)
            .read_to_end(&mut out)
            .ok();
        out
    });
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status, reader.join().unwrap_or_default()));
        }
        if Instant::now() >= deadline {
            child.kill().ok();
            child.wait().ok();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "git timed out"));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Where this machine's ssh-agent listens: `SSH_AUTH_SOCK` (`ssh_auth_sock`)
/// if set, else on Windows the OpenSSH agent's pipe.
pub fn ssh_agent_path(ssh_auth_sock: Option<OsString>, windows: bool) -> Option<PathBuf> {
    match ssh_auth_sock.filter(|path| !path.is_empty()) {
        Some(path) => Some(PathBuf::from(path)),
        None if windows => Some(PathBuf::from(WINDOWS_SSH_AGENT_PIPE)),
        None => None,
    }
}

trait Duplex: Read + Write + Send {}
impl<T: Read + Write + Send> Duplex for T {}

fn connect_ssh_agent(path: &std::path::Path) -> io::Result<Box<dyn Duplex>> {
    #[cfg(unix)]
    return Ok(Box::new(std::os::unix::net::UnixStream::connect(path)?));
    // A named pipe opens like a file (synchronous I/O suits the agent's
    // one-request-one-reply exchange).
    #[cfg(not(unix))]
    Ok(Box::new(
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?,
    ))
}

/// One forwarded ssh-agent connection's own connection here.
type SshConnection = Arc<Mutex<Option<Box<dyn Duplex>>>>;

/// Answers host services calls with this machine's git and ssh-agent.
pub struct Host {
    git: Git,
    ssh_agent: Option<PathBuf>,
    connections: Mutex<HashMap<u64, SshConnection>>,
}

impl Host {
    pub fn new(git: Git, ssh_agent: Option<PathBuf>) -> Self {
        Self {
            git,
            ssh_agent,
            connections: Mutex::default(),
        }
    }

    /// This machine's git and ssh-agent (one probe connection decides
    /// whether an agent runs).
    pub fn detect() -> Self {
        let ssh_agent = ssh_agent_path(std::env::var_os("SSH_AUTH_SOCK"), cfg!(windows))
            .filter(|path| connect_ssh_agent(path).is_ok());
        Self::new(Git::default(), ssh_agent)
    }

    /// What to lend: credentials when git runs here, the ssh-agent when one
    /// runs, and the identity git has here. Blocks while git runs.
    pub fn services(&self) -> HostServices {
        let identity = self.git.identity();
        let (git_user_name, git_user_email) = identity.clone().unwrap_or_default();
        HostServices {
            git_credentials: identity.is_some(),
            ssh_agent: self.ssh_agent.is_some(),
            git_user_name,
            git_user_email,
        }
    }

    /// Answers `request` on a thread of its own.
    pub fn handle(self: &Arc<Self>, request: HostRequest, reply: HostReply) {
        let host = self.clone();
        thread::Builder::new()
            .name("host-call".into())
            .spawn(move || {
                reply.send(match request {
                    HostRequest::GitCredential {
                        operation,
                        credential,
                    } => Ok(host.credential(operation, &credential)),
                    HostRequest::SshAgent {
                        connection,
                        message,
                    } => host.ssh_agent(connection, &message),
                });
            })
            .ok();
    }

    /// The forwarded ssh-agent connection `connection` ended.
    pub fn closed(&self, connection: u64) {
        self.connections.lock().remove(&connection);
    }

    /// The credential protocol answer (empty: nothing to lend).
    pub fn credential(&self, operation: Operation, text: &str) -> Vec<u8> {
        let credential = Credential::parse(text);
        let lendable = matches!(credential.get("protocol"), Some("https" | "http"))
            && credential.get("host").is_some_and(|host| !host.is_empty());
        if !lendable {
            return Vec::new();
        }
        let timeout = match operation {
            Operation::Get => FILL_TIMEOUT,
            Operation::Store | Operation::Erase => GIT_TIMEOUT,
        };
        let command = self.git.credential_command(operation);
        match run(command, credential.to_text().as_bytes(), timeout) {
            Ok((status, out)) if status.success() && operation == Operation::Get => {
                let answer = Credential::parse(&String::from_utf8_lossy(&out));
                if answer.get("password").is_some() {
                    answer.to_text().into_bytes()
                } else {
                    Vec::new()
                }
            }
            // No credential (no helper has one, prompts are off): the
            // project's git asks the user. Not logged: the user sees git's
            // own prompt.
            Ok(_) => Vec::new(),
            Err(err) => {
                eprintln!(
                    "next_code: git credential {}: {err}",
                    operation.git_command()
                );
                Vec::new()
            }
        }
    }

    /// Relays one request message to this machine's ssh-agent over the
    /// connection kept for `connection`.
    fn ssh_agent(&self, connection: u64, message: &[u8]) -> Result<Vec<u8>, String> {
        let Some(path) = &self.ssh_agent else {
            return Err("no ssh-agent on the editor's machine".into());
        };
        let slot = self
            .connections
            .lock()
            .entry(connection)
            .or_default()
            .clone();
        let mut stream = slot.lock();
        if stream.is_none() {
            *stream = Some(connect_ssh_agent(path).map_err(|err| err.to_string())?);
        }
        let result = exchange(stream.as_mut().expect("connected"), message);
        if result.is_err() {
            *stream = None;
        }
        result.map_err(|err| format!("ssh-agent: {err}"))
    }
}

/// Writes one length-prefixed message and reads the reply's body.
fn exchange(stream: &mut dyn Duplex, message: &[u8]) -> io::Result<Vec<u8>> {
    let mut frame = Vec::with_capacity(4 + message.len());
    frame.extend_from_slice(&(message.len() as u32).to_be_bytes());
    frame.extend_from_slice(message);
    stream.write_all(&frame)?;
    stream.flush()?;
    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_SSH_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad reply length",
        ));
    }
    let mut reply = vec![0; length];
    stream.read_exact(&mut reply)?;
    Ok(reply)
}

/// Lends `host` to `client`'s machine as `services` says.
pub fn serve_with(
    client: &Arc<AgentClient>,
    host: Arc<Host>,
    services: HostServices,
) -> io::Result<StreamHandle> {
    let closing = host.clone();
    client.serve_host(
        services,
        move |request, reply| host.handle(request, reply),
        move |connection| closing.closed(connection),
    )
}

/// Lends this machine's git credentials, ssh-agent and identity to
/// `client`'s machine; `enabled` false lends nothing and removes the git
/// helper an earlier session configured there. Blocks while git runs.
pub fn serve(client: &Arc<AgentClient>, enabled: bool) -> io::Result<StreamHandle> {
    if !enabled {
        return client.serve_host(HostServices::default(), |_, _| {}, |_| {});
    }
    let host = Arc::new(Host::detect());
    let services = host.services();
    serve_with(client, host, services)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn credential_commands_never_prompt_and_never_forward_back() {
        let git = Git::default();
        for (operation, verb) in [
            (Operation::Get, "fill"),
            (Operation::Store, "approve"),
            (Operation::Erase, "reject"),
        ] {
            let command = git.credential_command(operation);
            assert_eq!(command.get_program(), "git");
            assert_eq!(args(&command), ["credential", verb]);
            let envs: Vec<_> = command
                .get_envs()
                .map(|(key, value)| (key.to_owned(), value.map(|v| v.to_owned())))
                .collect();
            assert!(envs.contains(&("GIT_TERMINAL_PROMPT".into(), Some("0".into()))));
            assert!(envs.contains(&(credential::NO_FORWARDING_ENV.into(), Some("1".into()))));
        }
        let custom = Git {
            program: PathBuf::from(r"C:\Program Files\Git\cmd\git.exe"),
            args: vec!["-c".into(), "credential.helper=manager".into()],
        };
        let command = custom.credential_command(Operation::Get);
        assert_eq!(command.get_program(), r"C:\Program Files\Git\cmd\git.exe");
        assert_eq!(
            args(&command),
            ["-c", "credential.helper=manager", "credential", "fill"]
        );
        assert_eq!(
            args(&git.config_command("user.name")),
            ["config", "--global", "--get", "user.name"]
        );
    }

    #[test]
    fn the_ssh_agent_is_ssh_auth_sock_else_the_windows_pipe() {
        assert_eq!(
            ssh_agent_path(Some("/tmp/agent.1".into()), false),
            Some(PathBuf::from("/tmp/agent.1"))
        );
        assert_eq!(ssh_agent_path(Some("".into()), false), None);
        assert_eq!(ssh_agent_path(None, false), None);
        assert_eq!(
            ssh_agent_path(None, true),
            Some(PathBuf::from(r"\\.\pipe\openssh-ssh-agent"))
        );
        assert_eq!(
            ssh_agent_path(Some(r"\\.\pipe\other".into()), true),
            Some(PathBuf::from(r"\\.\pipe\other"))
        );
    }

    #[test]
    fn only_http_credentials_with_a_host_are_looked_up() {
        // A git that would answer anything: the request must not reach it.
        let host = Host::new(
            Git {
                program: PathBuf::from("/nonexistent/git"),
                args: Vec::new(),
            },
            None,
        );
        for text in [
            "protocol=ssh\nhost=h\n",
            "protocol=cert\npath=/x\n",
            "protocol=https\n",
            "host=h\n",
        ] {
            assert!(host.credential(Operation::Get, text).is_empty(), "{text}");
        }
    }
}
