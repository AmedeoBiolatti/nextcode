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
//!   "restore_windows": true,
//!   "word_wrap": false,
//!   "inlay_hints": true,
//!   "format_on_save": false,
//!   "vim_mode": false,
//!   "vim": { "ctrl_keys": true, "in_commit_message": false },
//!   "languages": { "markdown": { "word_wrap": true }, "python": { "inlay_hints": false,
//!     "format_on_save": true,
//!     "formatter": { "command": "ruff", "args": ["format", "--stdin-filename", "{file}", "-"] },
//!     "server": {
//!     "command": "basedpyright-langserver", "args": ["--stdio"],
//!     "settings": { "basedpyright": { "analysis": { "typeCheckingMode": "strict" } } }
//! } } } },
//!   "debug": { "adapters": { "lldb": { "command": "/opt/codelldb/adapter/codelldb" },
//!     "python": { "command": "python3", "args": ["-m", "debugpy.adapter"] } } },
//!   "tests": { "discover_on_save": true, "pytest_args": ["-p", "no:randomly"],
//!     "cargo_test_args": ["--features", "slow-tests"] },
//!   "ports": { "auto_forward": true },
//!   "git": { "staging_area": false, "update_method": "merge", "inline_blame": false },
//!   "editor": { "minimap": { "enabled": true, "max_columns": 120, "side": "right" },
//!     "sticky_scroll": { "enabled": true, "max_lines": 5 } },
//!   "spell": { "enabled": true, "languages": ["en_US"], "check_code": true, "in_problems": false } }
//! ```
//!
//! A file's effective settings layer, later winning key by key: these, a
//! workspace file's `"settings"`, then its folder's own
//! `.next_code/settings.json` (see `Project::settings_for`,
//! [`Settings::layered`]).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};

use parking_lot::RwLock;
use serde::Deserialize;
use serde_json::{Value, json};
use syntax::Language;

#[derive(Debug, Clone, Deserialize)]
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
    /// A launch without a project reopens the windows and project tabs
    /// open when the editor last closed (else the welcome screen).
    pub restore_windows: bool,
    /// Wrap long lines at the editor's width (Alt+Z toggles per editor).
    pub word_wrap: bool,
    /// Show language servers' inlay hints (inferred types, parameter
    /// names) inside the text.
    pub inlay_hints: bool,
    /// Format files when saving them with Ctrl+S (see `formatter`).
    pub format_on_save: bool,
    pub languages: HashMap<String, LanguageSettings>,
    pub debug: DebugSettings,
    pub tests: TestSettings,
    pub ports: PortSettings,
    pub git: GitSettings,
    pub spell: SpellSettings,
    pub editor: EditorSettings,
    /// Vim emulation in file editors ("Toggle Vim Mode" flips it until
    /// the editor exits).
    pub vim_mode: bool,
    pub vim: VimSettings,
    /// The JSON these settings were read from (an empty object for
    /// defaults), so workspace and folder settings can layer over it.
    #[serde(skip)]
    pub raw: Arc<Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: None,
            semantic_highlighting: true,
            auto_close_brackets: true,
            restore_session: true,
            restore_windows: true,
            word_wrap: false,
            inlay_hints: true,
            format_on_save: false,
            languages: HashMap::new(),
            debug: DebugSettings::default(),
            tests: TestSettings::default(),
            ports: PortSettings::default(),
            git: GitSettings::default(),
            spell: SpellSettings::default(),
            editor: EditorSettings::default(),
            vim_mode: false,
            vim: VimSettings::default(),
            raw: Arc::new(json!({})),
        }
    }
}

/// `"vim"`: options of the Vim emulation (`"vim_mode"` turns it on).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VimSettings {
    /// Ctrl keys Vim uses in normal and visual mode (Ctrl+D/U/F/B/R/A/X/V)
    /// do the Vim thing; `false` leaves them to the IDE.
    pub ctrl_keys: bool,
    /// Vim also edits the Commit tool window's message.
    pub in_commit_message: bool,
}

impl Default for VimSettings {
    fn default() -> Self {
        Self {
            ctrl_keys: true,
            in_commit_message: false,
        }
    }
}

/// `"editor"`: the code editor's chrome.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EditorSettings {
    pub minimap: MinimapSettings,
    pub sticky_scroll: StickyScrollSettings,
}

/// `"editor.minimap"`: the scaled-down document beside file editors.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MinimapSettings {
    pub enabled: bool,
    /// Columns drawn per line (longer lines are cut).
    pub max_columns: usize,
    pub side: MinimapSide,
}

impl Default for MinimapSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_columns: 120,
            side: MinimapSide::Right,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MinimapSide {
    Left,
    #[default]
    Right,
}

/// `"editor.sticky_scroll"`: headers of the scopes around the first
/// visible line pinned at the top of the text.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StickyScrollSettings {
    pub enabled: bool,
    pub max_lines: usize,
}

impl Default for StickyScrollSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_lines: 5,
        }
    }
}

/// `"debug"`: how debug adapters start.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DebugSettings {
    /// The adapter command by kind (`"python"`, `"lldb"`), run where the
    /// project lives instead of the one found automatically.
    pub adapters: HashMap<String, AdapterCommandSetting>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterCommandSetting {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// `"tests"`: the Tests panel.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TestSettings {
    /// Rediscover tests after saving a test file (debounced).
    pub discover_on_save: bool,
    /// Extra pytest arguments for discovery and runs.
    pub pytest_args: Vec<String>,
    /// Extra `cargo test` arguments (before `--`) for discovery and runs.
    pub cargo_test_args: Vec<String>,
}

impl Default for TestSettings {
    fn default() -> Self {
        Self {
            discover_on_save: true,
            pytest_args: Vec::new(),
            cargo_test_args: Vec::new(),
        }
    }
}

/// `"ports"`: port forwarding for WSL and dev container projects.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PortSettings {
    /// Forward ports that start listening where the project lives (in a
    /// container also those already listening when it opens).
    pub auto_forward: bool,
}

impl Default for PortSettings {
    fn default() -> Self {
        Self { auto_forward: true }
    }
}

/// `"git"`: the Commit window, Update Project and the editor's blame.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitSettings {
    /// Show git's index (Staged / Unstaged groups with stage and unstage),
    /// like PyCharm's "Enable staging area"; else commits take the checked
    /// files directly.
    pub staging_area: bool,
    /// How Update Project brings in the upstream's commits.
    pub update_method: UpdateMethod,
    /// Show the author, age and summary of the commit that last changed
    /// the cursor's line after its end (VS Code's inline blame).
    pub inline_blame: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateMethod {
    #[default]
    Merge,
    Rebase,
}

/// `"spell"`: spell checking in editors (see `editor::spell`).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpellSettings {
    pub enabled: bool,
    /// Hunspell dictionaries: `en_US` is built in, others are read from
    /// `<config>/next_code/dictionaries/<language>.aff` and `.dic`.
    pub languages: Vec<String>,
    /// Check comments, docstrings and strings of code; off, only Markdown,
    /// plain text and commit messages are checked.
    pub check_code: bool,
    /// List the open files' unknown words in the Problems panel.
    pub in_problems: bool,
}

impl Default for SpellSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            languages: vec![spell::BUILT_IN.to_owned()],
            check_code: true,
            in_problems: false,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LanguageSettings {
    pub server: ServerOverrides,
    /// Overrides `word_wrap` for the language (Markdown wraps by default).
    pub word_wrap: Option<bool>,
    /// Overrides `inlay_hints` for the language.
    pub inlay_hints: Option<bool>,
    /// Overrides `format_on_save` for the language.
    pub format_on_save: Option<bool>,
    /// How to format the language's files (default: the language server;
    /// for Python, `ruff format` when it is installed).
    pub formatter: Option<FormatterSetting>,
    /// Overrides `spell.enabled` for the language.
    pub spell: Option<bool>,
}

impl Settings {
    /// Whether editors of `language` (`None`: plain text) start wrapped.
    pub fn word_wrap(&self, language: Option<Language>) -> bool {
        let configured = language
            .and_then(|language| self.languages.get(language.id()))
            .and_then(|settings| settings.word_wrap);
        configured.unwrap_or(self.word_wrap || language == Some(Language::Markdown))
    }

    /// Whether editors of `language` show inlay hints.
    pub fn inlay_hints(&self, language: Option<Language>) -> bool {
        language
            .and_then(|language| self.languages.get(language.id()))
            .and_then(|settings| settings.inlay_hints)
            .unwrap_or(self.inlay_hints)
    }

    /// Whether saving a `language` file formats it first.
    pub fn format_on_save(&self, language: Option<Language>) -> bool {
        language
            .and_then(|language| self.languages.get(language.id()))
            .and_then(|settings| settings.format_on_save)
            .unwrap_or(self.format_on_save)
    }

    /// Whether editors of `language` (`None`: plain text) are spell
    /// checked.
    pub fn spell(&self, language: Option<Language>) -> bool {
        language
            .and_then(|language| self.languages.get(language.id()))
            .and_then(|settings| settings.spell)
            .unwrap_or(self.spell.enabled)
    }

    /// How `language` files are formatted.
    pub fn formatter(&self, language: Language) -> Formatter {
        let configured = self
            .languages
            .get(language.id())
            .and_then(|settings| settings.formatter.clone());
        match configured {
            Some(FormatterSetting::Named(FormatterName::LanguageServer)) => Formatter::Server,
            Some(FormatterSetting::External(external)) => Formatter::External {
                command: external.command,
                args: external.args,
                fallback: false,
            },
            None if language == Language::Python => Formatter::External {
                command: "ruff".into(),
                args: ["format", "--stdin-filename", "{file}", "-"]
                    .map(str::to_owned)
                    .into(),
                fallback: true,
            },
            None => Formatter::Server,
        }
    }
}

/// `"formatter"`: `"language_server"`, or a program reading the text on
/// stdin and writing the formatted text to stdout.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum FormatterSetting {
    Named(FormatterName),
    External(ExternalFormatter),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormatterName {
    LanguageServer,
}

/// Run where the project lives, in its root; `{file}` in `args` becomes
/// the file's path.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalFormatter {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// A resolved formatter choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Formatter {
    /// The language server's `textDocument/formatting`.
    Server,
    /// A program; with `fallback`, the language server formats when the
    /// program is not installed.
    External {
        command: String,
        args: Vec<String>,
        fallback: bool,
    },
}

/// The settings as of the last `theme::refresh` (the GUI re-reads them when
/// `settings.json` changes).
static CURRENT: LazyLock<RwLock<Arc<Settings>>> = LazyLock::new(Default::default);

pub fn current() -> Arc<Settings> {
    CURRENT.read().clone()
}

pub fn set_current(settings: Settings) {
    *CURRENT.write() = Arc::new(settings);
}

/// User overrides; unset fields keep the built-in default.
#[derive(Debug, Clone, Default, Deserialize)]
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
        let parsed = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str::<Value>(&text)
                .map_err(|err| err.to_string())
                .and_then(|raw| Self::default().layered(&[&raw])),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err.to_string()),
        };
        match parsed {
            Ok(settings) => (settings, None),
            Err(err) => (Self::default(), Some(format!("{}: {err}", path.display()))),
        }
    }

    /// These settings with `layers` merged over their JSON in order (later
    /// layers win; `null` layers are skipped): workspace file settings,
    /// then a folder's `.next_code/settings.json` (see `Project::settings`).
    pub fn layered(&self, layers: &[&Value]) -> Result<Self, String> {
        let mut raw = match &*self.raw {
            Value::Object(_) => (*self.raw).clone(),
            _ => json!({}),
        };
        for layer in layers.iter().filter(|layer| !layer.is_null()) {
            if !layer.is_object() {
                return Err("settings must be a JSON object".into());
            }
            merge(&mut raw, (*layer).clone());
        }
        let mut settings: Self =
            serde_json::from_value(raw.clone()).map_err(|err| err.to_string())?;
        settings.raw = Arc::new(raw);
        Ok(settings)
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
    fn formatters_and_format_on_save_resolve_per_language() {
        let defaults = Settings::default();
        assert_eq!(defaults.formatter(Language::Rust), Formatter::Server);
        assert!(matches!(
            defaults.formatter(Language::Python),
            Formatter::External { ref command, fallback: true, .. } if command == "ruff"
        ));
        assert!(!defaults.format_on_save(Some(Language::Rust)));
        let settings: Settings = serde_json::from_value(json!({
            "format_on_save": true,
            "languages": {
                "python": { "formatter": "language_server", "format_on_save": false },
                "rust": { "formatter": { "command": "rustfmt", "args": ["--emit", "stdout"] } },
            }
        }))
        .unwrap();
        assert_eq!(settings.formatter(Language::Python), Formatter::Server);
        assert_eq!(
            settings.formatter(Language::Rust),
            Formatter::External {
                command: "rustfmt".into(),
                args: vec!["--emit".into(), "stdout".into()],
                fallback: false,
            }
        );
        assert!(!settings.format_on_save(Some(Language::Python)));
        assert!(settings.format_on_save(Some(Language::Rust)));
        let unknown = json!({ "languages": { "rust": { "formatter": "prettier" } } });
        assert!(serde_json::from_value::<Settings>(unknown).is_err());
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
