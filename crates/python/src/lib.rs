//! Python projects managed with uv: `pyproject.toml` and `uv.lock`, the
//! `uv` command lines the editor runs, and pytest node ids for running
//! tests.

mod pytest;
pub mod uv;

use std::{collections::BTreeMap, io};

use serde::Deserialize;

pub use pytest::test_at;

/// A PEP 508 requirement, split into the parts the editor shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    /// As written in `pyproject.toml`.
    pub raw: String,
    /// As written (`Flask`), see [`normalize`] for comparisons.
    pub name: String,
    pub extras: Vec<String>,
    /// Version specifiers (`>=2,<3`), or a direct reference (`@ https://…`).
    pub specifier: String,
    pub marker: Option<String>,
}

impl Requirement {
    pub fn parse(raw: &str) -> Self {
        let text = raw.trim();
        let (body, marker) = match text.split_once(';') {
            Some((body, marker)) => (body.trim(), Some(marker.trim().to_owned())),
            None => (text, None),
        };
        let name_end = body
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
            .unwrap_or(body.len());
        let name = body[..name_end].to_owned();
        let mut rest = body[name_end..].trim_start();
        let mut extras = Vec::new();
        if let Some(inner) = rest.strip_prefix('[')
            && let Some((list, after)) = inner.split_once(']')
        {
            extras = list
                .split(',')
                .map(str::trim)
                .filter(|extra| !extra.is_empty())
                .map(str::to_owned)
                .collect();
            rest = after.trim_start();
        }
        let specifier = rest
            .trim_start_matches('(')
            .trim_end_matches(')')
            .trim()
            .to_owned();
        Self {
            raw: raw.to_owned(),
            name,
            extras,
            specifier,
            marker,
        }
    }
}

/// PEP 503 name normalization: lowercase, runs of `-_.` become `-`.
pub fn normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut separator = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            separator = true;
        } else {
            if separator && !out.is_empty() {
                out.push('-');
            }
            separator = false;
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

/// Where a dependency is declared.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum DependencyKind {
    /// `[project] dependencies`.
    Main,
    /// `[project.optional-dependencies] <extra>`.
    Optional(String),
    /// `[dependency-groups] <group>` (`dev` is uv's `--dev`).
    Group(String),
}

impl DependencyKind {
    pub fn label(&self) -> String {
        match self {
            Self::Main => "dependencies".into(),
            Self::Optional(extra) => format!("extra: {extra}"),
            Self::Group(group) => format!("group: {group}"),
        }
    }
}

/// A declared dependency and the version `uv.lock` resolved for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub requirement: Requirement,
    pub kind: DependencyKind,
    pub locked: Option<String>,
}

/// What the editor reads from `pyproject.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PyProject {
    pub name: Option<String>,
    pub version: Option<String>,
    pub requires_python: Option<String>,
    pub dependencies: Vec<Requirement>,
    pub optional: BTreeMap<String, Vec<Requirement>>,
    pub groups: BTreeMap<String, Vec<Requirement>>,
    /// `[project.scripts]`: command name → `module:function`.
    pub scripts: BTreeMap<String, String>,
    /// `[tool.uv.workspace] members` globs.
    pub workspace_members: Vec<String>,
    /// Has a `[tool.uv]` table.
    pub uv_table: bool,
    /// `[build-system] build-backend`.
    pub build_backend: Option<String>,
}

impl PyProject {
    /// Managed with uv (rather than Poetry, PDM, …): a `[tool.uv]` table or
    /// uv's build backend; a `uv.lock` next to it says so too.
    pub fn uses_uv(&self) -> bool {
        self.uv_table || self.build_backend.as_deref() == Some("uv_build")
    }
}

#[derive(Deserialize)]
struct RawPyProject {
    project: Option<RawProject>,
    #[serde(rename = "dependency-groups", default)]
    dependency_groups: BTreeMap<String, Vec<toml::Value>>,
    tool: Option<RawTool>,
    #[serde(rename = "build-system")]
    build_system: Option<RawBuildSystem>,
}

#[derive(Deserialize)]
struct RawBuildSystem {
    #[serde(rename = "build-backend")]
    build_backend: Option<String>,
}

#[derive(Deserialize)]
struct RawProject {
    name: Option<String>,
    version: Option<String>,
    #[serde(rename = "requires-python")]
    requires_python: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(rename = "optional-dependencies", default)]
    optional_dependencies: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    scripts: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct RawTool {
    uv: Option<RawUv>,
}

#[derive(Deserialize)]
struct RawUv {
    workspace: Option<RawWorkspace>,
}

#[derive(Deserialize)]
struct RawWorkspace {
    #[serde(default)]
    members: Vec<String>,
}

fn invalid(file: &str, err: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{file}: {err}"))
}

impl PyProject {
    pub fn parse(text: &str) -> io::Result<Self> {
        let raw: RawPyProject =
            toml::from_str(text).map_err(|err| invalid("pyproject.toml", err))?;
        let parse_all = |list: &[String]| list.iter().map(|raw| Requirement::parse(raw)).collect();
        let project = raw.project;
        let uv = raw.tool.and_then(|tool| tool.uv);
        Ok(Self {
            name: project.as_ref().and_then(|project| project.name.clone()),
            version: project.as_ref().and_then(|project| project.version.clone()),
            requires_python: project
                .as_ref()
                .and_then(|project| project.requires_python.clone()),
            dependencies: project
                .as_ref()
                .map(|project| parse_all(&project.dependencies))
                .unwrap_or_default(),
            optional: project
                .as_ref()
                .map(|project| {
                    project
                        .optional_dependencies
                        .iter()
                        .map(|(extra, list)| (extra.clone(), parse_all(list)))
                        .collect()
                })
                .unwrap_or_default(),
            // Groups may also include other groups (`{ include-group = … }`).
            groups: raw
                .dependency_groups
                .into_iter()
                .map(|(group, list)| {
                    let requirements = list
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .map(Requirement::parse)
                        .collect();
                    (group, requirements)
                })
                .collect(),
            scripts: project.map(|project| project.scripts).unwrap_or_default(),
            workspace_members: uv
                .as_ref()
                .and_then(|uv| uv.workspace.as_ref())
                .map(|workspace| workspace.members.clone())
                .unwrap_or_default(),
            uv_table: uv.is_some(),
            build_backend: raw.build_system.and_then(|build| build.build_backend),
        })
    }

    /// Every declared dependency with its locked version.
    pub fn dependencies(&self, lock: Option<&Lock>) -> Vec<Dependency> {
        let locked = |requirement: &Requirement| {
            lock.and_then(|lock| lock.version(&requirement.name))
                .map(str::to_owned)
        };
        let main = self.dependencies.iter().map(|r| (r, DependencyKind::Main));
        let optional = self.optional.iter().flat_map(|(extra, list)| {
            list.iter()
                .map(move |r| (r, DependencyKind::Optional(extra.clone())))
        });
        let groups = self.groups.iter().flat_map(|(group, list)| {
            list.iter()
                .map(move |r| (r, DependencyKind::Group(group.clone())))
        });
        main.chain(optional)
            .chain(groups)
            .map(|(requirement, kind)| Dependency {
                locked: locked(requirement),
                requirement: requirement.clone(),
                kind,
            })
            .collect()
    }
}

/// Resolved versions from `uv.lock`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lock {
    /// Normalized name → version (the first one, if several are locked
    /// for different markers).
    packages: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct RawLock {
    #[serde(default)]
    package: Vec<RawPackage>,
}

#[derive(Deserialize)]
struct RawPackage {
    name: String,
    version: Option<String>,
}

impl Lock {
    pub fn parse(text: &str) -> io::Result<Self> {
        let raw: RawLock = toml::from_str(text).map_err(|err| invalid("uv.lock", err))?;
        let mut packages = BTreeMap::new();
        for package in raw.package {
            if let Some(version) = package.version {
                packages.entry(normalize(&package.name)).or_insert(version);
            }
        }
        Ok(Self { packages })
    }

    pub fn version(&self, name: &str) -> Option<&str> {
        self.packages.get(&normalize(name)).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.packages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }
}

/// A Python interpreter uv knows about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interpreter {
    pub version: String,
    pub implementation: String,
    /// `None` for downloadable versions not installed yet.
    pub path: Option<String>,
}

#[derive(Deserialize)]
struct RawInterpreter {
    key: String,
    version: String,
    implementation: String,
    path: Option<String>,
    symlink: Option<String>,
    /// `default`, or e.g. `freethreaded` (pinned differently; not offered).
    variant: Option<String>,
}

/// `uv python list --output-format json`, one entry per interpreter
/// (symlinks to a listed one are dropped).
pub fn parse_python_list(json: &str) -> io::Result<Vec<Interpreter>> {
    let raw: Vec<RawInterpreter> =
        serde_json::from_str(json).map_err(|err| invalid("uv python list", err))?;
    let mut seen = std::collections::HashSet::new();
    Ok(raw
        .into_iter()
        .filter(|interpreter| interpreter.symlink.is_none() || interpreter.path.is_none())
        .filter(|interpreter| {
            interpreter
                .variant
                .as_deref()
                .is_none_or(|v| v == "default")
        })
        .filter(|interpreter| seen.insert((interpreter.key.clone(), interpreter.path.clone())))
        .map(|interpreter| Interpreter {
            version: interpreter.version,
            implementation: interpreter.implementation,
            path: interpreter.path,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requirements_split_into_name_extras_specifier_and_marker() {
        let requirement =
            Requirement::parse("Flask_SQLAlchemy[async, mypy] >=3.1,<4 ; python_version < '3.13'");
        assert_eq!(requirement.name, "Flask_SQLAlchemy");
        assert_eq!(requirement.extras, ["async", "mypy"]);
        assert_eq!(requirement.specifier, ">=3.1,<4");
        assert_eq!(
            requirement.marker.as_deref(),
            Some("python_version < '3.13'")
        );
        let direct = Requirement::parse("pkg @ https://example.com/pkg.whl");
        assert_eq!(
            (direct.name.as_str(), direct.specifier.as_str()),
            ("pkg", "@ https://example.com/pkg.whl")
        );
        assert_eq!(Requirement::parse("rich").specifier, "");
        assert_eq!(normalize("Flask_SQL..Alchemy"), "flask-sql-alchemy");
    }

    #[test]
    fn uv_projects_list_dependencies_with_locked_versions() {
        let project = PyProject::parse(
            r#"
[project]
name = "uvp"
version = "0.1.0"
requires-python = ">=3.13"
dependencies = ["httpx>=0.28.1", "Typing_Extensions"]

[project.scripts]
uvp = "uvp:main"

[project.optional-dependencies]
fast = ["orjson>=3.13.0"]

[dependency-groups]
dev = ["pytest>=9.1.1", { include-group = "lint" }]
lint = ["ruff"]

[tool.uv.workspace]
members = ["packages/*"]
"#,
        )
        .unwrap();
        assert_eq!(project.name.as_deref(), Some("uvp"));
        assert_eq!(project.scripts["uvp"], "uvp:main");
        assert_eq!(project.workspace_members, ["packages/*"]);
        assert!(project.uses_uv());
        let poetry = PyProject::parse(
            "[project]\nname = \"p\"\n[build-system]\nbuild-backend = \"poetry.core.masonry.api\"\n",
        )
        .unwrap();
        assert!(!poetry.uses_uv());
        let uv_built = PyProject::parse(
            "[project]\nname = \"p\"\n[build-system]\nbuild-backend = \"uv_build\"\n",
        )
        .unwrap();
        assert!(uv_built.uses_uv());
        let lock = Lock::parse(
            r#"
version = 1
[[package]]
name = "httpx"
version = "0.28.1"
[[package]]
name = "typing-extensions"
version = "4.16.0"
[[package]]
name = "uvp"
source = { editable = "." }
"#,
        )
        .unwrap();
        let dependencies = project.dependencies(Some(&lock));
        let summary: Vec<(String, String, Option<String>)> = dependencies
            .iter()
            .map(|d| (d.kind.label(), d.requirement.name.clone(), d.locked.clone()))
            .collect();
        assert_eq!(
            summary,
            [
                ("dependencies".into(), "httpx".into(), Some("0.28.1".into())),
                (
                    "dependencies".into(),
                    "Typing_Extensions".into(),
                    Some("4.16.0".into())
                ),
                ("extra: fast".into(), "orjson".into(), None),
                ("group: dev".into(), "pytest".into(), None),
                ("group: lint".into(), "ruff".into(), None),
            ]
        );
    }

    #[test]
    fn interpreters_are_listed_once() {
        let json = r#"[
            {"key":"cpython-3.14.4-linux-x86_64-gnu","version":"3.14.4","implementation":"cpython","path":"/usr/bin/python3.14","symlink":null},
            {"key":"cpython-3.14.4-linux-x86_64-gnu","version":"3.14.4","implementation":"cpython","path":"/usr/bin/python3","symlink":"python3.14"},
            {"key":"cpython-3.13.15-linux-x86_64-gnu","version":"3.13.15","implementation":"cpython","path":null,"symlink":null,"variant":"default"},
            {"key":"cpython-3.13.15+freethreaded-linux-x86_64-gnu","version":"3.13.15","implementation":"cpython","path":null,"symlink":null,"variant":"freethreaded"}
        ]"#;
        let interpreters = parse_python_list(json).unwrap();
        assert_eq!(
            interpreters,
            [
                Interpreter {
                    version: "3.14.4".into(),
                    implementation: "cpython".into(),
                    path: Some("/usr/bin/python3.14".into())
                },
                Interpreter {
                    version: "3.13.15".into(),
                    implementation: "cpython".into(),
                    path: None
                },
            ]
        );
    }
}
