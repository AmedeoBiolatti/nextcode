//! `uv` command lines (arguments after `uv`).

use crate::DependencyKind;

fn args<const N: usize>(fixed: [&str; N]) -> Vec<String> {
    fixed.map(str::to_owned).to_vec()
}

/// Initializes a standalone application. The caller supplies its destination
/// as the final argument. No parent workspace, VCS or Python installation changes.
pub fn init(name: &str) -> Vec<String> {
    args([
        "init",
        "--app",
        "--no-package",
        "--name",
        name,
        "--vcs",
        "none",
        "--no-workspace",
        "--no-config",
        "--offline",
        "--no-python-downloads",
        "--color",
        "never",
    ])
}

/// Installs the locked environment (`.venv`).
pub fn sync() -> Vec<String> {
    args(["sync"])
}

/// Exits non-zero when the environment (or the lock) is out of date.
/// Offline: an outdated lock needing a resolution counts as out of date.
pub fn sync_check() -> Vec<String> {
    args(["sync", "--check", "--offline"])
}

/// Updates `uv.lock`; `upgrade` moves every package to its newest allowed
/// version.
pub fn lock(upgrade: bool) -> Vec<String> {
    let mut command = args(["lock"]);
    if upgrade {
        command.push("--upgrade".into());
    }
    command
}

fn kind_flags(kind: &DependencyKind) -> Vec<String> {
    match kind {
        DependencyKind::Main => Vec::new(),
        DependencyKind::Optional(extra) => vec!["--optional".into(), extra.clone()],
        DependencyKind::Group(group) => vec!["--group".into(), group.clone()],
    }
}

/// Adds `requirement` (e.g. `httpx>=0.28`) where `kind` says.
pub fn add(requirement: &str, kind: &DependencyKind) -> Vec<String> {
    let mut command = args(["add"]);
    command.extend(kind_flags(kind));
    command.push(requirement.to_owned());
    command
}

/// Removes the dependency `name` from where `kind` says.
pub fn remove(name: &str, kind: &DependencyKind) -> Vec<String> {
    let mut command = args(["remove"]);
    command.extend(kind_flags(kind));
    command.push(name.to_owned());
    command
}

/// Upgrades one package in the lock.
pub fn upgrade_package(name: &str) -> Vec<String> {
    let mut command = args(["lock", "--upgrade-package"]);
    command.push(name.to_owned());
    command
}

/// Writes `.python-version`.
pub fn pin(version: &str) -> Vec<String> {
    let mut command = args(["python", "pin"]);
    command.push(version.to_owned());
    command
}

/// Interpreters uv finds or can download, as JSON.
pub fn python_list() -> Vec<String> {
    args(["python", "list", "--output-format", "json"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependency_edits_name_their_section() {
        assert_eq!(
            add("httpx>=0.28", &DependencyKind::Main),
            ["add", "httpx>=0.28"]
        );
        assert_eq!(
            add("pytest", &DependencyKind::Group("dev".into())),
            ["add", "--group", "dev", "pytest"]
        );
        assert_eq!(
            remove("orjson", &DependencyKind::Optional("fast".into())),
            ["remove", "--optional", "fast", "orjson"]
        );
    }
}
