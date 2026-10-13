//! Starting `next_code_agent` inside a running container through
//! `docker exec -i`, with the docker CLI of the machine where the project
//! folder lives (Windows, or a WSL distro).

use std::{io, process::Command};

use crate::{
    launch::{self, Connection, Target, no_window},
    wsl,
};

/// Where the `docker` CLI runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerHost {
    /// `docker` on this machine's PATH.
    Local,
    /// `docker` inside a WSL distro.
    Wsl(String),
}

impl DockerHost {
    /// A `docker` command on this host, without a console window.
    pub fn docker(&self) -> Command {
        match self {
            Self::Local => {
                let mut command = Command::new("docker");
                no_window(&mut command);
                command
            }
            Self::Wsl(distro) => wsl::wsl(["-d", distro, "--exec", "docker"]),
        }
    }
}

/// A running container as an agent target: the agent and everything it
/// starts run as `user`, with `env` set and in `workdir`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    pub host: DockerHost,
    /// Container id or name.
    pub id: String,
    /// Shown in messages.
    pub name: String,
    pub user: Option<String>,
    pub env: Vec<(String, String)>,
    pub workdir: Option<String>,
}

impl Container {
    /// `docker exec -i [-u …] [-w …] [-e …] <id>` and then `args`.
    pub fn exec(&self, args: &[&str]) -> Command {
        let mut command = self.host.docker();
        command.args(["exec", "-i"]);
        if let Some(user) = &self.user {
            command.args(["-u", user]);
        }
        if let Some(workdir) = &self.workdir {
            command.args(["-w", workdir]);
        }
        for (key, value) in &self.env {
            command.arg("-e").arg(format!("{key}={value}"));
        }
        command.arg(&self.id).args(args);
        command
    }
}

impl Target for Container {
    fn describe(&self) -> String {
        format!("dev container '{}'", self.name)
    }

    fn shell(&self, login: bool, script: &str, args: &[&str]) -> Command {
        // Login profiles reset variables such as PATH: set `env` again after
        // them, so the agent and what it starts see the configured values.
        let script = if login && !self.env.is_empty() {
            let exports: String = self
                .env
                .iter()
                .map(|(key, value)| format!("export {key}='{}'\n", value.replace('\'', "'\\''")))
                .collect();
            format!("{exports}{script}")
        } else {
            script.to_owned()
        };
        let mut all = vec!["/bin/sh", if login { "-lc" } else { "-c" }, &script, "sh"];
        all.extend_from_slice(args);
        self.exec(&all)
    }

    fn spawn_error(&self, err: io::Error) -> io::Error {
        match &self.host {
            DockerHost::Local => io::Error::new(
                err.kind(),
                format!("Docker is not available (docker: {err}). Install Docker Desktop."),
            ),
            DockerHost::Wsl(_) => wsl::not_available(err),
        }
    }
}

/// Installs (if needed) and starts the agent in `container`, then opens
/// `root` there.
pub fn connect(container: &Container, root: &str) -> io::Result<Connection> {
    launch::connect(container, root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_commands_carry_user_workdir_and_env() {
        let container = Container {
            host: DockerHost::Local,
            id: "abc".into(),
            name: "dev".into(),
            user: Some("developer".into()),
            env: vec![("A".into(), "1 2".into())],
            workdir: Some("/w".into()),
        };
        let command = container.shell(true, "echo hi", &["x"]);
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "exec",
                "-i",
                "-u",
                "developer",
                "-w",
                "/w",
                "-e",
                "A=1 2",
                "abc",
                "/bin/sh",
                "-lc",
                "export A='1 2'\necho hi",
                "sh",
                "x"
            ]
        );
    }
}
