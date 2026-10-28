//! Starting `next_code_agent` on a machine reached with OpenSSH's `ssh`
//! client: a host as `ssh` takes it (an alias from `~/.ssh/config`, a
//! Tailscale peer's name, `user@address`), so the user's ssh configuration
//! (keys, jump hosts, proxies) applies.
//!
//! There is no terminal to type into, so `ssh` signs in without asking:
//! keys, ssh-agent, or Tailscale SSH (whose check mode prints a link to
//! open, which reaches the caller's progress). A host whose key is not
//! known yet is added to `known_hosts` unless the user's configuration
//! asks for something stricter; a changed key is always refused.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Sender},
    thread,
    time::Duration,
};

use base64::Engine;

use crate::launch::{self, Connection, no_window};

/// Overrides which `ssh` program is run.
pub const SSH_PROGRAM_ENV: &str = "NEXT_CODE_SSH";
/// Signing in through Tailscale SSH's check mode takes a visit to a web
/// page; a cold container start inside it takes a while too.
const START_TIMEOUT: Duration = Duration::from_secs(180);
/// How long reaching the machine may take (`ConnectTimeout`).
const CONNECT_TIMEOUT_SECS: u32 = 20;
/// Nested `Include`s followed while listing configured hosts.
const MAX_INCLUDE_DEPTH: usize = 8;

/// A machine as `ssh` reaches it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    /// `[user@]name`, as `ssh` takes it.
    pub destination: String,
    pub port: Option<u16>,
    /// An ssh configuration file read instead of `~/.ssh/config` (`-F`).
    pub config: Option<PathBuf>,
}

impl Host {
    /// Reads `[user@]name[:port]`; an IPv6 address takes a port as
    /// `[address]:port`.
    pub fn parse(text: &str) -> io::Result<Self> {
        let invalid = |why: &str| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("`{text}` is not an SSH host ({why})"),
            )
        };
        if text.is_empty() {
            return Err(invalid("it is empty"));
        }
        if text
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '\'' | '"'))
        {
            return Err(invalid("spaces, quotes and slashes are not allowed"));
        }
        let (user, rest) = match text.rsplit_once('@') {
            Some((user, rest)) => (Some(user), rest),
            None => (None, text),
        };
        if user.is_some_and(str::is_empty) {
            return Err(invalid("the user name is empty"));
        }
        let port = |port: &str| {
            port.parse::<u16>()
                .ok()
                .filter(|&port| port != 0)
                .ok_or_else(|| invalid("the port is not a number from 1 to 65535"))
        };
        let (name, port) = if let Some(bracketed) = rest.strip_prefix('[') {
            let (address, after) = bracketed
                .split_once(']')
                .ok_or_else(|| invalid("`[` without `]`"))?;
            match after {
                "" => (address, None),
                _ => match after.strip_prefix(':') {
                    Some(number) => (address, Some(port(number)?)),
                    None => return Err(invalid("text after `]`")),
                },
            }
        } else {
            match rest.split_once(':') {
                // Two colons or more: an IPv6 address without a port.
                Some((_, after)) if after.contains(':') => (rest, None),
                Some((name, number)) => (name, Some(port(number)?)),
                None => (rest, None),
            }
        };
        if name.is_empty() {
            return Err(invalid("the host name is empty"));
        }
        // `ssh` would read these as options.
        if name.starts_with('-') || user.is_some_and(|user| user.starts_with('-')) {
            return Err(invalid("it starts with `-`"));
        }
        Ok(Self {
            destination: match user {
                Some(user) => format!("{user}@{name}"),
                None => name.to_owned(),
            },
            port,
            config: None,
        })
    }

    /// `ssh` with the options every command to this host takes, up to the
    /// destination: what follows is the remote command.
    fn ssh(&self, options: &[&str]) -> Command {
        let mut command = Command::new(ssh_program());
        no_window(&mut command);
        if let Some(config) = &self.config {
            command.arg("-F").arg(config);
        }
        command.args(options);
        if let Some(port) = self.port {
            command.arg("-p").arg(port.to_string());
        }
        command.arg("--").arg(&self.destination);
        command
    }

    /// The `StrictHostKeyChecking` the user's configuration gives this
    /// host (`ask` when it does not say), from `ssh -G`.
    fn host_key_checking(&self) -> io::Result<String> {
        let output = launch::run(
            self.ssh(&["-G"]),
            None,
            &format!("Reading the ssh configuration for {}", self.destination),
            not_available,
        )?;
        Ok(output
            .lines()
            .find_map(|line| line.strip_prefix("stricthostkeychecking "))
            .unwrap_or("ask")
            .trim()
            .to_owned())
    }
}

impl std::fmt::Display for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.port {
            Some(port) if self.destination.contains(':') => {
                let (user, address) = match self.destination.rsplit_once('@') {
                    Some((user, address)) => (format!("{user}@"), address),
                    None => (String::new(), self.destination.as_str()),
                };
                write!(f, "{user}[{address}]:{port}")
            }
            Some(port) => write!(f, "{}:{port}", self.destination),
            None => f.write_str(&self.destination),
        }
    }
}

/// OpenSSH's client: [`SSH_PROGRAM_ENV`] when set, else Windows' own
/// when installed, else `ssh` on PATH.
fn ssh_program() -> PathBuf {
    if let Some(program) = env::var_os(SSH_PROGRAM_ENV) {
        return PathBuf::from(program);
    }
    if cfg!(windows)
        && let Some(path) = env::var_os("SystemRoot")
            .map(|root| PathBuf::from(root).join(r"System32\OpenSSH\ssh.exe"))
            .filter(|path| path.is_file())
    {
        return path;
    }
    PathBuf::from("ssh")
}

fn not_available(err: io::Error) -> io::Error {
    io::Error::new(
        err.kind(),
        format!(
            "The OpenSSH client is not available ({}: {err}). Install OpenSSH.",
            ssh_program().display()
        ),
    )
}

/// `text` as one word of a POSIX shell command.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The remote command running `script` in `/bin/sh` with `args`. `ssh`
/// hands it to the user's login shell, whichever that is (csh takes no
/// newlines in quotes, fish reads backslashes in them), so the script
/// travels in base64.
fn remote_command(login: bool, script: &str, args: &[&str]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(script);
    let flags = if login { "-lc" } else { "-c" };
    let mut command = format!("exec /bin/sh {flags} 'eval \"$(echo {encoded} | base64 -d)\"' sh");
    for arg in args {
        command.push(' ');
        command.push_str(&quote(arg));
    }
    command
}

/// A host as an agent target.
struct Target<'a> {
    host: &'a Host,
    /// Adds a new host's key to `known_hosts` (the user's configuration
    /// would ask, and nobody could answer).
    accept_new: bool,
    /// Gets the launch commands' stderr.
    stderr: Sender<String>,
}

impl launch::Target for Target<'_> {
    fn describe(&self) -> String {
        describe(&self.host.to_string())
    }

    fn shell(&self, login: bool, script: &str, args: &[&str]) -> Command {
        let timeout = format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}");
        let mut options = vec![
            // No terminal and no escape character: stdin carries the
            // agent's protocol, binary data included.
            "-T",
            "-e",
            "none",
            // Nobody can answer a prompt.
            "-o",
            "BatchMode=yes",
            "-o",
            &timeout,
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=4",
            // The user's port forwards belong to their own sessions.
            "-o",
            "ClearAllForwardings=yes",
        ];
        if self.accept_new {
            options.extend(["-o", "StrictHostKeyChecking=accept-new"]);
        }
        let mut command = self.host.ssh(&options);
        command.arg(remote_command(login, script, args));
        command
    }

    fn spawn_error(&self, err: io::Error) -> io::Error {
        not_available(err)
    }

    fn observe_stderr(&self) -> Option<launch::StderrObserver> {
        let stderr = self.stderr.clone();
        Some(Box::new(move |text| {
            stderr.send(text.to_owned()).ok();
        }))
    }

    fn start_timeout(&self) -> Duration {
        START_TIMEOUT
    }
}

/// What messages call `host`.
pub fn describe(host: &str) -> String {
    format!("SSH host '{host}'")
}

/// Installs (if needed) and starts the agent on `host`, then opens `root`
/// (an absolute path, or one starting with `~`). What `ssh` prints while
/// signing in (a Tailscale SSH sign-in link, a host key added) goes to
/// `progress`, a line at a time.
pub fn connect(host: &Host, root: &str, progress: &mut dyn FnMut(&str)) -> io::Result<Connection> {
    let accept_new = host.host_key_checking()? == "ask";
    let (stderr, lines) = mpsc::channel();
    let target = Target {
        host,
        accept_new,
        stderr,
    };
    let connected = thread::scope(|scope| {
        let connecting = scope.spawn(|| launch::connect(&target, root));
        let mut pending = String::new();
        let mut forward = |chunk: String| {
            pending.push_str(&chunk);
            while let Some(end) = pending.find('\n') {
                let line: String = pending.drain(..=end).collect();
                let line = line.trim();
                if !line.is_empty() {
                    progress(line);
                }
            }
        };
        while !connecting.is_finished() {
            if let Ok(chunk) = lines.recv_timeout(Duration::from_millis(50)) {
                forward(chunk);
            }
        }
        while let Ok(chunk) = lines.try_recv() {
            forward(chunk);
        }
        connecting
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("connecting panicked")))
    });
    connected.map_err(|err| explain(err, host))
}

/// Adds what to do about the usual reasons `ssh` fails.
fn explain(err: io::Error, host: &Host) -> io::Error {
    let text = err.to_string();
    let hint = if text.contains("Permission denied") {
        "The editor signs in without prompting: load your key into ssh-agent \
         (`ssh-add`), or name it with IdentityFile in ~/.ssh/config. Password \
         and passphrase prompts are not supported."
            .to_owned()
    } else if text.contains("Host key verification failed") {
        format!(
            "The host's key is not the one known_hosts has, or your ssh \
             configuration wants new keys confirmed: run `ssh {host}` in a \
             terminal once."
        )
    } else {
        return err;
    };
    io::Error::new(err.kind(), format!("{text}\n{hint}"))
}

/// The hosts named in the user's `~/.ssh/config` and the files it
/// includes, without patterns (`*`, `?`, `!`), in the order written.
pub fn configured_hosts() -> Vec<String> {
    let Some(dir) = home_dir().map(|home| home.join(".ssh")) else {
        return Vec::new();
    };
    let mut hosts = Vec::new();
    read_config(&dir.join("config"), &dir, 0, &mut hosts);
    hosts
}

fn home_dir() -> Option<PathBuf> {
    env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

fn read_config(path: &Path, ssh_dir: &Path, depth: usize, hosts: &mut Vec<String>) {
    if depth > MAX_INCLUDE_DEPTH {
        return;
    }
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    for (keyword, values) in config_lines(&text) {
        if keyword.eq_ignore_ascii_case("host") {
            for name in values {
                if !name.contains(['*', '?', '!']) && !hosts.contains(&name) {
                    hosts.push(name);
                }
            }
        } else if keyword.eq_ignore_ascii_case("include") {
            for pattern in values {
                for file in include_files(&pattern, ssh_dir) {
                    read_config(&file, ssh_dir, depth + 1, hosts);
                }
            }
        }
    }
}

/// A configuration's lines as (keyword, arguments): `Keyword a b`,
/// `Keyword=a`, arguments in double quotes; comments and blank lines left
/// out.
fn config_lines(text: &str) -> impl Iterator<Item = (String, Vec<String>)> + '_ {
    text.lines().filter_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let split = line.find(|c: char| c.is_whitespace() || c == '=')?;
        let (keyword, rest) = line.split_at(split);
        let rest = rest.trim_start().strip_prefix('=').unwrap_or(rest).trim();
        let mut values = Vec::new();
        let mut rest = rest;
        while !rest.is_empty() {
            let (value, after) = match rest.strip_prefix('"') {
                Some(quoted) => quoted.split_once('"').unwrap_or((quoted, "")),
                None => rest.split_once(char::is_whitespace).unwrap_or((rest, "")),
            };
            values.push(value.to_owned());
            rest = after.trim_start();
        }
        Some((keyword.to_owned(), values))
    })
}

/// The files an `Include` names: `~/` is the home folder, relative paths
/// are in `~/.ssh`, and the last component may hold `*` and `?`.
fn include_files(pattern: &str, ssh_dir: &Path) -> Vec<PathBuf> {
    let path = match pattern.strip_prefix("~/") {
        Some(rest) => match home_dir() {
            Some(home) => home.join(rest),
            None => return Vec::new(),
        },
        None => ssh_dir.join(pattern),
    };
    let Some(name) = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return Vec::new();
    };
    if !name.contains(['*', '?']) {
        return vec![path];
    }
    let Some(Ok(entries)) = path.parent().map(fs::read_dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| wildcard(&name, &entry.file_name().to_string_lossy()))
        .map(|entry| entry.path())
        .collect();
    files.sort();
    files
}

/// Whether `text` matches `pattern` (`*` any run, `?` any character).
fn wildcard(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    // Where the last `*` was, and the text position it now stands for.
    let mut star = None;
    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, t));
                p += 1;
            }
            Some(&c) if c == '?' || c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some((star_p, star_t)) => {
                    p = star_p + 1;
                    t = star_t + 1;
                    star = Some((star_p, star_t + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::Target as _;

    fn host(destination: &str, port: Option<u16>) -> Host {
        Host {
            destination: destination.into(),
            port,
            config: None,
        }
    }

    #[test]
    fn hosts_are_read_with_users_ports_and_ipv6_addresses() {
        assert_eq!(Host::parse("box").unwrap(), host("box", None));
        assert_eq!(
            Host::parse("me@box:2222").unwrap(),
            host("me@box", Some(2222))
        );
        assert_eq!(
            Host::parse("box.tail1234.ts.net").unwrap(),
            host("box.tail1234.ts.net", None)
        );
        assert_eq!(Host::parse("fd7a::1").unwrap(), host("fd7a::1", None));
        assert_eq!(
            Host::parse("me@[fd7a::1]:22").unwrap(),
            host("me@fd7a::1", Some(22))
        );
        assert_eq!(Host::parse("[fd7a::1]").unwrap(), host("fd7a::1", None));
        for text in [
            "",
            "-oProxyCommand=x",
            "-me@box",
            "@box",
            "box:",
            "box:0",
            "box:99999",
            "a b",
            "box/x",
            "[::1",
            "[::1]x",
            "it's",
        ] {
            assert!(Host::parse(text).is_err(), "{text:?} was accepted");
        }
        // Shown as typed.
        for text in ["box", "me@box:2222", "me@[fd7a::1]:22", "fd7a::1"] {
            assert_eq!(Host::parse(text).unwrap().to_string(), text);
        }
    }

    #[test]
    fn remote_commands_survive_any_login_shell() {
        let command = remote_command(true, "echo \"$1\"\nexit 3", &["a b", "it's"]);
        let encoded = base64::engine::general_purpose::STANDARD.encode("echo \"$1\"\nexit 3");
        assert_eq!(
            command,
            format!(
                "exec /bin/sh -lc 'eval \"$(echo {encoded} | base64 -d)\"' sh 'a b' 'it'\\''s'"
            )
        );
        assert!(!command.contains('\n'));
        // What the login shell runs, run by a POSIX shell.
        if cfg!(unix) && Path::new("/bin/sh").exists() {
            let output = Command::new("/bin/sh")
                .arg("-c")
                .arg(&command)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(3));
            assert_eq!(String::from_utf8_lossy(&output.stdout), "a b\n");
        }
    }

    #[test]
    fn ssh_commands_never_prompt_and_keep_stdin_binary() {
        let (stderr, _) = mpsc::channel();
        let mut host = host("me@box", Some(2222));
        host.config = Some(PathBuf::from("/tmp/ssh_config"));
        let target = Target {
            host: &host,
            accept_new: true,
            stderr,
        };
        let command = target.shell(false, "true", &[]);
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(&args[..2], ["-F", "/tmp/ssh_config"]);
        for option in ["BatchMode=yes", "StrictHostKeyChecking=accept-new"] {
            assert!(args.iter().any(|arg| arg == option), "{option} missing");
        }
        assert!(args.windows(2).any(|pair| pair == ["-e", "none"]));
        assert!(args.contains(&"-T".to_owned()));
        let at = args.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(&args[at - 2..at], ["-p", "2222"]);
        assert_eq!(args[at + 1], "me@box");
        assert_eq!(args.len(), at + 3);
        assert_eq!(target.describe(), "SSH host 'me@box:2222'");
    }

    #[test]
    fn configured_hosts_skip_patterns_and_follow_includes() {
        let dir = env::temp_dir().join(format!("next-code-ssh-config-{}", std::process::id()));
        fs::create_dir_all(dir.join("config.d")).unwrap();
        fs::write(
            dir.join("config"),
            "# comment\nHost box other.example\n  User me\nHost *.internal !x gate?\n\
             Include config.d/*.conf\nHost=\"quoted\"\nHost box\n",
        )
        .unwrap();
        fs::write(dir.join("config.d/a.conf"), "Host fromInclude\n").unwrap();
        fs::write(dir.join("config.d/skip.txt"), "Host notIncluded\n").unwrap();
        let mut hosts = Vec::new();
        read_config(&dir.join("config"), &dir, 0, &mut hosts);
        assert_eq!(hosts, ["box", "other.example", "fromInclude", "quoted"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wildcards_match_like_ssh() {
        assert!(wildcard("*.conf", "a.conf"));
        assert!(wildcard("*", ""));
        assert!(wildcard("a?c*", "abcdef"));
        assert!(wildcard("*a*b", "xxaxxb"));
        assert!(!wildcard("*.conf", "a.conf.bak"));
        assert!(!wildcard("a?c", "ac"));
    }

    #[test]
    fn sign_in_failures_say_what_to_do() {
        let host = host("box", None);
        let denied = explain(
            io::Error::other("failed: me@box: Permission denied (publickey)."),
            &host,
        );
        assert!(denied.to_string().contains("ssh-add"));
        let key = explain(io::Error::other("Host key verification failed."), &host);
        assert!(key.to_string().contains("`ssh box`"));
        let other = explain(io::Error::other("no route"), &host);
        assert_eq!(other.to_string(), "no route");
    }
}
