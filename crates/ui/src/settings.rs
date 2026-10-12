//! User settings and language-server resolution.
//!
//! `settings.json` lives in the platform config dir (`%APPDATA%\next_code` on
//! Windows, `~/.config/next_code` on Linux). Every field is optional and is
//! layered over built-in defaults:
//!
//! ```json
//! { "languages": { "python": { "server": {
//!     "command": "basedpyright-langserver", "args": ["--stdio"],
//!     "settings": { "basedpyright": { "analysis": { "typeCheckingMode": "strict" } } }
//! } } } }
//! ```

use std::{collections::HashMap, path::PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};
use syntax::Language;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub languages: HashMap<String, LanguageSettings>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LanguageSettings {
    pub server: ServerOverrides,
}

/// User overrides; unset fields keep the built-in default.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerOverrides {
    pub enabled: Option<bool>,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    /// Deep-merged over the default `settings`.
    pub settings: Option<Value>,
    pub initialization_options: Option<Value>,
}

/// A fully resolved server configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerConfig {
    pub enabled: bool,
    pub command: String,
    pub args: Vec<String>,
    /// Answers `workspace/configuration`, looked up by dotted section.
    pub settings: Value,
    pub initialization_options: Value,
    /// How to install the server when `command` is missing.
    pub install_hint: Option<&'static str>,
}

impl Settings {
    pub fn path() -> Option<PathBuf> {
        Some(dirs::config_dir()?.join("next_code").join("settings.json"))
    }

    /// Loads the user settings; a missing file yields defaults, a broken one
    /// yields defaults plus the error to show.
    pub fn load() -> (Self, Option<String>) {
        let Some(path) = Self::path() else {
            return (Self::default(), None);
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(settings) => (settings, None),
                Err(err) => (Self::default(), Some(format!("{}: {err}", path.display()))),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (Self::default(), None),
            Err(err) => (Self::default(), Some(format!("{}: {err}", path.display()))),
        }
    }

    /// Server for `language`, or `None` if the language has none.
    pub fn server_config(&self, language: Language) -> Option<ServerConfig> {
        let mut config = default_server(language)?;
        let Some(overrides) = self.languages.get(language.id()).map(|l| &l.server) else {
            return Some(config);
        };
        if let Some(enabled) = overrides.enabled {
            config.enabled = enabled;
        }
        if let Some(command) = &overrides.command {
            config.command.clone_from(command);
        }
        if let Some(args) = &overrides.args {
            config.args.clone_from(args);
        }
        if let Some(settings) = &overrides.settings {
            merge(&mut config.settings, settings.clone());
        }
        if let Some(options) = &overrides.initialization_options {
            config.initialization_options = options.clone();
        }
        Some(config)
    }
}

fn default_server(language: Language) -> Option<ServerConfig> {
    let (command, args, settings, install_hint): (&str, &[&str], Value, Option<&'static str>) =
        match language {
            Language::Python => (
                "basedpyright-langserver",
                &["--stdio"],
                // basedpyright's own default ("recommended") flags far more than PyCharm does.
                json!({ "basedpyright": { "analysis": { "typeCheckingMode": "standard" } } }),
                Some("uv tool install basedpyright"),
            ),
            Language::Rust => (
                "rust-analyzer",
                &[],
                json!({}),
                Some("rustup component add rust-analyzer"),
            ),
            Language::C | Language::Cpp => (
                "clangd",
                &[],
                json!({}),
                Some(if cfg!(windows) {
                    "winget install LLVM.LLVM"
                } else {
                    "install clangd with your package manager"
                }),
            ),
            Language::Sql => return None,
        };
    Some(ServerConfig {
        enabled: true,
        command: command.to_owned(),
        args: args.iter().map(|&arg| arg.to_owned()).collect(),
        settings,
        initialization_options: Value::Null,
        install_hint,
    })
}

/// Recursively merges `overlay` into `base`; non-object values replace.
pub fn merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                merge(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// The value at a dotted `section` (`"basedpyright.analysis"`), or the whole
/// settings for `None`; missing sections are `null`.
pub fn section(settings: &Value, section: Option<&str>) -> Value {
    let Some(section) = section.filter(|s| !s.is_empty()) else {
        return settings.clone();
    };
    section
        .split('.')
        .try_fold(settings, |value, key| value.get(key))
        .cloned()
        .unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_overrides_layer_over_defaults() {
        let settings: Settings = serde_json::from_value(json!({
            "languages": { "python": { "server": {
                "args": ["--stdio", "--verbose"],
                "settings": { "basedpyright": { "analysis": { "diagnosticMode": "workspace" } } }
            } } }
        }))
        .unwrap();
        let config = settings.server_config(Language::Python).unwrap();
        assert_eq!(config.command, "basedpyright-langserver");
        assert_eq!(config.args, ["--stdio", "--verbose"]);
        assert_eq!(
            section(&config.settings, Some("basedpyright.analysis")),
            json!({ "typeCheckingMode": "standard", "diagnosticMode": "workspace" })
        );
        assert_eq!(
            section(&config.settings, Some("python.missing")),
            Value::Null
        );
        assert_eq!(settings.server_config(Language::Sql), None);
    }

    #[test]
    fn unknown_keys_are_rejected_instead_of_ignored() {
        let typo = json!({ "languages": { "python": { "server": { "comand": "x" } } } });
        assert!(serde_json::from_value::<Settings>(typo).is_err());
    }
}
