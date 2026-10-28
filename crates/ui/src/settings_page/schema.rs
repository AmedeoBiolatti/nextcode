//! What the Settings page shows: every user setting with its category,
//! title, description, control and default. The keys are `settings.json`'s
//! dotted paths; `tests` checks each one against [`Settings`]'s
//! deserializer, so the page cannot offer a key the file would reject.

use serde_json::{Value, json};
use syntax::Language;

use crate::settings::Settings;

/// The page's sections, in sidebar order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Appearance,
    Editor,
    Languages,
    Vim,
    Spelling,
    Git,
    Tests,
    Remote,
    Notebooks,
    Workflows,
    System,
}

impl Category {
    pub const ALL: [Self; 11] = [
        Self::Appearance,
        Self::Editor,
        Self::Languages,
        Self::Vim,
        Self::Spelling,
        Self::Git,
        Self::Tests,
        Self::Remote,
        Self::Notebooks,
        Self::Workflows,
        Self::System,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Editor => "Editor",
            Self::Languages => "Languages",
            Self::Vim => "Vim",
            Self::Spelling => "Spelling",
            Self::Git => "Git",
            Self::Tests => "Tests",
            Self::Remote => "WSL & Containers",
            Self::Notebooks => "Notebooks",
            Self::Workflows => "Workflows",
            Self::System => "Startup & Windows",
        }
    }

    /// A one-color icon (`assets/icons/<name>.svg`).
    pub fn icon(self) -> &'static str {
        match self {
            Self::Appearance => "palette",
            Self::Editor => "file",
            Self::Languages => "code",
            Self::Vim => "keyboard",
            Self::Spelling => "check",
            Self::Git => "branch",
            Self::Tests => "flask",
            Self::Remote => "cloud",
            Self::Notebooks => "book",
            Self::Workflows => "workflow",
            Self::System => "window",
        }
    }
}

/// How a setting is edited.
#[derive(Debug, Clone, PartialEq)]
pub enum Control {
    Toggle,
    /// One of these values, with their labels.
    Choice(Vec<(Value, &'static str)>),
    Number {
        min: u64,
        max: u64,
    },
    /// A list of strings.
    List {
        placeholder: &'static str,
    },
    Theme,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Setting {
    /// Dotted path in `settings.json`.
    pub key: String,
    pub category: Category,
    pub title: String,
    pub description: String,
    pub control: Control,
    /// The value in effect when the key is not set; `null` for a
    /// language's override, which then follows `inherited`.
    pub default: Value,
    /// What an unset override resolves to (the global setting, or the
    /// language's own default such as Markdown wrapping).
    pub inherited: Option<bool>,
}

impl Setting {
    /// Whether `needle` (lowercase) is in its title, description or key.
    pub fn matches(&self, needle: &str) -> bool {
        [&self.title, &self.description, &self.key]
            .iter()
            .any(|text| text.to_lowercase().contains(needle))
    }
}

fn setting(
    key: &str,
    category: Category,
    title: &str,
    description: &str,
    control: Control,
    default: Value,
) -> Setting {
    Setting {
        key: key.to_owned(),
        category,
        title: title.to_owned(),
        description: description.to_owned(),
        control,
        default,
        inherited: None,
    }
}

/// The global settings (the Languages category is per language, see
/// [`language_settings`]).
pub fn settings() -> Vec<Setting> {
    use Category::*;
    use Control::*;
    let toggle = |key, category, title, description, default: bool| {
        setting(key, category, title, description, Toggle, default.into())
    };
    vec![
        setting(
            "theme",
            Appearance,
            "Theme",
            "Colors of the UI, the terminal and syntax highlighting. User themes are JSON files in the themes folder next to settings.json.",
            Theme,
            crate::theme::DEFAULT.into(),
        ),
        toggle(
            "semantic_highlighting",
            Appearance,
            "Semantic highlighting",
            "Layer language servers' semantic tokens over syntax highlighting.",
            true,
        ),
        toggle(
            "word_wrap",
            Editor,
            "Word wrap",
            "Wrap long lines at the editor's width (Alt+Z toggles it per editor).",
            false,
        ),
        toggle(
            "auto_close_brackets",
            Editor,
            "Auto-close brackets",
            "Typing an opening bracket or quote inserts its closer.",
            true,
        ),
        toggle(
            "inlay_hints",
            Editor,
            "Inlay hints",
            "Show language servers' inferred types and parameter names inside the text.",
            true,
        ),
        toggle(
            "format_on_save",
            Editor,
            "Format on save",
            "Format files when saving them with Ctrl+S.",
            false,
        ),
        toggle(
            "editor.minimap.enabled",
            Editor,
            "Minimap",
            "A scaled-down view of the document beside file editors.",
            true,
        ),
        setting(
            "editor.minimap.side",
            Editor,
            "Minimap side",
            "Which side of the editor the minimap is drawn on.",
            Choice(vec![(json!("left"), "Left"), (json!("right"), "Right")]),
            json!("right"),
        ),
        setting(
            "editor.minimap.max_columns",
            Editor,
            "Minimap columns",
            "Columns drawn per line in the minimap; longer lines are cut.",
            Number { min: 20, max: 400 },
            json!(120),
        ),
        toggle(
            "editor.sticky_scroll.enabled",
            Editor,
            "Sticky scroll",
            "Pin the headers of the scopes around the first visible line at the top of the text.",
            true,
        ),
        setting(
            "editor.sticky_scroll.max_lines",
            Editor,
            "Sticky scroll lines",
            "The most scope headers pinned at once.",
            Number { min: 1, max: 20 },
            json!(5),
        ),
        toggle(
            "vim_mode",
            Vim,
            "Vim mode",
            "Vim emulation in file editors (\"Toggle Vim Mode\" flips it until the editor exits).",
            false,
        ),
        toggle(
            "vim.ctrl_keys",
            Vim,
            "Vim Ctrl keys",
            "Ctrl+D/U/F/B/R/A/X/V do the Vim thing in normal and visual mode; off leaves them to the IDE.",
            true,
        ),
        toggle(
            "vim.in_commit_message",
            Vim,
            "Vim in commit messages",
            "Vim also edits the Commit tool window's message.",
            false,
        ),
        toggle(
            "spell.enabled",
            Spelling,
            "Spell checking",
            "Underline unknown words in editors.",
            true,
        ),
        toggle(
            "spell.check_code",
            Spelling,
            "Check code",
            "Check comments, docstrings and strings of code; off, only Markdown, plain text and commit messages are checked.",
            true,
        ),
        toggle(
            "spell.in_problems",
            Spelling,
            "Show in Problems",
            "List the open files' unknown words in the Problems panel.",
            false,
        ),
        setting(
            "spell.languages",
            Spelling,
            "Dictionaries",
            "Hunspell dictionaries: en_US is built in, others are read from dictionaries/<language>.aff and .dic next to settings.json.",
            List {
                placeholder: "Add a dictionary, e.g. de_DE",
            },
            json!([spell::BUILT_IN]),
        ),
        toggle(
            "git.staging_area",
            Git,
            "Staging area",
            "Show git's index as Staged and Unstaged groups with stage and unstage; off, commits take the checked files directly.",
            false,
        ),
        setting(
            "git.update_method",
            Git,
            "Update method",
            "How Update Project brings in the upstream's commits.",
            Choice(vec![(json!("merge"), "Merge"), (json!("rebase"), "Rebase")]),
            json!("merge"),
        ),
        toggle(
            "git.inline_blame",
            Git,
            "Inline blame",
            "Show the author, age and summary of the commit that last changed the cursor's line after its end.",
            false,
        ),
        setting(
            "git.credential_hosts",
            Git,
            "Credential hosts",
            "Hosts whose credentials a WSL or dev container project may ask of this machine, besides the hosts of its git remotes.",
            List {
                placeholder: "Add a host, e.g. github.com",
            },
            json!([]),
        ),
        toggle(
            "tests.discover_on_save",
            Tests,
            "Discover on save",
            "Rediscover tests after saving a test file.",
            true,
        ),
        setting(
            "tests.pytest_args",
            Tests,
            "pytest arguments",
            "Extra pytest arguments for discovery and runs.",
            List {
                placeholder: "Add an argument, e.g. -p no:randomly",
            },
            json!([]),
        ),
        setting(
            "tests.cargo_test_args",
            Tests,
            "cargo test arguments",
            "Extra `cargo test` arguments (before `--`) for discovery and runs.",
            List {
                placeholder: "Add an argument, e.g. --all-features",
            },
            json!([]),
        ),
        toggle(
            "ports.auto_forward",
            Remote,
            "Forward ports automatically",
            "Forward ports that start listening where the project lives (in a container also those already listening when it opens).",
            true,
        ),
        toggle(
            "devcontainer.forward_git_credentials",
            Remote,
            "Lend git credentials",
            "Lend this machine's git credentials, ssh-agent and user.name/user.email to WSL and container projects.",
            true,
        ),
        toggle(
            "devcontainer.ssh_agent_forwarding",
            Remote,
            "Forward ssh-agent",
            "With git credentials lent, also relay this machine's ssh-agent to the project's machine.",
            true,
        ),
        setting(
            "notebook.output_max_lines",
            Notebooks,
            "Output lines",
            "Lines of a cell's text output shown before \"Show more\".",
            Number {
                min: 10,
                max: 100_000,
            },
            json!(200),
        ),
        setting(
            "workflows.folders",
            Workflows,
            "Workflow folders",
            "More folders whose *.yaml workflows are listed (absolute, or starting with ~).",
            List {
                placeholder: "Add a folder",
            },
            json!([]),
        ),
        toggle(
            "restore_session",
            System,
            "Restore editor tabs",
            "Reopen a project's editor tabs and splits where they were left.",
            true,
        ),
        toggle(
            "restore_windows",
            System,
            "Restore windows",
            "A launch without a project reopens the windows and project tabs open when the editor last closed.",
            true,
        ),
        setting(
            "keep_running",
            System,
            "Keep running",
            "Keep the editor running after its last window closes, so the next launch is instant. Auto does so except for builds run from a cargo target directory.",
            Choice(vec![
                (json!("auto"), "Auto"),
                (json!(true), "Always"),
                (json!(false), "Never"),
            ]),
            json!("auto"),
        ),
    ]
}

/// The Languages category for `language`: overrides of global settings
/// (unset follows them, as resolved by `current`) and its language
/// server, when it has one.
pub fn language_settings(language: Language, current: &Settings) -> Vec<Setting> {
    let id = language.id();
    let name = language.name();
    // `current` as if `language` had no overrides.
    let mut base = current.clone();
    base.languages.remove(id);
    let override_of = |field: &str, title: &str, global: &str, inherited: bool| Setting {
        inherited: Some(inherited),
        ..setting(
            &format!("languages.{id}.{field}"),
            Category::Languages,
            title,
            &format!("For {name} files; Default follows {global}."),
            Control::Choice(vec![(json!(true), "On"), (json!(false), "Off")]),
            Value::Null,
        )
    };
    let mut settings = vec![
        override_of(
            "word_wrap",
            "Word wrap",
            "Editor › Word wrap",
            base.word_wrap(Some(language)),
        ),
        override_of(
            "inlay_hints",
            "Inlay hints",
            "Editor › Inlay hints",
            base.inlay_hints(Some(language)),
        ),
        override_of(
            "format_on_save",
            "Format on save",
            "Editor › Format on save",
            base.format_on_save(Some(language)),
        ),
        override_of(
            "spell",
            "Spell checking",
            "Spelling › Spell checking",
            base.spell(Some(language)),
        ),
    ];
    if let Some(server) = Settings::default().server_config(language) {
        settings.push(Setting {
            inherited: Some(server.enabled),
            ..setting(
                &format!("languages.{id}.server.enabled"),
                Category::Languages,
                "Language server",
                &format!(
                    "Start {name}'s language server ({}). Its command, arguments and settings are edited in settings.json.",
                    server.command
                ),
                Control::Choice(vec![(json!(true), "On"), (json!(false), "Off")]),
                Value::Null,
            )
        });
    }
    settings
}

/// The value at dotted `key` in `raw`, if set.
pub fn get<'a>(raw: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.').try_fold(raw, |value, key| value.get(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `{"a": {"b": value}}` for key `a.b`.
    fn nested(key: &str, value: Value) -> Value {
        key.rsplit('.')
            .fold(value, |inner, part| json!({ part: inner }))
    }

    /// A value of the setting's type other than its default.
    fn sample(setting: &Setting) -> Value {
        match &setting.control {
            Control::Toggle => json!(!setting.default.as_bool().unwrap()),
            Control::Choice(choices) => choices
                .iter()
                .map(|(value, _)| value.clone())
                .find(|value| *value != setting.default)
                .unwrap(),
            Control::Number { min, .. } => json!(min),
            Control::List { .. } => json!(["sample"]),
            Control::Theme => json!("Light"),
        }
    }

    #[test]
    fn every_key_is_accepted_by_settings_json() {
        let all = settings().into_iter().chain(
            Language::ALL
                .into_iter()
                .flat_map(|language| language_settings(language, &Settings::default())),
        );
        for setting in all {
            let value = sample(&setting);
            let layer = nested(&setting.key, value.clone());
            let layered = Settings::default().layered(&[&layer]);
            assert!(layered.is_ok(), "{}: {:?}", setting.key, layered.err());
            assert_eq!(get(&layered.unwrap().raw, &setting.key), Some(&value));
            if !setting.default.is_null() {
                let layer = nested(&setting.key, setting.default.clone());
                assert!(
                    Settings::default().layered(&[&layer]).is_ok(),
                    "{} default",
                    setting.key
                );
            }
        }
        // A misspelt key is refused, so the check above means something.
        assert!(
            Settings::default()
                .layered(&[&nested("git.staging_areas", json!(true))])
                .is_err()
        );
    }

    #[test]
    fn language_defaults_follow_the_global_settings() {
        let inherited = |language: Language, current: &Settings, field: &str| {
            language_settings(language, current)
                .into_iter()
                .find(|setting| setting.key.ends_with(field))
                .and_then(|setting| setting.inherited)
        };
        let defaults = Settings::default();
        // Markdown wraps even when the global setting does not.
        assert_eq!(
            inherited(Language::Markdown, &defaults, ".word_wrap"),
            Some(true)
        );
        assert_eq!(
            inherited(Language::Python, &defaults, ".word_wrap"),
            Some(false)
        );
        // A language's own override does not count as its default.
        let current = defaults
            .layered(&[&json!({ "inlay_hints": false,
                "languages": { "python": { "inlay_hints": true } } })])
            .unwrap();
        assert_eq!(
            inherited(Language::Python, &current, ".inlay_hints"),
            Some(false)
        );
        // Only languages with a server get its switch.
        assert!(
            language_settings(Language::Python, &defaults)
                .iter()
                .any(|setting| setting.key == "languages.python.server.enabled")
        );
    }

    #[test]
    fn defaults_match_the_built_in_settings() {
        let defaults = Settings::default();
        let check = |key: &str, actual: Value| {
            let setting = settings().into_iter().find(|s| s.key == key).unwrap();
            assert_eq!(setting.default, actual, "{key}");
        };
        check("word_wrap", defaults.word_wrap.into());
        check("inlay_hints", defaults.inlay_hints.into());
        check("format_on_save", defaults.format_on_save.into());
        check("auto_close_brackets", defaults.auto_close_brackets.into());
        check(
            "semantic_highlighting",
            defaults.semantic_highlighting.into(),
        );
        check("restore_session", defaults.restore_session.into());
        check("restore_windows", defaults.restore_windows.into());
        check("vim_mode", defaults.vim_mode.into());
        check("vim.ctrl_keys", defaults.vim.ctrl_keys.into());
        check(
            "vim.in_commit_message",
            defaults.vim.in_commit_message.into(),
        );
        check(
            "editor.minimap.enabled",
            defaults.editor.minimap.enabled.into(),
        );
        check(
            "editor.minimap.max_columns",
            defaults.editor.minimap.max_columns.into(),
        );
        check(
            "editor.sticky_scroll.enabled",
            defaults.editor.sticky_scroll.enabled.into(),
        );
        check(
            "editor.sticky_scroll.max_lines",
            defaults.editor.sticky_scroll.max_lines.into(),
        );
        check("spell.enabled", defaults.spell.enabled.into());
        check("spell.check_code", defaults.spell.check_code.into());
        check("spell.in_problems", defaults.spell.in_problems.into());
        check("spell.languages", json!(defaults.spell.languages));
        check("git.staging_area", defaults.git.staging_area.into());
        check("git.inline_blame", defaults.git.inline_blame.into());
        check(
            "tests.discover_on_save",
            defaults.tests.discover_on_save.into(),
        );
        check("ports.auto_forward", defaults.ports.auto_forward.into());
        check(
            "devcontainer.forward_git_credentials",
            defaults.devcontainer.forward_git_credentials.into(),
        );
        check(
            "devcontainer.ssh_agent_forwarding",
            defaults.devcontainer.ssh_agent_forwarding.into(),
        );
        check(
            "notebook.output_max_lines",
            defaults.notebook.output_max_lines.into(),
        );
    }
}
