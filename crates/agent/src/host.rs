//! Host services on the agent's machine: git credentials and ssh-agent
//! borrowed from the editor's machine, as VS Code's dev containers do.
//!
//! While the editor runs a [`proto::Request::HostServices`] stream, the
//! agent listens on Unix sockets in `<runtime dir>/host/` (private to the
//! user, see [`crate::pinpoint::runtime_dir`]):
//!
//! - `<pid>-<n>-git.sock`: `next_code_agent git-credential <operation>` (the
//!   `credential.helper` [`configure_git`] sets) writes the operation on one
//!   line, then git's credential text, and half-closes; the agent forwards
//!   it to the editor and writes back its answer (nothing: git goes on with
//!   its other helpers or prompts).
//! - `<pid>-<n>-ssh.sock`: an ssh-agent socket; each request message is
//!   forwarded to the editor, which relays it to its own ssh-agent over a
//!   connection of its own per socket connection.
//!
//! Processes the agent starts get `SSH_AUTH_SOCK` and [`GIT_SOCKET_ENV`]
//! ([`process_env`]); git started elsewhere finds the newest git socket.

use std::{
    collections::HashMap,
    env, fs,
    io::{self, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use parking_lot::Mutex;
use proto::{
    Event, HostRequest, HostServices,
    credential::{self, Credential, Operation},
};

use crate::pinpoint::{check_private, peer_uid, private_dir, runtime_dirs, uid};

/// Names the git socket a helper uses, skipping discovery.
pub const GIT_SOCKET_ENV: &str = "NEXT_CODE_GIT_CREDENTIAL_SOCKET";
/// OpenSSH's limit on an agent message.
const MAX_SSH_MESSAGE: usize = 256 * 1024;
/// `SSH_AGENT_FAILURE`, sent when the editor could not answer.
const SSH_AGENT_FAILURE: [u8; 5] = [0, 0, 0, 1, 5];
/// How long a helper waits for the editor: `get` may show the editor's
/// machine's sign-in window (Git Credential Manager).
const HELPER_TIMEOUT: Duration = Duration::from_secs(300);

type Reply = Result<Vec<u8>, String>;
type Pending = Arc<Mutex<HashMap<u64, mpsc::Sender<Reply>>>>;

/// Variables of the running services, for the processes the agent starts.
static ENV: LazyLock<Mutex<Vec<(String, String)>>> = LazyLock::new(Mutex::default);

/// Environment for processes the agent starts (their request's own
/// variables win).
pub(crate) fn process_env() -> Vec<(String, String)> {
    ENV.lock().clone()
}

/// The directory of the host sockets below `runtime_dir`.
fn host_dir(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join("host")
}

struct Shared {
    pending: Pending,
    stopped: AtomicBool,
    calls: AtomicU64,
    forward: Box<dyn Fn(Event) + Send + Sync>,
}

impl Shared {
    /// Forwards `request` to the editor and waits for its answer.
    fn call(&self, request: HostRequest) -> Reply {
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().insert(call, tx);
        // Stopped meanwhile: the drop that clears `pending` may have passed.
        if self.stopped.load(Ordering::Acquire) {
            self.pending.lock().remove(&call);
        } else {
            (self.forward)(Event::HostCall { call, request });
        }
        rx.recv()
            .unwrap_or_else(|_| Err("the editor stopped serving".into()))
    }
}

/// Running host services; dropping them stops listening, removes the
/// sockets and fails the calls still waiting for the editor.
pub struct Services {
    sockets: Vec<PathBuf>,
    env: Vec<(String, String)>,
    shared: Arc<Shared>,
}

impl Services {
    /// Listens in `<runtime_dir>/host` under `name` for what `services`
    /// lends; `forward` sends events to the editor, whose answers come
    /// through [`Services::reply`].
    pub fn start(
        runtime_dir: &Path,
        name: &str,
        services: &HostServices,
        forward: impl Fn(Event) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            pending: Pending::default(),
            stopped: AtomicBool::new(false),
            calls: AtomicU64::new(1),
            forward: Box::new(forward),
        });
        let mut started = Self {
            sockets: Vec::new(),
            env: Vec::new(),
            shared,
        };
        if !services.git_credentials && !services.ssh_agent {
            return Ok(started);
        }
        private_dir(runtime_dir)?;
        let dir = host_dir(runtime_dir);
        private_dir(&dir)?;
        if services.git_credentials {
            let socket = dir.join(format!("{name}-git.sock"));
            listen(&socket, &started.shared, serve_git)?;
            started.sockets.push(socket.clone());
            started
                .env
                .push((GIT_SOCKET_ENV.into(), socket.to_string_lossy().into()));
        }
        if services.ssh_agent {
            let socket = dir.join(format!("{name}-ssh.sock"));
            listen(&socket, &started.shared, serve_ssh)?;
            started.sockets.push(socket.clone());
            started
                .env
                .push(("SSH_AUTH_SOCK".into(), socket.to_string_lossy().into()));
        }
        let mut env = ENV.lock();
        env.retain(|(key, _)| !started.env.iter().any(|(ours, _)| ours == key));
        env.extend(started.env.iter().cloned());
        Ok(started)
    }

    /// The sockets' environment (`SSH_AUTH_SOCK`, [`GIT_SOCKET_ENV`]).
    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }

    /// Delivers the editor's answer to call `call`.
    pub fn reply(&self, call: u64, reply: Reply) {
        if let Some(waiting) = self.shared.pending.lock().remove(&call) {
            waiting.send(reply).ok();
        }
    }
}

impl Drop for Services {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        ENV.lock().retain(|variable| !self.env.contains(variable));
        for socket in &self.sockets {
            // Wakes the accept loop so it sees `stopped`.
            UnixStream::connect(socket).ok();
            fs::remove_file(socket).ok();
        }
        // Waiting calls answer that the editor is gone.
        self.shared.pending.lock().clear();
    }
}

/// Accepts connections on `socket` on a thread, each served on its own.
fn listen(socket: &Path, shared: &Arc<Shared>, serve: fn(UnixStream, &Shared)) -> io::Result<()> {
    // Left behind by a killed agent whose pid came round again.
    fs::remove_file(socket).ok();
    let listener = UnixListener::bind(socket)
        .map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", socket.display())))?;
    let shared = shared.clone();
    thread::Builder::new()
        .name("agent-host".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if shared.stopped.load(Ordering::Acquire) {
                    return;
                }
                let Ok(stream) = stream else { continue };
                // The directory keeps others out; this also refuses them
                // where root created it for everybody.
                if !matches!(peer_uid(&stream), Some(peer) if peer == uid() || peer == 0) {
                    continue;
                }
                let shared = shared.clone();
                thread::Builder::new()
                    .name("agent-host-call".into())
                    .spawn(move || serve(stream, &shared))
                    .ok();
            }
        })?;
    Ok(())
}

/// One helper call: `<operation>\n<credential>`, answered with the
/// editor's credential text (or nothing).
fn serve_git(mut stream: UnixStream, shared: &Shared) {
    let mut input = Vec::new();
    let limit = (credential::MAX_TEXT + 16) as u64;
    if (&stream).take(limit).read_to_end(&mut input).is_err() || input.len() as u64 >= limit {
        return;
    }
    let input = String::from_utf8_lossy(&input);
    let (operation, text) = input.split_once('\n').unwrap_or((&input, ""));
    let Some(operation) = Operation::parse(operation.trim()) else {
        return;
    };
    let request = HostRequest::GitCredential {
        operation,
        credential: Credential::parse(text).to_text(),
    };
    if let Ok(answer) = shared.call(request) {
        stream.write_all(&answer).ok();
    }
}

/// One ssh-agent connection: each length-prefixed request goes to the
/// editor and its reply comes back, in order.
fn serve_ssh(mut stream: UnixStream, shared: &Shared) {
    static CONNECTIONS: AtomicU64 = AtomicU64::new(1);
    let connection = CONNECTIONS.fetch_add(1, Ordering::Relaxed);
    loop {
        let mut length = [0; 4];
        if stream.read_exact(&mut length).is_err() {
            break;
        }
        let length = u32::from_be_bytes(length) as usize;
        if length == 0 || length > MAX_SSH_MESSAGE {
            break;
        }
        let mut message = vec![0; length];
        if stream.read_exact(&mut message).is_err() {
            break;
        }
        let written = match shared.call(HostRequest::SshAgent {
            connection,
            message,
        }) {
            Ok(reply) if !reply.is_empty() && reply.len() <= MAX_SSH_MESSAGE => stream
                .write_all(&(reply.len() as u32).to_be_bytes())
                .and_then(|()| stream.write_all(&reply)),
            _ => stream.write_all(&SSH_AGENT_FAILURE),
        };
        if written.is_err() {
            break;
        }
    }
    if !shared.stopped.load(Ordering::Acquire) {
        (shared.forward)(Event::SshAgentClosed { connection });
    }
}

/// The git sockets to try: [`GIT_SOCKET_ENV`], else every live agent's,
/// newest first.
fn git_sockets() -> Vec<PathBuf> {
    if let Some(socket) = env::var_os(GIT_SOCKET_ENV).filter(|socket| !socket.is_empty()) {
        return vec![PathBuf::from(socket)];
    }
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = runtime_dirs()
        .iter()
        // Another user's (or a world-writable) directory is not trusted.
        .filter(|dir| check_private(dir).is_ok() && check_private(&host_dir(dir)).is_ok())
        .filter_map(|dir| fs::read_dir(host_dir(dir)).ok())
        .flat_map(|entries| entries.flatten())
        .filter(|entry| entry.file_name().to_string_lossy().ends_with("-git.sock"))
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    found.into_iter().map(|(_, path)| path).collect()
}

/// Asks the editor serving this machine for `operation` on `credential`;
/// `None` when no editor serves git credentials or it has none to give.
pub fn credential_call(operation: Operation, credential: &str) -> Option<String> {
    let explicit = env::var_os(GIT_SOCKET_ENV).is_some_and(|socket| !socket.is_empty());
    for socket in git_sockets() {
        let mut stream = match UnixStream::connect(&socket) {
            Ok(stream) => stream,
            Err(err)
                if !explicit
                    && matches!(
                        err.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
            {
                // A killed agent's.
                fs::remove_file(&socket).ok();
                continue;
            }
            Err(_) => continue,
        };
        stream.set_read_timeout(Some(HELPER_TIMEOUT)).ok();
        let request = format!(
            "{}\n{}",
            operation.as_str(),
            Credential::parse(credential).to_text()
        );
        stream.write_all(request.as_bytes()).ok()?;
        stream.shutdown(std::net::Shutdown::Write).ok()?;
        let mut answer = Vec::new();
        (&stream)
            .take(credential::MAX_TEXT as u64)
            .read_to_end(&mut answer)
            .ok()?;
        let answer = Credential::parse(&String::from_utf8_lossy(&answer));
        return (!answer.is_empty()).then(|| answer.to_text());
    }
    None
}

/// `next_code_agent git-credential <get|store|erase>`: git's credential
/// helper, served by the editor that opened this machine's project. Always
/// succeeds: without an answer git uses its other helpers or prompts.
pub fn credential_cli(args: &[String]) -> ExitCode {
    // Operations git may add later are ignored, as the protocol asks.
    let Some(operation) = args.first().and_then(|argument| Operation::parse(argument)) else {
        return ExitCode::SUCCESS;
    };
    if env::var_os(credential::NO_FORWARDING_ENV).is_some() {
        return ExitCode::SUCCESS;
    }
    let mut input = Vec::new();
    io::stdin()
        .take(credential::MAX_TEXT as u64)
        .read_to_end(&mut input)
        .ok();
    if let Some(answer) = credential_call(operation, &String::from_utf8_lossy(&input)) {
        let mut stdout = io::stdout();
        stdout
            .write_all(answer.as_bytes())
            .and_then(|()| stdout.flush())
            .ok();
    }
    ExitCode::SUCCESS
}

/// What the user's git config holds of what [`configure_git`] sets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct GitConfig {
    pub helpers: Vec<String>,
    pub name: Option<String>,
    pub email: Option<String>,
    pub azure_use_http_path: Option<String>,
}

/// Whether a `credential.helper` value is one [`configure_git`] set (see
/// [`helper_value`]; whatever the agent's path).
fn is_ours(helper: &str) -> bool {
    helper.starts_with("!'") && helper.ends_with("' git-credential")
}

/// Matches our helper values (`git config`'s value pattern, an ERE).
const OURS_PATTERN: &str = "^!'.*' git-credential$";
/// Azure's organisation is part of the path, required by the host's GCM.
const AZURE_USE_HTTP_PATH: &str = "credential.https://dev.azure.com.useHttpPath";

/// The `credential.helper` value running `program`.
pub(crate) fn helper_value(program: &Path) -> String {
    let quoted = program.to_string_lossy().replace('\'', r"'\''");
    format!("!'{quoted}' git-credential")
}

/// The `git config` changes (arguments after the scope) that make
/// `current` lend what `services` says with `helper`: our helper only where
/// the user has none (refreshed when it is ours but differs; removed when
/// lending stops), the editor's identity only where unset, and Azure's
/// organisation path when using our helper unless the user configured it.
pub(crate) fn plan(current: &GitConfig, services: &HostServices, helper: &str) -> Vec<Vec<String>> {
    let args = |args: &[&str]| args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    let mut changes = Vec::new();
    let all_ours = current.helpers.iter().all(|value| is_ours(value));
    let any_ours = current.helpers.iter().any(|value| is_ours(value));
    if services.git_credentials {
        if all_ours && current.helpers != [helper] {
            if any_ours {
                changes.push(args(&["--unset-all", "credential.helper", OURS_PATTERN]));
            }
            changes.push(args(&["--add", "credential.helper", helper]));
        }
        if all_ours && current.azure_use_http_path.is_none() {
            changes.push(args(&[AZURE_USE_HTTP_PATH, "true"]));
        }
    } else if any_ours {
        changes.push(args(&["--unset-all", "credential.helper", OURS_PATTERN]));
    }
    for (key, value, set) in [
        ("user.name", &services.git_user_name, &current.name),
        ("user.email", &services.git_user_email, &current.email),
    ] {
        if let (Some(value), None) = (value.as_deref().filter(|v| !v.is_empty()), set) {
            changes.push(args(&[key, value]));
        }
    }
    changes
}

/// Runs `git config <scope> <args>`; `None` when git cannot run.
fn git_config(scope: &[String], args: &[&str]) -> Option<(bool, String)> {
    let output = Command::new("git")
        .arg("config")
        .args(scope)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    Some((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    ))
}

/// Reads the user's git config in `scope` (`--global`, or `--file F`);
/// `None` without git.
pub(crate) fn read_git_config(scope: &[String]) -> Option<GitConfig> {
    let value = |key: &str| {
        git_config(scope, &["--get", key]).map(|(ok, out)| {
            ok.then(|| out.trim_end_matches('\n').to_owned())
                .filter(|value| !value.is_empty())
        })
    };
    let (_, helpers) = git_config(scope, &["--get-all", "credential.helper"])?;
    Some(GitConfig {
        helpers: helpers.lines().map(str::to_owned).collect(),
        name: value("user.name")?,
        email: value("user.email")?,
        azure_use_http_path: value(AZURE_USE_HTTP_PATH)?,
    })
}

/// Sets up the user's git config in `scope` for `services` (see [`plan`]),
/// with the helper running `program`. Without git nothing happens.
pub(crate) fn configure_git(scope: &[String], services: &HostServices, program: &Path) {
    let Some(current) = read_git_config(scope) else {
        return;
    };
    for change in plan(&current, services, &helper_value(program)) {
        let change: Vec<&str> = change.iter().map(String::as_str).collect();
        git_config(scope, &change);
    }
}

/// The program git's helper runs: the stable `next_code_agent` link next
/// to an installed build (it survives editor updates), else this one.
pub(crate) fn helper_program() -> PathBuf {
    let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("next_code_agent"));
    match (exe.parent(), exe.file_name().and_then(|name| name.to_str())) {
        (Some(dir), Some(name)) if name.starts_with("next_code_agent-") => {
            dir.join("next_code_agent")
        }
        _ => exe,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("next_code_host_{name}_{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        dir
    }

    fn lend(git: bool, name: Option<&str>, email: Option<&str>) -> HostServices {
        HostServices {
            git_credentials: git,
            ssh_agent: false,
            git_user_name: name.map(str::to_owned),
            git_user_email: email.map(str::to_owned),
        }
    }

    #[test]
    fn azure_credentials_keep_the_organisation_path() {
        let dir = temp_dir("azure-path");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("gitconfig");
        let scope = vec!["--file".to_owned(), file.to_string_lossy().into_owned()];
        let store = dir.join("credentials");
        fs::write(
            &store,
            "https://other:wrong-organisation@dev.azure.com/OtherOrg/Analytics/_git/repo\n\
             https://ada:correct-organisation@dev.azure.com/Contoso/Analytics/_git/repo\n",
        )
        .unwrap();
        // A path-sensitive provider behind the installed agent helper.
        let program = dir.join("next_code_agent");
        fs::write(
            &program,
            format!(
                "#!/bin/sh\nshift\nexec git credential-store --file='{}' \"$@\"\n",
                store.to_string_lossy().replace('\'', "'\\''")
            ),
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        configure_git(&scope, &lend(true, None, None), &program);
        let mut git = Command::new("git")
            .current_dir(&dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &file)
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(["credential", "fill"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        git.stdin
            .take()
            .unwrap()
            .write_all(b"url=https://dev.azure.com/Contoso/Analytics/_git/repo\n\n")
            .unwrap();
        let output = git.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let answer = Credential::parse(&String::from_utf8(output.stdout).unwrap());
        assert_eq!(answer.get("username"), Some("ada"));
        assert_eq!(answer.get("password"), Some("correct-organisation"));
        assert_eq!(answer.get("path"), Some("Contoso/Analytics/_git/repo"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn git_config_is_written_once_and_removable() {
        if Command::new("git").arg("--version").output().is_err() {
            eprintln!("skipped: no git");
            return;
        }
        let dir = temp_dir("config");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("gitconfig");
        let scope = vec!["--file".to_owned(), file.to_string_lossy().into_owned()];
        let program = Path::new("/opt/nc/next_code_agent");
        configure_git(&scope, &lend(true, Some("Ada L"), Some("ada@x")), program);
        configure_git(&scope, &lend(true, Some("Other"), None), program);
        let config = read_git_config(&scope).unwrap();
        assert_eq!(config.helpers, [helper_value(program)]);
        assert_eq!(config.name.as_deref(), Some("Ada L"));
        assert_eq!(config.email.as_deref(), Some("ada@x"));
        // An explicit user choice is not overridden, even with our helper.
        git_config(&scope, &[AZURE_USE_HTTP_PATH, "false"]);
        configure_git(&scope, &lend(true, None, None), program);
        assert_eq!(
            read_git_config(&scope)
                .unwrap()
                .azure_use_http_path
                .as_deref(),
            Some("false")
        );

        configure_git(&scope, &lend(false, None, None), program);
        let config = read_git_config(&scope).unwrap();
        assert!(config.helpers.is_empty());
        assert_eq!(config.name.as_deref(), Some("Ada L"));

        // A user's helper is left alone.
        git_config(&scope, &["credential.helper", "store"]);
        git_config(&scope, &["--unset", AZURE_USE_HTTP_PATH]);
        configure_git(&scope, &lend(true, None, None), program);
        assert_eq!(read_git_config(&scope).unwrap().helpers, ["store"]);
        assert!(
            read_git_config(&scope)
                .unwrap()
                .azure_use_http_path
                .is_none()
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn helper_calls_and_ssh_messages_reach_the_editor() {
        let dir = temp_dir("sockets");
        let services: Arc<Mutex<Option<Services>>> = Arc::default();
        let editor = services.clone();
        let closed = Arc::new(Mutex::new(Vec::new()));
        let seen_closed = closed.clone();
        let started = Services::start(
            &dir,
            "1-1",
            &HostServices {
                git_credentials: true,
                ssh_agent: true,
                ..HostServices::default()
            },
            move |event| match event {
                Event::HostCall { call, request } => {
                    let editor = editor.clone();
                    thread::spawn(move || {
                        let reply = match request {
                            HostRequest::GitCredential {
                                operation: Operation::Get,
                                credential,
                            } => Ok(format!("{credential}username=u\npassword=p\nquit=1\n")
                                .into_bytes()),
                            HostRequest::GitCredential { .. } => Ok(Vec::new()),
                            HostRequest::SshAgent { message, .. } if message == [11] => {
                                Ok(vec![12, 0, 0, 0, 0])
                            }
                            HostRequest::SshAgent { .. } => Err("refused".into()),
                        };
                        editor.lock().as_ref().unwrap().reply(call, reply);
                    });
                }
                Event::SshAgentClosed { connection } => seen_closed.lock().push(connection),
                _ => {}
            },
        )
        .unwrap();
        let env = started.env().to_vec();
        *services.lock() = Some(started);
        assert!(process_env().iter().any(|v| env.contains(v)));
        let socket = |key: &str| PathBuf::from(&env.iter().find(|(k, _)| k == key).unwrap().1);
        let git = socket(GIT_SOCKET_ENV);
        assert_eq!(
            fs::metadata(dir.join("host")).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let mut stream = UnixStream::connect(&git).unwrap();
        stream
            .write_all(b"get\nprotocol=https\nhost=h\nquit=1\n\n")
            .unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        // The agent passes the text on; the helper keeps known keys only.
        assert_eq!(
            Credential::parse(&answer).to_text(),
            "protocol=https\nhost=h\nusername=u\npassword=p\n"
        );

        let mut ssh = UnixStream::connect(socket("SSH_AUTH_SOCK")).unwrap();
        ssh.write_all(&[0, 0, 0, 1, 11]).unwrap();
        let mut reply = [0; 9];
        ssh.read_exact(&mut reply).unwrap();
        assert_eq!(reply, [0, 0, 0, 5, 12, 0, 0, 0, 0]);
        ssh.write_all(&[0, 0, 0, 1, 13]).unwrap();
        let mut failure = [0; 5];
        ssh.read_exact(&mut failure).unwrap();
        assert_eq!(failure, SSH_AGENT_FAILURE);
        drop(ssh);
        while closed.lock().is_empty() {
            thread::sleep(Duration::from_millis(5));
        }

        services.lock().take();
        assert!(!git.exists());
        assert!(!process_env().iter().any(|v| env.contains(v)));
        fs::remove_dir_all(&dir).ok();
    }
}
