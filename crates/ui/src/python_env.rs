//! The Python state of a project where it lives (locally, in WSL or in a
//! container): its uv project, environment and interpreter, read and
//! changed with uv. Used by the Python panel (`builtins::python`), the
//! notebook interpreter picker and headless mode.

use std::{io, path::Path, time::Duration};

use python::{Dependency, Lock, PyProject};
use remote::RunOutput;
use serde::Serialize;

use crate::project::Project;

/// Long enough for a first `uv sync` downloading a Python and wheels.
const UV_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// What the Python panel shows about the project.
#[derive(Debug, Clone, Serialize)]
pub struct PythonInfo {
    pub name: Option<String>,
    pub version: Option<String>,
    pub requires_python: Option<String>,
    #[serde(skip)]
    pub project: PyProject,
    #[serde(skip)]
    pub dependencies: Vec<Dependency>,
    pub locked_packages: Option<usize>,
    /// The environment's interpreter (`.venv`), if any.
    pub interpreter: Option<String>,
    /// From `.venv/pyvenv.cfg`.
    pub environment_python: Option<String>,
    /// `.python-version`.
    pub pinned: Option<String>,
    /// Where `uv` is, if installed.
    pub uv: Option<String>,
    /// `uv sync --check`: `None` without uv or for non-uv projects.
    pub in_sync: Option<bool>,
}

/// Reads the Python state of the project's first folder (its uv project,
/// `.venv` and interpreter). `None` without a `pyproject.toml` there.
/// Blocks (several reads and a `uv sync --check`).
pub fn load(project: &Project) -> io::Result<Option<PythonInfo>> {
    let read = |name: &str| match project.read_text(&project.resolve(name)) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    };
    let Some(text) = read("pyproject.toml")? else {
        return Ok(None);
    };
    let pyproject = PyProject::parse(&text)?;
    let lock = read("uv.lock")?
        .map(|text| Lock::parse(&text))
        .transpose()?;
    let environment_python = read(".venv/pyvenv.cfg")?.and_then(|cfg| {
        cfg.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            matches!(key.trim(), "version_info" | "version").then(|| value.trim().to_owned())
        })
    });
    let pinned = read(".python-version")?
        .and_then(|text| text.lines().next().map(|line| line.trim().to_owned()))
        .filter(|line| !line.is_empty());
    let uv = project
        .find_executable("uv")?
        .map(|path| path.to_string_lossy().into_owned());
    let uv_project = lock.is_some() || pyproject.uses_uv();
    let in_sync = match &uv {
        Some(uv) if uv_project => {
            let output = run_uv(project, uv, &python::uv::sync_check())?;
            Some(output.code == Some(0))
        }
        _ => None,
    };
    Ok(Some(PythonInfo {
        name: pyproject.name.clone(),
        version: pyproject.version.clone(),
        requires_python: pyproject.requires_python.clone(),
        dependencies: pyproject.dependencies(lock.as_ref()),
        locked_packages: lock.as_ref().map(Lock::len),
        interpreter: project
            .python_interpreter()?
            .map(|path| path.to_string_lossy().into_owned()),
        environment_python,
        pinned,
        uv,
        in_sync,
        project: pyproject,
    }))
}

/// Runs `uv` with `args` in the first folder. Blocks.
pub fn run_uv(project: &Project, uv: &str, args: &[String]) -> io::Result<RunOutput> {
    project.run_program(
        uv,
        args,
        &[("NO_COLOR", "1"), ("UV_NO_PROGRESS", "1")],
        None,
        UV_TIMEOUT,
    )
}

/// A path inside the project, as uv and pytest take it (relative to its
/// folder, `/`-separated), or as is when outside every folder.
pub fn project_relative(project: &Project, path: &Path) -> String {
    let root = project.root_for(path);
    match path.strip_prefix(&root.path) {
        Ok(relative) => relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}
