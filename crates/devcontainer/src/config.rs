//! `devcontainer.json`: the parts of the spec this editor implements.
//! Unknown properties (editor customizations, …) are ignored.

use std::{collections::BTreeMap, io};

use serde::Deserialize;
use serde_json::Value;

use crate::{
    jsonc,
    ports::{self, ForwardPort, PortAttributes, PortPattern, PortsConfig},
};

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub name: Option<String>,
    pub image: Option<String>,
    pub build: Option<Build>,
    /// Legacy spelling of `build.dockerfile`.
    pub docker_file: Option<String>,
    /// Legacy spelling of `build.context`.
    pub context: Option<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub docker_compose_file: Vec<String>,
    pub service: Option<String>,
    #[serde(default)]
    pub run_services: Vec<String>,
    pub workspace_folder: Option<String>,
    pub workspace_mount: Option<String>,
    #[serde(default)]
    pub mounts: Vec<Mount>,
    #[serde(default)]
    pub container_env: BTreeMap<String, String>,
    #[serde(default)]
    pub remote_env: BTreeMap<String, Option<String>>,
    pub remote_user: Option<String>,
    pub container_user: Option<String>,
    #[serde(default)]
    pub run_args: Vec<String>,
    pub override_command: Option<bool>,
    pub shutdown_action: Option<ShutdownAction>,
    pub init: Option<bool>,
    pub privileged: Option<bool>,
    #[serde(default)]
    pub cap_add: Vec<String>,
    #[serde(default)]
    pub security_opt: Vec<String>,
    pub initialize_command: Option<Lifecycle>,
    pub on_create_command: Option<Lifecycle>,
    pub update_content_command: Option<Lifecycle>,
    pub post_create_command: Option<Lifecycle>,
    pub post_start_command: Option<Lifecycle>,
    pub post_attach_command: Option<Lifecycle>,
    /// Dev Container Features are not installed; their names are reported.
    #[serde(default)]
    pub features: BTreeMap<String, Value>,
    /// Container ports forwarded when the editor connects.
    #[serde(default, deserialize_with = "ports::forward_ports")]
    pub forward_ports: Vec<ForwardPort>,
    #[serde(default, deserialize_with = "ports::ports_attributes")]
    pub ports_attributes: Vec<(PortPattern, PortAttributes)>,
    pub other_ports_attributes: Option<PortAttributes>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Build {
    pub dockerfile: Option<String>,
    pub context: Option<String>,
    #[serde(default)]
    pub args: BTreeMap<String, String>,
    pub target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ShutdownAction {
    None,
    StopContainer,
    StopCompose,
}

/// A mount, as a `--mount` string or its fields.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Mount {
    Text(String),
    Fields {
        source: Option<String>,
        target: String,
        #[serde(rename = "type")]
        kind: Option<String>,
    },
}

impl Mount {
    /// The `--mount` argument.
    pub fn to_arg(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Fields {
                source,
                target,
                kind,
            } => {
                let mut arg = format!("type={}", kind.as_deref().unwrap_or("bind"));
                if let Some(source) = source {
                    arg.push_str(&format!(",source={source}"));
                }
                arg.push_str(&format!(",target={target}"));
                arg
            }
        }
    }
}

/// A lifecycle command: a shell line, an argv, or named commands that run
/// in parallel.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Lifecycle {
    Shell(String),
    Exec(Vec<String>),
    Parallel(BTreeMap<String, Lifecycle>),
}

impl Lifecycle {
    /// Each command to run as `(label, argv)`; shell lines become
    /// `/bin/sh -c <line>` (`cmd /C` for `initializeCommand` on Windows is
    /// the caller's business: see [`Lifecycle::commands_with`]).
    pub fn commands(&self) -> Vec<(String, Vec<String>)> {
        self.commands_with(&["/bin/sh", "-c"])
    }

    /// Like [`Lifecycle::commands`], running shell lines with `shell`.
    pub fn commands_with(&self, shell: &[&str]) -> Vec<(String, Vec<String>)> {
        match self {
            Self::Shell(line) => {
                let mut argv: Vec<String> = shell.iter().map(|&arg| arg.to_owned()).collect();
                argv.push(line.clone());
                vec![(line.clone(), argv)]
            }
            Self::Exec(argv) => vec![(argv.join(" "), argv.clone())],
            Self::Parallel(named) => named
                .iter()
                .flat_map(|(name, command)| {
                    command
                        .commands_with(shell)
                        .into_iter()
                        .map(move |(label, argv)| (format!("{name}: {label}"), argv))
                })
                .collect(),
        }
    }
}

fn one_or_many<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(one) => vec![one],
        OneOrMany::Many(many) => many,
    })
}

/// How the container is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind<'a> {
    Image(&'a str),
    Dockerfile {
        dockerfile: &'a str,
        context: &'a str,
    },
    Compose {
        files: &'a [String],
        service: &'a str,
    },
}

impl Config {
    /// `forwardPorts`, `portsAttributes` and `otherPortsAttributes`.
    pub fn ports(&self) -> PortsConfig {
        PortsConfig {
            forward: self.forward_ports.clone(),
            attributes: self.ports_attributes.clone(),
            other: self.other_ports_attributes.clone(),
        }
    }

    pub fn parse(text: &str) -> io::Result<Self> {
        serde_json::from_str(&jsonc::to_json(text)).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("devcontainer.json: {err}"),
            )
        })
    }

    /// How the container is made; paths are relative to the
    /// `devcontainer.json` directory.
    pub fn kind(&self) -> io::Result<Kind<'_>> {
        if !self.docker_compose_file.is_empty() {
            let service = self.service.as_deref().ok_or_else(|| {
                invalid("`dockerComposeFile` needs `service` (the container to work in)")
            })?;
            return Ok(Kind::Compose {
                files: &self.docker_compose_file,
                service,
            });
        }
        let dockerfile = self
            .build
            .as_ref()
            .and_then(|build| build.dockerfile.as_deref())
            .or(self.docker_file.as_deref());
        if let Some(dockerfile) = dockerfile {
            let context = self
                .build
                .as_ref()
                .and_then(|build| build.context.as_deref())
                .or(self.context.as_deref())
                .unwrap_or(".");
            return Ok(Kind::Dockerfile {
                dockerfile,
                context,
            });
        }
        match &self.image {
            Some(image) => Ok(Kind::Image(image)),
            None => Err(invalid(
                "devcontainer.json needs `image`, `build.dockerfile` or `dockerComposeFile`",
            )),
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_configs_like_vs_code_writes_them_are_read() {
        let config = Config::parse(
            r#"{
            "name": "mix-utils-dev",
            "dockerComposeFile": "./docker-compose.yml",
            "service": "dev_container",
            "shutdownAction": "stopCompose",
            "customizations": { "vscode": { "extensions": ["ms-python.python"] } },
            "remoteUser": "developer",
            "workspaceFolder": "/home/developer/git/MiX.Utils",
            "postCreateCommand": "./run.sh container-setup",
            "postAttachCommand": "",
            "remoteEnv": { "PATH": "${containerEnv:PATH}:/x", "UNSET": null },
            "mounts": ["source=a,target=/a,type=bind", { "source": "v", "target": "/v", "type": "volume" }],
            "features": { "ghcr.io/devcontainers/features/node:1": {} },
        }"#,
        )
        .unwrap();
        assert_eq!(
            config.kind().unwrap(),
            Kind::Compose {
                files: &["./docker-compose.yml".to_owned()],
                service: "dev_container"
            }
        );
        assert_eq!(config.shutdown_action, Some(ShutdownAction::StopCompose));
        assert_eq!(
            config.post_create_command.unwrap().commands(),
            [(
                "./run.sh container-setup".to_owned(),
                vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "./run.sh container-setup".into()
                ]
            )]
        );
        assert_eq!(config.remote_env["UNSET"], None);
        assert_eq!(
            config.mounts.iter().map(Mount::to_arg).collect::<Vec<_>>(),
            [
                "source=a,target=/a,type=bind",
                "type=volume,source=v,target=/v"
            ]
        );
        assert_eq!(config.features.len(), 1);
    }

    #[test]
    fn build_and_image_configs_pick_their_kind() {
        let build =
            Config::parse(r#"{ "build": { "dockerfile": "Dockerfile", "context": ".." } }"#)
                .unwrap();
        assert_eq!(
            build.kind().unwrap(),
            Kind::Dockerfile {
                dockerfile: "Dockerfile",
                context: ".."
            }
        );
        let legacy = Config::parse(r#"{ "dockerFile": "Dockerfile" }"#).unwrap();
        assert_eq!(
            legacy.kind().unwrap(),
            Kind::Dockerfile {
                dockerfile: "Dockerfile",
                context: "."
            }
        );
        let image = Config::parse(r#"{ "image": "python:3.12" }"#).unwrap();
        assert_eq!(image.kind().unwrap(), Kind::Image("python:3.12"));
        assert!(Config::parse("{}").unwrap().kind().is_err());
        assert!(
            Config::parse(r#"{ "dockerComposeFile": ["a.yml", "b.yml"] }"#)
                .unwrap()
                .kind()
                .is_err(),
            "compose needs a service"
        );
    }

    #[test]
    fn parallel_lifecycle_commands_are_labelled() {
        let config = Config::parse(
            r#"{ "image": "x", "postStartCommand": { "a": "echo a", "b": ["echo", "b"] } }"#,
        )
        .unwrap();
        let commands = config.post_start_command.unwrap().commands();
        assert_eq!(commands[0].0, "a: echo a");
        assert_eq!(
            commands[1],
            ("b: echo b".into(), vec!["echo".into(), "b".into()])
        );
    }
}
