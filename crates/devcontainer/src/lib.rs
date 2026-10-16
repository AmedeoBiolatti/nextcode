//! Dev containers: reads `devcontainer.json` and brings its container up
//! with the `docker` CLI where the project folder lives (Windows or a WSL
//! distro), like the Dev Containers spec's reference implementation does,
//! without Node. Supported: `image`, `build` (Dockerfile) and
//! `dockerComposeFile` configs, mounts, environment, users, run arguments
//! and lifecycle commands. Dev Container Features are not installed.
//!
//! Containers are labelled `devcontainer.local_folder` /
//! `devcontainer.config_file` (compose ones are found by compose project and
//! service, named as VS Code names them), so a container started by VS Code
//! for the same folder is reused rather than duplicated.

mod config;
/// JSONC (comments, trailing commas), shared with run configurations'
/// `launch.json`.
pub mod jsonc;
pub mod ports;

use std::{collections::BTreeMap, io, time::Duration};

pub use config::{Build, Config, Kind, Lifecycle, Mount, ShutdownAction};
pub use ports::PortsConfig;

/// Quick docker calls (`ps`, `inspect`, `start`).
const SHORT: Duration = Duration::from_secs(120);
/// Builds, pulls, `compose up` and lifecycle scripts.
const LONG: Duration = Duration::from_secs(60 * 60);

/// What a finished command produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    /// `None` when killed by a signal.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// The machine where the project folder lives.
pub trait Host: Send + Sync {
    /// Runs `program` in the project folder to completion.
    fn run(
        &self,
        program: &str,
        args: &[String],
        input: Option<&[u8]>,
        timeout: Duration,
    ) -> io::Result<Output>;
    /// A file's bytes, `None` if it does not exist.
    fn read_file(&self, path: &str) -> io::Result<Option<Vec<u8>>>;
    fn write_file(&self, path: &str, data: &[u8]) -> io::Result<()>;
    /// Names of the subdirectories of `path` (empty if it does not exist).
    fn list_dirs(&self, path: &str) -> io::Result<Vec<String>>;
    /// A directory for generated files (compose overrides).
    fn temp_dir(&self) -> String;
    /// An environment variable of the host (`${localEnv:…}`).
    fn env_var(&self, name: &str) -> Option<String>;
    /// Windows paths (`\`, drive letters) rather than POSIX ones.
    fn windows(&self) -> bool;
}

fn join(host: &dyn Host, dir: &str, name: &str) -> String {
    let separator = if host.windows() { '\\' } else { '/' };
    let dir = dir.trim_end_matches(['/', '\\']);
    let name = name.trim_start_matches("./");
    if host.windows() {
        format!("{dir}{separator}{}", name.replace('/', "\\"))
    } else {
        format!("{dir}{separator}{name}")
    }
}

/// `path` without its last component.
fn parent(path: &str) -> &str {
    path.trim_end_matches(['/', '\\'])
        .rsplit_once(['/', '\\'])
        .map_or("", |(parent, _)| parent)
}

fn basename(path: &str) -> &str {
    path.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
}

/// `path` resolved against `dir` unless it is absolute.
fn resolve(host: &dyn Host, dir: &str, path: &str) -> String {
    let absolute = path.starts_with('/')
        || path.starts_with('\\')
        || (host.windows() && path.as_bytes().get(1) == Some(&b':'));
    if absolute {
        return path.to_owned();
    }
    let mut dir = dir.to_owned();
    let mut rest = path;
    loop {
        if let Some(stripped) = rest.strip_prefix("./").or_else(|| rest.strip_prefix(".\\")) {
            rest = stripped;
        } else if let Some(stripped) = rest
            .strip_prefix("../")
            .or_else(|| rest.strip_prefix("..\\"))
        {
            dir = parent(&dir).to_owned();
            rest = stripped;
        } else if rest == ".." {
            dir = parent(&dir).to_owned();
            rest = "";
        } else if rest == "." {
            rest = "";
        } else {
            break;
        }
    }
    if rest.is_empty() {
        dir
    } else {
        join(host, &dir, rest)
    }
}

/// The `devcontainer.json` files of `folder`: `.devcontainer/devcontainer.json`,
/// `.devcontainer.json`, then `.devcontainer/<name>/devcontainer.json`.
pub fn find_configs(host: &dyn Host, folder: &str) -> io::Result<Vec<String>> {
    let dir = join(host, folder, ".devcontainer");
    let mut found = Vec::new();
    for candidate in [
        join(host, &dir, "devcontainer.json"),
        join(host, folder, ".devcontainer.json"),
    ] {
        if host.read_file(&candidate)?.is_some() {
            found.push(candidate);
        }
    }
    for name in host.list_dirs(&dir)? {
        let candidate = join(host, &join(host, &dir, &name), "devcontainer.json");
        if host.read_file(&candidate)?.is_some() {
            found.push(candidate);
        }
    }
    Ok(found)
}

/// The project folder a `devcontainer.json` belongs to.
pub fn folder_of(config_path: &str) -> &str {
    let dir = parent(config_path);
    if basename(config_path) == ".devcontainer.json" {
        return dir;
    }
    // `.devcontainer/devcontainer.json` or `.devcontainer/<name>/devcontainer.json`.
    if basename(dir) == ".devcontainer" {
        return parent(dir);
    }
    let up = parent(dir);
    if basename(up) == ".devcontainer" {
        parent(up)
    } else {
        dir
    }
}

pub fn read_config(host: &dyn Host, path: &str) -> io::Result<Config> {
    let bytes = host
        .read_file(path)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{path} does not exist")))?;
    Config::parse(&String::from_utf8_lossy(&bytes))
}

/// Stable id of a folder's dev container (`${devcontainerId}`, image tag).
fn devcontainer_id(folder: &str, config_path: &str) -> String {
    let hash = format!("{folder}\0{config_path}")
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
    format!("{hash:016x}")
}

/// Values for `${…}` in the configuration.
struct Variables<'a> {
    host: &'a dyn Host,
    local_folder: &'a str,
    container_folder: &'a str,
    id: &'a str,
    /// The container's environment, once it runs (`${containerEnv:…}`).
    container_env: Option<&'a BTreeMap<String, String>>,
}

impl Variables<'_> {
    fn substitute(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("${") {
            out.push_str(&rest[..start]);
            let Some(end) = rest[start..].find('}') else {
                out.push_str(&rest[start..]);
                return out;
            };
            let name = &rest[start + 2..start + end];
            match self.value(name) {
                Some(value) => out.push_str(&value),
                None => out.push_str(&rest[start..start + end + 1]),
            }
            rest = &rest[start + end + 1..];
        }
        out.push_str(rest);
        out
    }

    fn value(&self, name: &str) -> Option<String> {
        let (scope, rest) = name.split_once(':').unwrap_or((name, ""));
        let (variable, default) = rest.split_once(':').unwrap_or((rest, ""));
        match scope {
            "localWorkspaceFolder" => Some(self.local_folder.to_owned()),
            "localWorkspaceFolderBasename" => Some(basename(self.local_folder).to_owned()),
            "containerWorkspaceFolder" => Some(self.container_folder.to_owned()),
            "containerWorkspaceFolderBasename" => Some(basename(self.container_folder).to_owned()),
            "devcontainerId" => Some(self.id.to_owned()),
            "localEnv" | "env" => Some(
                self.host
                    .env_var(variable)
                    .unwrap_or_else(|| default.to_owned()),
            ),
            "containerEnv" => self.container_env.map(|env| {
                env.get(variable)
                    .cloned()
                    .unwrap_or_else(|| default.to_owned())
            }),
            _ => None,
        }
    }
}

/// How to stop the container when the editor is done with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shutdown {
    None,
    Container(String),
    Compose { project: String, files: Vec<String> },
}

/// A running dev container, ready for the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub id: String,
    /// The configuration's `name`, else the container's.
    pub name: String,
    /// Who the editor works as; `None` is the image's default user.
    pub user: Option<String>,
    pub workspace_folder: String,
    /// `remoteEnv`, resolved.
    pub remote_env: Vec<(String, String)>,
    /// Whether this call created or started the container (so the editor
    /// stops it when done, following `shutdownAction`).
    pub started: bool,
    pub shutdown: Shutdown,
    /// `forwardPorts`, `portsAttributes`, `otherPortsAttributes`.
    pub ports: PortsConfig,
}

/// Docker's answer, or an error with its output.
fn docker(host: &dyn Host, args: &[String], timeout: Duration, what: &str) -> io::Result<String> {
    let output = host.run("docker", args, None, timeout).map_err(|err| {
        if err.kind() == io::ErrorKind::NotFound {
            io::Error::new(
                err.kind(),
                format!("Docker is not installed or not on PATH ({err})"),
            )
        } else {
            err
        }
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.code != Some(0) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let details = [stderr.trim(), stdout.trim()]
            .into_iter()
            .find(|text| !text.is_empty())
            .unwrap_or("no output");
        return Err(io::Error::other(format!("{what} failed: {details}")));
    }
    Ok(stdout)
}

fn args<const N: usize>(fixed: [&str; N]) -> Vec<String> {
    fixed.map(str::to_owned).to_vec()
}

/// A container `docker ps` listed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Found {
    /// Full id, as `docker run` prints it.
    id: String,
    running: bool,
    name: String,
}

/// Containers matching all `labels`.
fn find_containers(host: &dyn Host, labels: &[String]) -> io::Result<Vec<Found>> {
    let mut command = args([
        "ps",
        "-a",
        "--no-trunc",
        "--format",
        "{{.ID}}\t{{.State}}\t{{.Names}}",
    ]);
    for label in labels {
        command.push("--filter".into());
        command.push(format!("label={label}"));
    }
    let output = docker(host, &command, SHORT, "Looking for the dev container")?;
    Ok(parse_found(&output))
}

fn parse_found(output: &str) -> Vec<Found> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let id = fields.next()?.trim();
            let state = fields.next()?.trim();
            // Several names are comma-separated (legacy links).
            let name = fields.next().unwrap_or_default().split(',').next();
            (!id.is_empty()).then(|| Found {
                id: id.to_owned(),
                running: state == "running",
                name: name.unwrap_or_default().trim().to_owned(),
            })
        })
        .collect()
}

/// The compose project name VS Code gives a dev container: the folder name
/// for configs in `.devcontainer`, else the compose file's directory name;
/// a top-level `name:` in the first compose file wins.
fn compose_project(host: &dyn Host, folder: &str, first_file: &str) -> io::Result<String> {
    if let Some(bytes) = host.read_file(first_file)? {
        let text = String::from_utf8_lossy(&bytes);
        if let Some(name) = text.lines().find_map(|line| {
            line.strip_prefix("name:")
                .map(|value| value.trim().trim_matches(['"', '\'']).to_owned())
        }) && !name.is_empty()
        {
            return Ok(name);
        }
    }
    let dir = parent(first_file);
    let base = if basename(dir) == ".devcontainer" {
        format!("{}_devcontainer", basename(folder))
    } else {
        basename(dir).to_owned()
    };
    Ok(base
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect())
}

/// JSON strings are valid YAML scalars.
fn yaml(text: &str) -> String {
    serde_json::to_string(text).expect("strings serialize")
}

/// The first line of an error, for progress messages.
fn first_line(err: &io::Error) -> String {
    err.to_string()
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// Runs the configuration's commands inside the container.
fn run_in_container(
    host: &dyn Host,
    started: &Started,
    what: &str,
    lifecycle: Option<&Lifecycle>,
    progress: &mut dyn FnMut(&str),
) -> io::Result<()> {
    let Some(lifecycle) = lifecycle else {
        return Ok(());
    };
    for (label, argv) in lifecycle.commands_with(&["/bin/sh", "-lc"]) {
        if label.trim().is_empty() {
            continue;
        }
        progress(&format!("Running {what}: {label}"));
        let mut command = args(["exec"]);
        if let Some(user) = &started.user {
            command.extend(["-u".into(), user.clone()]);
        }
        command.extend(["-w".into(), started.workspace_folder.clone()]);
        for (key, value) in &started.remote_env {
            command.extend(["-e".into(), format!("{key}={value}")]);
        }
        command.push(started.id.clone());
        command.extend(argv);
        let output = docker(host, &command, LONG, what)?;
        for line in output.lines().filter(|line| !line.trim().is_empty()) {
            progress(line);
        }
    }
    Ok(())
}

/// Brings up the dev container of `config_path` (creating, building or
/// starting it as needed), runs its lifecycle commands, and reports steps
/// to `progress`.
pub fn up(
    host: &dyn Host,
    config_path: &str,
    progress: &mut dyn FnMut(&str),
) -> io::Result<Started> {
    let config = read_config(host, config_path)?;
    let folder = folder_of(config_path).to_owned();
    let config_dir = parent(config_path).to_owned();
    let id = devcontainer_id(&folder, config_path);
    let kind = config.kind()?;
    let compose = matches!(kind, Kind::Compose { .. });
    let default_folder = if compose {
        "/".to_owned()
    } else {
        format!("/workspaces/{}", basename(&folder))
    };
    let early = Variables {
        host,
        local_folder: &folder,
        container_folder: &default_folder,
        id: &id,
        container_env: None,
    };
    let workspace_folder = config
        .workspace_folder
        .as_deref()
        .map_or_else(|| default_folder.clone(), |value| early.substitute(value));
    let vars = Variables {
        container_folder: &workspace_folder,
        ..early
    };
    if !config.features.is_empty() {
        progress(&format!(
            "Note: Dev Container Features are not installed by this editor: {}",
            config
                .features
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    if let Some(initialize) = &config.initialize_command {
        let shell: &[&str] = if host.windows() {
            &["cmd.exe", "/C"]
        } else {
            &["/bin/sh", "-c"]
        };
        for (label, argv) in initialize.commands_with(shell) {
            if label.trim().is_empty() {
                continue;
            }
            progress(&format!("Running initializeCommand: {label}"));
            let output = host.run(&argv[0], &argv[1..], None, LONG)?;
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                progress(line);
            }
            if output.code != Some(0) {
                return Err(io::Error::other(format!(
                    "initializeCommand failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
        }
    }

    let labels = vec![
        format!("devcontainer.local_folder={folder}"),
        format!("devcontainer.config_file={config_path}"),
    ];
    let (container, created, started, shutdown) = match kind {
        Kind::Compose { files, service } => {
            let files: Vec<String> = files
                .iter()
                .map(|file| resolve(host, &config_dir, &vars.substitute(file)))
                .collect();
            let project = compose_project(host, &folder, &files[0])?;
            let compose_labels = [
                format!("com.docker.compose.project={project}"),
                format!("com.docker.compose.service={service}"),
            ];
            let mut base = args(["compose", "-p"]);
            base.push(project.clone());
            for file in &files {
                base.extend(["-f".into(), file.clone()]);
            }
            let found = find_containers(host, &compose_labels)?;
            // Each docker call costs a few hundred milliseconds (on Windows
            // especially): a container found is used as listed.
            let reused = match found.first() {
                Some(container) if container.running => Some((container.clone(), false)),
                Some(container) => {
                    progress(&format!("Starting compose project {project}"));
                    let mut start = base.clone();
                    start.push("start".into());
                    match docker(host, &start, LONG, "docker compose start") {
                        Ok(_) => Some((container.clone(), true)),
                        // E.g. a mount from a former VS Code session is gone.
                        Err(err) => {
                            progress(&format!("{}; recreating it", first_line(&err)));
                            None
                        }
                    }
                }
                None => None,
            };
            let (container, created, started) = match reused {
                Some((container, started)) => (container, false, started),
                None => {
                    let override_path = join(
                        host,
                        &host.temp_dir(),
                        &format!("next_code-devcontainer-{id}.yml"),
                    );
                    let mut text = format!("services:\n  {}:\n    labels:\n", yaml(service));
                    for label in &labels {
                        text.push_str(&format!("      - {}\n", yaml(label)));
                    }
                    if !config.container_env.is_empty() {
                        text.push_str("    environment:\n");
                        for (key, value) in &config.container_env {
                            text.push_str(&format!(
                                "      {}: {}\n",
                                yaml(key),
                                yaml(&vars.substitute(value))
                            ));
                        }
                    }
                    if config.override_command == Some(true) {
                        text.push_str(
                            "    entrypoint: [\"/bin/sh\", \"-c\", \"echo Container started; trap 'exit 0' TERM; while sleep 1 & wait $$!; do :; done\"]\n",
                        );
                    }
                    if !config.mounts.is_empty() {
                        progress(
                            "Note: `mounts` are not applied to compose dev containers; use the compose file's volumes",
                        );
                    }
                    host.write_file(&override_path, text.as_bytes())?;
                    progress(&format!(
                        "Starting compose project {project} (service {service}); building images if needed"
                    ));
                    let mut up = base.clone();
                    up.extend(["-f".into(), override_path, "up".into(), "-d".into()]);
                    // An existing container that did not start is replaced.
                    if !found.is_empty() {
                        up.push("--force-recreate".into());
                    }
                    up.push(service.to_owned());
                    up.extend(config.run_services.iter().cloned());
                    let output = docker(host, &up, LONG, "docker compose up")?;
                    for line in output.lines() {
                        progress(line);
                    }
                    let container = find_containers(host, &compose_labels)?
                        .into_iter()
                        .find(|container| container.running)
                        .ok_or_else(|| {
                            io::Error::other(format!(
                                "service {service} of compose project {project} is not running"
                            ))
                        })?;
                    (container, true, true)
                }
            };
            let shutdown = match config.shutdown_action {
                Some(ShutdownAction::None) => Shutdown::None,
                Some(ShutdownAction::StopContainer) => Shutdown::Container(container.id.clone()),
                Some(ShutdownAction::StopCompose) | None => Shutdown::Compose { project, files },
            };
            (container, created, started, shutdown)
        }
        Kind::Image(_) | Kind::Dockerfile { .. } => {
            let found = find_containers(host, &labels)?;
            let reused = match found.first() {
                Some(container) if container.running => Some((container.clone(), false, false)),
                Some(container) => {
                    progress("Starting the dev container");
                    match docker(
                        host,
                        &["start".into(), container.id.clone()],
                        SHORT,
                        "docker start",
                    ) {
                        Ok(_) => Some((container.clone(), false, true)),
                        Err(err) => {
                            progress(&format!("{}; recreating it", first_line(&err)));
                            docker(
                                host,
                                &["rm".into(), "-f".into(), container.id.clone()],
                                SHORT,
                                "docker rm",
                            )?;
                            None
                        }
                    }
                }
                None => None,
            };
            let (container, created, started) = match reused {
                Some(state) => state,
                None => {
                    let image = match kind {
                        Kind::Image(image) => vars.substitute(image),
                        Kind::Dockerfile {
                            dockerfile,
                            context,
                        } => {
                            let tag = format!("next_code-devcontainer-{id}");
                            let mut build = args(["build", "-f"]);
                            build.push(resolve(host, &config_dir, &vars.substitute(dockerfile)));
                            build.extend(["-t".into(), tag.clone()]);
                            if let Some(spec) = &config.build {
                                for (key, value) in &spec.args {
                                    build.extend([
                                        "--build-arg".into(),
                                        format!("{key}={}", vars.substitute(value)),
                                    ]);
                                }
                                if let Some(target) = &spec.target {
                                    build.extend(["--target".into(), target.clone()]);
                                }
                            }
                            build.push(resolve(host, &config_dir, &vars.substitute(context)));
                            progress("Building the dev container image");
                            let output = docker(host, &build, LONG, "docker build")?;
                            for line in output
                                .lines()
                                .rev()
                                .take(3)
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                            {
                                progress(line);
                            }
                            tag
                        }
                        Kind::Compose { .. } => unreachable!("handled above"),
                    };
                    let mut run = args(["run", "-d"]);
                    for label in &labels {
                        run.extend(["--label".into(), label.clone()]);
                    }
                    let mount = config.workspace_mount.as_deref().map_or_else(
                        || format!("type=bind,source={folder},target={workspace_folder}"),
                        |mount| vars.substitute(mount),
                    );
                    run.extend(["--mount".into(), mount]);
                    for mount in &config.mounts {
                        run.extend(["--mount".into(), vars.substitute(&mount.to_arg())]);
                    }
                    for (key, value) in &config.container_env {
                        run.extend(["-e".into(), format!("{key}={}", vars.substitute(value))]);
                    }
                    if let Some(user) = &config.container_user {
                        run.extend(["-u".into(), user.clone()]);
                    }
                    if config.init == Some(true) {
                        run.push("--init".into());
                    }
                    if config.privileged == Some(true) {
                        run.push("--privileged".into());
                    }
                    for cap in &config.cap_add {
                        run.extend(["--cap-add".into(), cap.clone()]);
                    }
                    for opt in &config.security_opt {
                        run.extend(["--security-opt".into(), opt.clone()]);
                    }
                    run.extend(config.run_args.iter().map(|arg| vars.substitute(arg)));
                    let keep_alive = config.override_command != Some(false);
                    if keep_alive {
                        run.extend(["--entrypoint".into(), "/bin/sh".into()]);
                    }
                    run.push(image);
                    if keep_alive {
                        run.extend([
                            "-c".into(),
                            "echo Container started; trap 'exit 0' TERM; while sleep 1 & wait $!; do :; done"
                                .into(),
                        ]);
                    }
                    progress("Creating the dev container");
                    let id = docker(host, &run, LONG, "docker run")?.trim().to_owned();
                    // Its name is read below if needed.
                    let container = Found {
                        id,
                        running: true,
                        name: String::new(),
                    };
                    (container, true, true)
                }
            };
            let shutdown = match config.shutdown_action {
                Some(ShutdownAction::None) => Shutdown::None,
                _ => Shutdown::Container(container.id.clone()),
            };
            (container, created, started, shutdown)
        }
    };

    let Found {
        id: container,
        name: listed_name,
        ..
    } = container;
    let configured_user = config
        .remote_user
        .clone()
        .or_else(|| config.container_user.clone());
    let known_name = config
        .name
        .clone()
        .or_else(|| (!listed_name.is_empty()).then_some(listed_name));
    // `docker inspect` only for what neither the configuration nor
    // `docker ps` said: the image's user, or the name of a new container.
    let (name, user) = match (known_name, configured_user) {
        (Some(name), Some(user)) => (name, Some(user)),
        (known_name, configured_user) => {
            let inspect = docker(
                host,
                &args(["inspect", "-f", "{{.Name}}\t{{.Config.User}}", &container]),
                SHORT,
                "docker inspect",
            )?;
            let (container_name, image_user) = inspect
                .trim()
                .split_once('\t')
                .unwrap_or((inspect.trim(), ""));
            let name =
                known_name.unwrap_or_else(|| container_name.trim_start_matches('/').to_owned());
            let user =
                configured_user.or_else(|| (!image_user.is_empty()).then(|| image_user.to_owned()));
            (name, user)
        }
    };

    // `${containerEnv:…}` needs the container's environment, as that user.
    let mut env_args = args(["exec"]);
    if let Some(user) = &user {
        env_args.extend(["-u".into(), user.clone()]);
    }
    env_args.extend([container.clone(), "env".into()]);
    let container_env: BTreeMap<String, String> = if config
        .remote_env
        .values()
        .flatten()
        .any(|value| value.contains("${containerEnv:"))
    {
        docker(host, &env_args, SHORT, "Reading the container environment")?
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect()
    } else {
        BTreeMap::new()
    };
    let vars = Variables {
        container_env: Some(&container_env),
        ..vars
    };
    let remote_env = config
        .remote_env
        .iter()
        .filter_map(|(key, value)| Some((key.clone(), vars.substitute(value.as_deref()?))))
        .collect();

    let started_container = Started {
        id: container,
        name,
        user,
        workspace_folder,
        remote_env,
        started,
        shutdown,
        ports: config.ports(),
    };
    if created {
        for (what, command) in [
            ("onCreateCommand", &config.on_create_command),
            ("updateContentCommand", &config.update_content_command),
            ("postCreateCommand", &config.post_create_command),
        ] {
            run_in_container(host, &started_container, what, command.as_ref(), progress)?;
        }
    }
    if started {
        run_in_container(
            host,
            &started_container,
            "postStartCommand",
            config.post_start_command.as_ref(),
            progress,
        )?;
    }
    run_in_container(
        host,
        &started_container,
        "postAttachCommand",
        config.post_attach_command.as_ref(),
        progress,
    )?;
    Ok(started_container)
}

/// The `docker` arguments stopping the container (or compose project) as
/// `shutdownAction` says; `None` leaves it running.
pub fn stop_args(shutdown: &Shutdown) -> Option<Vec<String>> {
    match shutdown {
        Shutdown::None => None,
        Shutdown::Container(id) => Some(vec!["stop".into(), id.clone()]),
        Shutdown::Compose { project, files } => {
            let mut command = args(["compose", "-p"]);
            command.push(project.clone());
            for file in files {
                command.extend(["-f".into(), file.clone()]);
            }
            command.push("stop".into());
            Some(command)
        }
    }
}

/// Stops the container (or compose project) as `shutdownAction` says.
pub fn stop(host: &dyn Host, shutdown: &Shutdown) -> io::Result<()> {
    match stop_args(shutdown) {
        Some(command) => docker(host, &command, SHORT, "Stopping the dev container").map(drop),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Posix;

    impl Host for Posix {
        fn run(&self, _: &str, _: &[String], _: Option<&[u8]>, _: Duration) -> io::Result<Output> {
            unreachable!()
        }
        fn read_file(&self, _: &str) -> io::Result<Option<Vec<u8>>> {
            Ok(None)
        }
        fn write_file(&self, _: &str, _: &[u8]) -> io::Result<()> {
            Ok(())
        }
        fn list_dirs(&self, _: &str) -> io::Result<Vec<String>> {
            Ok(Vec::new())
        }
        fn temp_dir(&self) -> String {
            "/tmp".into()
        }
        fn env_var(&self, name: &str) -> Option<String> {
            (name == "HOME").then(|| "/home/me".into())
        }
        fn windows(&self) -> bool {
            false
        }
    }

    struct Windows;

    impl Host for Windows {
        fn run(&self, _: &str, _: &[String], _: Option<&[u8]>, _: Duration) -> io::Result<Output> {
            unreachable!()
        }
        fn read_file(&self, _: &str) -> io::Result<Option<Vec<u8>>> {
            Ok(None)
        }
        fn write_file(&self, _: &str, _: &[u8]) -> io::Result<()> {
            Ok(())
        }
        fn list_dirs(&self, _: &str) -> io::Result<Vec<String>> {
            Ok(Vec::new())
        }
        fn temp_dir(&self) -> String {
            r"C:\Temp".into()
        }
        fn env_var(&self, _: &str) -> Option<String> {
            None
        }
        fn windows(&self) -> bool {
            true
        }
    }

    #[test]
    fn variables_are_substituted_and_unknown_ones_kept() {
        let env = BTreeMap::from([("PATH".to_owned(), "/usr/bin".to_owned())]);
        let vars = Variables {
            host: &Posix,
            local_folder: "/home/me/proj",
            container_folder: "/workspaces/proj",
            id: "abc",
            container_env: Some(&env),
        };
        assert_eq!(
            vars.substitute(
                "${localWorkspaceFolderBasename}:${containerWorkspaceFolder}:${localEnv:HOME}:\
                 ${localEnv:NOPE:dflt}:${containerEnv:PATH}:${devcontainerId}:${other}"
            ),
            "proj:/workspaces/proj:/home/me:dflt:/usr/bin:abc:${other}"
        );
    }

    #[test]
    fn config_paths_name_their_folder_and_relative_paths_resolve() {
        assert_eq!(folder_of("/p/.devcontainer/devcontainer.json"), "/p");
        assert_eq!(folder_of("/p/.devcontainer/py/devcontainer.json"), "/p");
        assert_eq!(folder_of("/p/.devcontainer.json"), "/p");
        assert_eq!(
            folder_of(r"C:\repos\MiX.Utils\.devcontainer\devcontainer.json"),
            r"C:\repos\MiX.Utils"
        );
        assert_eq!(
            resolve(&Posix, "/p/.devcontainer", "./docker-compose.yml"),
            "/p/.devcontainer/docker-compose.yml"
        );
        assert_eq!(resolve(&Posix, "/p/.devcontainer", ".."), "/p");
        assert_eq!(
            resolve(&Windows, r"C:\p\.devcontainer", "../Dockerfile"),
            r"C:\p\Dockerfile"
        );
        assert_eq!(resolve(&Windows, r"C:\p", r"D:\x"), r"D:\x");
    }

    #[test]
    fn compose_projects_are_named_like_vs_code_names_them() {
        assert_eq!(
            compose_project(
                &Windows,
                r"C:\repos\MiX.Utils",
                r"C:\repos\MiX.Utils\.devcontainer\docker-compose.yml"
            )
            .unwrap(),
            "mixutils_devcontainer"
        );
        assert_eq!(
            compose_project(&Posix, "/p", "/p/compose/docker-compose.yml").unwrap(),
            "compose"
        );
    }

    #[test]
    fn listed_containers_carry_their_state_and_first_name() {
        let found = parse_found("abc\trunning\tproj-dev-1\ndef\texited\tone,two\n\nghi\tcreated\n");
        let summary: Vec<_> = found
            .iter()
            .map(|found| (found.id.as_str(), found.running, found.name.as_str()))
            .collect();
        assert_eq!(
            summary,
            [
                ("abc", true, "proj-dev-1"),
                ("def", false, "one"),
                ("ghi", false, "")
            ]
        );
    }
}
