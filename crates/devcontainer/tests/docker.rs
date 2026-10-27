//! Real dev containers through the local `docker` CLI. Needs Docker:
//!
//! ```sh
//! cargo test -p devcontainer --test docker -- --ignored
//! ```

use std::{
    fs, io,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

use devcontainer::{Host, Output, Shutdown};

/// The local machine, with the project folder as working directory.
struct Local(PathBuf);

impl Host for Local {
    fn run(
        &self,
        program: &str,
        args: &[String],
        input: Option<&[u8]>,
        _: Duration,
    ) -> io::Result<Output> {
        assert!(input.is_none());
        let output = Command::new(program)
            .args(args)
            .current_dir(&self.0)
            .stdin(Stdio::null())
            .output()?;
        Ok(Output {
            code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
    fn read_file(&self, path: &str) -> io::Result<Option<Vec<u8>>> {
        match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }
    fn write_file(&self, path: &str, data: &[u8]) -> io::Result<()> {
        fs::write(path, data)
    }
    fn list_dirs(&self, path: &str) -> io::Result<Vec<String>> {
        Ok(fs::read_dir(path)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.path().is_dir())
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default())
    }
    fn temp_dir(&self) -> String {
        std::env::temp_dir().to_string_lossy().into_owned()
    }
    fn env_var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
    fn windows(&self) -> bool {
        cfg!(windows)
    }
}

/// A project folder with `files`, removed (with its containers) on drop.
struct Project {
    root: PathBuf,
    cleanup: Vec<Vec<String>>,
}

impl Project {
    fn new(name: &str, files: &[(&str, &str)]) -> Self {
        let root = std::env::temp_dir().join(format!("next_code_dc_{name}_{}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        for (path, text) in files {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        Self {
            root,
            cleanup: Vec::new(),
        }
    }

    fn host(&self) -> Local {
        Local(self.root.clone())
    }

    fn config(&self) -> String {
        devcontainer::find_configs(&self.host(), self.root.to_str().unwrap())
            .unwrap()
            .remove(0)
    }

    fn exec(&self, id: &str, script: &str) -> String {
        let output = Command::new("docker")
            .args(["exec", id, "/bin/sh", "-c", script])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        for command in &self.cleanup {
            Command::new("docker").args(command).output().ok();
        }
        fs::remove_dir_all(&self.root).ok();
    }
}

#[test]
#[ignore = "needs Docker"]
fn image_containers_are_created_reused_restarted_and_stopped() {
    let mut project = Project::new(
        "image",
        &[(
            ".devcontainer/devcontainer.json",
            r#"{
                // Comments are allowed.
                "name": "alpine-dev",
                "image": "alpine:3.20",
                "containerEnv": { "FROM_CONFIG": "${localWorkspaceFolderBasename}" },
                "remoteEnv": { "PATH": "${containerEnv:PATH}:/extra" },
                "postCreateCommand": "echo created >> /tmp/created",
                "postStartCommand": ["/bin/sh", "-c", "echo started >> /tmp/started"],
            }"#,
        )],
    );
    let host = project.host();
    let config = project.config();
    let mut log = Vec::new();
    let started = devcontainer::up(&host, &config, &mut |line| log.push(line.to_owned())).unwrap();
    project
        .cleanup
        .push(vec!["rm".into(), "-f".into(), started.id.clone()]);
    assert!(started.started);
    assert_eq!(started.name, "alpine-dev");
    let base = project
        .root
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(started.workspace_folder, format!("/workspaces/{base}"));
    let path = &started.remote_env[0];
    assert_eq!(path.0, "PATH");
    assert!(
        path.1.ends_with(":/extra") && path.1.contains("/usr/bin"),
        "{path:?}"
    );
    assert_eq!(project.exec(&started.id, "echo $FROM_CONFIG"), base);
    assert_eq!(project.exec(&started.id, "cat /tmp/created"), "created");
    // The folder is mounted.
    fs::write(project.root.join("hello.txt"), "hi").unwrap();
    assert_eq!(
        project.exec(&started.id, &format!("cat /workspaces/{base}/hello.txt")),
        "hi"
    );

    // Running: reused as is.
    let again = devcontainer::up(&host, &config, &mut |_| {}).unwrap();
    assert_eq!(again.id, started.id);
    assert!(!again.started);

    // Stopped: started again, postCreate not repeated, postStart is.
    devcontainer::stop(&host, &started.shutdown).unwrap();
    let restarted = devcontainer::up(&host, &config, &mut |_| {}).unwrap();
    assert_eq!(restarted.id, started.id);
    assert!(restarted.started);
    assert_eq!(project.exec(&started.id, "cat /tmp/created"), "created");
    assert_eq!(
        project.exec(&started.id, "cat /tmp/started"),
        "started\nstarted"
    );
}

#[test]
#[ignore = "needs Docker"]
fn dockerfile_and_compose_containers_come_up() {
    let mut project = Project::new(
        "compose",
        &[
            (
                ".devcontainer/devcontainer.json",
                r#"{
                    "dockerComposeFile": "./docker-compose.yml",
                    "service": "dev",
                    "remoteUser": "developer",
                    "workspaceFolder": "/work",
                    "postCreateCommand": "id -un > /tmp/who"
                }"#,
            ),
            (
                ".devcontainer/docker-compose.yml",
                "services:\n  dev:\n    build:\n      context: ..\n      dockerfile: ./.devcontainer/Dockerfile\n    volumes:\n      - ..:/work\n    command: sleep infinity\n",
            ),
            (
                ".devcontainer/Dockerfile",
                "FROM alpine:3.20\nRUN adduser -D developer\n",
            ),
        ],
    );
    let host = project.host();
    let config = project.config();
    let started = devcontainer::up(&host, &config, &mut |_| {}).unwrap();
    let Shutdown::Compose { project: name, .. } = &started.shutdown else {
        panic!("compose projects stop as a whole: {:?}", started.shutdown);
    };
    project.cleanup.push(vec![
        "compose".into(),
        "-p".into(),
        name.clone(),
        "-f".into(),
        project
            .root
            .join(".devcontainer/docker-compose.yml")
            .to_string_lossy()
            .into_owned(),
        "down".into(),
        "--rmi".into(),
        "local".into(),
    ]);
    assert_eq!(started.user.as_deref(), Some("developer"));
    assert_eq!(started.workspace_folder, "/work");
    assert_eq!(project.exec(&started.id, "cat /tmp/who"), "developer");
    let again = devcontainer::up(&host, &config, &mut |_| {}).unwrap();
    assert_eq!(again.id, started.id);
    devcontainer::stop(&host, &started.shutdown).unwrap();
}

#[test]
#[ignore = "needs Docker"]
fn a_stopped_container_that_no_longer_starts_is_recreated() {
    let mut project = Project::new(
        "recreate",
        &[
            (
                ".devcontainer/devcontainer.json",
                r#"{ "image": "alpine:3.20",
                     "mounts": ["type=bind,source=${localWorkspaceFolder}/data,target=/data"] }"#,
            ),
            ("data/x.txt", "x"),
        ],
    );
    let host = project.host();
    let config = project.config();
    let first = devcontainer::up(&host, &config, &mut |_| {}).unwrap();
    project
        .cleanup
        .push(vec!["rm".into(), "-f".into(), first.id.clone()]);
    devcontainer::stop(&host, &first.shutdown).unwrap();
    // The bind mount's source disappears (as a former session's socket
    // does): the stopped container cannot start anymore.
    fs::remove_dir_all(project.root.join("data")).unwrap();
    fs::write(
        project.root.join(".devcontainer/devcontainer.json"),
        r#"{ "image": "alpine:3.20" }"#,
    )
    .unwrap();
    let mut log = Vec::new();
    let second = devcontainer::up(&host, &config, &mut |line| log.push(line.to_owned())).unwrap();
    project
        .cleanup
        .push(vec!["rm".into(), "-f".into(), second.id.clone()]);
    assert_ne!(second.id, first.id);
    assert!(second.started);
    assert!(
        log.iter().any(|line| line.contains("recreating it")),
        "{log:?}"
    );
}
