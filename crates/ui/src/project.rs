//! The open project and where its files live: on this machine, or inside a
//! WSL distro behind `next_code_agent`.
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

use futures::channel::mpsc::UnboundedReceiver;
use lsp::{Incoming, LanguageServer, Logger};
use remote::{AgentClient, RunOutput, StreamHandle, agent_path};
use search::{SearchEvent, SearchLimits, SearchQuery};
use serde::{Deserialize, Serialize};
use terminal::{Palette, Terminal, TerminalEvent};
use watcher::Batch;
use worktree::{DirEntry, FileKind};

/// What the user asked to open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Location {
    Local(PathBuf),
    /// A Linux path inside a WSL distro, never reinterpreted as a Windows path.
    Wsl {
        distro: String,
        path: String,
    },
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local(root) => write!(f, "{}", root.display()),
            Self::Wsl { distro, path } => write!(f, "{path} [WSL: {distro}]"),
        }
    }
}

impl Location {
    /// Reads a typed location: `\\wsl.localhost\<distro>\path` (or
    /// `\\wsl$\...`), `<distro>:/linux/path` (or `<distro>:~/...`), else a
    /// local path.
    pub fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        if input.is_empty() {
            return None;
        }
        if let Some(wsl) = wsl_unc_path(input) {
            return Some(wsl);
        }
        // One-letter prefixes are Windows drives (`C:/dev`).
        if let Some((distro, path)) = input.split_once(':')
            && distro.len() > 1
            && !distro.contains(['/', '\\', ' '])
            && (path.starts_with('/') || path.starts_with('~'))
        {
            return Some(Self::Wsl {
                distro: distro.to_owned(),
                path: path.to_owned(),
            });
        }
        Some(Self::Local(PathBuf::from(input)))
    }
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
    root: PathBuf,
    backend: Backend,
}

enum Backend {
    Local,
    Wsl {
        distro: String,
        client: Arc<AgentClient>,
    },
}

/// Stops watching when dropped.
pub enum WatchHandle {
    Local(#[allow(dead_code)] watcher::ProjectWatcher),
    Remote(#[allow(dead_code)] StreamHandle),
}

/// Cancels the search when dropped.
pub enum SearchHandle {
    Local(#[allow(dead_code)] search::SearchHandle),
    Remote(#[allow(dead_code)] StreamHandle),
}

impl Project {
    pub fn local(root: PathBuf) -> Self {
        Self(Arc::new(Inner {
            root,
            backend: Backend::Local,
        }))
    }

    /// Starts the agent in `distro` and opens `path` there. Blocks for as
    /// long as WSL takes to start.
    pub fn connect_wsl(distro: &str, path: &str) -> io::Result<Self> {
        let connection = remote::wsl::connect(distro, path)?;
        Ok(Self(Arc::new(Inner {
            root: PathBuf::from(connection.info.root),
            backend: Backend::Wsl {
                distro: distro.to_owned(),
                client: connection.client,
            },
        })))
    }

    pub fn root(&self) -> &Path {
        &self.0.root
    }

    pub fn location(&self) -> Location {
        match &self.0.backend {
            Backend::Local => Location::Local(self.0.root.clone()),
            Backend::Wsl { distro, .. } => Location::Wsl {
                distro: distro.clone(),
                path: self.0.root.to_string_lossy().into_owned(),
            },
        }
    }

    /// Another directory reached the same way as this project (e.g. a git
    /// worktree path as git prints it).
    pub fn location_of(&self, path: &str) -> Location {
        match &self.0.backend {
            Backend::Local => Location::Local(PathBuf::from(path)),
            Backend::Wsl { distro, .. } => Location::Wsl {
                distro: distro.clone(),
                path: path.to_owned(),
            },
        }
    }

    /// Identifies the project in per-project state such as the saved layout.
    pub fn state_key(&self) -> String {
        match &self.0.backend {
            Backend::Local => self.0.root.to_string_lossy().into_owned(),
            Backend::Wsl { distro, .. } => format!("wsl:{distro}:{}", self.0.root.display()),
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self.0.backend, Backend::Wsl { .. })
    }

    /// Receives why the agent disconnected; `None` for local projects.
    pub fn disconnects(&self) -> Option<UnboundedReceiver<String>> {
        match &self.0.backend {
            Backend::Local => None,
            Backend::Wsl { client, .. } => Some(client.disconnects()),
        }
    }

    /// `dir/name`, in the project's path syntax (`name` may have several
    /// `/`-separated components).
    pub fn join(&self, dir: &Path, name: &str) -> PathBuf {
        match &self.0.backend {
            // Rebuilt from components so `/` becomes `\` on Windows.
            Backend::Local => dir.join(name).components().collect(),
            Backend::Wsl { .. } => {
                let dir = dir.to_string_lossy();
                PathBuf::from(format!("{}/{name}", dir.trim_end_matches('/')))
            }
        }
    }

    /// The absolute path of a root-relative one from [`Project::list_files`].
    pub fn resolve(&self, relative: &str) -> PathBuf {
        self.join(&self.0.root, relative)
    }

    /// A path given by a user or script: absolute as is, else root-relative.
    /// Local paths are rebuilt from their components, so `src/a.rs` matches
    /// the `src\a.rs` the rest of the editor uses on Windows.
    pub fn path(&self, input: &str) -> PathBuf {
        match &self.0.backend {
            Backend::Local => self.0.root.join(input).components().collect(),
            Backend::Wsl { .. } if input.starts_with('/') => PathBuf::from(input),
            Backend::Wsl { .. } => self.resolve(input),
        }
    }

    /// The children of `dir`, gitignore-aware, dirs first.
    pub fn read_dir(&self, dir: &Path) -> io::Result<Vec<DirEntry>> {
        match &self.0.backend {
            Backend::Local => worktree::read_dir(dir),
            Backend::Wsl { client, .. } => client.read_dir(agent_path(dir)?),
        }
    }

    /// Every non-ignored file, as sorted root-relative paths.
    pub fn list_files(&self) -> io::Result<Vec<String>> {
        match &self.0.backend {
            Backend::Local => Ok(worktree::list_files(&self.0.root)),
            Backend::Wsl { client, .. } => client.list_files(agent_path(&self.0.root)?),
        }
    }

    /// A file's bytes.
    pub fn read_bytes(&self, path: &Path) -> io::Result<Vec<u8>> {
        match &self.0.backend {
            Backend::Local => fs::read(path),
            Backend::Wsl { client, .. } => client.read_file(agent_path(path)?),
        }
    }

    /// A file's text; fails for binary or non-UTF-8 files.
    pub fn read_text(&self, path: &Path) -> io::Result<String> {
        worktree::decode_text(self.read_bytes(path)?)
    }

    pub fn write(&self, path: &Path, data: Vec<u8>) -> io::Result<()> {
        match &self.0.backend {
            Backend::Local => worktree::write_file(path, &data),
            Backend::Wsl { client, .. } => client.write_file(agent_path(path)?, data),
        }
    }

    /// What is at `path`, or `None` if nothing is.
    pub fn kind(&self, path: &Path) -> io::Result<Option<FileKind>> {
        match &self.0.backend {
            Backend::Local => worktree::kind(path),
            Backend::Wsl { client, .. } => client.stat(agent_path(path)?),
        }
    }

    pub fn watch(&self, debounce: Duration) -> io::Result<(WatchHandle, UnboundedReceiver<Batch>)> {
        match &self.0.backend {
            Backend::Local => {
                let (watcher, batches) =
                    watcher::watch(&self.0.root, debounce).map_err(io::Error::other)?;
                Ok((WatchHandle::Local(watcher), batches))
            }
            Backend::Wsl { client, .. } => {
                let (handle, batches) = client.watch(agent_path(&self.0.root)?, debounce)?;
                Ok((WatchHandle::Remote(handle), batches))
            }
        }
    }

    /// Searches the project; the error is shown as-is.
    pub fn search(
        &self,
        query: &SearchQuery,
        limits: SearchLimits,
    ) -> Result<(SearchHandle, UnboundedReceiver<SearchEvent>), String> {
        match &self.0.backend {
            Backend::Local => search::search(&self.0.root, query, limits)
                .map(|(handle, events)| (SearchHandle::Local(handle), events))
                .map_err(|err| err.to_string()),
            Backend::Wsl { client, .. } => agent_path(&self.0.root)
                .and_then(|root| client.search(root, query, limits))
                .map(|(handle, events)| (SearchHandle::Remote(handle), events))
                .map_err(|err| err.to_string()),
        }
    }

    /// Finds a tool where the project lives (its virtualenv first).
    pub fn find_executable(&self, command: &str) -> io::Result<Option<PathBuf>> {
        match &self.0.backend {
            Backend::Local => Ok(worktree::find_executable(command, &self.0.root)),
            Backend::Wsl { client, .. } => Ok(client
                .find_executable(command, agent_path(&self.0.root)?)?
                .map(PathBuf::from)),
        }
    }

    pub fn python_interpreter(&self) -> io::Result<Option<PathBuf>> {
        match &self.0.backend {
            Backend::Local => Ok(worktree::python_interpreter(&self.0.root)),
            Backend::Wsl { client, .. } => Ok(client
                .python_interpreter(agent_path(&self.0.root)?)?
                .map(PathBuf::from)),
        }
    }

    /// Starts a language server where the project lives, in its root.
    pub fn spawn_language_server(
        &self,
        program: &Path,
        args: &[String],
        logger: Option<Logger>,
    ) -> io::Result<(LanguageServer, UnboundedReceiver<Incoming>)> {
        match &self.0.backend {
            Backend::Local => {
                let mut command = Command::new(program);
                command.args(args).current_dir(&self.0.root);
                #[cfg(windows)]
                {
                    // Don't flash a console window from a GUI process.
                    use std::os::windows::process::CommandExt;
                    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
                }
                LanguageServer::spawn(&mut command, logger)
            }
            Backend::Wsl { client, .. } => client.spawn_language_server(
                agent_path(program)?,
                args,
                agent_path(&self.0.root)?,
                logger,
            ),
        }
    }

    /// Starts the user's login shell inside the WSL distro, in the project
    /// root. Local projects pick their shell from the terminal profiles.
    pub fn spawn_remote_shell(
        &self,
        columns: u16,
        rows: u16,
        scrollback: usize,
        palette: Palette,
    ) -> io::Result<(Terminal, UnboundedReceiver<TerminalEvent>)> {
        match &self.0.backend {
            Backend::Local => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "local projects start shells directly",
            )),
            Backend::Wsl { client, .. } => client.spawn_terminal(
                agent_path(&self.0.root)?,
                columns,
                rows,
                scrollback,
                palette,
            ),
        }
    }

    /// Runs a shell command line in the project root where the project
    /// lives (`cmd /C` on Windows, `sh -c` elsewhere), killing it after
    /// `timeout`.
    pub fn run(&self, command_line: &str, timeout: Duration) -> io::Result<RunOutput> {
        let (shell, flag) = if self.is_remote() || !cfg!(windows) {
            ("/bin/sh", "-c")
        } else {
            ("cmd.exe", "/C")
        };
        self.run_program(
            shell,
            &[flag.into(), command_line.into()],
            &[],
            None,
            timeout,
        )
    }

    /// Runs `program` in the project root where the project lives with
    /// `input` on its stdin, killing it after `timeout`.
    pub fn run_program(
        &self,
        program: &str,
        args: &[String],
        env: &[(&str, &str)],
        input: Option<&[u8]>,
        timeout: Duration,
    ) -> io::Result<RunOutput> {
        match &self.0.backend {
            Backend::Wsl { client, .. } => client.run(
                program,
                args,
                agent_path(&self.0.root)?,
                env,
                input,
                timeout,
            ),
            Backend::Local => {
                let mut command = Command::new(program);
                command
                    .args(args)
                    .envs(env.iter().copied())
                    .current_dir(&self.0.root)
                    .stdin(if input.is_some() {
                        Stdio::piped()
                    } else {
                        Stdio::null()
                    })
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
                }
                let mut child = command.spawn()?;
                if let (Some(mut stdin), Some(input)) = (child.stdin.take(), input) {
                    let input = input.to_vec();
                    // Written on a thread so a full output pipe cannot deadlock.
                    thread::spawn(move || {
                        use std::io::Write;
                        stdin.write_all(&input).ok();
                    });
                }
                let collect = |mut pipe: Box<dyn Read + Send>| {
                    thread::spawn(move || {
                        let mut data = Vec::new();
                        pipe.read_to_end(&mut data).ok();
                        data
                    })
                };
                let stdout = collect(Box::new(child.stdout.take().expect("piped stdout")));
                let stderr = collect(Box::new(child.stderr.take().expect("piped stderr")));
                let deadline = Instant::now() + timeout;
                let status = loop {
                    if let Some(status) = child.try_wait()? {
                        break status;
                    }
                    if Instant::now() >= deadline {
                        child.kill().ok();
                        child.wait().ok();
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("{program} did not finish within {} s", timeout.as_secs()),
                        ));
                    }
                    thread::sleep(Duration::from_millis(20));
                };
                Ok(RunOutput {
                    code: status.code(),
                    stdout: stdout.join().unwrap_or_default(),
                    stderr: stderr.join().unwrap_or_default(),
                })
            }
        }
    }

    /// The git repository containing the project root, if any. Blocks.
    pub fn repository(&self) -> io::Result<Option<git::Repository>> {
        let root = self.0.root.to_string_lossy().into_owned();
        git::Repository::discover(Arc::new(GitRunner(self.clone())), &root)
    }

    /// A root-relative repository path (`/`-separated) as a project path.
    /// Paths inside the project are built from the project root, so they
    /// match the editor's paths even where git spells the root differently
    /// (e.g. long vs 8.3 names on Windows).
    pub fn repository_path(&self, repository: &git::Repository, relative: &str) -> PathBuf {
        match relative.strip_prefix(repository.prefix()) {
            Some(inside) => self.join(&self.0.root, inside),
            None => self.join(Path::new(repository.root()), relative),
        }
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

/// Runs git where the project lives.
struct GitRunner(Project);

/// Long enough for commit hooks.
const GIT_TIMEOUT: Duration = Duration::from_secs(120);

impl git::Runner for GitRunner {
    fn run_git(&self, args: &[String], input: Option<&[u8]>) -> io::Result<git::Output> {
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
