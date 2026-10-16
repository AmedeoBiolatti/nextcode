//! Run configurations and tests in the workspace: the selected
//! configuration (picker, persisted per project), Run in a terminal tab,
//! Debug through [`Workspace::start_debugging`], Edit Configurations, the
//! Tests panel's events, and the Python panel's run/test buttons routed
//! through the same paths.

use std::{collections::HashMap, sync::Arc};

use gpui::{App, Subscription};
use run::{Configuration, PythonTarget, Source};

use super::*;
use crate::{
    editor::{GutterMarker, GutterMarkerKind},
    run_config::{
        self, Active, ConfigurationPicker, ConfigurationPickerEvent, DebugConfiguration,
        EditConfigurations, RunAllTests, RunConfigs, RunConfiguration, RunFailedTests,
        SelectConfiguration,
    },
    test_explorer::{RunTarget, TestsPanel, TestsPanelEvent, engine},
};

actions!(workspace, [ToggleTests]);

pub fn bind_keys(cx: &mut App) {
    let ctx = Some("Workspace");
    cx.bind_keys([
        // VS Code: Run Without Debugging; JetBrains: Run.
        KeyBinding::new("ctrl-f5", RunConfiguration, ctx),
        KeyBinding::new("shift-f10", RunConfiguration, ctx),
        // JetBrains: Debug (F5 dispatches it too when no session is paused).
        KeyBinding::new("shift-f9", DebugConfiguration, ctx),
    ]);
    run_config::picker::bind_keys(cx);
}

/// The workspace's run state.
pub(super) struct Runs {
    configs: RunConfigs,
    pub(super) tests: Entity<TestsPanel>,
    picker: Option<(Entity<ConfigurationPicker>, Subscription)>,
    _load: Option<Task<()>>,
    /// A debug request being prepared (cargo build).
    _debug: Option<Task<()>>,
    _tests: Subscription,
}

impl Runs {
    pub(super) fn new(project: Project, window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let tests = cx.new(|cx| TestsPanel::new(project, cx));
        let subscription = cx.subscribe_in(&tests, window, |this, _, event, window, cx| {
            this.on_tests_event(event, window, cx)
        });
        cx.defer_in(window, |this, _, cx| this.reload_configurations(cx));
        Self {
            configs: RunConfigs::default(),
            tests,
            picker: None,
            _load: None,
            _debug: None,
            _tests: subscription,
        }
    }

    pub(super) fn picker(&self) -> Option<&Entity<ConfigurationPicker>> {
        self.picker.as_ref().map(|(picker, _)| picker)
    }
}

/// Files whose change reloads the configurations.
const CONFIG_FILES: &[&str] = &["launch.json", "run.json", "pyproject.toml", "Cargo.toml"];

impl Workspace {
    /// Reads the configurations again (where the project lives).
    pub(super) fn reload_configurations(&mut self, cx: &mut Context<Self>) {
        let project = self.project.clone();
        let task = cx.background_spawn(async move { run_config::load(&project) });
        self.runs._load = Some(cx.spawn(async move |this, cx| {
            let mut configs = task.await;
            this.update(cx, |this, cx| {
                configs.selected = this
                    .runs
                    .configs
                    .selected
                    .take()
                    .or_else(|| run_config::load_selected(&this.project.state_key()));
                this.runs.configs = configs;
                this.runs._load = None;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Reloads configurations when their files change.
    pub(super) fn run_files_changed(&mut self, batch: &Batch, cx: &mut Context<Self>) {
        let touched = batch.rescan
            || batch.events.iter().any(|event| {
                event
                    .path
                    .file_name()
                    .is_some_and(|name| CONFIG_FILES.iter().any(|file| name == *file))
            });
        if touched {
            self.reload_configurations(cx);
        }
    }

    /// A saved test file is rediscovered (debounced).
    pub(super) fn run_file_saved(&mut self, path: &Path, cx: &mut Context<Self>) {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let test_file = (name.starts_with("test_") && name.ends_with(".py"))
            || name.ends_with("_test.py")
            || name == "conftest.py"
            || name.ends_with(".rs");
        if test_file {
            self.runs
                .tests
                .update(cx, |tests, cx| tests.schedule_discover(cx));
        }
    }

    /// The selected configuration's name, for the title bar selector.
    pub fn selected_configuration(&self) -> Option<SharedString> {
        self.runs
            .configs
            .selected()
            .map(|config| config.name.clone().into())
    }

    /// The editor's file and line, for `${file}` and `${lineNumber}`.
    fn active(&self, cx: &App) -> Active {
        let Some(tab) = self.active_tab() else {
            return Active::default();
        };
        let line = tab.editor().map(|editor| {
            let editor = editor.read(cx);
            editor.buffer().line_of(editor.cursor()) + 1
        });
        Active {
            file: tab
                .editor()
                .map(|_| tab.path.to_string_lossy().into_owned()),
            line,
        }
    }

    fn report(&mut self, message: String, cx: &mut Context<Self>) {
        // Long errors (compiler output) keep their first lines.
        let mut lines: Vec<&str> = message.lines().take(8).collect();
        if message.lines().count() > 8 {
            lines.push("…");
        }
        self.notice = Some(lines.join("\n").into());
        cx.notify();
    }

    /// The selected configuration and its folder.
    fn selected_or_notice(&mut self, cx: &mut Context<Self>) -> Option<(Configuration, usize)> {
        let selected = self.runs.configs.selected().cloned();
        if selected.is_none() {
            let message = if self.runs.configs.loaded {
                "No run configuration: add one with Run > Edit Configurations…"
            } else {
                "Run configurations are still loading"
            };
            self.report(message.into(), cx);
        }
        selected.map(|config| {
            let folder = self.runs.configs.folder(&config.name);
            (config, folder)
        })
    }

    pub fn run_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((config, folder)) = self.selected_or_notice(cx) {
            self.run_in_terminal(config, folder, window, cx);
        }
    }

    pub fn debug_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((config, folder)) = self.selected_or_notice(cx) {
            self.debug(config, folder, window, cx);
        }
    }

    pub(super) fn run_configuration(
        &mut self,
        _: &RunConfiguration,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_selected(window, cx);
    }

    pub(super) fn debug_configuration(
        &mut self,
        _: &DebugConfiguration,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.debug_selected(window, cx);
    }

    /// Runs `config` (of folder `folder`) in a terminal tab named after it
    /// (a rerun replaces the tab), started in that folder where the
    /// project lives.
    fn run_in_terminal(
        &mut self,
        config: Configuration,
        folder: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let resolved = match run_config::resolve(&self.project, folder, &config, &self.active(cx)) {
            Ok(resolved) => resolved,
            Err(err) => return self.report(format!("Cannot run {}: {err}", config.name), cx),
        };
        let project = self.project.clone();
        let task = cx.background_spawn(async move {
            let runner = run_config::python_runner(&project, folder);
            resolved
                .command(&runner)
                .map(|command| run_config::terminal_line(&project, folder, &command))
        });
        // The first folder is the terminal's default.
        let dir = (folder > 0).then(|| self.project.roots()[folder].path.clone());
        cx.spawn_in(window, async move |this, cx| {
            let line = task.await;
            this.update_in(cx, |this, window, cx| match line {
                Ok(line) => {
                    this.show_panel(PanelId::Terminal, cx);
                    this.terminal.update(cx, |terminal, cx| {
                        terminal.run_command(&config.name, &line, dir.as_deref(), window, cx)
                    });
                }
                Err(err) => this.report(format!("Cannot run {}: {err}", config.name), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Debugs `config` (of folder `folder`): builds the debug request
    /// (building cargo configurations first) and hands it to the debugger.
    fn debug(
        &mut self,
        config: Configuration,
        folder: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let resolved = match run_config::resolve(&self.project, folder, &config, &self.active(cx)) {
            Ok(resolved) => resolved,
            Err(err) => return self.report(format!("Cannot debug {}: {err}", config.name), cx),
        };
        if matches!(resolved.kind, run::Kind::Cargo { .. }) {
            self.report(format!("Building {} for debugging…", config.name), cx);
        }
        let project = self.project.clone();
        let task =
            cx.background_spawn(
                async move { run_config::debug_request(&project, folder, &resolved) },
            );
        self.hand_to_debugger(config.name, task, window, cx);
    }

    fn hand_to_debugger(
        &mut self,
        name: String,
        task: Task<Result<crate::debugger::DebugRequest, String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.runs._debug = Some(cx.spawn_in(window, async move |this, cx| {
            let request = task.await;
            this.update_in(cx, |this, window, cx| {
                this.runs._debug = None;
                match request {
                    Ok(request) => {
                        this.notice = None;
                        this.start_debugging(request, window, cx);
                    }
                    Err(err) => this.report(format!("Cannot debug {name}: {err}"), cx),
                }
            })
            .ok();
        }));
    }

    pub fn open_configuration_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.runs.picker.is_some() {
            self.runs.picker = None;
            self.focus_active(window, cx);
            return;
        }
        let configs = self.runs.configs.configurations.clone();
        let current = self.selected_configuration().map(|name| name.to_string());
        let picker = cx.new(|cx| ConfigurationPicker::new(&configs, current, cx));
        let subscription = cx.subscribe_in(&picker, window, |this, _, event, window, cx| {
            this.runs.picker = None;
            match event {
                ConfigurationPickerEvent::Select(name) => this.select_configuration_named(name, cx),
                ConfigurationPickerEvent::Edit => this.edit_configurations_file(window, cx),
                ConfigurationPickerEvent::Dismissed => {}
            }
            if !matches!(event, ConfigurationPickerEvent::Edit) {
                this.focus_active(window, cx);
            }
            cx.notify();
        });
        window.focus(&picker.focus_handle(cx));
        self.runs.picker = Some((picker, subscription));
        cx.notify();
    }

    pub(crate) fn select_configuration_named(&mut self, name: &str, cx: &mut Context<Self>) {
        self.runs.configs.selected = Some(name.to_owned());
        if let Err(err) = run_config::save_selected(&self.project.state_key(), name) {
            self.notice = Some(format!("Could not remember the configuration: {err}").into());
        }
        cx.notify();
    }

    pub(super) fn select_configuration(
        &mut self,
        _: &SelectConfiguration,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_configuration_picker(window, cx);
    }

    pub(super) fn edit_configurations(
        &mut self,
        _: &EditConfigurations,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit_configurations_file(window, cx);
    }

    /// Opens `.next_code/run.json` of the active file's folder (else the
    /// first), creating it (with an example and that folder's automatic
    /// configurations as templates) if missing.
    fn edit_configurations_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let folder = self
            .active_tab()
            .and_then(|tab| self.project.root_index_of(&tab.path))
            .unwrap_or(0);
        let path = self.project.resolve_in(folder, run::config::RUN_JSON);
        if matches!(self.project.kind(&path), Ok(None)) {
            let dir = self.project.resolve_in(folder, ".next_code");
            let created = self.project.create_dir_all(&dir).and_then(|()| {
                self.project.write(
                    &path,
                    run_config::template(&self.runs.configs, folder).into_bytes(),
                )
            });
            if let Err(err) = created {
                return self.report(format!("Cannot create {}: {err}", path.display()), cx);
            }
        }
        self.open(&path, window, cx);
    }

    pub(super) fn toggle_tests(
        &mut self,
        _: &ToggleTests,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_panel(PanelId::Tests, cx);
        let visible = self
            .layout
            .position_of(PanelId::Tests)
            .is_some_and(|position| {
                let dock = self.layout.dock(position);
                dock.open && dock.active == Some(PanelId::Tests)
            });
        if visible {
            self.runs.tests.read(cx).focus(window, cx);
        } else {
            self.focus_active(window, cx);
        }
    }

    /// Shows the Tests panel and runs `target` there.
    pub(crate) fn show_tests_and_run(&mut self, target: RunTarget, cx: &mut Context<Self>) {
        self.show_panel(PanelId::Tests, cx);
        self.runs
            .tests
            .update(cx, |tests, cx| tests.run(target, cx));
    }

    pub(super) fn run_all_tests(
        &mut self,
        _: &RunAllTests,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_tests_and_run(RunTarget::All, cx);
    }

    pub(super) fn run_failed_tests(
        &mut self,
        _: &RunFailedTests,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_tests_and_run(RunTarget::Failed, cx);
    }

    fn on_tests_event(
        &mut self,
        event: &TestsPanelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            TestsPanelEvent::Jump { path, line } => {
                self.record_position(cx);
                self.forward.clear();
                if let Some(editor) = self.open_editor(path, window, cx) {
                    editor.update(cx, |editor, cx| editor.go_to_line(*line, 0, cx));
                }
            }
            TestsPanelEvent::Debug(node, package_dirs) => {
                self.debug_test(node.as_ref().clone(), package_dirs.clone(), window, cx)
            }
            TestsPanelEvent::Changed => {
                self.refresh_test_markers(cx);
                cx.notify();
            }
        }
    }

    fn debug_test(
        &mut self,
        node: run::Node,
        package_dirs: Arc<HashMap<String, String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if node.tool == run::Tool::Cargo {
            self.report(format!("Building {} for debugging…", node.label), cx);
        }
        let project = self.project.clone();
        let name = node.label.clone();
        let task = cx
            .background_spawn(async move { engine::debug_request(&project, &node, &package_dirs) });
        self.hand_to_debugger(name, task, window, cx);
    }

    /// The Python panel's run buttons: scripts (of the first folder, whose
    /// project the panel shows) and the current file (in its folder) run
    /// as configurations in terminal tabs; tests run in the Tests panel.
    pub(super) fn run_python(
        &mut self,
        request: RunRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active = self.active_tab().and_then(|tab| {
            let editor = tab.editor()?.read(cx);
            let line = editor.buffer().line_of(editor.cursor());
            Some((tab.path.clone(), line, editor.text()))
        });
        let folder = active
            .as_ref()
            .and_then(|(path, _, _)| self.project.root_index_of(path))
            .unwrap_or(0);
        // Relative to its folder, as pytest and the interpreter there take it.
        let file = active
            .as_ref()
            .map(|(path, _, _)| crate::python_panel::project_relative(&self.project, path));
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
            RunRequest::Script(name) => self.run_in_terminal(
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
                (Some(file), Some((path, _, _))) => self.run_in_terminal(
                    configuration(
                        format!("Run {}", self.project.display_path(path)),
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
                _ => self.report(need_file(), cx),
            },
            RunRequest::Tests => self.show_tests_and_run(RunTarget::Pytest, cx),
            RunRequest::TestFile => match file {
                Some(file) => {
                    let id = engine::test_id(&self.project, folder, &file);
                    self.show_tests_and_run(RunTarget::Ids(vec![id]), cx)
                }
                None => self.report(need_file(), cx),
            },
            RunRequest::TestAtCursor => match (&active, &file) {
                (Some((_, line, text)), Some(file)) => match python::test_at(text, *line) {
                    Some(node) => {
                        let id = engine::test_id(&self.project, folder, &format!("{file}::{node}"));
                        self.show_tests_and_run(RunTarget::Ids(vec![id]), cx)
                    }
                    None => self.report("The cursor is not in a test function or class".into(), cx),
                },
                _ => self.report(need_file(), cx),
            },
        }
    }

    /// ▶ in the gutter of `editor` (showing `path`) at each discovered
    /// test, parametrized function and test class.
    pub(super) fn set_test_markers(
        &self,
        editor: &Entity<Editor>,
        path: &Path,
        cx: &mut Context<Self>,
    ) {
        let tests = self.runs.tests.read(cx);
        let markers = match tests.tree() {
            Some(tree) => {
                let summaries = tree.summarize(tests.results(), &Default::default());
                let mut markers: Vec<GutterMarker> =
                    engine::nodes_in_file(tree, &self.project, path)
                        .into_iter()
                        .filter_map(|(index, node)| {
                            let status = match summaries[index].status {
                                run::Status::Passed => " (passed)",
                                run::Status::Failed | run::Status::Error => " (failed)",
                                run::Status::Skipped => " (skipped)",
                                _ => "",
                            };
                            Some(GutterMarker {
                                line: node.line?,
                                kind: GutterMarkerKind::RunTest,
                                tooltip: format!("Run {}{status}", node.label).into(),
                            })
                        })
                        .collect();
                markers.sort_by_key(|marker| marker.line);
                markers.dedup_by_key(|marker| marker.line);
                markers
            }
            None => Vec::new(),
        };
        editor.update(cx, |editor, cx| editor.set_gutter_markers(markers, cx));
    }

    /// Re-marks every open editor (after discovery or a run).
    fn refresh_test_markers(&mut self, cx: &mut Context<Self>) {
        let editors: Vec<(Entity<Editor>, PathBuf)> = self
            .all_tabs()
            .filter_map(|tab| Some((tab.editor()?.clone(), tab.path.clone())))
            .collect();
        for (editor, path) in editors {
            self.set_test_markers(&editor, &path, cx);
        }
    }

    /// ▶ clicked: runs the test (or class, or parametrized function)
    /// defined on that line in the Tests panel.
    pub(super) fn gutter_marker_clicked(
        &mut self,
        editor: &Entity<Editor>,
        line: usize,
        kind: GutterMarkerKind,
        cx: &mut Context<Self>,
    ) {
        let GutterMarkerKind::RunTest = kind;
        let Some(path) = self
            .all_tabs()
            .find(|tab| tab.editor() == Some(editor))
            .map(|tab| tab.path.clone())
        else {
            return;
        };
        let id = self.runs.tests.read(cx).tree().and_then(|tree| {
            engine::nodes_in_file(tree, &self.project, &path)
                .into_iter()
                .find(|(_, node)| node.line == Some(line))
                .map(|(_, node)| node.id.clone())
        });
        if let Some(id) = id {
            self.show_tests_and_run(RunTarget::Ids(vec![id]), cx);
        }
    }

    pub(super) fn render_configuration_picker(&self) -> Option<impl IntoElement + use<>> {
        self.runs.picker().map(|picker| {
            div()
                .absolute()
                .top(px(72.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(picker.clone())
        })
    }

    #[cfg(test)]
    pub(crate) fn run_configurations(&self) -> &RunConfigs {
        &self.runs.configs
    }
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, WindowHandle};
    use serde_json::json;

    use super::*;

    fn project_dir(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("nc-runs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".vscode")).unwrap();
        std::fs::create_dir_all(root.join(".next_code")).unwrap();
        std::fs::write(
            root.join("pyproject.toml"),
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n[project.scripts]\ndemo = \"demo:main\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join(".vscode/launch.json"),
            r#"{
                // comments and trailing commas, as VS Code writes them
                "configurations": [
                    { "name": "Hello", "type": "debugpy", "request": "launch", "program": "${file}", "justMyCode": false, },
                    { "name": "Web", "type": "node", "request": "launch", "program": "index.js" },
                ],
            }"#,
        )
        .unwrap();
        std::fs::write(
            root.join(".next_code/run.json"),
            r#"{ "configurations": [ { "name": "Hello", "type": "shell", "command": "echo hi" } ] }"#,
        )
        .unwrap();
        root
    }

    fn cleanup(root: &Path) {
        std::fs::remove_dir_all(root).ok();
        if let Some(state) = crate::layout::state_root() {
            std::fs::remove_dir_all(state).ok();
        }
        if let Some(recent) = recent::path() {
            std::fs::remove_file(recent).ok();
        }
    }

    fn open_workspace(root: &Path, cx: &mut TestAppContext) -> WindowHandle<Workspace> {
        cx.update(|cx| {
            crate::editor::bind_keys(cx);
            super::super::bind_keys(cx);
        });
        let project = Project::local(root.to_path_buf());
        let window = cx.add_window(|window, cx| Workspace::new(project, None, window, cx));
        cx.run_until_parked();
        window
    }

    fn names(
        window: WindowHandle<Workspace>,
        cx: &mut TestAppContext,
    ) -> Vec<(String, Option<String>)> {
        window
            .read_with(cx, |workspace, _| {
                workspace
                    .run_configurations()
                    .configurations
                    .iter()
                    .map(|config| (config.name.clone(), config.disabled.clone()))
                    .collect()
            })
            .unwrap()
    }

    #[gpui::test]
    fn configurations_load_from_both_files_and_the_selection_persists(cx: &mut TestAppContext) {
        let root = project_dir("load");
        let window = open_workspace(&root, cx);
        let configs = names(window, cx);
        let listed: Vec<&str> = configs.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            listed,
            [
                "Hello",
                "Hello (.vscode/launch.json)",
                "Web",
                "Python: Current File",
                "pytest: All Tests",
                "uv run demo"
            ]
        );
        assert!(configs[2].1.as_deref().unwrap().contains("`node`"));
        // The first usable configuration until one is picked.
        let selected = |window: WindowHandle<Workspace>, cx: &mut TestAppContext| {
            window
                .read_with(cx, |workspace, _| workspace.selected_configuration())
                .unwrap()
                .map(|name| name.to_string())
        };
        assert_eq!(selected(window, cx).as_deref(), Some("Hello"));
        window
            .update(cx, |workspace, _, cx| {
                workspace.select_configuration_named("uv run demo", cx)
            })
            .unwrap();
        // Remembered for the project.
        let again = open_workspace(&root, cx);
        assert_eq!(selected(again, cx).as_deref(), Some("uv run demo"));
        cleanup(&root);
    }

    #[gpui::test]
    fn edit_configurations_creates_run_json_from_the_automatic_ones(cx: &mut TestAppContext) {
        let root = project_dir("edit");
        std::fs::remove_file(root.join(".next_code/run.json")).unwrap();
        std::fs::remove_dir(root.join(".next_code")).unwrap();
        let window = open_workspace(&root, cx);
        window
            .update(cx, |workspace, window, cx| {
                workspace.edit_configurations(&EditConfigurations, window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        let text = std::fs::read_to_string(root.join(".next_code/run.json")).unwrap();
        let parsed = run::config::parse_file(&text, Source::RunJson).unwrap();
        let parsed: Vec<&str> = parsed.iter().map(|config| config.name.as_str()).collect();
        assert_eq!(
            parsed,
            ["Python: Current File", "pytest: All Tests", "uv run demo"]
        );
        let active = window
            .read_with(cx, |workspace, _| {
                workspace.active_tab().map(|tab| tab.path.clone())
            })
            .unwrap();
        assert_eq!(active, Some(root.join(".next_code").join("run.json")));
        // The new file is read back as configurations.
        window
            .update(cx, |workspace, _, cx| workspace.reload_configurations(cx))
            .unwrap();
        cx.run_until_parked();
        assert!(
            names(window, cx)
                .iter()
                .any(|(name, _)| name == "uv run demo (automatic)")
        );
        cleanup(&root);
    }

    #[test]
    fn debug_requests_carry_the_resolved_launch_arguments() {
        let root = std::env::temp_dir().join(format!("nc-runs-debug-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let project = Project::local(root.clone());
        let configs = run::config::parse_file(
            r#"{ "configurations": [
                { "name": "App", "type": "python", "module": "app", "args": ["--port", "${env:NC_TEST_PORT}"],
                  "cwd": "sub", "env": { "MODE": "dev" }, "justMyCode": false },
                { "name": "Tests", "type": "pytest", "targets": ["${relativeFile}"] },
                { "name": "Attach", "type": "debugpy", "request": "attach", "connect": { "port": 5678 } },
                { "name": "Echo", "type": "shell", "command": "echo" }
            ] }"#,
            Source::RunJson,
        )
        .unwrap();
        // SAFETY: test-only variable, read back on this thread.
        unsafe { std::env::set_var("NC_TEST_PORT", "8000") };
        let file = root
            .join("tests")
            .join("test_x.py")
            .to_string_lossy()
            .into_owned();
        let active = Active {
            file: Some(file),
            line: Some(3),
        };
        let request = |config: &Configuration| {
            run_config::resolve(&project, 0, config, &active)
                .and_then(|resolved| run_config::debug_request(&project, 0, &resolved))
        };
        let app = request(&configs[0]).unwrap();
        assert_eq!(app.adapter, crate::debugger::DebugAdapterKind::Python);
        assert_eq!(app.request, crate::debugger::DebugRequestKind::Launch);
        let sub = root.join("sub").to_string_lossy().into_owned();
        assert_eq!(
            app.configuration,
            json!({
                "module": "app",
                "args": ["--port", "8000"],
                "cwd": sub,
                "env": { "MODE": "dev" },
                "justMyCode": false,
            })
        );
        let tests = request(&configs[1]).unwrap();
        let relative = if cfg!(windows) {
            r"tests\test_x.py"
        } else {
            "tests/test_x.py"
        };
        assert_eq!(tests.configuration["module"], "pytest");
        assert_eq!(tests.configuration["args"], json!([relative]));
        let attach = request(&configs[2]).unwrap();
        assert_eq!(attach.request, crate::debugger::DebugRequestKind::Attach);
        assert_eq!(attach.configuration["connect"]["port"], 5678);
        assert_eq!(
            request(&configs[3]).unwrap_err(),
            "shell configurations cannot be debugged"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
