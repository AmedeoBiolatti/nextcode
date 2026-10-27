//! Multi-root workspace files: next_code's `*.next_code-workspace` and VS
//! Code's `*.code-workspace` (folders and a subset of its settings; never
//! written).
//!
//! ```json
//! { "folders": [{ "path": "." }, { "path": "../api", "name": "API" }],
//!   "settings": { "format_on_save": true } }
//! ```
//!
//! Folder paths may be relative to the file's directory. Paths are strings
//! in the syntax of the machine holding the file: POSIX for WSL (and every
//! Linux path), the platform's own for local files.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// next_code's workspace file extension.
pub const EXTENSION: &str = "next_code-workspace";
/// VS Code's, opened read-only.
pub const VSCODE_EXTENSION: &str = "code-workspace";

/// A folder of a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    /// Absolute once parsed; as written (maybe relative) in the file.
    pub path: String,
    /// Shown instead of the folder's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    NextCode,
    VsCode,
}

impl Format {
    /// The format of a workspace file path (by extension), `None` for any
    /// other path.
    pub fn of(path: &str) -> Option<Self> {
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(&format!(".{EXTENSION}")) {
            Some(Self::NextCode)
        } else if lower.ends_with(&format!(".{VSCODE_EXTENSION}")) {
            Some(Self::VsCode)
        } else {
            None
        }
    }
}

/// A parsed workspace file.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceFile {
    /// With absolute paths.
    pub folders: Vec<Folder>,
    /// In next_code's settings schema (VS Code keys translated); `Null`
    /// when the file has none.
    pub settings: Value,
    /// Entries that were skipped (e.g. remote VS Code folder URIs).
    pub warnings: Vec<String>,
}

/// Reads a workspace file's text (JSON with comments and trailing
/// commas); `file` is its absolute path, which relative folders are
/// resolved against. `posix` selects the path syntax.
pub fn parse(text: &str, file: &str, posix: bool) -> Result<WorkspaceFile, String> {
    let format = Format::of(file).unwrap_or(Format::NextCode);
    let value: Value = serde_json::from_str(&devcontainer::jsonc::to_json(text))
        .map_err(|err| format!("{file}: {err}"))?;
    let Some(object) = value.as_object() else {
        return Err(format!("{file}: not a JSON object"));
    };
    let dir = parent(file, posix);
    let mut warnings = Vec::new();
    let mut folders = Vec::new();
    let entries = match object.get("folders") {
        Some(Value::Array(entries)) => entries.as_slice(),
        None => &[],
        Some(_) => return Err(format!("{file}: \"folders\" must be an array")),
    };
    for entry in entries {
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .map(str::to_owned);
        let path = match (entry.get("path"), entry.get("uri")) {
            (Some(Value::String(path)), _) => path.clone(),
            (_, Some(Value::String(uri))) => match file_uri_path(uri, posix) {
                Some(path) => path,
                None => {
                    warnings.push(format!("folder {uri} skipped: only file: URIs open"));
                    continue;
                }
            },
            _ => return Err(format!("{file}: each folder needs a \"path\"")),
        };
        if path.trim().is_empty() {
            return Err(format!("{file}: a folder has an empty \"path\""));
        }
        folders.push(Folder {
            path: resolve(&dir, &path, posix),
            name,
        });
    }
    let settings = match object.get("settings") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::Object(settings)) => match format {
            Format::NextCode => Value::Object(settings.clone()),
            Format::VsCode => translate_vscode(settings),
        },
        Some(_) => return Err(format!("{file}: \"settings\" must be an object")),
    };
    Ok(WorkspaceFile {
        folders,
        settings,
        warnings,
    })
}

/// The text of a `*.next_code-workspace` file at `file` for `folders`
/// (absolute): folders sharing a directory with the file are written
/// relative to it, so the file can move with them.
pub fn to_text(folders: &[Folder], settings: &Value, file: &str, posix: bool) -> String {
    let dir = parent(file, posix);
    let folders: Vec<Value> = folders
        .iter()
        .map(|folder| {
            let path =
                relative_to(&dir, &folder.path, posix).unwrap_or_else(|| folder.path.clone());
            let mut entry = json!({ "path": path });
            if let Some(name) = &folder.name {
                entry["name"] = name.clone().into();
            }
            entry
        })
        .collect();
    let mut object = Map::new();
    object.insert("folders".into(), folders.into());
    if !settings.is_null() {
        object.insert("settings".into(), settings.clone());
    }
    serde_json::to_string_pretty(&Value::Object(object)).unwrap_or_default() + "\n"
}

/// The directory of `file`.
pub fn parent(file: &str, posix: bool) -> String {
    if posix {
        match file.trim_end_matches('/').rsplit_once('/') {
            Some(("", _)) => "/".into(),
            Some((dir, _)) => dir.into(),
            None => ".".into(),
        }
    } else {
        Path::new(file)
            .parent()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

/// The last component of a folder path (its default name).
pub fn folder_name(path: &str, posix: bool) -> String {
    let name = if posix {
        path.trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned()
    } else {
        Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    if name.is_empty() {
        path.to_owned()
    } else {
        name
    }
}

/// `path` made absolute against `dir`, with `.` and `..` removed.
pub fn resolve(dir: &str, path: &str, posix: bool) -> String {
    if posix {
        let joined = if path.starts_with('/') {
            path.to_owned()
        } else {
            format!("{}/{path}", dir.trim_end_matches('/'))
        };
        let mut parts: Vec<&str> = Vec::new();
        for part in joined.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                part => parts.push(part),
            }
        }
        format!("/{}", parts.join("/"))
    } else {
        let mut out = PathBuf::new();
        for component in Path::new(dir).join(path).components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    out.pop();
                }
                other => out.push(other),
            }
        }
        out.to_string_lossy().into_owned()
    }
}

/// `path` relative to `dir` when they share more than the filesystem root
/// (same drive on Windows), else `None`.
pub fn relative_to(dir: &str, path: &str, posix: bool) -> Option<String> {
    let (dir_parts, path_parts, separator) = if posix {
        let split = |s: &str| -> Vec<String> {
            s.split('/')
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect()
        };
        (split(dir), split(path), "/")
    } else {
        let split = |s: &str| -> Vec<String> {
            Path::new(s)
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect()
        };
        (split(dir), split(path), std::path::MAIN_SEPARATOR_STR)
    };
    let same = |a: &String, b: &String| {
        if posix || !cfg!(windows) {
            a == b
        } else {
            a.eq_ignore_ascii_case(b)
        }
    };
    let common = dir_parts
        .iter()
        .zip(&path_parts)
        .take_while(|(a, b)| same(a, b))
        .count();
    // POSIX parts drop the root; Windows ones keep the prefix and root.
    let root_parts = if posix || !cfg!(windows) {
        if posix { 0 } else { 1 }
    } else {
        2
    };
    if common <= root_parts {
        return None;
    }
    let mut parts: Vec<&str> = vec![".."; dir_parts.len() - common];
    parts.extend(path_parts[common..].iter().map(String::as_str));
    Some(if parts.is_empty() {
        ".".into()
    } else {
        parts.join(separator)
    })
}

/// The path of a `file:` URI (percent-decoded); `None` for other schemes.
fn file_uri_path(uri: &str, posix: bool) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let mut bytes = Vec::with_capacity(rest.len());
    let raw = rest.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%'
            && let Some(byte) = rest
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            bytes.push(byte);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    let path = String::from_utf8(bytes).ok()?;
    // `file:///c%3A/src` → `c:/src` on Windows.
    if !posix
        && let Some(drive) = path.strip_prefix('/')
        && drive.as_bytes().get(1) == Some(&b':')
    {
        return Some(drive.replace('/', "\\"));
    }
    Some(path)
}

/// The VS Code settings next_code understands, in its own schema:
/// `editor.wordWrap`, `editor.formatOnSave`, `editor.inlayHints.enabled`,
/// `editor.autoClosingBrackets`, `editor.minimap.enabled`,
/// `editor.stickyScroll.enabled`, `git.blame.editorDecoration.enabled`,
/// `python.analysis.typeCheckingMode` and language blocks (`"[python]"`)
/// with the editor keys. Others are ignored.
fn translate_vscode(settings: &Map<String, Value>) -> Value {
    let mut out = json!({});
    translate_editor_keys(settings, &mut out, true);
    for (key, value) in settings {
        let Some(languages) = key.strip_prefix('[').and_then(|key| key.strip_suffix(']')) else {
            continue;
        };
        let Value::Object(block) = value else {
            continue;
        };
        // `"[javascript][typescript]"` names several languages.
        for language in languages.split("][") {
            let mut overrides = json!({});
            translate_editor_keys(block, &mut overrides, false);
            if overrides.as_object().is_some_and(|map| !map.is_empty()) {
                out["languages"][language] = overrides;
            }
        }
    }
    let flag = |key: &str| settings.get(key).and_then(Value::as_bool);
    if let Some(enabled) = flag("git.blame.editorDecoration.enabled") {
        out["git"]["inline_blame"] = enabled.into();
    }
    if let Some(mode) = settings
        .get("python.analysis.typeCheckingMode")
        .and_then(Value::as_str)
    {
        out["languages"]["python"]["server"]["settings"]["basedpyright"]["analysis"]["typeCheckingMode"] =
            mode.into();
    }
    if out.as_object().is_some_and(|map| map.is_empty()) {
        Value::Null
    } else {
        out
    }
}

/// Editor keys of a VS Code settings object (top level or a language
/// block, which only takes the per-language ones).
fn translate_editor_keys(settings: &Map<String, Value>, out: &mut Value, top: bool) {
    let text = |key: &str| settings.get(key).and_then(Value::as_str);
    let flag = |key: &str| settings.get(key).and_then(Value::as_bool);
    if let Some(wrap) = text("editor.wordWrap") {
        out["word_wrap"] = (wrap != "off").into();
    }
    if let Some(format) = flag("editor.formatOnSave") {
        out["format_on_save"] = format.into();
    }
    if let Some(hints) = text("editor.inlayHints.enabled") {
        out["inlay_hints"] = matches!(hints, "on" | "onUnlessPressed").into();
    }
    if !top {
        return;
    }
    if let Some(close) = text("editor.autoClosingBrackets") {
        out["auto_close_brackets"] = (close != "never").into();
    }
    if let Some(enabled) = flag("editor.minimap.enabled") {
        out["editor"]["minimap"]["enabled"] = enabled.into();
    }
    if let Some(enabled) = flag("editor.stickyScroll.enabled") {
        out["editor"]["sticky_scroll"]["enabled"] = enabled.into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_folders_resolve_against_the_file_directory() {
        let text = r#"{
            // comments and trailing commas, as VS Code allows
            "folders": [
                { "path": "." },
                { "path": "../api", "name": "API" },
                { "path": "/abs/lib" },
                { "path": "./sub/../tools/" },
            ],
        }"#;
        let parsed = parse(text, "/home/me/ws/app.next_code-workspace", true).unwrap();
        let paths: Vec<_> = parsed.folders.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "/home/me/ws",
                "/home/me/api",
                "/abs/lib",
                "/home/me/ws/tools"
            ]
        );
        assert_eq!(parsed.folders[1].name.as_deref(), Some("API"));
        assert_eq!(parsed.folders[0].name, None);
        assert_eq!(parsed.settings, Value::Null);
    }

    #[test]
    fn next_code_settings_are_kept_and_bad_shapes_are_errors() {
        let text = r#"{ "folders": [{"path": "a"}], "settings": { "format_on_save": true } }"#;
        let parsed = parse(text, "/w/x.next_code-workspace", true).unwrap();
        assert_eq!(parsed.settings, json!({ "format_on_save": true }));
        assert!(parse(r#"{"folders": {}}"#, "/w/x.next_code-workspace", true).is_err());
        assert!(
            parse(
                r#"{"folders": [{"name": "n"}]}"#,
                "/w/x.next_code-workspace",
                true
            )
            .is_err()
        );
        assert!(parse("[]", "/w/x.next_code-workspace", true).is_err());
        assert!(parse("{ broken", "/w/x.next_code-workspace", true).is_err());
        // No folders is a valid (empty) file; opening it reports that.
        assert!(
            parse("{}", "/w/x.next_code-workspace", true)
                .unwrap()
                .folders
                .is_empty()
        );
    }

    #[test]
    fn vscode_files_give_folders_uris_and_a_settings_subset() {
        let text = r#"{
            "folders": [
                { "path": "frontend" },
                { "uri": "file:///srv/my%20lib", "name": "Lib" },
                { "uri": "vscode-remote://ssh-remote+box/home/x" }
            ],
            "settings": {
                "editor.wordWrap": "on",
                "editor.formatOnSave": true,
                "editor.minimap.enabled": false,
                "files.exclude": { "**/.git": true },
                "python.analysis.typeCheckingMode": "strict",
                "[python]": { "editor.formatOnSave": false, "editor.tabSize": 4 },
                "[javascript][typescript]": { "editor.wordWrap": "off" }
            },
            "extensions": { "recommendations": [] }
        }"#;
        let parsed = parse(text, "/ws/team.code-workspace", true).unwrap();
        assert_eq!(
            parsed.folders,
            [
                Folder {
                    path: "/ws/frontend".into(),
                    name: None
                },
                Folder {
                    path: "/srv/my lib".into(),
                    name: Some("Lib".into())
                },
            ]
        );
        assert_eq!(parsed.warnings.len(), 1, "the remote folder is reported");
        assert_eq!(
            parsed.settings,
            json!({
                "word_wrap": true,
                "format_on_save": true,
                "editor": { "minimap": { "enabled": false } },
                "languages": {
                    "python": {
                        "format_on_save": false,
                        "server": { "settings": { "basedpyright": { "analysis": {
                            "typeCheckingMode": "strict" } } } }
                    },
                    "javascript": { "word_wrap": false },
                    "typescript": { "word_wrap": false }
                }
            })
        );
        // The translation fits next_code's settings schema.
        crate::settings::Settings::default()
            .layered(&[&parsed.settings])
            .unwrap();
    }

    #[test]
    fn writing_uses_paths_relative_to_the_file_when_they_share_a_folder() {
        let folders = [
            Folder {
                path: "/home/me/ws".into(),
                name: None,
            },
            Folder {
                path: "/home/me/api".into(),
                name: Some("API".into()),
            },
            Folder {
                path: "/opt/lib".into(),
                name: None,
            },
        ];
        let file = "/home/me/ws/all.next_code-workspace";
        let text = to_text(&folders, &json!({ "word_wrap": true }), file, true);
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["folders"],
            json!([{ "path": "." }, { "path": "../api", "name": "API" }, { "path": "/opt/lib" }])
        );
        // And reads back to the same folders and settings.
        let parsed = parse(&text, file, true).unwrap();
        assert_eq!(parsed.folders, folders);
        assert_eq!(parsed.settings, json!({ "word_wrap": true }));
    }

    #[test]
    fn local_paths_resolve_with_the_platform_syntax() {
        let dir = std::env::temp_dir().join("ws");
        let file = dir.join("a.next_code-workspace");
        let text = r#"{ "folders": [{ "path": "../b" }, { "path": "c" }] }"#;
        let parsed = parse(text, &file.to_string_lossy(), false).unwrap();
        assert_eq!(
            PathBuf::from(&parsed.folders[0].path),
            std::env::temp_dir().join("b")
        );
        assert_eq!(PathBuf::from(&parsed.folders[1].path), dir.join("c"));
        assert_eq!(folder_name(&parsed.folders[1].path, false), "c");
        assert_eq!(folder_name("/home/me/p/", true), "p");
    }

    #[test]
    fn workspace_file_formats_are_recognised_by_extension() {
        assert_eq!(
            Format::of("/a/b.next_code-workspace"),
            Some(Format::NextCode)
        );
        assert_eq!(Format::of(r"C:\a\B.Code-Workspace"), Some(Format::VsCode));
        assert_eq!(Format::of("/a/b.json"), None);
        assert_eq!(Format::of("/a/code-workspace"), None);
    }
}
