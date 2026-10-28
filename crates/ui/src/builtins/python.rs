//! The Python built-in extension: the Python panel (the first folder's uv
//! project, its environment and dependencies) and the Python run and test
//! commands. On by default.

use gpui::{Context, Div, Window, prelude::*};
use run::{Configuration, PythonTarget, Source};
use watcher::Batch;

use super::{Builtin, Entry, MenuItem, Panel, PanelView};
use crate::{
    layout::{DockPosition, PanelId},
    python_env::project_relative,
    test_explorer::{RunTarget, engine},
    workspace::{OfferKind, Workspace},
};

pub mod panel;

use panel::{PythonPanel, PythonPanelEvent, RunRequest};

gpui::actions!(
    python,
    [
        Sync,
        Lock,
        RunFile,
        RunTests,
        RunTestFile,
        RunTestAtCursor,
        /// Its name before Python became a built-in extension still works
        /// (in `menus.json`, for instance).
        #[action(deprecated_aliases = ["workspace::TogglePython"])]
        TogglePanel
    ]
);

pub const BUILTIN: Builtin = Builtin {
    id: "python",
    name: "Python",
    description: "Python panel (uv project, environment, dependencies) and Python run and test commands.",
    default_enabled: true,
    labels: &[
        ("python::TogglePanel", "Python Panel"),
        ("python::Sync", "Sync Environment (uv sync)"),
        ("python::Lock", "Update Lock File (uv lock)"),
        ("python::RunFile", "Run Current Python File"),
        ("python::RunTests", "Run Python Tests (pytest)"),
        ("python::RunTestFile", "Run Tests in Current File"),
        ("python::RunTestAtCursor", "Run Test at Cursor"),
    ],
    menu_items: &[
        MenuItem {
            menu: "View/Panels",
            after: "workspace::ToggleGitLog",
            items: &[Entry::Command("python::TogglePanel")],
        },
        MenuItem {
            menu: "Run/Tests",
            after: "run::RunFailedTests",
            items: &[
                Entry::Command("python::RunTestFile"),
                Entry::Command("python::RunTestAtCursor"),
            ],
        },
        MenuItem {
            menu: "Run/Python",
            after: "",
            items: &[
                Entry::Command("python::RunFile"),
                Entry::Separator,
                Entry::Command("python::Sync"),
                Entry::Command("python::Lock"),
            ],
        },
    ],
    workspace_actions,
    panels: &[Panel {
        key: "python",
        title: "Python",
        position: DockPosition::Left,
        after: "commit",
        build: build_panel,
        badge: None,
    }],
    files_changed,
    accept_offer,
    ..Builtin::NONE
};

const PANEL: PanelId = PanelId::Builtin("python");

const SYNC_KEY: &str = "python-sync";

fn sync_offer() -> OfferKind {
    OfferKind::Builtin {
        extension: "python",
        key: SYNC_KEY.into(),
        action: "Sync",
    }
}

fn workspace_actions(element: Div, cx: &mut Context<Workspace>) -> Div {
    element
        .on_action(
            cx.listener(|workspace, _: &TogglePanel, _, cx| workspace.toggle_panel(PANEL, cx)),
        )
        .on_action(
            cx.listener(|workspace, _: &Sync, _, cx| show_and(workspace, PythonPanel::sync, cx)),
        )
        .on_action(
            cx.listener(|workspace, _: &Lock, _, cx| show_and(workspace, PythonPanel::lock, cx)),
        )
        .on_action(cx.listener(|workspace, _: &RunFile, window, cx| {
            run(workspace, RunRequest::CurrentFile, window, cx)
        }))
        .on_action(cx.listener(|workspace, _: &RunTests, window, cx| {
            run(workspace, RunRequest::Tests, window, cx)
        }))
        .on_action(cx.listener(|workspace, _: &RunTestFile, window, cx| {
            run(workspace, RunRequest::TestFile, window, cx)
        }))
        .on_action(cx.listener(|workspace, _: &RunTestAtCursor, window, cx| {
            run(workspace, RunRequest::TestAtCursor, window, cx)
        }))
}

/// Shows the panel and has it run `operation`.
fn show_and(
    workspace: &mut Workspace,
    operation: fn(&mut PythonPanel, &mut Context<PythonPanel>),
    cx: &mut Context<Workspace>,
) {
    workspace.show_panel(PANEL, cx);
    if let Some(panel) = workspace.builtin_panel::<PythonPanel>(PANEL.key()) {
        panel.update(cx, operation);
    }
}

fn build_panel(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> PanelView {
    let panel = cx.new(|cx| PythonPanel::new(workspace.project().clone(), cx));
    let subscription = cx.subscribe_in(
        &panel,
        window,
        |workspace, _, event, window, cx| match event {
            PythonPanelEvent::Run(request) => run(workspace, request.clone(), window, cx),
            PythonPanelEvent::EnvironmentChanged => workspace
                .lsp_store()
                .update(cx, |store, cx| store.refresh_python_interpreter(cx)),
            PythonPanelEvent::SyncNeeded(true) => workspace.offer(
                sync_offer(),
                "The Python environment (.venv) is out of date with uv.lock / pyproject.toml.",
                cx,
            ),
            PythonPanelEvent::SyncNeeded(false) => workspace.withdraw_offer(&sync_offer(), cx),
        },
    );
    PanelView::new(panel, vec![subscription])
}

/// The panel re-reads the project when its uv files change.
fn files_changed(
    workspace: &mut Workspace,
    batch: &Batch,
    _: &mut Window,
    cx: &mut Context<Workspace>,
) {
    const FILES: [&str; 3] = ["pyproject.toml", "uv.lock", ".python-version"];
    let changed = batch.rescan
        || batch.events.iter().any(|event| {
            event
                .path
                .file_name()
                .is_some_and(|name| FILES.iter().any(|file| name == *file))
        });
    if changed && let Some(panel) = workspace.builtin_panel::<PythonPanel>(PANEL.key()) {
        panel.update(cx, |panel, cx| panel.refresh(cx));
    }
}

fn accept_offer(workspace: &mut Workspace, key: &str, _: &mut Window, cx: &mut Context<Workspace>) {
    if key == SYNC_KEY
        && let Some(panel) = workspace.builtin_panel::<PythonPanel>(PANEL.key())
    {
        panel.update(cx, |panel, cx| panel.sync(cx));
    }
}

/// The panel's run buttons and the run commands: scripts (of the first
/// folder, whose project the panel shows) and the current file (in its
/// folder) run as configurations in terminal tabs; tests run in the Tests
/// panel.
fn run(
    workspace: &mut Workspace,
    request: RunRequest,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let active = workspace.active_file_editor().map(|(path, editor)| {
        let editor = editor.read(cx);
        let line = editor.buffer().line_of(editor.cursor());
        (path, line, editor.text())
    });
    let project = workspace.project().clone();
    let folder = active
        .as_ref()
        .and_then(|(path, _, _)| project.root_index_of(path))
        .unwrap_or(0);
    // Relative to its folder, as pytest and the interpreter there take it.
    let file = active
        .as_ref()
        .map(|(path, _, _)| project_relative(&project, path));
    let need_file = || "Open a Python file first".to_owned();
    let configuration = |name: String, kind: run::Kind| Configuration {
        name,
        source: Source::Automatic,
        kind,
        cwd: None,
        env: Default::default(),
        disabled: None,
        notes: Vec::new(),
        debug_options: Default::default(),
    };
    match request {
        RunRequest::Script(name) => workspace.run_in_terminal(
            configuration(
                format!("uv run {name}"),
                run::Kind::UvScript {
                    script: name,
                    args: Vec::new(),
                },
            ),
            0,
            window,
            cx,
        ),
        RunRequest::CurrentFile => match (file, &active) {
            (Some(file), Some((path, _, _))) => workspace.run_in_terminal(
                configuration(
                    format!("Run {}", project.display_path(path)),
                    run::Kind::Python {
                        target: PythonTarget::Script(file),
                        args: Vec::new(),
                        interpreter: None,
                    },
                ),
                folder,
                window,
                cx,
            ),
            _ => workspace.report(need_file(), cx),
        },
        RunRequest::Tests => workspace.show_tests_and_run(RunTarget::Pytest, cx),
        RunRequest::TestFile => match file {
            Some(file) => {
                let id = engine::test_id(&project, folder, &file);
                workspace.show_tests_and_run(RunTarget::Ids(vec![id]), cx)
            }
            None => workspace.report(need_file(), cx),
        },
        RunRequest::TestAtCursor => match (&active, &file) {
            (Some((_, line, text)), Some(file)) => match python::test_at(text, *line) {
                Some(node) => {
                    let id = engine::test_id(&project, folder, &format!("{file}::{node}"));
                    workspace.show_tests_and_run(RunTarget::Ids(vec![id]), cx)
                }
                None => {
                    workspace.report("The cursor is not in a test function or class".into(), cx)
                }
            },
            _ => workspace.report(need_file(), cx),
        },
    }
}

#[cfg(test)]
mod tests {
    use crate::menus::{Item, MenuBar};

    fn commands(path: &str) -> Vec<String> {
        let bar = MenuBar::default();
        let mut path = path.split('/');
        let root = path.next().unwrap();
        let mut items = &bar
            .menus
            .iter()
            .find(|menu| menu.label == root)
            .unwrap()
            .items;
        for label in path {
            let Item::Submenu {
                items: children, ..
            } = items
                .iter()
                .find(|item| matches!(item, Item::Submenu { label: own, .. } if own == label))
                .unwrap()
            else {
                panic!("submenu")
            };
            items = children;
        }
        items
            .iter()
            .filter_map(|item| match item {
                Item::Plain(command) | Item::Command { command, .. } => Some(command.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn python_commands_are_grouped_without_losing_test_actions() {
        assert_eq!(
            commands("Run/Python"),
            ["python::RunFile", "-", "python::Sync", "python::Lock"]
        );
        assert_eq!(
            commands("Run/Tests"),
            [
                "run::RunAllTests",
                "run::RunFailedTests",
                "python::RunTestFile",
                "python::RunTestAtCursor"
            ]
        );
        let panels = commands("View/Panels");
        let git = panels
            .iter()
            .position(|item| item == "workspace::ToggleGitLog")
            .unwrap();
        assert_eq!(panels[git + 1], "python::TogglePanel");
    }
    #[gpui::test]
    fn the_panel_command_keeps_its_old_name(cx: &mut gpui::TestAppContext) {
        let action = cx
            .update(|cx| cx.build_action("workspace::TogglePython", None))
            .unwrap();
        assert_eq!(action.name(), "python::TogglePanel");
    }
}
