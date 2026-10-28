//! The open project and where its files live: on this machine, or inside a
//! WSL distro, a dev container or on a machine a built-in extension reaches
//! (SSH), behind `next_code_agent`.
//!
//! Paths of a WSL project are the distro's POSIX paths (`/home/me/p/x.rs`),
//! held in `PathBuf`s. They never reach `std::fs` here, and are extended with
//! [`Project::join`], never `Path::join` (which would insert `\` on Windows).

use std::{
    fmt, fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use lsp::{Incoming, LanguageServer, Logger};
use parking_lot::Mutex;
use remote::{AgentClient, PipedProcess, RunOutput, StreamHandle, agent_path};
use search::{SearchEvent, SearchLimits, SearchQuery};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use terminal::{Palette, Terminal, TerminalEvent};
use watcher::Batch;
use worktree::{DirEntry, FileKind};

use crate::{
    settings::Settings,
    workspace_file::{self, Folder},
};

mod extension_process;
mod pinpoint;
mod process_tree;

pub use process_tree::CancelToken;
pub(crate) use process_tree::spawn_detached;
use process_tree::{GRACE, Outcome, Tree};

/// What the user asked to open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Location {
    Local(PathBuf),
    /// A Linux path inside a WSL distro, never reinterpreted as a Windows path.
    Wsl {
        distro: String,
        path: String,
    },
    /// A folder (local or in WSL) opened inside its dev container.
    DevContainer {
        folder: Box<Location>,
        /// The `devcontainer.json` (in the folder's path syntax); `None` is
        /// the folder's first one.
        config: Option<String>,
    },
    /// Several folders on one machine (a multi-root workspace), saved in a
    /// workspace file or not (untitled).
    Workspace {
        /// The WSL distro holding the folders (and the file); `None` for
        /// this machine.
        distro: Option<String>,
        /// A `*.next_code-workspace` or VS Code `*.code-workspace` file;
        /// opening reads its folders and settings.
        file: Option<String>,
        /// Absolute folders: the workspace for an untitled one, the last
        /// ones read from `file` otherwise.
        folders: Vec<Folder>,
    },
    /// A path on a machine a built-in extension's connector reaches
    /// (`builtins::Connector`), typed `<scheme>://<host>/<path>`.
    Remote {
        /// The connector's scheme (`ssh`).
        scheme: String,
        /// The machine, as the connector takes it (`me@box:2222`).
        host: String,
        /// An absolute POSIX path, or one starting with `~`.
        path: String,
    },
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local(root) => write!(f, "{}", root.display()),
            Self::Wsl { distro, path } => write!(f, "{path} [WSL: {distro}]"),
            Self::DevContainer { folder, .. } => write!(f, "{folder} [Dev Container]"),
            Self::Workspace {
                distro,
                file,
                folders,
            } => {
                match (file, folders.as_slice()) {
                    (Some(file), _) => write!(f, "{file} [Workspace]")?,
                    (None, [first, rest @ ..]) => {
                        write!(f, "{}", first.path)?;
                        if !rest.is_empty() {
                            write!(f, " + {} more [Workspace]", rest.len())?;
                        }
                    }
                    (None, []) => write!(f, "[Workspace]")?,
                }
                match distro {
                    Some(distro) => write!(f, " [WSL: {distro}]"),
                    None => Ok(()),
                }
            }
            Self::Remote { scheme, host, path } => {
                // `~/p` follows the host's `/`; absolute paths have their own.
                let slash = if path.starts_with('/') { "" } else { "/" };
                write!(f, "{scheme}://{host}{slash}{path}")
            }
        }
    }
}

/// Prefix of typed dev container locations (`devcontainer:C:\src\p`).
const DEVCONTAINER_PREFIX: &str = "devcontainer:";

/// A folder's own next_code files (`settings.json`, `run.json`, …).
const FOLDER_SETTINGS_DIR: &str = ".next_code";

impl Location {
    /// Reads a typed location: `\\wsl.localhost\<distro>\path` (or
    /// `\\wsl$\...`), `<distro>:/linux/path` (or `<distro>:~/...`),
    /// `<scheme>://<host>/path` for a built-in extension's connector
    /// (`ssh://box/~/p`), `devcontainer:<one of those or a local path>`,
    /// else a local path.
    /// Paths of workspace files (`*.next_code-workspace`,
    /// `*.code-workspace`) are workspaces.
    pub fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        if input.is_empty() {
            return None;
        }
        if let Some(folder) = input.strip_prefix(DEVCONTAINER_PREFIX) {
            return Some(Self::DevContainer {
                folder: Box::new(Self::parse(folder)?),
                config: None,
            });
        }
        if let Some(wsl) = wsl_unc_path(input) {
            return Some(wsl.workspace_file_as_workspace());
        }
        // Before `<distro>:/path`, which `ssh://box/p` also looks like.
        if let Some((scheme, _)) = input.split_once("://")
            && crate::builtins::connector(scheme).is_some()
        {
            return remote_url(input);
        }
        // One-letter prefixes are Windows drives (`C:/dev`).
        if let Some((distro, path)) = input.split_once(':')
            && distro.len() > 1
            && !distro.contains(['/', '\\', ' '])
            && (path.starts_with('/') || path.starts_with('~'))
        {
            return Some(
                Self::Wsl {
                    distro: distro.to_owned(),
                    path: path.to_owned(),
                }
                .workspace_file_as_workspace(),
            );
        }
        Some(Self::Local(PathBuf::from(input)).workspace_file_as_workspace())
    }

    /// A local or WSL path naming a workspace file becomes that workspace.
    pub fn workspace_file_as_workspace(self) -> Self {
        match self {
            Self::Local(path) if workspace_file::Format::of(&path.to_string_lossy()).is_some() => {
                Self::Workspace {
                    distro: None,
                    file: Some(path.to_string_lossy().into_owned()),
                    folders: Vec::new(),
                }
            }
            Self::Wsl { distro, path } if workspace_file::Format::of(&path).is_some() => {
                Self::Workspace {
                    distro: Some(distro),
                    file: Some(path),
                    folders: Vec::new(),
                }
            }
            other => other,
        }
    }

    /// Local paths made absolute (also inside a dev container location).
    pub fn absolute(self) -> Self {
        let local = |path: &str| {
            std::path::absolute(path)
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_else(|_| path.to_owned())
        };
        match self {
            Self::Local(root) => Self::Local(std::path::absolute(&root).unwrap_or(root)),
            Self::DevContainer { folder, config } => Self::DevContainer {
                folder: Box::new(folder.absolute()),
                config,
            },
            Self::Workspace {
                distro: None,
                file,
                folders,
            } => Self::Workspace {
                distro: None,
                file: file.as_deref().map(local),
                folders: folders
                    .into_iter()
                    .map(|folder| Folder {
                        path: local(&folder.path),
                        name: folder.name,
                    })
                    .collect(),
            },
            other => other,
        }
    }

    /// Whether both open the same project; a dev container location without
    /// a config means the folder's first one, whichever that is. Saved
    /// workspaces are the same file whatever folders they had.
    pub fn opens_same(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::DevContainer {
                    folder: a,
                    config: config_a,
                },
                Self::DevContainer {
                    folder: b,
                    config: config_b,
                },
            ) => a == b && (config_a.is_none() || config_b.is_none() || config_a == config_b),
            (
                Self::Workspace {
                    distro: distro_a,
                    file: Some(file_a),
                    ..
                },
                Self::Workspace {
                    distro: distro_b,
                    file: Some(file_b),
                    ..
                },
            ) => distro_a == distro_b && same_path(file_a, file_b, distro_a.is_some()),
            _ => self == other,
        }
    }

    /// The machine's path syntax: POSIX inside WSL and containers (and
    /// everywhere off Windows).
    pub fn posix(&self) -> bool {
        match self {
            Self::Local(_) | Self::Workspace { distro: None, .. } => !cfg!(windows),
            _ => true,
        }
    }

    /// The per-project state key (layout, session) of a workspace: its
    /// file, or a hash of an untitled one's folders. `None` for other
    /// locations (see [`Project::state_key`]).
    pub fn workspace_state_key(&self) -> Option<String> {
        let Self::Workspace {
            distro,
            file,
            folders,
        } = self
        else {
            return None;
        };
        let machine = distro
            .as_deref()
            .map_or("local".into(), |d| format!("wsl:{d}"));
        Some(match file {
            Some(file) => format!("workspace:{machine}:{file}"),
            None => {
                // FNV-1a: stable across Rust versions, unlike `DefaultHasher`.
                let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
                for byte in folders
                    .iter()
                    .flat_map(|folder| folder.path.bytes().chain([0]))
                {
                    hash = (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
                }
                let first = folders.first().map_or(String::new(), |folder| {
                    workspace_file::folder_name(&folder.path, self.posix())
                });
                format!("workspace:{machine}:untitled:{first}:{hash:016x}")
            }
        })
    }
}

/// Whether two paths of one machine name the same file (Windows paths
/// ignore case).
fn same_path(a: &str, b: &str, posix: bool) -> bool {
    if posix || !cfg!(windows) {
        a == b
    } else {
        a.eq_ignore_ascii_case(b)
    }
}

/// A folder of the project, named by `name` or its last component.
fn root_of_path(path: PathBuf, name: Option<String>, posix: bool) -> Root {
    let path = if posix || !cfg!(windows) {
        path
    } else {
        without_verbatim_prefix(path)
    };
    Root {
        name: name
            .clone()
            .unwrap_or_else(|| workspace_file::folder_name(&path.to_string_lossy(), posix)),
        custom_name: name,
        path,
    }
}

/// `\\?\C:\p` → `C:\p` and `\\?\UNC\host\share` → `\\host\share`, as
/// `canonicalize` spells them on Windows: language servers' URIs
/// (`file:///C:/p`), git and shells never spell them back, so files of
/// the root would not match what they report.
pub(crate) fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        // Other verbatim forms (`\\?\Volume{…}`) have no plain spelling.
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}

/// A folder-relative path labelled with its folder (`api › src/main.rs`),
/// as multi-root lists show it.
pub fn root_label(root: &str, relative: &str) -> String {
    format!("{root} › {relative}")
}

/// `<scheme>://<host>[/<path>]` for a connector's scheme: `/~` and `/~/…`
/// are in the home folder, other paths are absolute, and no path at all is
/// the home folder.
fn remote_url(input: &str) -> Option<Location> {
    let (scheme, rest) = input.split_once("://")?;
    crate::builtins::connector(scheme)?;
    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    if host.is_empty() {
        return None;
    }
    let path = match path.strip_prefix('/') {
        None => "~",
        Some(home @ ("~" | "~/")) => home,
        Some(in_home) if in_home.starts_with("~/") => in_home,
        Some(_) => path,
    };
    let path = match path.trim_end_matches('/') {
        "" => "/",
        trimmed => trimmed,
    };
    Some(Location::Remote {
        scheme: scheme.to_owned(),
        host: host.to_owned(),
        path: path.to_owned(),
    })
}

/// `\\wsl.localhost\Ubuntu\home\me` (or `\\wsl$\...`) names a WSL project;
/// it is opened through the agent rather than Windows' network share.
fn wsl_unc_path(path: &str) -> Option<Location> {
    let path = path.replace('/', "\\");
    let lower = path.to_ascii_lowercase();
    let rest = ["\\\\wsl.localhost\\", "\\\\wsl$\\"]
        .iter()
        .find(|prefix| lower.starts_with(*prefix))
        .map(|prefix| &path[prefix.len()..])?;
    let (distro, linux) = rest.split_once('\\').unwrap_or((rest, ""));
    if distro.is_empty() {
        return None;
    }
    let linux = linux.trim_end_matches('\\').replace('\\', "/");
    Some(Location::Wsl {
        distro: distro.to_owned(),
        path: format!("/{linux}"),
    })
}

#[derive(Clone)]
pub struct Project(Arc<Inner>);

struct Inner {
    /// The first folder (`roots[0]`): the working directory of language
    /// servers and tools, and the folder of single-folder projects.
    root: PathBuf,
    /// Every folder of the project in order; one unless it is a
    /// multi-root workspace. Fixed for the project's life: changing the
    /// folders reopens the project (see `Workspace::set_folders`).
    roots: Vec<Root>,
    /// Set for workspaces (a workspace file, or several folders).
    workspace: Option<WorkspaceInfo>,
    /// Settings layered per folder (see [`Project::settings_for`]).
    folder_settings: Mutex<Vec<Option<FolderSettings>>>,
    backend: Backend,
    /// Serves `next_code_agent pinpoint` on a remote project's machine.
    _pinpoint: Option<StreamHandle>,
    /// Lends this machine's git credentials and ssh-agent to a remote
    /// project's machine (`remote::host`); filled by a thread, since finding
    /// out what to lend runs git here.
    _host: Arc<Mutex<Option<StreamHandle>>>,
}

/// A folder of the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub path: PathBuf,
    /// What the tree and labels show: the workspace file's `name`, else
    /// the folder's own name.
    pub name: String,
    /// The workspace file's `name`, if it gave one.
    pub custom_name: Option<String>,
}

struct WorkspaceInfo {
    /// The workspace file, in the machine's path syntax; `None` while
    /// untitled.
    file: Option<String>,
    /// The file's `settings` (VS Code keys translated); `Null` if none.
    settings: Value,
}

/// One folder's layered settings and the user settings they were built
/// from (rebuilt when those change).
struct FolderSettings {
    user: Arc<Settings>,
    /// The folder's `settings.json` (`Null` if none) and why it was
    /// unreadable, kept so new user settings layer without reading again.
    folder: (Value, Option<String>),
    layered: Arc<Settings>,
    /// Why the folder's or workspace's settings were ignored.
    error: Option<String>,
}

enum Backend {
    Local,
    /// Behind `next_code_agent`.
    Remote {
        client: Arc<AgentClient>,
        host: RemoteHost,
    },
}

enum RemoteHost {
    Wsl {
        distro: String,
    },
    Container(Box<ContainerHost>),
    /// A machine a built-in extension's connector reached.
    Connector {
        scheme: String,
        host: String,
    },
}

/// A dev container and what is needed to stop it.
struct ContainerHost {
    /// The folder the container was opened for.
    folder: Location,
    config: String,
    docker: remote::container::DockerHost,
    /// Set when the editor created or started the container: it is stopped
    /// (as `shutdownAction` says) when the project is closed.
    stop: Option<Vec<String>>,
    /// The configuration's port forwarding settings.
    ports: devcontainer::PortsConfig,
}

impl Drop for ContainerHost {
    fn drop(&mut self) {
        // Detached, so it finishes even when the editor exits right after:
        // outside the editor's job on Windows; on Unix in its own process
        // group, so whatever ends the editor's group (a supervisor, Ctrl+C
        // in a terminal) spares it.
        if let Some(args) = &self.stop {
            let mut command = self.docker.docker();
            command
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            process_tree::spawn_detached(&mut command).ok();
        }
    }
}

/// Stops watching when dropped.
pub enum WatchHandle {
    Local(#[allow(dead_code)] watcher::ProjectWatcher),
    Remote(#[allow(dead_code)] StreamHandle),
    /// One per folder of a multi-root project.
    Many(#[allow(dead_code)] Vec<WatchHandle>),
}

/// Cancels the search when dropped.
pub enum SearchHandle {
    Local(#[allow(dead_code)] search::SearchHandle),
    Remote(#[allow(dead_code)] StreamHandle),
    /// One per folder of a multi-root project.
    Many(#[allow(dead_code)] Vec<SearchHandle>),
}

/// Forwards the folders' searches as one: each file once (nested folders
/// find some twice), up to `limits.max_matches` matches, then a `Done`
/// with the totals.
async fn merge_searches(
    receivers: Vec<UnboundedReceiver<SearchEvent>>,
    limits: SearchLimits,
    tx: futures::channel::mpsc::UnboundedSender<SearchEvent>,
) {
    let mut pending = receivers.len();
    let mut events = futures::stream::select_all(receivers);
    let mut seen = std::collections::HashSet::new();
    let (mut files_searched, mut matches, mut limit_hit) = (0, 0, false);
    while pending > 0
        && let Some(event) = events.next().await
    {
        match event {
            SearchEvent::File(file) => {
                if matches >= limits.max_matches {
                    limit_hit = true;
                    break;
                }
                if seen.insert(file.path.clone()) {
                    matches += file
                        .lines
                        .iter()
                        .map(|line| line.ranges.len().max(1))
                        .sum::<usize>();
                    if tx.unbounded_send(SearchEvent::File(file)).is_err() {
                        return;
                    }
                }
            }
            SearchEvent::Done {
                files_searched: searched,
                limit_hit: hit,
                ..
            } => {
                pending -= 1;
                files_searched += searched;
                limit_hit |= hit;
            }
        }
    }
    tx.unbounded_send(SearchEvent::Done {
        files_searched,
        matches,
        limit_hit,
    })
    .ok();
}

fn cancelled(program: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        format!("{program} was cancelled"),
    )
}

/// Runs `command` (named `program` in errors) locally in `cwd` to
/// completion with `input` on its stdin, collecting its output. After
/// `timeout`, or once `cancel` is cancelled, the process and whatever it
/// started are asked to end and killed after [`GRACE`].
pub(crate) fn run_local(
    mut command: Command,
    program: &str,
    cwd: &Path,
    input: Option<&[u8]>,
    timeout: Duration,
    cancel: Option<&CancelToken>,
) -> io::Result<RunOutput> {
    let deadline = Instant::now() + timeout;
    command
        .current_dir(cwd)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    remote::launch::no_window(&mut command);
    let mut spawned = Tree::spawn(&mut command)?;
    let child = spawned.child.as_mut().expect("just spawned");
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    // From here on an error drops the tree, which ends the process.
    let mut tree = spawned.watch()?;
    if let (Some(mut stdin), Some(input)) = (stdin, input) {
        let input = input.to_vec();
        // Written on a thread so a full output pipe cannot deadlock.
        thread::Builder::new()
            .name("run-stdin".into())
            .spawn(move || {
                use std::io::Write;
                stdin.write_all(&input).ok();
            })?;
    }
    let collect = |mut pipe: Box<dyn Read + Send>| {
        thread::Builder::new()
            .name("run-output".into())
            .spawn(move || {
                let mut data = Vec::new();
                pipe.read_to_end(&mut data).ok();
                data
            })
    };
    let stdout = collect(Box::new(stdout))?;
    let stderr = collect(Box::new(stderr))?;
    match tree.wait_until(deadline, cancel) {
        Outcome::Exited(status) => Ok(RunOutput {
            code: status.and_then(|status| status.code()),
            stdout: stdout.join().unwrap_or_default(),
            stderr: stderr.join().unwrap_or_default(),
        }),
        Outcome::TimedOut => {
            tree.terminate(GRACE);
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{program} did not finish within {} s", timeout.as_secs()),
            ))
        }
        Outcome::Cancelled => {
            tree.terminate(GRACE);
            Err(cancelled(program))
        }
    }
}

/// Lends git credentials, the ssh-agent and the git identity to `client`'s
/// machine (`devcontainer.forward_git_credentials`), or removes the helper
/// set there earlier when that is off. The handle lands in `slot` unless the
/// project closed meanwhile (dropping it stops serving).
fn serve_host(
    client: Arc<AgentClient>,
    project: Project,
    slot: std::sync::Weak<Mutex<Option<StreamHandle>>>,
) {
    let user = crate::settings::current();
    let enabled = user.devcontainer.forward_git_credentials;
    thread::Builder::new()
        .name("host-services".into())
        .spawn(move || {
            // Credentials only for the hosts of the project's remotes (and
            // the user's `git.credential_hosts`); the project's own settings
            // (read where it lives) may keep the ssh-agent to this machine.
            let policy = remote::host::Policy {
                root: project.root().to_string_lossy().into_owned(),
                credential_hosts: user.git.credential_hosts.clone(),
                ssh_agent: enabled && project.settings_for(None).devcontainer.ssh_agent_forwarding,
            };
            drop(project);
            match remote::host::serve(&client, enabled, &policy) {
                Ok(handle) => {
                    if let Some(slot) = slot.upgrade() {
                        *slot.lock() = Some(handle);
                    }
                }
                // The project works without; git there prompts by itself.
                Err(err) => eprintln!("next_code: git credential forwarding unavailable: {err}"),
            }
        })
        .ok();
}

impl Project {
    /// An SSH-shaped project served by an in-process agent, without host
    /// credential forwarding or changes to the user's agent/configuration.
    #[cfg(all(test, unix))]
    pub(crate) fn test_ssh(root: PathBuf) -> Self {
        let (reader, agent_writer) = io::pipe().unwrap();
        let (agent_reader, writer) = io::pipe().unwrap();
        thread::spawn(move || next_code_agent::serve(agent_reader, agent_writer).unwrap());
        let (client, _) = AgentClient::connect(
            remote::Transport {
                reader: Box::new(reader),
                writer: Box::new(writer),
                child: None,
                stderr: None,
            },
            "test SSH machine",
            root.to_str().unwrap(),
            Duration::from_secs(5),
        )
        .unwrap();
        Self(Arc::new(Inner {
            root: root.clone(),
            roots: vec![root_of_path(root, None, true)],
            workspace: None,
            folder_settings: Mutex::new(vec![None]),
            backend: Backend::Remote {
                client,
                host: RemoteHost::Connector {
                    scheme: "ssh".into(),
                    host: "test-host".into(),
                },
            },
            _pinpoint: None,
            _host: Arc::new(Mutex::new(None)),
        }))
    }

    pub fn local(root: PathBuf) -> Self {
        Self::build(vec![root_of_path(root, None, false)], None, Backend::Local)
    }

    /// A local untitled workspace over `folders` (tests of multi-root
    /// behaviour).
    #[cfg(test)]
    pub fn local_folders(folders: &[&Path]) -> Self {
        let roots = folders
            .iter()
            .map(|path| root_of_path(path.to_path_buf(), None, false))
            .collect();
        Self::build(
            roots,
            Some(WorkspaceInfo {
                file: None,
                settings: Value::Null,
            }),
            Backend::Local,
        )
    }

    fn build(roots: Vec<Root>, workspace: Option<WorkspaceInfo>, backend: Backend) -> Self {
        let pinpoint = match &backend {
            Backend::Local => None,
            // Without it the project still works; only CLI agents on the
            // remote machine can't reach this window.
            Backend::Remote { client, .. } => pinpoint::serve(client, &roots)
                .inspect_err(|err| eprintln!("next_code: remote pinpoint unavailable: {err}"))
                .ok(),
        };
        let host = Arc::new(Mutex::new(None));
        let project = Self(Arc::new(Inner {
            root: roots[0].path.clone(),
            folder_settings: Mutex::new((0..roots.len()).map(|_| None).collect()),
            roots,
            workspace,
            backend,
            _pinpoint: pinpoint,
            _host: host.clone(),
        }));
        if let Backend::Remote { client, .. } = &project.0.backend {
            serve_host(client.clone(), project.clone(), Arc::downgrade(&host));
        }
        project
    }

    /// Starts the agent in `distro` and opens `path` there. Blocks for as
    /// long as WSL takes to start.
    pub fn connect_wsl(distro: &str, path: &str) -> io::Result<Self> {
        let connection = remote::wsl::connect(distro, path)?;
        Ok(Self::build(
            vec![root_of_path(
                PathBuf::from(connection.info.root),
                None,
                true,
            )],
            None,
            Backend::Remote {
                client: connection.client,
                host: RemoteHost::Wsl {
                    distro: distro.to_owned(),
                },
            },
        ))
    }

    /// Opens `location`, reporting steps of slow ones (dev containers) to
    /// `progress`. Blocks.
    pub fn open(location: &Location, progress: &mut dyn FnMut(&str)) -> io::Result<Self> {
        match location {
            Location::Local(root) => Ok(Self::local(root.clone())),
            Location::Wsl { distro, path } => Self::connect_wsl(distro, path),
            Location::DevContainer { folder, config } => {
                Self::connect_devcontainer(folder, config.as_deref(), progress)
            }
            Location::Workspace {
                distro,
                file,
                folders,
            } => Self::open_workspace(distro.as_deref(), file.as_deref(), folders, progress),
            Location::Remote { scheme, host, path } => {
                Self::connect_remote(scheme, host, path, progress)
            }
        }
    }

    /// Opens `path` on `host` through the connector of `scheme`, when the
    /// built-in extension declaring it is on.
    fn connect_remote(
        scheme: &str,
        host: &str,
        path: &str,
        progress: &mut dyn FnMut(&str),
    ) -> io::Result<Self> {
        let Some((builtin, connector)) = crate::builtins::connector(scheme) else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("Nothing opens {scheme}:// locations"),
            ));
        };
        if !crate::extensions::saved_builtin_enabled(builtin) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "{} is turned off: turn it on in Manage Extensions to open {}",
                    builtin.name,
                    (connector.describe)(host)
                ),
            ));
        }
        let connection = (connector.connect)(host, path, progress)?;
        Ok(Self::build(
            vec![root_of_path(
                PathBuf::from(connection.info.root),
                None,
                true,
            )],
            None,
            Backend::Remote {
                client: connection.client,
                host: RemoteHost::Connector {
                    scheme: scheme.to_owned(),
                    host: host.to_owned(),
                },
            },
        ))
    }

    /// Opens a multi-root workspace: reads its file (where it lives) when
    /// it has one, else takes `folders`.
    fn open_workspace(
        distro: Option<&str>,
        file: Option<&str>,
        folders: &[Folder],
        progress: &mut dyn FnMut(&str),
    ) -> io::Result<Self> {
        let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidData, message);
        let posix = distro.is_some() || !cfg!(windows);
        // The agent starts in the file's folder, else the first folder.
        let start = match (file, folders.first()) {
            (Some(file), _) => workspace_file::parent(file, posix),
            (None, Some(folder)) => folder.path.clone(),
            (None, None) => return Err(invalid("the workspace has no folders".into())),
        };
        let backend = match distro {
            None => Backend::Local,
            Some(distro) => {
                progress(&format!("Connecting to WSL distro '{distro}'"));
                let connection = remote::wsl::connect(distro, &start)?;
                Backend::Remote {
                    client: connection.client,
                    host: RemoteHost::Wsl {
                        distro: distro.to_owned(),
                    },
                }
            }
        };
        let (folders, settings) = match file {
            Some(file) => {
                let text = match &backend {
                    Backend::Local => fs::read(file),
                    Backend::Remote { client, .. } => client.read_file(file),
                }
                .and_then(worktree::decode_text)
                .map_err(|err| io::Error::new(err.kind(), format!("{file}: {err}")))?;
                let parsed = workspace_file::parse(&text, file, posix).map_err(invalid)?;
                for warning in &parsed.warnings {
                    progress(warning);
                }
                (parsed.folders, parsed.settings)
            }
            None => (folders.to_vec(), Value::Null),
        };
        if folders.is_empty() {
            return Err(invalid(format!(
                "{} has no folders",
                file.unwrap_or("the workspace")
            )));
        }
        let roots = folders
            .into_iter()
            .map(|folder| root_of_path(PathBuf::from(folder.path), folder.name, posix))
            .collect();
        Ok(Self::build(
            roots,
            Some(WorkspaceInfo {
                file: file.map(str::to_owned),
                settings,
            }),
            backend,
        ))
    }

    /// Brings up the dev container of `folder` and opens its workspace
    /// folder through an agent inside it.
    fn connect_devcontainer(
        folder: &Location,
        config: Option<&str>,
        progress: &mut dyn FnMut(&str),
    ) -> io::Result<Self> {
        let (host_project, docker) = match folder {
            Location::Local(root) => (
                Self::local(root.clone()),
                remote::container::DockerHost::Local,
            ),
            Location::Wsl { distro, path } => {
                progress(&format!("Connecting to WSL distro '{distro}'"));
                (
                    Self::connect_wsl(distro, path)?,
                    remote::container::DockerHost::Wsl(distro.clone()),
                )
            }
            Location::DevContainer { .. } => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a dev container cannot contain another one",
                ));
            }
            Location::Workspace { .. } => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "dev containers open a single folder, not a multi-root workspace: \
                     reopen one of its folders in the container",
                ));
            }
            Location::Remote { .. } => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("{folder} cannot be opened in a dev container yet"),
                ));
            }
        };
        let host = FolderHost(host_project.clone());
        let folder_path = host_project.root().to_string_lossy().into_owned();
        let config = match config {
            Some(config) => config.to_owned(),
            None => devcontainer::find_configs(&host, &folder_path)?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("{folder} has no .devcontainer/devcontainer.json"),
                    )
                })?,
        };
        let started = devcontainer::up(&host, &config, progress)?;
        progress(&format!("Starting the editor agent in {}", started.name));
        let target = remote::container::Container {
            host: docker.clone(),
            id: started.id.clone(),
            name: started.name.clone(),
            user: started.user.clone(),
            env: started.remote_env.clone(),
            workdir: Some(started.workspace_folder.clone()),
        };
        let container = ContainerHost {
            folder: folder.clone(),
            config: config.clone(),
            docker,
            stop: started
                .started
                .then(|| devcontainer::stop_args(&started.shutdown))
                .flatten(),
            ports: started.ports.clone(),
        };
        let connection = remote::container::connect(&target, &started.workspace_folder)?;
        Ok(Self::build(
            vec![root_of_path(
                PathBuf::from(connection.info.root),
                None,
                true,
            )],
            None,
            Backend::Remote {
                client: connection.client,
                host: RemoteHost::Container(Box::new(container)),
            },
        ))
    }

    /// The `devcontainer.json` files of this project's folder (none for a
    /// project already in a container, with several folders (dev
    /// containers open one), or on a connector's machine). Blocks.
    pub fn devcontainer_configs(&self) -> io::Result<Vec<String>> {
        if self.is_multi_root()
            || matches!(
                &self.0.backend,
                Backend::Remote {
                    host: RemoteHost::Container(_) | RemoteHost::Connector { .. },
                    ..
                }
            )
        {
            return Ok(Vec::new());
        }
        let host = FolderHost(self.clone());
        devcontainer::find_configs(&host, &self.0.root.to_string_lossy())
    }

    /// The first folder (the only one unless this is a multi-root
    /// workspace): tools and language servers run here.
    pub fn root(&self) -> &Path {
        &self.0.root
    }

    /// Every folder, in the workspace's order (`roots()[0]` is
    /// [`Project::root`]).
    pub fn roots(&self) -> &[Root] {
        &self.0.roots
    }

    /// Whether the project has several folders (lists then label paths
    /// with their folder, see [`Project::display_path`]).
    pub fn is_multi_root(&self) -> bool {
        self.0.roots.len() > 1
    }

    /// The workspace file, when the project is a saved workspace.
    pub fn workspace_file(&self) -> Option<&str> {
        self.0.workspace.as_ref()?.file.as_deref()
    }

    /// Whether the project is a workspace (saved or untitled) rather than
    /// a single folder.
    pub fn is_workspace(&self) -> bool {
        self.0.workspace.is_some()
    }

    /// The workspace file's settings (`Null` if none).
    pub fn workspace_settings(&self) -> &Value {
        static NULL: Value = Value::Null;
        self.0
            .workspace
            .as_ref()
            .map_or(&NULL, |workspace| &workspace.settings)
    }

    /// The index of the folder holding `path` (the innermost when folders
    /// nest), `None` outside every folder.
    pub fn root_index_of(&self, path: &Path) -> Option<usize> {
        self.0
            .roots
            .iter()
            .enumerate()
            .filter(|(_, root)| path.starts_with(&root.path))
            .max_by_key(|(_, root)| root.path.components().count())
            .map(|(index, _)| index)
    }

    /// The folder holding `path`, else the first one.
    pub fn root_for(&self, path: &Path) -> &Root {
        &self.0.roots[self.root_index_of(path).unwrap_or(0)]
    }

    /// `path` relative to its folder, `/`-separated, with that folder's
    /// index; `None` outside every folder (and for a folder itself).
    pub fn relative(&self, path: &Path) -> Option<(usize, String)> {
        let index = self.root_index_of(path)?;
        let relative = path.strip_prefix(&self.0.roots[index].path).ok()?;
        let parts: Vec<_> = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect();
        (!parts.is_empty()).then(|| (index, parts.join("/")))
    }

    /// How lists show `path`: relative to its folder, after the folder's
    /// name and ` › ` when the project has several (`api › src/main.rs`);
    /// the full path outside every folder.
    pub fn display_path(&self, path: &Path) -> String {
        match self.relative(path) {
            Some((index, relative)) if self.is_multi_root() => {
                root_label(&self.0.roots[index].name, &relative)
            }
            Some((_, relative)) => relative,
            None => match self.root_index_of(path) {
                Some(index) if self.is_multi_root() => self.0.roots[index].name.clone(),
                _ => path.to_string_lossy().into_owned(),
            },
        }
    }

    /// `path` split as result lists show it: the file name, and the folder
    /// it is in as [`Project::display_path`] shows folders (`api › src`,
    /// just `api` at a folder's top) when the project has several; else
    /// relative to the root (empty at its top), the full parent outside.
    pub fn display_parts(&self, path: &Path) -> (String, String) {
        let name = |path: &Path| {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        if self.is_multi_root()
            && let Some(index) = self.root_index_of(path)
            && let Some(dir) = path
                .parent()
                .filter(|dir| dir.starts_with(&self.0.roots[index].path))
        {
            let root = &self.0.roots[index];
            let dir = match self.relative(dir) {
                Some((_, relative)) => root_label(&root.name, &relative),
                None => root.name.clone(),
            };
            return (name(path), dir);
        }
        let shown = path.strip_prefix(&self.0.root).unwrap_or(path);
        let dir = shown
            .parent()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default();
        (name(shown), dir)
    }

    pub fn location(&self) -> Location {
        if self.0.workspace.is_some() {
            return Location::Workspace {
                distro: match &self.0.backend {
                    Backend::Remote {
                        host: RemoteHost::Wsl { distro },
                        ..
                    } => Some(distro.clone()),
                    _ => None,
                },
                file: self.workspace_file().map(str::to_owned),
                folders: self.folders(),
            };
        }
        match &self.0.backend {
            Backend::Local => Location::Local(self.0.root.clone()),
            Backend::Remote {
                host: RemoteHost::Wsl { distro },
                ..
            } => Location::Wsl {
                distro: distro.clone(),
                path: self.0.root.to_string_lossy().into_owned(),
            },
            Backend::Remote {
                host: RemoteHost::Container(container),
                ..
            } => Location::DevContainer {
                folder: Box::new(container.folder.clone()),
                config: Some(container.config.clone()),
            },
            Backend::Remote {
                host: RemoteHost::Connector { scheme, host },
                ..
            } => Location::Remote {
                scheme: scheme.clone(),
                host: host.clone(),
                path: self.0.root.to_string_lossy().into_owned(),
            },
        }
    }

    /// The folders as a workspace file lists them (absolute paths).
    pub fn folders(&self) -> Vec<Folder> {
        self.0
            .roots
            .iter()
            .map(|root| Folder {
                path: root.path.to_string_lossy().into_owned(),
                name: root.custom_name.clone(),
            })
            .collect()
    }

    /// The project with `folders` instead of its own, on the same machine
    /// (keeping the workspace file); `Err` says why it cannot (dev
    /// containers have one folder). One folder without a workspace file
    /// is a plain folder project again.
    pub fn with_folders(&self, folders: Vec<Folder>) -> Result<Location, String> {
        if self.in_container() {
            return Err("A dev container project has a single folder: \
                        open the other folder in its own project tab."
                .into());
        }
        if let Backend::Remote {
            host: RemoteHost::Connector { scheme, host },
            ..
        } = &self.0.backend
        {
            let machine = crate::builtins::connector(scheme)
                .map_or_else(|| host.clone(), |(_, connector)| (connector.describe)(host));
            return Err(format!(
                "A project on {machine} has a single folder: \
                 open the other folder in its own project tab."
            ));
        }
        if folders.is_empty() {
            return Err("A workspace needs at least one folder.".into());
        }
        let file = self.workspace_file().map(str::to_owned);
        let distro = match self.location() {
            Location::Wsl { distro, .. }
            | Location::Workspace {
                distro: Some(distro),
                ..
            } => Some(distro),
            _ => None,
        };
        if file.is_none() && folders.len() == 1 && folders[0].name.is_none() {
            let path = folders[0].path.clone();
            return Ok(match distro {
                Some(distro) => Location::Wsl { distro, path },
                None => Location::Local(PathBuf::from(path)),
            });
        }
        Ok(Location::Workspace {
            distro,
            file,
            folders,
        })
    }

    /// The text to save as a workspace file at `file` (on the project's
    /// machine): the folders, relative to the file where they share a
    /// folder, and the workspace settings.
    pub fn workspace_text(&self, file: &str) -> String {
        workspace_file::to_text(
            &self.folders(),
            self.workspace_settings(),
            file,
            self.location().posix(),
        )
    }

    /// Another directory reached the same way as this project (e.g. a git
    /// worktree path as git prints it); `None` inside a container, whose
    /// other directories cannot be opened on their own.
    pub fn location_of(&self, path: &str) -> Option<Location> {
        match &self.0.backend {
            Backend::Local => Some(Location::Local(PathBuf::from(path))),
            Backend::Remote {
                host: RemoteHost::Wsl { distro },
                ..
            } => Some(Location::Wsl {
                distro: distro.clone(),
                path: path.to_owned(),
            }),
            Backend::Remote {
                host: RemoteHost::Container(_),
                ..
            } => None,
            Backend::Remote {
                host: RemoteHost::Connector { scheme, host },
                ..
            } => Some(Location::Remote {
                scheme: scheme.clone(),
                host: host.clone(),
                path: path.to_owned(),
            }),
        }
    }

    /// Identifies the project in per-project state such as the saved layout.
    pub fn state_key(&self) -> String {
        if let Some(key) = self.location().workspace_state_key() {
            return key;
        }
        match &self.0.backend {
            Backend::Local => self.0.root.to_string_lossy().into_owned(),
            Backend::Remote {
                host: RemoteHost::Wsl { distro },
                ..
            } => format!("wsl:{distro}:{}", self.0.root.display()),
            Backend::Remote {
                host: RemoteHost::Container(container),
                ..
            } => format!(
                "devcontainer:{}:{}",
                container.config,
                self.0.root.display()
            ),
            Backend::Remote {
                host: RemoteHost::Connector { scheme, host },
                ..
            } => format!("{scheme}:{host}:{}", self.0.root.display()),
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self.0.backend, Backend::Remote { .. })
    }

    /// The agent serving a remote project (WSL, container, a connector's
    /// machine).
    pub fn agent(&self) -> Option<Arc<AgentClient>> {
        match &self.0.backend {
            Backend::Local => None,
            Backend::Remote { client, .. } => Some(client.clone()),
        }
    }

    pub fn in_container(&self) -> bool {
        matches!(
            self.0.backend,
            Backend::Remote {
                host: RemoteHost::Container(_),
                ..
            }
        )
    }

    /// `forwardPorts` and port attributes of the dev container (none for
    /// other projects).
    pub fn ports_config(&self) -> devcontainer::PortsConfig {
        match &self.0.backend {
            Backend::Remote {
                host: RemoteHost::Container(container),
                ..
            } => container.ports.clone(),
            _ => devcontainer::PortsConfig::default(),
        }
    }

    /// Receives why the agent disconnected; `None` for local projects.
    pub fn disconnects(&self) -> Option<UnboundedReceiver<String>> {
        match &self.0.backend {
            Backend::Local => None,
            Backend::Remote { client, .. } => Some(client.disconnects()),
        }
    }

    /// `dir/name`, in the project's path syntax (`name` may have several
    /// `/`-separated components).
    pub fn join(&self, dir: &Path, name: &str) -> PathBuf {
        match &self.0.backend {
            // Rebuilt from components so `/` becomes `\` on Windows.
            Backend::Local => dir.join(name).components().collect(),
            Backend::Remote { .. } => {
                let dir = dir.to_string_lossy();
                PathBuf::from(format!("{}/{name}", dir.trim_end_matches('/')))
            }
        }
    }

    /// The absolute path of a path relative to the first folder.
    pub fn resolve(&self, relative: &str) -> PathBuf {
        self.join(&self.0.root, relative)
    }

    /// The absolute path of a path relative to folder `root` (as
    /// [`Project::list_files`] lists them).
    pub fn resolve_in(&self, root: usize, relative: &str) -> PathBuf {
        self.join(&self.0.roots[root].path, relative)
    }

    /// A path given by a user or script: absolute as is, else relative to
    /// the first folder. Local paths are rebuilt from their components, so
    /// `src/a.rs` matches the `src\a.rs` the rest of the editor uses on
    /// Windows.
    pub fn path(&self, input: &str) -> PathBuf {
        self.path_in(0, input)
    }

    /// [`Project::path`] relative to folder `root`.
    pub fn path_in(&self, root: usize, input: &str) -> PathBuf {
        let root = &self.0.roots[root].path;
        match &self.0.backend {
            Backend::Local => root.join(input).components().collect(),
            Backend::Remote { .. } if input.starts_with('/') => PathBuf::from(input),
            Backend::Remote { .. } => self.join(root, input),
        }
    }

    /// The folder a user or script names: its index, its name, or its
    /// path.
    pub fn root_named(&self, name: &str) -> Option<usize> {
        let roots = &self.0.roots;
        if let Ok(index) = name.parse::<usize>()
            && index < roots.len()
        {
            return Some(index);
        }
        roots
            .iter()
            .position(|root| root.name == name)
            .or_else(|| roots.iter().position(|root| root.path == Path::new(name)))
    }

    /// The children of `dir`, gitignore-aware, dirs first.
    pub fn read_dir(&self, dir: &Path) -> io::Result<Vec<DirEntry>> {
        match &self.0.backend {
            Backend::Local => worktree::read_dir(dir),
            Backend::Remote { client, .. } => client.read_dir(agent_path(dir)?),
        }
    }

    /// A folder dialog on this project's machine, without tree ignore rules.
    pub fn browse_folders(&self, path: &str) -> io::Result<worktree::FolderListing> {
        match &self.0.backend {
            Backend::Local => worktree::browse_folders(Path::new(path)),
            Backend::Remote { client, .. } => client.browse_folders(path),
        }
    }

    /// Every non-ignored file of folder `root`, as sorted paths relative
    /// to it (see [`Project::resolve_in`]).
    pub fn list_files(&self, root: usize) -> io::Result<Vec<String>> {
        let root = &self.0.roots[root].path;
        match &self.0.backend {
            Backend::Local => Ok(worktree::list_files(root)),
            Backend::Remote { client, .. } => client.list_files(agent_path(root)?),
        }
    }

    /// A file's bytes.
    pub fn read_bytes(&self, path: &Path) -> io::Result<Vec<u8>> {
        match &self.0.backend {
            Backend::Local => fs::read(path),
            Backend::Remote { client, .. } => client.read_file(agent_path(path)?),
        }
    }

    /// A file's text; fails for binary or non-UTF-8 files.
    pub fn read_text(&self, path: &Path) -> io::Result<String> {
        worktree::decode_text(self.read_bytes(path)?)
    }

    pub fn write(&self, path: &Path, data: Vec<u8>) -> io::Result<()> {
        match &self.0.backend {
            Backend::Local => worktree::write_file(path, &data),
            Backend::Remote { client, .. } => client.write_file(agent_path(path)?, data),
        }
    }

    /// What is at `path`, or `None` if nothing is.
    pub fn kind(&self, path: &Path) -> io::Result<Option<FileKind>> {
        match &self.0.backend {
            Backend::Local => worktree::kind(path),
            Backend::Remote { client, .. } => client.stat(agent_path(path)?),
        }
    }

    /// Creates `dir` and its missing parents.
    pub fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        match &self.0.backend {
            Backend::Local => fs::create_dir_all(dir),
            Backend::Remote { .. } => self.run_tool("mkdir", &["-p", "--", agent_path(dir)?]),
        }
    }

    /// Moves a file or directory, replacing a file at `to`.
    pub fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        match &self.0.backend {
            Backend::Local => fs::rename(from, to),
            Backend::Remote { .. } => {
                self.run_tool("mv", &["-f", "--", agent_path(from)?, agent_path(to)?])
            }
        }
    }

    /// Deletes a file, or a directory (only an empty one unless `recursive`).
    pub fn remove(&self, path: &Path, recursive: bool) -> io::Result<()> {
        let dir = matches!(self.kind(path)?, Some(FileKind::Dir));
        match &self.0.backend {
            Backend::Local if !dir => fs::remove_file(path),
            Backend::Local if recursive => fs::remove_dir_all(path),
            Backend::Local => fs::remove_dir(path),
            Backend::Remote { .. } => {
                let path = agent_path(path)?;
                match (dir, recursive) {
                    (false, _) => self.run_tool("rm", &["-f", "--", path]),
                    (true, true) => self.run_tool("rm", &["-rf", "--", path]),
                    (true, false) => self.run_tool("rmdir", &["--", path]),
                }
            }
        }
    }

    /// Creates an empty file; fails if anything is at `path`.
    pub fn create_file(&self, path: &Path) -> io::Result<()> {
        match &self.0.backend {
            Backend::Local => fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map(drop),
            Backend::Remote { .. } => {
                if self.kind(path)?.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} already exists", path.display()),
                    ));
                }
                self.write(path, Vec::new())
            }
        }
    }

    /// Copies a file, or a folder with its contents, to `to` (which must
    /// not exist). Symlinks are copied as links where the project lives on
    /// Unix.
    pub fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        match &self.0.backend {
            Backend::Local => copy_local(from, to),
            Backend::Remote { .. } => self.run_tool(
                "cp",
                &["-R", "-P", "--", agent_path(from)?, agent_path(to)?],
            ),
        }
    }

    /// Whether [`Project::trash`] moves files to a recycle bin: local
    /// projects on Windows.
    pub fn can_trash(&self) -> bool {
        cfg!(windows) && !self.is_remote()
    }

    /// Moves files and folders to the Windows Recycle Bin (see
    /// [`Project::can_trash`]).
    pub fn trash(&self, paths: &[PathBuf]) -> io::Result<()> {
        if !self.can_trash() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this project has no recycle bin",
            ));
        }
        trash_local(paths)
    }

    /// Runs a POSIX tool where a remote project lives; its stderr is the
    /// error when it fails.
    fn run_tool(&self, program: &str, args: &[&str]) -> io::Result<()> {
        let args: Vec<String> = args.iter().map(|&arg| arg.to_owned()).collect();
        let output = self.run_program(program, &args, &[], None, Duration::from_secs(30))?;
        if output.code == Some(0) {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(io::Error::other(format!("{program}: {}", stderr.trim())))
    }

    /// Watches every folder; batches of all of them arrive on one channel.
    pub fn watch(&self, debounce: Duration) -> io::Result<(WatchHandle, UnboundedReceiver<Batch>)> {
        let mut handles = Vec::new();
        let mut receivers = Vec::new();
        for root in &self.0.roots {
            let (handle, batches) = match &self.0.backend {
                Backend::Local => {
                    let (watcher, batches) =
                        watcher::watch(&root.path, debounce).map_err(io::Error::other)?;
                    (WatchHandle::Local(watcher), batches)
                }
                Backend::Remote { client, .. } => {
                    let (handle, batches) = client.watch(agent_path(&root.path)?, debounce)?;
                    (WatchHandle::Remote(handle), batches)
                }
            };
            handles.push(handle);
            receivers.push(batches);
        }
        if handles.len() == 1 {
            return Ok((handles.remove(0), receivers.remove(0)));
        }
        let (tx, rx) = futures::channel::mpsc::unbounded();
        thread::Builder::new()
            .name("watch-merge".into())
            .spawn(move || {
                futures::executor::block_on(futures::stream::select_all(receivers).for_each(
                    |batch| {
                        tx.unbounded_send(batch).ok();
                        async {}
                    },
                ));
            })?;
        Ok((WatchHandle::Many(handles), rx))
    }

    /// Searches every folder; the error is shown as-is. Several folders'
    /// results arrive as one search: files as found, then one `Done`
    /// totalling them, after `limits.max_matches` matches at most.
    pub fn search(
        &self,
        query: &SearchQuery,
        limits: SearchLimits,
    ) -> Result<(SearchHandle, UnboundedReceiver<SearchEvent>), String> {
        let mut handles = Vec::new();
        let mut receivers = Vec::new();
        for root in &self.0.roots {
            let (handle, events) = match &self.0.backend {
                Backend::Local => search::search(&root.path, query, limits)
                    .map(|(handle, events)| (SearchHandle::Local(handle), events))
                    .map_err(|err| err.to_string()),
                Backend::Remote { client, .. } => agent_path(&root.path)
                    .and_then(|root| client.search(root, query, limits))
                    .map(|(handle, events)| (SearchHandle::Remote(handle), events))
                    .map_err(|err| err.to_string()),
            }?;
            handles.push(handle);
            receivers.push(events);
        }
        if handles.len() == 1 {
            return Ok((handles.remove(0), receivers.remove(0)));
        }
        let (tx, rx) = futures::channel::mpsc::unbounded();
        thread::Builder::new()
            .name("search-merge".into())
            .spawn(move || futures::executor::block_on(merge_searches(receivers, limits, tx)))
            .map_err(|err| err.to_string())?;
        Ok((SearchHandle::Many(handles), rx))
    }

    /// Finds a tool where the project lives (the first folder's
    /// virtualenv first).
    pub fn find_executable(&self, command: &str) -> io::Result<Option<PathBuf>> {
        self.find_executable_in(command, &self.0.root)
    }

    /// Finds a tool where the project lives (`dir`'s virtualenv first).
    pub fn find_executable_in(&self, command: &str, dir: &Path) -> io::Result<Option<PathBuf>> {
        match &self.0.backend {
            Backend::Local => Ok(worktree::find_executable(command, dir)),
            Backend::Remote { client, .. } => Ok(client
                .find_executable(command, agent_path(dir)?)?
                .map(PathBuf::from)),
        }
    }

    /// The first folder's virtualenv interpreter.
    pub fn python_interpreter(&self) -> io::Result<Option<PathBuf>> {
        self.python_interpreter_in(&self.0.root)
    }

    /// `dir`'s virtualenv interpreter.
    pub fn python_interpreter_in(&self, dir: &Path) -> io::Result<Option<PathBuf>> {
        match &self.0.backend {
            Backend::Local => Ok(worktree::python_interpreter(dir)),
            Backend::Remote { client, .. } => Ok(client
                .python_interpreter(agent_path(dir)?)?
                .map(PathBuf::from)),
        }
    }

    /// Starts a language server where the project lives, in `cwd` (a
    /// folder of the project).
    pub fn spawn_language_server(
        &self,
        program: &Path,
        args: &[String],
        cwd: &Path,
        logger: Option<Logger>,
    ) -> io::Result<(LanguageServer, UnboundedReceiver<Incoming>)> {
        match &self.0.backend {
            Backend::Local => {
                let mut command = Command::new(program);
                command
                    .args(args)
                    .current_dir(cwd)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                remote::launch::no_window(&mut command);
                let mut spawned = Tree::spawn(&mut command)?;
                let child = spawned.child.as_mut().expect("just spawned");
                let stdin = child.stdin.take().expect("piped stdin");
                let stdout = child.stdout.take().expect("piped stdout");
                let stderr = child.stderr.take().expect("piped stderr");
                let tree = spawned.watch()?;
                LanguageServer::attach(stdout, stdin, stderr, Box::new(tree), logger)
            }
            Backend::Remote { client, .. } => {
                client.spawn_language_server(agent_path(program)?, args, agent_path(cwd)?, logger)
            }
        }
    }

    /// Starts `program` where the project lives, in its root, with piped
    /// stdio (e.g. a debug adapter); stderr chunks go to `stderr`.
    pub fn spawn_piped(
        &self,
        program: &str,
        args: &[String],
        env: &[(String, String)],
        stderr: impl FnMut(&[u8]) + Send + 'static,
    ) -> io::Result<PipedProcess> {
        match &self.0.backend {
            Backend::Local => {
                let mut command = Command::new(program);
                command
                    .args(args)
                    .envs(env.iter().map(|(key, value)| (key, value)))
                    .current_dir(&self.0.root)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                remote::launch::no_window(&mut command);
                let mut spawned = Tree::spawn(&mut command)?;
                let child = spawned.child.as_mut().expect("just spawned");
                let stdin = child.stdin.take().expect("piped stdin");
                let stdout = child.stdout.take().expect("piped stdout");
                let mut error = child.stderr.take().expect("piped stderr");
                let tree = spawned.watch()?;
                let mut stderr = stderr;
                thread::Builder::new()
                    .name("piped-stderr".into())
                    .spawn(move || {
                        let mut buffer = [0; 4096];
                        while let Ok(read @ 1..) = error.read(&mut buffer) {
                            stderr(&buffer[..read]);
                        }
                    })?;
                Ok(PipedProcess {
                    stdout: Box::new(stdout),
                    stdin: Box::new(stdin),
                    // Dropping it ends the process and what it started.
                    guard: Box::new(tree),
                })
            }
            Backend::Remote { client, .. } => {
                client.spawn_piped(program, args, agent_path(&self.0.root)?, env, stderr)
            }
        }
    }

    /// Starts `program` (else the user's login shell) in a pty inside the
    /// WSL distro or container, in `cwd` (default: the project root); `tap`
    /// follows its output. Local projects start their terminals directly.
    pub fn spawn_remote_terminal(
        &self,
        program: Option<(&str, &[String])>,
        cwd: Option<&Path>,
        size: (u16, u16, usize),
        palette: Palette,
        tap: Option<terminal::OutputTap>,
    ) -> io::Result<(Terminal, UnboundedReceiver<TerminalEvent>)> {
        let (columns, rows, scrollback) = size;
        match &self.0.backend {
            Backend::Local => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "local projects start terminals directly",
            )),
            Backend::Remote { client, .. } => client.spawn_terminal(
                agent_path(cwd.unwrap_or(&self.0.root))?,
                program,
                columns,
                rows,
                scrollback,
                palette,
                tap,
            ),
        }
    }

    /// Runs a shell command line in the project root where the project
    /// lives (`cmd /C` on Windows, `sh -c` elsewhere), killing it after
    /// `timeout`.
    pub fn run(&self, command_line: &str, timeout: Duration) -> io::Result<RunOutput> {
        #[cfg(windows)]
        if !self.is_remote() {
            use std::os::windows::process::CommandExt;
            // The line goes to cmd as it is: std's quoting (`\"`) is not cmd's.
            let mut command = Command::new("cmd.exe");
            command.raw_arg("/C").raw_arg(format!("\"{command_line}\""));
            return run_local(command, "cmd.exe", &self.0.root, None, timeout, None);
        }
        self.run_program(
            "/bin/sh",
            &["-c".into(), command_line.into()],
            &[],
            None,
            timeout,
        )
    }

    /// Runs `program` in the first folder where the project lives with
    /// `input` on its stdin, killing it after `timeout`.
    pub fn run_program(
        &self,
        program: &str,
        args: &[String],
        env: &[(&str, &str)],
        input: Option<&[u8]>,
        timeout: Duration,
    ) -> io::Result<RunOutput> {
        self.run_program_in(&self.0.root, program, args, env, input, timeout)
    }

    /// [`Project::run_program`] in `cwd`.
    pub fn run_program_in(
        &self,
        cwd: &Path,
        program: &str,
        args: &[String],
        env: &[(&str, &str)],
        input: Option<&[u8]>,
        timeout: Duration,
    ) -> io::Result<RunOutput> {
        self.run_program_cancellable(cwd, program, args, env, input, timeout, None)
    }

    /// [`Project::run_program_in`] that `cancel` can end early: the
    /// process (and what it started) is asked to end, then killed, and
    /// the call returns `ErrorKind::Interrupted`; a token cancelled before
    /// the call fails it at once. Where the project lives behind the agent
    /// only that early check applies for now.
    #[allow(clippy::too_many_arguments)]
    pub fn run_program_cancellable(
        &self,
        cwd: &Path,
        program: &str,
        args: &[String],
        env: &[(&str, &str)],
        input: Option<&[u8]>,
        timeout: Duration,
        cancel: Option<&CancelToken>,
    ) -> io::Result<RunOutput> {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Err(cancelled(program));
        }
        match &self.0.backend {
            Backend::Remote { client, .. } => {
                client.run(program, args, agent_path(cwd)?, env, input, timeout)
            }
            Backend::Local => {
                let mut command = Command::new(program);
                command.args(args).envs(env.iter().copied());
                run_local(command, program, cwd, input, timeout, cancel)
            }
        }
    }

    /// The git repository containing folder `root`, if any. Blocks.
    pub fn repository_in(&self, root: usize) -> io::Result<Option<git::Repository>> {
        let root = self.0.roots[root].path.to_string_lossy().into_owned();
        git::Repository::discover(Arc::new(GitRunner(self.clone())), &root)
    }

    /// The repository containing the first folder with its status, read
    /// concurrently. Blocks.
    pub fn open_repository(&self) -> io::Result<Option<(git::Repository, git::Status)>> {
        self.open_repository_in(0)
    }

    /// [`Project::open_repository`] for folder `root`.
    pub fn open_repository_in(
        &self,
        root: usize,
    ) -> io::Result<Option<(git::Repository, git::Status)>> {
        let root = self.0.roots[root].path.to_string_lossy().into_owned();
        git::Repository::open(Arc::new(GitRunner(self.clone())), &root)
    }

    /// Marks the repository at `path` safe for git where the project lives
    /// (global `safe.directory`). Blocks.
    pub fn trust_repository(&self, path: &str) -> io::Result<()> {
        git::trust(&GitRunner(self.clone()), path)
    }

    /// A root-relative repository path (`/`-separated) as a project path.
    /// Paths inside the folder the repository was discovered from are
    /// built from that folder, so they match the editor's paths even where
    /// git spells the root differently (e.g. long vs 8.3 names on Windows).
    pub fn repository_path(&self, repository: &git::Repository, relative: &str) -> PathBuf {
        match relative.strip_prefix(repository.prefix()) {
            Some(inside) => self.join(Path::new(repository.dir()), inside),
            None => self.join(Path::new(repository.root()), relative),
        }
    }

    /// The settings for `path` (or the first folder): the user's, under
    /// the workspace file's `settings`, under the folder's own
    /// `.next_code/settings.json`. Cached per folder; rebuilt (without
    /// reading again) when the user settings change. The first call per
    /// folder, and the first after [`Project::folder_settings_changed`],
    /// reads its file where the project lives (`Workspace` does that on a
    /// background thread when it opens).
    pub fn settings_for(&self, path: Option<&Path>) -> Arc<Settings> {
        self.layered_settings(path).0
    }

    /// [`Project::settings_for`] and why workspace or folder settings were
    /// ignored, if they were.
    pub fn layered_settings(&self, path: Option<&Path>) -> (Arc<Settings>, Option<String>) {
        let user = crate::settings::current();
        let index = path.and_then(|path| self.root_index_of(path)).unwrap_or(0);
        let folder = match &self.0.folder_settings.lock()[index] {
            Some(cached) if Arc::ptr_eq(&cached.user, &user) => {
                return (cached.layered.clone(), cached.error.clone());
            }
            Some(cached) => Some(cached.folder.clone()),
            None => None,
        };
        let file = self.folder_settings_file(index);
        let (folder, read_error) = match folder {
            Some(folder) => folder,
            None => match self.read_text(&file) {
                Ok(text) => {
                    match serde_json::from_str::<Value>(&devcontainer::jsonc::to_json(&text)) {
                        Ok(value) => (value, None),
                        Err(err) => (Value::Null, Some(format!("{}: {err}", file.display()))),
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => (Value::Null, None),
                Err(err) => (Value::Null, Some(format!("{}: {err}", file.display()))),
            },
        };
        let mut error = read_error.clone();
        // Project settings of untrusted folders run no programs (see `trust`).
        let (safe_workspace, safe_folder);
        let workspace = if crate::trust::is_trusted(self) {
            self.workspace_settings()
        } else {
            safe_workspace = crate::settings::without_commands(self.workspace_settings());
            &safe_workspace
        };
        let folder_layer = if crate::trust::is_root_trusted(self, index) {
            &folder
        } else {
            safe_folder = crate::settings::without_commands(&folder);
            &safe_folder
        };
        let layered = user
            .layered(&[workspace, folder_layer])
            .unwrap_or_else(|err| {
                error = Some(format!("{}: {err}", file.display()));
                // The workspace's settings alone may still be fine.
                user.layered(&[workspace]).unwrap_or_else(|err| {
                    error = Some(format!(
                        "{}: {err}",
                        self.workspace_file().unwrap_or("workspace settings")
                    ));
                    (*user).clone()
                })
            });
        let layered = Arc::new(layered);
        self.0.folder_settings.lock()[index] = Some(FolderSettings {
            user,
            folder: (folder, read_error),
            layered: layered.clone(),
            error: error.clone(),
        });
        (layered, error)
    }

    /// Folder `root`'s own settings file.
    fn folder_settings_file(&self, root: usize) -> PathBuf {
        let dir = self.join(&self.0.roots[root].path, FOLDER_SETTINGS_DIR);
        self.join(&dir, "settings.json")
    }

    /// Forgets the settings of folders whose `.next_code/settings.json`
    /// is among `paths` (changed on disk); true if any was.
    pub fn folder_settings_changed<'a>(&self, paths: impl IntoIterator<Item = &'a Path>) -> bool {
        let mut changed = false;
        for path in paths {
            if let Some(index) = self.root_index_of(path)
                && path == self.folder_settings_file(index)
            {
                self.0.folder_settings.lock()[index] = None;
                changed = true;
            }
        }
        changed
    }

    /// A repository file's working-tree bytes, read where the project
    /// lives; `None` if it doesn't exist. Blocks.
    pub fn worktree_file(
        &self,
        repository: &git::Repository,
        relative: &str,
    ) -> io::Result<Option<Vec<u8>>> {
        match self.read_bytes(&self.repository_path(repository, relative)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }
}

/// Copies a file or a folder tree on this machine; `to` must not exist.
fn copy_local(from: &Path, to: &Path) -> io::Result<()> {
    if fs::symlink_metadata(to).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", to.display()),
        ));
    }
    #[cfg(unix)]
    if fs::symlink_metadata(from)?.file_type().is_symlink() {
        return std::os::unix::fs::symlink(fs::read_link(from)?, to);
    }
    if fs::metadata(from)?.is_dir() {
        fs::create_dir(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_local(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(drop)
    }
}

/// Moves paths to the Recycle Bin (`SHFileOperationW` with undo allowed,
/// no UI). Drives without a recycle bin delete for good, as Explorer does
/// after asking.
#[cfg(windows)]
fn trash_local(paths: &[PathBuf]) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows::{
        Win32::UI::Shell::{
            FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
            SHFILEOPSTRUCTW, SHFileOperationW,
        },
        core::PCWSTR,
    };

    // The shell does not take `\\?\` paths: `\\?\C:\x` → `C:\x`,
    // `\\?\UNC\host\share` → `\\host\share`.
    let verbatim: Vec<u16> = r"\\?\".encode_utf16().collect();
    let verbatim_unc: Vec<u16> = r"\\?\UNC\".encode_utf16().collect();
    // NUL-separated, double-NUL-terminated.
    let mut from: Vec<u16> = Vec::new();
    for path in paths {
        let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if let Some(rest) = wide.strip_prefix(verbatim_unc.as_slice()) {
            from.extend(r"\\".encode_utf16());
            from.extend_from_slice(rest);
        } else if let Some(rest) = wide.strip_prefix(verbatim.as_slice()) {
            from.extend_from_slice(rest);
        } else {
            from.extend_from_slice(&wide);
        }
        from.push(0);
    }
    from.push(0);
    let mut operation = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO.0 | FOF_NOCONFIRMATION.0 | FOF_NOERRORUI.0 | FOF_SILENT.0) as u16,
        ..Default::default()
    };
    // SAFETY: `operation` points at `from`, which outlives the call.
    let code = unsafe { SHFileOperationW(&mut operation) };
    if code != 0 {
        return Err(io::Error::other(format!(
            "could not move to the Recycle Bin (shell error {code:#x})"
        )));
    }
    if operation.fAnyOperationsAborted.as_bool() {
        return Err(io::Error::other("moving to the Recycle Bin was cancelled"));
    }
    Ok(())
}

#[cfg(not(windows))]
fn trash_local(_: &[PathBuf]) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "there is no recycle bin here",
    ))
}

/// Runs git where the project lives.
struct GitRunner(Project);

/// Long enough for commit hooks.
const GIT_TIMEOUT: Duration = Duration::from_secs(120);

impl git::Runner for GitRunner {
    fn run_git(&self, args: &[String], input: Option<&[u8]>) -> io::Result<git::Output> {
        // A dev container's workspace is a bind mount usually owned by
        // another user than the container's, which git refuses ("dubious
        // ownership"); the container is a sandbox the user chose to open,
        // so its folders are trusted for the editor's own git runs (the
        // config is untouched; command-line config counts as protected).
        let trusted;
        let args = if self.0.in_container() {
            trusted = ["-c", "safe.directory=*"]
                .into_iter()
                .map(String::from)
                .chain(args.iter().cloned())
                .collect::<Vec<_>>();
            &trusted[..]
        } else {
            args
        };
        let output = self.0.run_program(
            "git",
            args,
            &[("GIT_TERMINAL_PROMPT", "0"), ("GIT_OPTIONAL_LOCKS", "0")],
            input,
            GIT_TIMEOUT,
        )?;
        Ok(git::Output {
            code: output.code,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

/// The machine where a project folder lives, for `devcontainer`: docker and
/// files go through the project (locally or through the WSL agent).
struct FolderHost(Project);

impl devcontainer::Host for FolderHost {
    fn trusted(&self) -> bool {
        crate::trust::is_trusted(&self.0)
    }

    fn run(
        &self,
        program: &str,
        args: &[String],
        input: Option<&[u8]>,
        timeout: Duration,
    ) -> io::Result<devcontainer::Output> {
        let output = self.0.run_program(program, args, &[], input, timeout)?;
        Ok(devcontainer::Output {
            code: output.code,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn read_file(&self, path: &str) -> io::Result<Option<Vec<u8>>> {
        match self.0.read_bytes(Path::new(path)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn write_file(&self, path: &str, data: &[u8]) -> io::Result<()> {
        self.0.write(Path::new(path), data.to_vec())
    }

    fn list_dirs(&self, path: &str) -> io::Result<Vec<String>> {
        match self.0.read_dir(Path::new(path)) {
            Ok(entries) => Ok(entries
                .into_iter()
                .filter(|entry| entry.is_dir)
                .map(|entry| entry.name)
                .collect()),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(Vec::new())
            }
            Err(err) => Err(err),
        }
    }

    fn temp_dir(&self) -> String {
        if self.0.is_remote() {
            "/tmp".into()
        } else {
            std::env::temp_dir().to_string_lossy().into_owned()
        }
    }

    fn env_var(&self, name: &str) -> Option<String> {
        if !self.0.is_remote() {
            return std::env::var(name).ok();
        }
        let output = self
            .0
            .run_program(
                "printenv",
                &[name.to_owned()],
                &[],
                None,
                Duration::from_secs(30),
            )
            .ok()?;
        (output.code == Some(0)).then(|| {
            String::from_utf8_lossy(&output.stdout)
                .trim_end_matches('\n')
                .to_owned()
        })
    }

    fn windows(&self) -> bool {
        cfg!(windows) && !self.0.is_remote()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh temp dir for one test.
    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nc-project-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn folder(path: &Path) -> Folder {
        Folder {
            path: path.to_string_lossy().into_owned(),
            name: None,
        }
    }

    fn untitled(folders: Vec<Folder>) -> Location {
        Location::Workspace {
            distro: None,
            file: None,
            folders,
        }
    }

    #[test]
    fn workspace_file_paths_parse_as_workspaces_wherever_they_live() {
        let workspace = |distro: Option<&str>, file: &str| Location::Workspace {
            distro: distro.map(str::to_owned),
            file: Some(file.to_owned()),
            folders: Vec::new(),
        };
        assert_eq!(
            Location::parse("/src/all.next_code-workspace").unwrap(),
            workspace(None, "/src/all.next_code-workspace")
        );
        assert_eq!(
            Location::parse("Ubuntu:/home/me/team.code-workspace").unwrap(),
            workspace(Some("Ubuntu"), "/home/me/team.code-workspace")
        );
        assert_eq!(
            Location::parse(r"\\wsl.localhost\Ubuntu\home\me\a.next_code-workspace").unwrap(),
            workspace(Some("Ubuntu"), "/home/me/a.next_code-workspace")
        );
        // Folders (and other files) stay what they were.
        assert_eq!(
            Location::parse("Ubuntu:/home/me/p").unwrap(),
            Location::Wsl {
                distro: "Ubuntu".into(),
                path: "/home/me/p".into()
            }
        );
        assert!(matches!(
            Location::parse("/src/workspace.json").unwrap(),
            Location::Local(_)
        ));
    }

    #[test]
    fn a_saved_workspace_is_the_same_project_whatever_its_folders() {
        let file = |folders: Vec<Folder>| Location::Workspace {
            distro: None,
            file: Some("/w/a.next_code-workspace".into()),
            folders,
        };
        let one = file(vec![folder(Path::new("/w/x"))]);
        let two = file(vec![folder(Path::new("/w/x")), folder(Path::new("/w/y"))]);
        assert!(one.opens_same(&two));
        assert_eq!(one.workspace_state_key(), two.workspace_state_key());
        // Untitled workspaces are their folders, in order.
        let a = untitled(vec![folder(Path::new("/w/x")), folder(Path::new("/w/y"))]);
        let b = untitled(vec![folder(Path::new("/w/y")), folder(Path::new("/w/x"))]);
        assert!(!a.opens_same(&b));
        assert_ne!(a.workspace_state_key(), b.workspace_state_key());
        assert_eq!(
            a.workspace_state_key(),
            untitled(vec![folder(Path::new("/w/x")), folder(Path::new("/w/y"))])
                .workspace_state_key(),
            "keys are stable"
        );
        assert_eq!(Location::Local("/w/x".into()).workspace_state_key(), None);
    }

    #[test]
    fn verbatim_windows_paths_get_their_plain_spelling() {
        let plain = |path: &str| without_verbatim_prefix(PathBuf::from(path));
        assert_eq!(plain(r"\\?\C:\Users\me\p"), PathBuf::from(r"C:\Users\me\p"));
        assert_eq!(
            plain(r"\\?\UNC\host\share\p"),
            PathBuf::from(r"\\host\share\p")
        );
        assert_eq!(plain(r"\\?\Volume{1}\p"), PathBuf::from(r"\\?\Volume{1}\p"));
        assert_eq!(plain(r"C:\p"), PathBuf::from(r"C:\p"));
        if cfg!(windows) {
            let project = Project::local(PathBuf::from(r"\\?\C:\Users\me\p"));
            assert_eq!(project.roots()[0].path, PathBuf::from(r"C:\Users\me\p"));
        }
    }

    #[test]
    fn paths_resolve_to_the_innermost_folder_and_are_labelled_by_it() {
        let dir = temp("roots");
        let (api, web, nested) = (
            dir.join("api"),
            dir.join("web"),
            dir.join("web").join("lib"),
        );
        for folder in [&api, &web, &nested] {
            fs::create_dir_all(folder).unwrap();
        }
        let mut folders = vec![folder(&api), folder(&web), folder(&nested)];
        folders[1].name = Some("Frontend".into());
        let project = Project::open(&untitled(folders), &mut |_| {}).unwrap();
        assert!(project.is_multi_root() && project.is_workspace());
        assert_eq!(project.root(), api.as_path());
        let names: Vec<_> = project
            .roots()
            .iter()
            .map(|root| root.name.as_str())
            .collect();
        assert_eq!(names, ["api", "Frontend", "lib"]);

        let file = nested.join("x").join("a.rs");
        assert_eq!(project.root_index_of(&file), Some(2), "innermost folder");
        assert_eq!(project.relative(&file), Some((2, "x/a.rs".into())));
        assert_eq!(project.display_path(&file), "lib › x/a.rs");
        assert_eq!(
            project.display_path(&web.join("index.ts")),
            "Frontend › index.ts"
        );
        let outside = dir.join("elsewhere.txt");
        assert_eq!(project.root_index_of(&outside), None);
        assert_eq!(project.root_for(&outside).name, "api");
        assert_eq!(project.display_path(&outside), outside.to_string_lossy());
        assert_eq!(project.root_named("Frontend"), Some(1));
        assert_eq!(project.root_named("2"), Some(2));
        assert_eq!(project.root_named("nope"), None);
        assert_eq!(project.path_in(1, "src/a.ts"), web.join("src").join("a.ts"));

        // A single folder labels nothing.
        let single = Project::local(api.clone());
        assert_eq!(single.display_path(&api.join("src/main.rs")), "src/main.rs");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn workspace_files_open_with_relative_folders_and_layer_settings() {
        let dir = temp("file");
        let (app, lib) = (dir.join("ws").join("app"), dir.join("lib"));
        fs::create_dir_all(&app).unwrap();
        fs::create_dir_all(lib.join(".next_code")).unwrap();
        let file = dir.join("ws").join("all.next_code-workspace");
        fs::write(
            &file,
            r#"{ "folders": [{ "path": "app" }, { "path": "../lib", "name": "Library" }],
                 // comments are fine
                 "settings": { "format_on_save": true, "word_wrap": true } }"#,
        )
        .unwrap();
        fs::write(
            lib.join(".next_code").join("settings.json"),
            r#"{ "word_wrap": false }"#,
        )
        .unwrap();
        let location = Location::parse(&file.to_string_lossy()).unwrap();
        let project = Project::open(&location, &mut |_| {}).unwrap();
        let paths: Vec<_> = project
            .roots()
            .iter()
            .map(|root| root.path.clone())
            .collect();
        assert_eq!(paths, [app.clone(), lib.clone()]);
        assert_eq!(project.workspace_file(), Some(&*file.to_string_lossy()));
        assert!(matches!(
            project.location(),
            Location::Workspace { file: Some(_), ref folders, .. } if folders.len() == 2
        ));

        // user < workspace file < folder settings.
        let in_app = project.settings_for(Some(&app.join("main.py")));
        assert!(in_app.format_on_save && in_app.word_wrap);
        let in_lib = project.settings_for(Some(&lib.join("x.py")));
        assert!(in_lib.format_on_save && !in_lib.word_wrap);

        // A broken folder file is reported and skipped once it changes.
        let settings = lib.join(".next_code").join("settings.json");
        fs::write(&settings, "{ broken").unwrap();
        assert!(
            project.layered_settings(Some(&lib)).1.is_none(),
            "still cached"
        );
        assert!(project.folder_settings_changed([settings.as_path()]));
        let (in_lib, error) = project.layered_settings(Some(&lib));
        assert!(error.is_some_and(|error| error.contains("settings.json")));
        assert!(in_lib.word_wrap, "the workspace layer still applies");
        assert!(!project.folder_settings_changed([app.join("settings.json").as_path()]));

        // Saving writes the folders relative to the file where possible.
        let text = project.workspace_text(&file.to_string_lossy());
        let parsed = workspace_file::parse(&text, &file.to_string_lossy(), !cfg!(windows)).unwrap();
        assert_eq!(parsed.folders, project.folders());
        assert!(text.contains("\"app\""), "{text}");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn changing_folders_keeps_the_machine_and_drops_back_to_a_folder() {
        let dir = temp("with");
        let (a, b) = (dir.join("a"), dir.join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let single = Project::local(a.clone());
        assert_eq!(
            single.with_folders(vec![folder(&a), folder(&b)]),
            Ok(untitled(vec![folder(&a), folder(&b)]))
        );
        let two = Project::open(&untitled(vec![folder(&a), folder(&b)]), &mut |_| {}).unwrap();
        assert_eq!(
            two.with_folders(vec![folder(&b)]),
            Ok(Location::Local(b.clone()))
        );
        assert!(two.with_folders(Vec::new()).is_err());
        assert!(
            Project::open(&untitled(Vec::new()), &mut |_| {}).is_err(),
            "a workspace needs a folder"
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_search_covers_every_folder_once() {
        let dir = temp("search");
        let (a, b) = (dir.join("a"), dir.join("a").join("b"));
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("one.txt"), "needle\n").unwrap();
        fs::write(b.join("two.txt"), "needle needle\n").unwrap();
        let project = Project::open(&untitled(vec![folder(&a), folder(&b)]), &mut |_| {}).unwrap();
        let query = SearchQuery {
            pattern: "needle".into(),
            ..Default::default()
        };
        let collect = |limits| {
            let (_handle, events) = project.search(&query, limits).unwrap();
            futures::executor::block_on(events.collect::<Vec<_>>())
        };
        let events = collect(SearchLimits::default());
        let mut files: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                SearchEvent::File(file) => Some(file.path.clone()),
                SearchEvent::Done { .. } => None,
            })
            .collect();
        files.sort();
        // `b` lies inside `a` too: its file is reported once.
        assert_eq!(files, [a.join("b").join("two.txt"), a.join("one.txt")]);
        assert!(matches!(
            events.last(),
            Some(SearchEvent::Done {
                matches: 3,
                limit_hit: false,
                ..
            })
        ));
        let limited = collect(SearchLimits {
            max_matches: 1,
            ..SearchLimits::default()
        });
        assert!(matches!(
            limited.last(),
            Some(SearchEvent::Done {
                limit_hit: true,
                ..
            })
        ));
        fs::remove_dir_all(dir).ok();
    }
}
