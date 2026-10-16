//! The menu bar's contents. Built-in menus are used unless the platform
//! config dir has `next_code/menus.json`, which replaces them entirely
//! (`app::CustomizeMenus` writes the built-in menus there to start from):
//!
//! ```json
//! { "menus": [
//!     { "label": "File", "items": [
//!         "app::OpenProject",
//!         { "label": "Recent Projects", "list": "recent_projects" },
//!         "-",
//!         { "command": "editor::Save", "label": "Save File" },
//!         { "label": "More", "items": ["app::About"] }
//!     ] }
//! ] }
//! ```
//!
//! An item is a command name (any action the command palette runs), `"-"`
//! for a separator, a command with its own label, a submenu, or a list
//! filled in when the menu opens.

use std::{path::PathBuf, rc::Rc, time::SystemTime};

use gpui::{App, Global};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MenuBar {
    pub menus: Vec<Menu>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Menu {
    pub label: String,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Item {
    /// A command name, or [`SEPARATOR`].
    Plain(String),
    Command {
        command: String,
        label: String,
    },
    Submenu {
        label: String,
        items: Vec<Item>,
    },
    List {
        label: String,
        list: List,
    },
}

/// Lists computed when their menu opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum List {
    RecentProjects,
}

pub const SEPARATOR: &str = "-";

fn plain(command: &str) -> Item {
    Item::Plain(command.to_owned())
}

fn labeled(command: &str, label: &str) -> Item {
    Item::Command {
        command: command.to_owned(),
        label: label.to_owned(),
    }
}

fn menu(label: &str, items: Vec<Item>) -> Menu {
    Menu {
        label: label.to_owned(),
        items,
    }
}

impl Default for MenuBar {
    fn default() -> Self {
        let separator = || plain(SEPARATOR);
        Self {
            menus: vec![
                menu(
                    "File",
                    vec![
                        plain("app::OpenProject"),
                        plain("app::OpenProjectInNewWindow"),
                        plain("app::OpenRecent"),
                        Item::List {
                            label: "Recent Projects".into(),
                            list: List::RecentProjects,
                        },
                        separator(),
                        plain("workspace::ReopenInContainer"),
                        separator(),
                        plain("editor::Save"),
                        plain("workspace::SaveAll"),
                        separator(),
                        plain("workspace::CloseTab"),
                        plain("app::CloseProject"),
                        separator(),
                        plain("app::OpenSettings"),
                        plain("app::CustomizeMenus"),
                        plain("app::SelectTheme"),
                        plain("app::CustomizeTheme"),
                        separator(),
                        plain("app::Quit"),
                    ],
                ),
                menu(
                    "Edit",
                    vec![
                        plain("editor::Undo"),
                        plain("editor::Redo"),
                        separator(),
                        plain("editor::Cut"),
                        plain("editor::Copy"),
                        plain("editor::Paste"),
                        separator(),
                        plain("editor::Find"),
                        plain("editor::FindReplace"),
                        plain("workspace::ToggleSearch"),
                        separator(),
                        plain("editor::ToggleLineComment"),
                        plain("editor::ToggleBlockComment"),
                        plain("editor::Indent"),
                        plain("editor::Outdent"),
                        separator(),
                        plain("editor::ShowCompletions"),
                    ],
                ),
                menu(
                    "Selection",
                    vec![
                        plain("editor::SelectAll"),
                        separator(),
                        plain("editor::CopyLineUp"),
                        plain("editor::CopyLineDown"),
                        plain("editor::MoveLineUp"),
                        plain("editor::MoveLineDown"),
                        plain("editor::DeleteLine"),
                        plain("editor::InsertLineAbove"),
                        plain("editor::InsertLineBelow"),
                        separator(),
                        plain("editor::AddCursorAbove"),
                        plain("editor::AddCursorBelow"),
                        plain("editor::SelectNextOccurrence"),
                        plain("editor::SelectAllOccurrences"),
                    ],
                ),
                menu(
                    "View",
                    vec![
                        plain("app::ShowCommandPalette"),
                        separator(),
                        plain("workspace::ToggleFiles"),
                        labeled("workspace::ToggleSearch", "Search Panel"),
                        labeled("workspace::ToggleCommit", "Commit"),
                        labeled("workspace::ToggleGitLog", "Git"),
                        plain("workspace::TogglePython"),
                        plain("workspace::ToggleProblems"),
                        labeled("workspace::TogglePorts", "Ports"),
                        labeled("workspace::ToggleTests", "Tests"),
                        labeled("workspace::ToggleCallHierarchy", "Call Hierarchy"),
                        labeled("workspace::ToggleOutline", "Outline"),
                        labeled("debugger::ToggleDebugPanel", "Debug"),
                        labeled("debugger::ToggleDebugConsole", "Debug Console"),
                        plain("workspace::ToggleTerminal"),
                        labeled("workspace::ToggleWorkflows", "Workflows"),
                        plain("workspace::ToggleBottomDock"),
                        separator(),
                        plain("workspace::SplitRight"),
                        plain("workspace::SplitDown"),
                        plain("workspace::FocusNextPane"),
                        plain("workspace::MoveTabToNextPane"),
                        plain("workspace::MoveTabToPreviousPane"),
                        plain("workspace::ClosePane"),
                        separator(),
                        labeled("editor::ToggleWordWrap", "Word Wrap"),
                        labeled("editor::ToggleInlayHints", "Inlay Hints"),
                        Item::Submenu {
                            label: "Folding".into(),
                            items: vec![
                                plain("editor::Fold"),
                                plain("editor::Unfold"),
                                plain("editor::ToggleFold"),
                                plain("editor::FoldRecursively"),
                                plain("editor::UnfoldRecursively"),
                                separator(),
                                plain("editor::FoldAll"),
                                plain("editor::UnfoldAll"),
                            ],
                        },
                        separator(),
                        plain("workspace::SaveLayoutAsDefault"),
                        plain("workspace::ResetLayout"),
                        separator(),
                        plain("app::NextProject"),
                        plain("app::PreviousProject"),
                    ],
                ),
                menu(
                    "Go",
                    vec![
                        plain("workspace::ToggleFileFinder"),
                        plain("editor::GoToSymbol"),
                        plain("workspace::GoToWorkspaceSymbol"),
                        separator(),
                        plain("workspace::GoBack"),
                        plain("workspace::GoForward"),
                        separator(),
                        plain("editor::GoToDefinition"),
                        plain("editor::GoToDeclaration"),
                        plain("editor::GoToTypeDefinition"),
                        plain("editor::GoToImplementation"),
                        plain("editor::FindReferences"),
                        plain("editor::ShowCallHierarchy"),
                        plain("editor::GoToLine"),
                        plain("editor::JumpToBracket"),
                        separator(),
                        plain("editor::NextDiagnostic"),
                        plain("editor::PrevDiagnostic"),
                    ],
                ),
                menu(
                    "Refactor",
                    vec![
                        plain("editor::RenameSymbol"),
                        plain("editor::QuickFix"),
                        separator(),
                        plain("editor::FormatDocument"),
                        plain("editor::FormatSelection"),
                    ],
                ),
                menu(
                    "Run",
                    vec![
                        labeled("run::RunConfiguration", "Run"),
                        labeled("run::DebugConfiguration", "Debug"),
                        plain("run::SelectConfiguration"),
                        plain("run::EditConfigurations"),
                        separator(),
                        plain("run::RunAllTests"),
                        plain("run::RunFailedTests"),
                        plain("python::RunTestFile"),
                        plain("python::RunTestAtCursor"),
                        labeled("workspace::ToggleTests", "Show Tests"),
                        separator(),
                        plain("python::RunFile"),
                        separator(),
                        plain("python::Sync"),
                        plain("python::Lock"),
                        separator(),
                        plain("workflows::ShowWorkflows"),
                        plain("workflows::NewWorkflow"),
                        separator(),
                        plain("workspace::NewTerminal"),
                        plain("ports::ForwardPort"),
                    ],
                ),
                menu(
                    "Debug",
                    vec![
                        plain("debugger::StartDebugging"),
                        plain("debugger::DebugCurrentFile"),
                        separator(),
                        plain("debugger::Continue"),
                        plain("debugger::Pause"),
                        plain("debugger::StepOver"),
                        plain("debugger::StepInto"),
                        plain("debugger::StepOut"),
                        plain("debugger::Restart"),
                        plain("debugger::Stop"),
                        separator(),
                        plain("debugger::ToggleBreakpoint"),
                        plain("debugger::EditBreakpoint"),
                        plain("debugger::ToggleAllBreakpoints"),
                        plain("debugger::RemoveAllBreakpoints"),
                        separator(),
                        plain("debugger::ToggleDebugPanel"),
                        plain("debugger::ToggleDebugConsole"),
                    ],
                ),
                menu(
                    "Git",
                    vec![
                        labeled("workspace::ToggleCommit", "Commit…"),
                        plain("git::Push"),
                        plain("git::Update"),
                        plain("git::Pull"),
                        plain("git::Fetch"),
                        separator(),
                        plain("git::Branches"),
                        plain("git::NewBranch"),
                        plain("git::CheckoutRevision"),
                        plain("git::Worktrees"),
                        separator(),
                        plain("git::Rollback"),
                        separator(),
                        plain("git::ShowLog"),
                        plain("git::ShowFileHistory"),
                        plain("git::Compare"),
                        separator(),
                        plain("git::Refresh"),
                    ],
                ),
                menu(
                    "Help",
                    vec![plain("app::ShowCommandPalette"), plain("app::About")],
                ),
            ],
        }
    }
}

impl MenuBar {
    /// Command names in the menus that `known` does not contain.
    pub fn unknown_commands(&self, known: &[&str]) -> Vec<String> {
        fn walk(items: &[Item], known: &[&str], unknown: &mut Vec<String>) {
            for item in items {
                match item {
                    Item::Plain(command) | Item::Command { command, .. } => {
                        if command != SEPARATOR
                            && !known.contains(&command.as_str())
                            && !unknown.contains(command)
                        {
                            unknown.push(command.clone());
                        }
                    }
                    Item::Submenu { items, .. } => walk(items, known, unknown),
                    Item::List { .. } => {}
                }
            }
        }
        let mut unknown = Vec::new();
        for menu in &self.menus {
            walk(&menu.items, known, &mut unknown);
        }
        unknown
    }
}

/// `menus.json` in the platform config dir (none in unit tests, which
/// expect the built-in menus).
pub fn path() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    Some(dirs::config_dir()?.join("next_code").join("menus.json"))
}

/// Parses a `menus.json`; unknown commands are reported, not fatal (their
/// items show disabled).
fn parse(json: &str, known: &[&str]) -> Result<(MenuBar, Option<String>), String> {
    let bar: MenuBar = serde_json::from_str(json).map_err(|err| err.to_string())?;
    let unknown = bar.unknown_commands(known);
    let warning =
        (!unknown.is_empty()).then(|| format!("unknown commands: {}", unknown.join(", ")));
    Ok((bar, warning))
}

/// The menus in use, re-read when `menus.json` changes.
struct Loaded {
    bar: Rc<MenuBar>,
    modified: Option<SystemTime>,
    problem: Option<String>,
}

impl Global for Loaded {}

fn modified() -> Option<SystemTime> {
    std::fs::metadata(path()?)
        .and_then(|meta| meta.modified())
        .ok()
}

/// The menu bar, and what is wrong with `menus.json` if anything. Cheap
/// unless the file changed since the last call.
pub fn current(cx: &mut App) -> (Rc<MenuBar>, Option<String>) {
    let modified = modified();
    if let Some(loaded) = cx.try_global::<Loaded>()
        && loaded.modified == modified
    {
        return (loaded.bar.clone(), loaded.problem.clone());
    }
    let read = modified
        .and_then(|_| path())
        .map(|path| std::fs::read_to_string(&path).map_err(|err| err.to_string()));
    let (bar, problem) = match read {
        None => (MenuBar::default(), None),
        Some(Err(err)) => (MenuBar::default(), Some(err)),
        Some(Ok(json)) => match parse(&json, cx.all_action_names()) {
            Ok((bar, warning)) => (bar, warning),
            Err(err) => (
                MenuBar::default(),
                Some(format!("{err}; using the built-in menus")),
            ),
        },
    };
    let problem = problem.map(|problem| format!("menus.json: {problem}"));
    let bar = Rc::new(bar);
    cx.set_global(Loaded {
        bar: bar.clone(),
        modified,
        problem: problem.clone(),
    });
    (bar, problem)
}

/// Writes the built-in menus to `menus.json` unless it exists; returns
/// its path.
pub fn ensure_file() -> std::io::Result<PathBuf> {
    let path = path().ok_or_else(|| std::io::Error::other("no config directory"))?;
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json =
            serde_json::to_string_pretty(&MenuBar::default()).map_err(std::io::Error::other)?;
        std::fs::write(&path, json + "\n")?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered() -> Vec<&'static str> {
        gpui::generate_list_of_all_registered_actions()
            .map(|action| action.name)
            .collect()
    }

    #[test]
    fn built_in_menus_only_name_registered_commands() {
        let known = registered();
        assert_eq!(
            MenuBar::default().unknown_commands(&known),
            Vec::<String>::new()
        );
    }

    #[test]
    fn built_in_menus_round_trip_through_menus_json() {
        let json = serde_json::to_string_pretty(&MenuBar::default()).unwrap();
        let (bar, warning) = parse(&json, &registered()).unwrap();
        assert_eq!(bar, MenuBar::default());
        assert_eq!(warning, None);
        assert!(json.contains("\"app::OpenProject\""), "{json}");
        assert!(json.contains("\"list\": \"recent_projects\""), "{json}");
    }

    #[test]
    fn every_item_form_parses() {
        let json = r#"{ "menus": [ { "label": "Mine", "items": [
            "editor::Save",
            "-",
            { "command": "editor::Undo", "label": "Take Back" },
            { "label": "Nested", "items": ["app::About"] },
            { "label": "Recent", "list": "recent_projects" }
        ] } ] }"#;
        let (bar, warning) = parse(json, &registered()).unwrap();
        assert_eq!(warning, None);
        assert_eq!(
            bar.menus[0].items,
            vec![
                plain("editor::Save"),
                plain(SEPARATOR),
                labeled("editor::Undo", "Take Back"),
                Item::Submenu {
                    label: "Nested".into(),
                    items: vec![plain("app::About")],
                },
                Item::List {
                    label: "Recent".into(),
                    list: List::RecentProjects,
                },
            ]
        );
    }

    #[test]
    fn unknown_commands_are_reported_and_bad_shapes_rejected() {
        let json = r#"{ "menus": [ { "label": "M", "items": [
            "editor::Save", "nope::Missing", { "label": "S", "items": ["nope::Missing"] }
        ] } ] }"#;
        let (_, warning) = parse(json, &registered()).unwrap();
        assert_eq!(warning.as_deref(), Some("unknown commands: nope::Missing"));
        assert!(parse(r#"{ "menus": [ { "label": "M", "items": [{ "command": "editor::Save", "colour": "red" }] } ] }"#, &[]).is_err());
        assert!(parse(r#"{ "menus": [ { "label": "M", "items": [{ "label": "L", "list": "favourites" }] } ] }"#, &[]).is_err());
    }
}
