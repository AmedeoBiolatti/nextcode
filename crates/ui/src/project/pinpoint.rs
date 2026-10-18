//! Pinpoint calls from a remote project's machine: CLI agents inside WSL or
//! the dev container run `next_code_agent pinpoint …`, the project's agent
//! relays them here, and they are dispatched like the local CLI's, scoped to
//! this project's window.
//!
//! The caller speaks the remote machine's paths: absolute `file`s and code
//! `ref`s below a project folder become project paths (as
//! [`crate::pinpoint::file_key`] writes them) before dispatch, and every
//! `file` and code `reference` of the answer goes back as the remote
//! absolute path (`context` answers too: their `file`s are project paths).

use std::io;

use remote::{AgentClient, StreamHandle};
use serde_json::Value;

use super::Root;
use crate::pinpoint::{code_reference, parse_code_reference};

/// A project folder: its path on the remote machine and its name.
#[derive(Debug, Clone)]
struct Folder {
    /// Without a trailing `/` (but `/` itself).
    path: String,
    name: String,
}

impl Folder {
    fn of(root: &Root) -> Self {
        let path = root.path.to_string_lossy().replace('\\', "/");
        let trimmed = path.trim_end_matches('/');
        Self {
            path: if trimmed.is_empty() {
                "/".into()
            } else {
                trimmed.into()
            },
            name: root.name.clone(),
        }
    }

    /// `file`'s path inside this folder (empty for the folder itself).
    fn relative<'a>(&self, file: &'a str) -> Option<&'a str> {
        let rest = file.strip_prefix(self.path.as_str())?;
        if self.path == "/" {
            return Some(rest);
        }
        match rest.strip_prefix('/') {
            Some(rest) => Some(rest),
            None if rest.is_empty() => Some(rest),
            None => None,
        }
    }

    fn join(&self, relative: &str) -> String {
        match (relative.is_empty(), self.path.as_str()) {
            (true, _) => self.path.clone(),
            (false, "/") => format!("/{relative}"),
            (false, path) => format!("{path}/{relative}"),
        }
    }
}

/// Starts serving the project's remote pinpoint calls on `client`.
pub(super) fn serve(
    client: &std::sync::Arc<AgentClient>,
    roots: &[Root],
) -> io::Result<StreamHandle> {
    let folders: Vec<Folder> = roots.iter().map(Folder::of).collect();
    let paths = folders.iter().map(|folder| folder.path.clone()).collect();
    client.serve_pinpoint(paths, move |request, reply| {
        let mut request = match serde_json::from_str::<Value>(&request) {
            Ok(request @ Value::Object(_)) => request,
            _ => {
                reply.send(r#"{"ok":false,"error":"the request is not a JSON object"}"#.into());
                return;
            }
        };
        to_project(&mut request, &folders);
        let folders = folders.clone();
        crate::pinpoint::control::dispatch(request, move |mut answer| {
            to_remote(&mut answer, &folders);
            reply.send(answer.to_string());
        });
    })
}

/// The project path of a remote absolute `file` below one of `folders`
/// (the innermost), as `file_key` writes it.
fn project_path(file: &str, folders: &[Folder]) -> Option<String> {
    let (folder, relative) = folders
        .iter()
        .filter_map(|folder| Some((folder, folder.relative(file)?)))
        .max_by_key(|(folder, _)| folder.path.len())?;
    Some(match (folders.len(), relative.is_empty()) {
        (1, true) => ".".into(),
        (1, false) => relative.into(),
        (_, true) => folder.name.clone(),
        (_, false) => format!("{}/{relative}", folder.name),
    })
}

/// The remote absolute path of a project path (`None`: already absolute).
fn remote_path(key: &str, folders: &[Folder]) -> Option<String> {
    if key.starts_with('/') {
        return None;
    }
    match folders {
        [folder] => Some(folder.join(if key == "." { "" } else { key })),
        _ => {
            let (name, rest) = key.split_once('/').unwrap_or((key, ""));
            let folder = folders.iter().find(|folder| folder.name == name)?;
            Some(folder.join(rest))
        }
    }
}

/// Scopes `request` to this project and maps its `file`s to project paths.
/// Only absolute paths change, so element paths in `ref`s that end like a
/// code reference (`…/pane:1`) stay as they are.
fn to_project(request: &mut Value, folders: &[Folder]) {
    map_files(request, &["ref", "reference"], &mut |file| {
        project_path(file, folders)
    });
    if let Value::Object(map) = request {
        // The caller's working directory is a remote path: the project
        // already names the window.
        map.remove("cwd");
        map.insert(
            "project_roots".into(),
            folders
                .iter()
                .map(|folder| Value::from(folder.path.as_str()))
                .collect(),
        );
    }
}

/// Maps project paths in `answer` back to remote absolute paths: `file`s
/// and the code `reference`s of picks (an echoed `ref` may be an element
/// path, which must not become a file).
fn to_remote(answer: &mut Value, folders: &[Folder]) {
    map_files(answer, &["reference"], &mut |file| {
        remote_path(file, folders)
    });
}

/// Replaces every `"file"` string `map` maps, anywhere in `value`, and the
/// file of every code reference (`src/a.rs:3:1-3:9`) under one of
/// `reference_keys`.
fn map_files(
    value: &mut Value,
    reference_keys: &[&str],
    map: &mut dyn FnMut(&str) -> Option<String>,
) {
    match value {
        Value::Object(object) => {
            for (key, child) in object.iter_mut() {
                match child {
                    Value::String(file) if key == "file" => {
                        if let Some(mapped) = map(file) {
                            *file = mapped;
                        }
                    }
                    // Element references (`nextcode://ui/…`) name no file,
                    // even where they end like a line number (`pane:1`).
                    Value::String(reference)
                        if reference_keys.contains(&key.as_str())
                            && !reference.starts_with("nextcode://") =>
                    {
                        if let Some(mut code) = parse_code_reference(reference)
                            && let Some(mapped) = map(&code.file)
                        {
                            code.file = mapped;
                            *reference = code_reference(&code);
                        }
                    }
                    _ => map_files(child, reference_keys, map),
                }
            }
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| map_files(item, reference_keys, map)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    fn folders(roots: &[(&str, &str)]) -> Vec<Folder> {
        roots
            .iter()
            .map(|(path, name)| {
                Folder::of(&Root {
                    path: PathBuf::from(path),
                    name: name.to_string(),
                    custom_name: None,
                })
            })
            .collect()
    }

    #[test]
    fn remote_files_become_project_paths_and_back() {
        let single = folders(&[("/home/me/proj/", "proj")]);
        assert_eq!(
            project_path("/home/me/proj/src/a.rs", &single).as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(project_path("/home/me/proj", &single).as_deref(), Some("."));
        assert_eq!(project_path("/home/me/project/a.rs", &single), None);
        assert_eq!(project_path("src/a.rs", &single), None);
        assert_eq!(
            remote_path("src/a.rs", &single).as_deref(),
            Some("/home/me/proj/src/a.rs")
        );
        assert_eq!(remote_path(".", &single).as_deref(), Some("/home/me/proj"));
        assert_eq!(remote_path("/etc/hosts", &single), None);

        // Multi-root: `<folder name>/<path>`, the innermost folder winning.
        let multi = folders(&[
            ("/w/api", "api"),
            ("/w/api/web", "web"),
            ("/", "everything"),
        ]);
        assert_eq!(
            project_path("/w/api/main.rs", &multi).as_deref(),
            Some("api/main.rs")
        );
        assert_eq!(
            project_path("/w/api/web/x.ts", &multi).as_deref(),
            Some("web/x.ts")
        );
        assert_eq!(
            project_path("/etc/hosts", &multi).as_deref(),
            Some("everything/etc/hosts")
        );
        assert_eq!(project_path("/w/api", &multi).as_deref(), Some("api"));
        assert_eq!(
            remote_path("web/x.ts", &multi).as_deref(),
            Some("/w/api/web/x.ts")
        );
        assert_eq!(
            remote_path("everything/etc/hosts", &multi).as_deref(),
            Some("/etc/hosts")
        );
        assert_eq!(remote_path("api", &multi).as_deref(), Some("/w/api"));
        assert_eq!(remote_path("nope/x", &multi), None);
    }

    #[test]
    fn requests_are_scoped_to_the_project_and_answers_use_remote_paths() {
        let folders = folders(&[("/home/me/proj", "proj")]);
        let mut request = json!({
            "cmd": "highlight",
            "cwd": "/home/me/proj/src",
            "targets": [
                { "file": "/home/me/proj/src/main.rs", "line": 12, "column": 5 },
                { "file": "src/lib.rs", "line": 1 },
                { "file": "/opt/other.rs", "line": 1 },
                { "ref": "/home/me/proj/src/main.rs:3:5-3:9" },
                { "ref": "/home/me/proj/nb.ipynb#cell=2:1" },
                { "ref": "workspace/editor-area/pane:1" },
                { "ref": "nextcode://ui/workspace/left-dock/panel:files/row:src/main.rs" },
            ],
        });
        to_project(&mut request, &folders);
        assert_eq!(
            request,
            json!({
                "cmd": "highlight",
                "targets": [
                    { "file": "src/main.rs", "line": 12, "column": 5 },
                    { "file": "src/lib.rs", "line": 1 },
                    { "file": "/opt/other.rs", "line": 1 },
                    { "ref": "src/main.rs:3:5-3:9" },
                    { "ref": "nb.ipynb#cell=2:1" },
                    { "ref": "workspace/editor-area/pane:1" },
                    { "ref": "nextcode://ui/workspace/left-dock/panel:files/row:src/main.rs" },
                ],
                "project_roots": ["/home/me/proj"],
            })
        );

        let mut answer = json!({
            "ok": true,
            "pins": [
                {
                    "reference": "src/main.rs:12:5-12:18",
                    "target": { "file": "src/main.rs", "line": 12 },
                    "file_absolute": "/home/me/proj/src/main.rs",
                },
                {
                    "reference": "nextcode://ui/workspace/editor-area/pane:1",
                    "target": { "path": "workspace/editor-area/pane:1" },
                },
                { "reference": "/opt/other.rs:1", "target": { "file": "/opt/other.rs", "line": 1 } },
            ],
            "resolved": [{ "ref": "workspace/editor-area/pane:1" }],
        });
        to_remote(&mut answer, &folders);
        assert_eq!(
            answer,
            json!({
                "ok": true,
                "pins": [
                    {
                        "reference": "/home/me/proj/src/main.rs:12:5-12:18",
                        "target": { "file": "/home/me/proj/src/main.rs", "line": 12 },
                        "file_absolute": "/home/me/proj/src/main.rs",
                    },
                    {
                        "reference": "nextcode://ui/workspace/editor-area/pane:1",
                        "target": { "path": "workspace/editor-area/pane:1" },
                    },
                    { "reference": "/opt/other.rs:1", "target": { "file": "/opt/other.rs", "line": 1 } },
                ],
                "resolved": [{ "ref": "workspace/editor-area/pane:1" }],
            })
        );
    }

    #[test]
    fn context_answers_name_files_by_their_remote_path() {
        let single = folders(&[("/home/me/proj", "proj")]);
        let mut answer = json!({
            "ok": true,
            "project": { "roots": [{ "name": "proj", "path": "/home/me/proj" }], "remote": true },
            "editor": { "file": "app.py", "path": "/home/me/proj/app.py", "line_count": 3 },
            "tabs": [{ "file": "app.py", "title": "app.py" }, { "file": "tests/test_app.py" }],
            "problems": { "items": [{ "file": "app.py", "line": 2 }] },
            "tests": { "failures": [{ "id": "tests/test_app.py::test_x", "file": "tests/test_app.py" }] },
            "debugger": { "location": { "file": "app.py", "line": 2 } },
        });
        to_remote(&mut answer, &single);
        assert_eq!(answer["editor"]["file"], "/home/me/proj/app.py");
        assert_eq!(answer["tabs"][1]["file"], "/home/me/proj/tests/test_app.py");
        assert_eq!(answer["problems"]["items"][0]["file"], "/home/me/proj/app.py");
        assert_eq!(
            answer["tests"]["failures"][0]["file"],
            "/home/me/proj/tests/test_app.py"
        );
        // Test ids are not paths.
        assert_eq!(answer["tests"]["failures"][0]["id"], "tests/test_app.py::test_x");
        assert_eq!(answer["debugger"]["location"]["file"], "/home/me/proj/app.py");
    }
}
