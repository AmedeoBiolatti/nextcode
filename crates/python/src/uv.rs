//! `uv` command lines (arguments after `uv`).

use crate::DependencyKind;

fn args<const N: usize>(fixed: [&str; N]) -> Vec<String> {
    fixed.map(str::to_owned).to_vec()
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

/// What to run with `uv run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Run {
    /// A `[project.scripts]` entry point.
    Script(String),
    /// A Python file (project-relative or absolute).
    File(String),
    /// pytest on a target (`""` for every test, a file, or a node id).
    Pytest(String),
}

/// `uv run …`, as one shell line for a terminal (arguments quoted for
/// POSIX shells, PowerShell and cmd alike: double quotes when needed).
pub fn run_line(run: &Run) -> String {
    let quote = |text: &str| {
        if text.is_empty()
            || text
                .chars()
                .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '&' | '|' | ';' | '$'))
        {
            format!("\"{}\"", text.replace('"', "\\\""))
        } else {
            text.to_owned()
        }
    };
    match run {
        Run::Script(name) => format!("uv run {}", quote(name)),
        Run::File(path) => format!("uv run {}", quote(path)),
        Run::Pytest(target) if target.is_empty() => "uv run pytest".into(),
        Run::Pytest(target) => format!("uv run pytest {}", quote(target)),
    }
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

    #[test]
    fn run_lines_quote_paths_with_spaces() {
        assert_eq!(
            run_line(&Run::File("src/my app.py".into())),
            "uv run \"src/my app.py\""
        );
        assert_eq!(
            run_line(&Run::Pytest("tests/test_a.py::TestX::test_y".into())),
            "uv run pytest tests/test_a.py::TestX::test_y"
        );
        assert_eq!(run_line(&Run::Pytest(String::new())), "uv run pytest");
    }
}
