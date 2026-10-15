//! User settings and language-server resolution.
//!
//! `settings.json` lives in the platform config dir (`%APPDATA%\next_code` on
//! Windows, `~/.config/next_code` on Linux). Every field is optional and is
//! layered over built-in defaults:
//!
//! ```json
//! { "theme": "Light",
//!   "semantic_highlighting": true,
//!   "auto_close_brackets": true,
//!   "restore_session": true,
//!   "word_wrap": false,
//!   "languages": { "markdown": { "word_wrap": true }, "python": { "server": {
//!     "command": "basedpyright-langserver", "args": ["--stdio"],
//!     "settings": { "basedpyright": { "analysis": { "typeCheckingMode": "strict" } } }
//! } } } }
//! ```

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use serde_json::{Value, json};
use syntax::Language;

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// The theme's name (see `theme`); `None` for the default.
    pub theme: Option<String>,
    /// Layer language servers' semantic tokens over syntax highlighting.
    pub semantic_highlighting: bool,
    /// Typing an opening bracket or quote inserts its closer.
    pub auto_close_brackets: bool,
    /// Reopen a project's editor tabs and splits where they were left.
    pub restore_session: bool,
    /// Wrap long lines at the editor's width (Alt+Z toggles per editor).
    pub word_wrap: bool,
    pub languages: HashMap<String, LanguageSettings>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: None,
            semantic_highlighting: true,
            auto_close_brackets: true,
            restore_session: true,
            word_wrap: false,
            languages: HashMap::new(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LanguageSettings {
    pub server: ServerOverrides,
    /// Overrides `word_wrap` for the language (Markdown wraps by default).
    pub word_wrap: Option<bool>,
}

impl Settings {
    /// Whether editors of `language` (`None`: plain text) start wrapped.
    pub fn word_wrap(&self, language: Option<Language>) -> bool {
        let configured = language
            .and_then(|language| self.languages.get(language.id()))
            .and_then(|settings| settings.word_wrap);
        configured.unwrap_or(self.word_wrap || language == Some(Language::Markdown))
    }
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
        match Self::path() {
            Some(path) => Self::load_from(&path),
            None => (Self::default(), None),
        }
    }

    pub fn load_from(path: &Path) -> (Self, Option<String>) {
        match std::fs::read_to_string(path) {
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
            _ => return None,
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

/// Sets `"theme"` in the settings file at `path`, keeping everything else;
/// creates the file if missing.
pub fn write_theme(path: &Path, name: &str) -> Result<(), String> {
    let fail = |err: &dyn std::fmt::Display| format!("{}: {err}", path.display());
    let mut settings = match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => json!({}),
        Ok(text) => serde_json::from_str::<Value>(&text).map_err(|err| fail(&err))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(err) => return Err(fail(&err)),
    };
    let Some(object) = settings.as_object_mut() else {
        return Err(fail(&"not a JSON object"));
    };
    object.insert("theme".into(), name.into());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|err| fail(&err))?;
    }
    let text = serde_json::to_string_pretty(&settings).map_err(|err| fail(&err))?;
    std::fs::write(path, text + "\n").map_err(|err| fail(&err))
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
        assert!(settings.semantic_highlighting);
        let off: Settings =
            serde_json::from_value(json!({ "semantic_highlighting": false })).unwrap();
        assert!(!off.semantic_highlighting);
        assert!(!settings.word_wrap(Some(Language::Python)));
        assert!(settings.word_wrap(Some(Language::Markdown)));
        let wrapped: Settings = serde_json::from_value(json!({
            "word_wrap": true,
            "languages": { "python": { "word_wrap": false } }
        }))
        .unwrap();
        assert!(wrapped.word_wrap(None));
        assert!(!wrapped.word_wrap(Some(Language::Python)));
    }

    #[test]
    fn unknown_keys_are_rejected_instead_of_ignored() {
        let typo = json!({ "languages": { "python": { "server": { "comand": "x" } } } });
        assert!(serde_json::from_value::<Settings>(typo).is_err());
    }

    #[test]
    fn writing_the_theme_keeps_other_settings() {
        let dir =
            std::env::temp_dir().join(format!("next_code-test-settings-{}", std::process::id()));
        let path = dir.join("settings.json");
        write_theme(&path, "Light").unwrap();
        let (settings, problem) = Settings::load_from(&path);
        assert_eq!((settings.theme.as_deref(), problem), (Some("Light"), None));

        let languages = json!({ "python": { "server": { "enabled": false } } });
        std::fs::write(
            &path,
            json!({ "languages": languages, "theme": "Light" }).to_string(),
        )
        .unwrap();
        write_theme(&path, "Dark (custom)").unwrap();
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            written,
            json!({ "languages": languages, "theme": "Dark (custom)" })
        );

        std::fs::write(&path, "{ broken").unwrap();
        assert!(write_theme(&path, "Light").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ broken");
        std::fs::remove_dir_all(dir).ok();
    }
}
