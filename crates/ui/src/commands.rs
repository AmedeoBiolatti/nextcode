//! Commands: the gpui actions a user can run by name, with display labels
//! and key bindings. Menu items and the command palette both run them.

use gpui::{Action, FocusHandle, KeybindingKeystroke, Modifiers, Window};

/// Display labels where the action's name alone reads poorly.
const LABELS: &[(&str, &str)] = &[
    ("app::OpenProject", "Open Project…"),
    ("app::OpenProjectInNewWindow", "Open Project in New Window…"),
    ("app::OpenRecent", "Open Recent…"),
    ("app::CloseProject", "Close Project"),
    ("app::NextProject", "Next Project Tab"),
    ("app::PreviousProject", "Previous Project Tab"),
    ("app::ShowCommandPalette", "Command Palette…"),
    ("app::CustomizeMenus", "Customize Menus…"),
    ("app::OpenSettings", "Open Settings"),
    ("app::SelectTheme", "Select Theme…"),
    ("app::CustomizeTheme", "Customize Theme…"),
    ("app::Quit", "Exit"),
    ("app::About", "About next_code"),
    ("workspace::CloseTab", "Close Tab"),
    ("workspace::ToggleFileFinder", "Go to File…"),
    (
        "workspace::GoToWorkspaceSymbol",
        "Go to Symbol in Workspace…",
    ),
    ("workspace::GoBack", "Back"),
    ("workspace::GoForward", "Forward"),
    ("workspace::SaveAll", "Save All"),
    ("workspace::ToggleFiles", "Files Panel"),
    ("workspace::ToggleSearch", "Find in Files"),
    ("workspace::ToggleTerminal", "Terminal Panel"),
    ("workspace::ToggleCommit", "Commit Tool Window"),
    ("workspace::ToggleGitLog", "Git Tool Window"),
    ("workspace::ToggleProblems", "Problems Panel"),
    ("workspace::TogglePorts", "Ports Panel"),
    ("ports::ForwardPort", "Forward a Port…"),
    ("workspace::ToggleCallHierarchy", "Call Hierarchy Panel"),
    ("workspace::ToggleOutline", "Outline Panel"),
    ("workspace::TogglePython", "Python Panel"),
    ("workspace::ToggleBottomDock", "Toggle Bottom Dock"),
    ("workspace::SaveLayoutAsDefault", "Save Layout as Default"),
    ("workspace::ResetLayout", "Reset Layout"),
    ("workspace::NewTerminal", "New Terminal"),
    ("workspace::ReopenInContainer", "Reopen in Dev Container"),
    ("workspace::SplitRight", "Split Editor Right"),
    ("workspace::SplitDown", "Split Editor Down"),
    ("workspace::FocusFirstPane", "Focus First Editor Pane"),
    ("workspace::FocusSecondPane", "Focus Second Editor Pane"),
    ("workspace::FocusThirdPane", "Focus Third Editor Pane"),
    ("workspace::FocusNextPane", "Focus Next Editor Pane"),
    ("workspace::FocusPreviousPane", "Focus Previous Editor Pane"),
    ("workspace::MoveTabToNextPane", "Move Tab to Next Pane"),
    (
        "workspace::MoveTabToPreviousPane",
        "Move Tab to Previous Pane",
    ),
    ("workspace::ClosePane", "Close Editor Pane"),
    ("git::Branches", "Branches…"),
    ("git::NewBranch", "New Branch…"),
    ("git::CheckoutRevision", "Checkout Tag or Revision…"),
    ("git::Update", "Update Project…"),
    ("git::Rollback", "Rollback…"),
    ("git::Worktrees", "Worktrees…"),
    ("git::ShowLog", "Show Git Log"),
    ("git::ShowFileHistory", "Show History for Current File"),
    ("git::Compare", "Compare Revisions…"),
    ("git::Fetch", "Fetch"),
    ("git::Pull", "Pull"),
    ("git::Push", "Push…"),
    ("git::Refresh", "Refresh Status"),
    ("python::Sync", "Sync Environment (uv sync)"),
    ("python::Lock", "Update Lock File (uv lock)"),
    ("python::RunFile", "Run Current Python File"),
    ("python::RunTests", "Run Python Tests (pytest)"),
    ("python::RunTestFile", "Run Tests in Current File"),
    ("python::RunTestAtCursor", "Run Test at Cursor"),
    ("run::RunConfiguration", "Run Selected Configuration"),
    ("run::DebugConfiguration", "Debug Selected Configuration"),
    ("run::SelectConfiguration", "Select Configuration…"),
    ("run::EditConfigurations", "Edit Configurations…"),
    ("run::RunAllTests", "Run All Tests"),
    ("run::RunFailedTests", "Run Failed Tests"),
    ("workspace::ToggleTests", "Tests Panel"),
    ("debugger::StartDebugging", "Start Debugging"),
    ("debugger::DebugCurrentFile", "Debug Current File"),
    ("debugger::StepOver", "Step Over"),
    ("debugger::StepInto", "Step Into"),
    ("debugger::StepOut", "Step Out"),
    ("debugger::Stop", "Stop Debugging"),
    ("debugger::Restart", "Restart Debugging"),
    ("debugger::EditBreakpoint", "Edit Breakpoint…"),
    (
        "debugger::ToggleAllBreakpoints",
        "Enable/Disable All Breakpoints",
    ),
    ("debugger::ToggleDebugPanel", "Debug Panel"),
    ("debugger::ToggleDebugConsole", "Debug Console"),
    ("debugger::DownloadCodeLldb", "Download CodeLLDB"),
    ("debugger::ClearConsole", "Clear Debug Console"),
    ("editor::SelectAll", "Select All"),
    ("editor::GoToDefinition", "Go to Definition"),
    ("editor::GoToDeclaration", "Go to Declaration"),
    ("editor::GoToTypeDefinition", "Go to Type Definition"),
    ("editor::GoToImplementation", "Go to Implementation"),
    ("editor::ShowSignatureHelp", "Show Parameter Hints"),
    ("editor::ToggleInlayHints", "Toggle Inlay Hints"),
    ("editor::FindReferences", "Find References"),
    ("editor::ShowCallHierarchy", "Show Call Hierarchy"),
    ("editor::GoToSymbol", "Go to Symbol in File…"),
    ("editor::NextDiagnostic", "Next Problem"),
    ("editor::PrevDiagnostic", "Previous Problem"),
    ("editor::ShowCompletions", "Show Completions"),
    ("editor::SelectNextOccurrence", "Add Next Occurrence"),
    ("editor::Indent", "Indent Lines"),
    ("editor::Outdent", "Outdent Lines"),
    ("editor::JumpToBracket", "Go to Matching Bracket"),
    ("editor::GoToLine", "Go to Line…"),
    ("editor::Find", "Find…"),
    ("editor::FindReplace", "Replace…"),
    ("editor::ReplaceNext", "Replace"),
    ("editor::ToggleMatchCase", "Find: Toggle Match Case"),
    ("editor::ToggleWholeWord", "Find: Toggle Whole Word"),
    ("editor::ToggleRegex", "Find: Toggle Regular Expression"),
    ("editor::RenameSymbol", "Rename Symbol…"),
    ("editor::QuickFix", "Quick Fix…"),
    ("editor::FormatDocument", "Format Document"),
    ("editor::FormatSelection", "Format Selection"),
];

/// Namespaces of modal pickers: their actions only mean something inside.
const HIDDEN_NAMESPACES: &[&str] = &[
    "file_finder",
    "location_picker",
    "project_picker",
    "command_palette",
    "theme_picker",
    "configuration_picker",
    // Keys inside the Commit window and git dialogs.
    "commit_panel",
    "git_dialog",
];

/// Building blocks of typing and list navigation, not commands.
const HIDDEN: &[&str] = &[
    "editor::Backspace",
    "editor::Delete",
    "editor::DeleteWordLeft",
    "editor::DeleteWordRight",
    "editor::Left",
    "editor::Right",
    "editor::Up",
    "editor::Down",
    "editor::SelectLeft",
    "editor::SelectRight",
    "editor::SelectUp",
    "editor::SelectDown",
    "editor::WordLeft",
    "editor::WordRight",
    "editor::SelectWordLeft",
    "editor::SelectWordRight",
    "editor::Home",
    "editor::End",
    "editor::SelectHome",
    "editor::SelectEnd",
    "editor::DocStart",
    "editor::DocEnd",
    "editor::SelectDocStart",
    "editor::SelectDocEnd",
    "editor::PageUp",
    "editor::PageDown",
    "editor::Newline",
    "editor::Tab",
    "editor::SelectPrevCompletion",
    "editor::SelectNextCompletion",
    "editor::ConfirmCompletion",
    "editor::CloseCompletions",
    "editor::CloseSignatureHelp",
    "editor::NextSignature",
    "editor::PreviousSignature",
    "editor::CollapseSelections",
    "editor::CloseFind",
    "editor::ConfirmGoToLine",
    "editor::CancelGoToLine",
    "editor::ConfirmRename",
    "editor::CancelRename",
    "editor::SelectPrevCodeAction",
    "editor::SelectNextCodeAction",
    "editor::ConfirmCodeAction",
    "editor::CloseCodeActions",
    "editor::NextTabStop",
    "editor::PreviousTabStop",
    "editor::EndSnippet",
    "editor::ConfirmBreakpointEdit",
    "editor::CancelBreakpointEdit",
    "editor::NextBreakpointField",
    "debugger::AddWatch",
    "debugger::SubmitConsoleInput",
    "debugger::PreviousConsoleInput",
    "debugger::NextConsoleInput",
    "debugger::ConfirmSetVariable",
    "debugger::CancelSetVariable",
    "ports::ConfirmInput",
    "ports::CancelInput",
    "app::OpenRecentProject",
];

/// Whether the command palette lists the action.
pub fn in_palette(name: &str) -> bool {
    let namespace = name.split_once("::").map_or("", |(namespace, _)| namespace);
    !HIDDEN.contains(&name) && !HIDDEN_NAMESPACES.contains(&namespace) && name != "zed::NoAction"
}

/// What menus and the palette call the action.
pub fn label(name: &str) -> String {
    match LABELS.iter().find(|(known, _)| *known == name) {
        Some((_, label)) => (*label).to_owned(),
        None => humanize(name.rsplit("::").next().unwrap_or(name)),
    }
}

/// The palette's group for the action: its namespace, capitalized.
pub fn category(name: &str) -> String {
    match name.split_once("::") {
        Some((namespace, _)) => humanize(namespace),
        None => String::new(),
    }
}

/// `ToggleFileFinder` → `Toggle File Finder`, `open_recent` → `Open Recent`.
fn humanize(identifier: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    for part in identifier.split('_').filter(|part| !part.is_empty()) {
        let mut word = String::new();
        for ch in part.chars() {
            if ch.is_uppercase() && !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            word.push(ch);
        }
        words.push(word);
    }
    words
        .into_iter()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The key binding that runs `action` with focus on `focus` (or wherever
/// focus is), as `Ctrl+Shift+P`.
pub fn key_text(
    window: &Window,
    action: &dyn Action,
    focus: Option<&FocusHandle>,
) -> Option<String> {
    let binding = match focus {
        Some(focus) => window.highest_precedence_binding_for_action_in(action, focus),
        None => window.highest_precedence_binding_for_action(action),
    }?;
    Some(
        binding
            .keystrokes()
            .iter()
            .map(keystroke_text)
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn keystroke_text(keystroke: &KeybindingKeystroke) -> String {
    chord_text(keystroke.modifiers(), keystroke.key())
}

fn chord_text(modifiers: &Modifiers, key: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if modifiers.control {
        parts.push("Ctrl".into());
    }
    if modifiers.alt {
        parts.push("Alt".into());
    }
    if modifiers.shift {
        parts.push("Shift".into());
    }
    if modifiers.platform {
        parts.push(
            if cfg!(target_os = "macos") {
                "Cmd"
            } else {
                "Win"
            }
            .into(),
        );
    }
    let key = match key {
        "pageup" => "PageUp".to_owned(),
        "pagedown" => "PageDown".to_owned(),
        key if key.chars().count() == 1 => key.to_uppercase(),
        key => {
            let mut chars = key.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        }
    };
    parts.push(key);
    parts.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_fall_back_to_the_spaced_action_name() {
        assert_eq!(label("app::OpenProject"), "Open Project…");
        assert_eq!(label("editor::Undo"), "Undo");
        assert_eq!(label("git::ShowStashList"), "Show Stash List");
        assert_eq!(category("location_picker::Confirm"), "Location Picker");
    }

    #[test]
    fn labelled_and_hidden_names_are_registered_actions() {
        let registered: Vec<&str> = gpui::generate_list_of_all_registered_actions()
            .map(|action| action.name)
            .collect();
        let stale: Vec<&str> = LABELS
            .iter()
            .map(|(name, _)| *name)
            .chain(HIDDEN.iter().copied())
            .filter(|name| !registered.contains(name))
            .collect();
        assert_eq!(stale, Vec::<&str>::new());
    }

    #[test]
    fn typing_and_picker_actions_stay_out_of_the_palette() {
        assert!(!in_palette("editor::Left"));
        assert!(!in_palette("file_finder::Dismiss"));
        assert!(!in_palette("app::OpenRecentProject"));
        assert!(in_palette("editor::Copy"));
        assert!(in_palette("workspace::ToggleCommit"));
    }

    #[test]
    fn chords_read_like_the_platform_writes_them() {
        let ctrl_shift = Modifiers {
            control: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(chord_text(&ctrl_shift, "p"), "Ctrl+Shift+P");
        assert_eq!(chord_text(&Modifiers::default(), "f12"), "F12");
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        assert_eq!(chord_text(&alt, "pageup"), "Alt+PageUp");
        assert_eq!(chord_text(&alt, "left"), "Alt+Left");
    }
}
