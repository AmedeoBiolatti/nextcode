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

fn submenu(label: &str, items: Vec<Item>) -> Item {
    Item::Submenu {
        label: label.into(),
        items,
    }
}

impl Default for MenuBar {
    fn default() -> Self {
        let separator = || plain(SEPARATOR);
        let mut bar = Self {
            menus: vec![
                menu(
                    "File",
                    vec![
                        plain("app::NewProject"),
                        plain("file_tree::NewFile"),
                        separator(),
                        plain("app::OpenProject"),
                        plain("app::OpenProjectInNewWindow"),
                        Item::List {
                            label: "Open Recent".into(),
                            list: List::RecentProjects,
                        },
                        submenu(
                            "Workspace",
                            vec![
                                plain("app::OpenWorkspace"),
                                plain("workspace::AddFolderToWorkspace"),
                                plain("workspace::SaveWorkspaceAs"),
                            ],
                        ),
                        submenu("Remote", vec![plain("workspace::ReopenInContainer")]),
                        separator(),
                        plain("editor::Save"),
                        plain("workspace::SaveAll"),
                        separator(),
                        plain("workspace::CloseTab"),
                        plain("app::CloseProject"),
                        separator(),
                        submenu(
                            "Preferences",
                            vec![
                                plain("app::OpenSettings"),
                                plain("app::OpenSettingsJson"),
                                plain("extensions::ManageExtensions"),
                                separator(),
                                plain("app::SelectTheme"),
                                plain("app::CustomizeTheme"),
                                plain("app::CustomizeMenus"),
                            ],
                        ),
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
                        plain("editor::CopyReference"),
                        plain("editor::SelectAll"),
                        separator(),
                        plain("editor::Find"),
                        plain("editor::FindReplace"),
                        labeled("workspace::ToggleSearch", "Find in Files…"),
                        plain("workspace::ReplaceInFiles"),
                        separator(),
                        submenu(
                            "Lines",
                            vec![
                                plain("editor::CopyLineUp"),
                                plain("editor::CopyLineDown"),
                                plain("editor::MoveLineUp"),
                                plain("editor::MoveLineDown"),
                                plain("editor::DeleteLine"),
                                plain("editor::InsertLineAbove"),
                                plain("editor::InsertLineBelow"),
                                separator(),
                                plain("editor::Indent"),
                                plain("editor::Outdent"),
                                plain("editor::ToggleLineComment"),
                                plain("editor::ToggleBlockComment"),
                            ],
                        ),
                        submenu(
                            "Multiple Cursors",
                            vec![
                                plain("editor::AddCursorAbove"),
                                plain("editor::AddCursorBelow"),
                                plain("editor::SelectNextOccurrence"),
                                plain("editor::SelectAllOccurrences"),
                            ],
                        ),
                        submenu(
                            "Code Actions",
                            vec![
                                plain("editor::RenameSymbol"),
                                plain("editor::QuickFix"),
                                plain("editor::ShowCompletions"),
                                separator(),
                                plain("editor::FormatDocument"),
                                plain("editor::FormatSelection"),
                            ],
                        ),
                    ],
                ),
                menu(
                    "View",
                    vec![
                        plain("app::ShowCommandPalette"),
                        separator(),
                        submenu(
                            "Panels",
                            vec![
                                plain("workspace::ToggleFiles"),
                                labeled("workspace::ToggleGitLog", "Git"),
                                plain("workspace::ToggleProblems"),
                                labeled("workspace::TogglePorts", "Ports"),
                                labeled("workspace::ToggleTests", "Tests"),
                                labeled("workspace::ToggleCallHierarchy", "Call Hierarchy"),
                                labeled("workspace::ToggleOutline", "Outline"),
                                labeled("debugger::ToggleDebugPanel", "Debug"),
                                labeled("debugger::ToggleDebugConsole", "Debug Console"),
                                plain("workspace::ToggleTerminal"),
                            ],
                        ),
                        plain("workspace::ToggleBottomDock"),
                        submenu(
                            "Editor Layout",
                            vec![
                                plain("workspace::SplitRight"),
                                plain("workspace::SplitDown"),
                                plain("workspace::FocusNextPane"),
                                plain("workspace::MoveTabToNextPane"),
                                plain("workspace::MoveTabToPreviousPane"),
                                plain("workspace::ClosePane"),
                                separator(),
                                plain("workspace::SaveLayoutAsDefault"),
                                plain("workspace::ResetLayout"),
                            ],
                        ),
                        separator(),
                        submenu(
                            "Appearance",
                            vec![
                                labeled("editor::ToggleWordWrap", "Word Wrap"),
                                labeled("editor::ToggleInlayHints", "Inlay Hints"),
                            ],
                        ),
                        labeled("editor::ToggleVimMode", "Vim Mode"),
                        submenu(
                            "Markdown",
                            vec![
                                plain("markdown::ShowSource"),
                                plain("markdown::ShowPreview"),
                                plain("markdown::ShowSplit"),
                            ],
                        ),
                        submenu(
                            "Folding",
                            vec![
                                plain("editor::Fold"),
                                plain("editor::Unfold"),
                                plain("editor::ToggleFold"),
                                plain("editor::FoldRecursively"),
                                plain("editor::UnfoldRecursively"),
                                separator(),
                                plain("editor::FoldAll"),
                                plain("editor::UnfoldAll"),
                            ],
                        ),
                    ],
                ),
                menu(
                    "Go",
                    vec![
                        plain("workspace::ToggleFileFinder"),
                        plain("editor::GoToSymbol"),
                        plain("workspace::GoToWorkspaceSymbol"),
                        plain("editor::GoToLine"),
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
                        plain("editor::JumpToBracket"),
                        separator(),
                        plain("editor::NextDiagnostic"),
                        plain("editor::PrevDiagnostic"),
                        plain("editor::NextChange"),
                        plain("editor::PreviousChange"),
                        separator(),
                        plain("app::NextProject"),
                        plain("app::PreviousProject"),
                    ],
                ),
                menu(
                    "Run",
                    vec![
                        labeled("run::RunConfiguration", "Run"),
                        plain("debugger::StartDebugging"),
                        plain("run::SelectConfiguration"),
                        plain("run::EditConfigurations"),
                        separator(),
                        submenu(
                            "Debugging",
                            vec![
                                plain("debugger::DebugCurrentFile"),
                                separator(),
                                plain("debugger::Continue"),
                                plain("debugger::Pause"),
                                plain("debugger::StepOver"),
                                plain("debugger::StepInto"),
                                plain("debugger::StepOut"),
                                plain("debugger::Restart"),
                                plain("debugger::Stop"),
                            ],
                        ),
                        submenu(
                            "Breakpoints",
                            vec![
                                plain("debugger::ToggleBreakpoint"),
                                plain("debugger::EditBreakpoint"),
                                plain("debugger::ToggleAllBreakpoints"),
                                plain("debugger::RemoveAllBreakpoints"),
                            ],
                        ),
                        submenu(
                            "Tests",
                            vec![plain("run::RunAllTests"), plain("run::RunFailedTests")],
                        ),
                        submenu("Notebooks", vec![]),
                        submenu("Python", vec![]),
                        submenu("Workflows", vec![]),
                        separator(),
                        submenu(
                            "Tools",
                            vec![plain("workspace::NewTerminal"), plain("ports::ForwardPort")],
                        ),
                    ],
                ),
                menu(
                    "Git",
                    vec![
                        labeled("workspace::ToggleCommit", "Commit…"),
                        plain("git::Push"),
                        plain("git::Pull"),
                        plain("git::Fetch"),
                        separator(),
                        submenu(
                            "Branches",
                            vec![
                                plain("git::Branches"),
                                plain("git::NewBranch"),
                                plain("git::CheckoutRevision"),
                                plain("git::Worktrees"),
                            ],
                        ),
                        submenu(
                            "Changes",
                            vec![
                                plain("git::Rollback"),
                                plain("editor::RollbackLines"),
                                separator(),
                                plain("git::StashChanges"),
                                plain("git::UnstashChanges"),
                                separator(),
                                plain("git::ShelveChanges"),
                                plain("git::ShowShelf"),
                            ],
                        ),
                        submenu(
                            "History",
                            vec![
                                plain("git::ShowLog"),
                                plain("git::ShowFileHistory"),
                                plain("editor::ToggleBlame"),
                                plain("git::Compare"),
                            ],
                        ),
                        separator(),
                        plain("git::Update"),
                        plain("git::Refresh"),
                    ],
                ),
                menu(
                    "Help",
                    vec![plain("app::CheckForUpdates"), plain("app::About")],
                ),
            ],
        };
        for item in crate::builtins::ALL
            .iter()
            .flat_map(|builtin| builtin.menu_items)
        {
            bar.add_builtin_item(item);
        }
        bar
    }
}

impl MenuBar {
    /// A built-in extension's items in a slash-separated menu path, after
    /// its anchor command (last when there is none).
    fn add_builtin_item(&mut self, item: &crate::builtins::MenuItem) {
        let mut path = item.menu.split('/');
        let root = path.next().unwrap_or_default();
        let Some(menu) = self.menus.iter_mut().find(|menu| menu.label == root) else {
            return;
        };
        let mut items = &mut menu.items;
        for label in path {
            let Some(Item::Submenu {
                items: children, ..
            }) = items
                .iter_mut()
                .find(|item| matches!(item, Item::Submenu { label: own, .. } if own == label))
            else {
                return;
            };
            items = children;
        }
        let at = items
            .iter()
            .position(|existing| {
                matches!(existing, Item::Plain(command) | Item::Command { command, .. } if command == item.after)
            })
            .map_or(items.len(), |index| index + 1);
        let entries = item.items.iter().map(|entry| match entry {
            crate::builtins::Entry::Command(command) => plain(command),
            crate::builtins::Entry::Labeled(command, label) => labeled(command, label),
            crate::builtins::Entry::Separator => plain(SEPARATOR),
        });
        items.splice(at..at, entries);
    }

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
        worktree::write_atomic(&path, (json + "\n").as_bytes())?;
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
    fn defaults_have_seven_menus_and_each_command_has_one_home() {
        let bar = MenuBar::default();
        assert_eq!(
            bar.menus
                .iter()
                .map(|menu| menu.label.as_str())
                .collect::<Vec<_>>(),
            ["File", "Edit", "View", "Go", "Run", "Git", "Help"]
        );
        fn visit(items: &[Item], seen: &mut std::collections::HashSet<String>) {
            for item in items {
                match item {
                    Item::Plain(command) | Item::Command { command, .. }
                        if command != SEPARATOR =>
                    {
                        assert!(
                            seen.insert(command.clone()),
                            "duplicate menu command: {command}"
                        );
                    }
                    Item::Submenu { items, .. } => visit(items, seen),
                    _ => {}
                }
            }
        }
        let mut seen = std::collections::HashSet::new();
        for menu in &bar.menus {
            visit(&menu.items, &mut seen);
        }
        for command in [
            "ssh::ConnectToHost",
            "ports::ForwardPort",
            "editor::RenameSymbol",
            "debugger::StepOver",
            "notebook::RunCell",
            "workflows::OpenWorkflow",
        ] {
            assert!(seen.contains(command), "missing command: {command}");
        }
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
