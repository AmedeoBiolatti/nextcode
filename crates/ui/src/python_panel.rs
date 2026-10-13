//! Python panel: the uv project (`pyproject.toml`, `uv.lock`), its
//! environment and interpreter, dependency edits, and run/test commands.
//! uv runs where the project lives (locally, in WSL or in a container).

use std::{io, path::Path, time::Duration};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Subscription, Task,
    Window, div, prelude::*, px, rgb,
};
use python::{Dependency, DependencyKind, Interpreter, Lock, PyProject};
use remote::RunOutput;
use serde::Serialize;

use crate::{
    editor::{self, Editor, EditorEvent},
    project::Project,
    theme,
};

/// Long enough for a first `uv sync` downloading a Python and wheels.
const UV_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Command output lines kept in the panel.
const OUTPUT_LINES: usize = 12;

/// What the panel knows about the project.
#[derive(Debug, Clone, Serialize)]
pub struct PythonInfo {
    pub name: Option<String>,
    pub version: Option<String>,
    pub requires_python: Option<String>,
    #[serde(skip)]
    pub project: PyProject,
    #[serde(skip)]
    pub dependencies: Vec<Dependency>,
    pub locked_packages: Option<usize>,
    /// The environment's interpreter (`.venv`), if any.
    pub interpreter: Option<String>,
    /// From `.venv/pyvenv.cfg`.
    pub environment_python: Option<String>,
    /// `.python-version`.
    pub pinned: Option<String>,
    /// Where `uv` is, if installed.
    pub uv: Option<String>,
    /// `uv sync --check`: `None` without uv or for non-uv projects.
    pub in_sync: Option<bool>,
}

/// Reads the project's Python state. `None` without a `pyproject.toml`.
/// Blocks (several reads and a `uv sync --check`).
pub fn load(project: &Project) -> io::Result<Option<PythonInfo>> {
    let read = |name: &str| match project.read_text(&project.resolve(name)) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    };
    let Some(text) = read("pyproject.toml")? else {
        return Ok(None);
    };
    let pyproject = PyProject::parse(&text)?;
    let lock = read("uv.lock")?
        .map(|text| Lock::parse(&text))
        .transpose()?;
    let environment_python = read(".venv/pyvenv.cfg")?.and_then(|cfg| {
        cfg.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            matches!(key.trim(), "version_info" | "version").then(|| value.trim().to_owned())
        })
    });
    let pinned = read(".python-version")?
        .and_then(|text| text.lines().next().map(|line| line.trim().to_owned()))
        .filter(|line| !line.is_empty());
    let uv = project
        .find_executable("uv")?
        .map(|path| path.to_string_lossy().into_owned());
    let uv_project = lock.is_some() || pyproject.uses_uv();
    let in_sync = match &uv {
        Some(uv) if uv_project => {
            let output = run_uv(project, uv, &python::uv::sync_check())?;
            Some(output.code == Some(0))
        }
        _ => None,
    };
    Ok(Some(PythonInfo {
        name: pyproject.name.clone(),
        version: pyproject.version.clone(),
        requires_python: pyproject.requires_python.clone(),
        dependencies: pyproject.dependencies(lock.as_ref()),
        locked_packages: lock.as_ref().map(Lock::len),
        interpreter: project
            .python_interpreter()?
            .map(|path| path.to_string_lossy().into_owned()),
        environment_python,
        pinned,
        uv,
        in_sync,
        project: pyproject,
    }))
}

/// Runs `uv` with `args` in the project root. Blocks.
pub fn run_uv(project: &Project, uv: &str, args: &[String]) -> io::Result<RunOutput> {
    project.run_program(
        uv,
        args,
        &[("NO_COLOR", "1"), ("UV_NO_PROGRESS", "1")],
        None,
        UV_TIMEOUT,
    )
}

/// `stderr` then `stdout` of a uv run, as display lines.
fn output_lines(output: &RunOutput) -> Vec<String> {
    let mut lines: Vec<String> = String::from_utf8_lossy(&output.stderr)
        .lines()
        .chain(String::from_utf8_lossy(&output.stdout).lines())
        .map(str::to_owned)
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.len() > OUTPUT_LINES {
        lines.drain(..lines.len() - OUTPUT_LINES);
    }
    lines
}

/// What the user asked to run; the workspace knows the active file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunRequest {
    Script(String),
    CurrentFile,
    Tests,
    TestFile,
    TestAtCursor,
}

pub enum PythonPanelEvent {
    Run(RunRequest),
    /// `.venv` may have changed: language servers re-read the interpreter.
    EnvironmentChanged,
    /// Whether the environment is out of date with `uv.lock`/`pyproject.toml`.
    SyncNeeded(bool),
}

impl EventEmitter<PythonPanelEvent> for PythonPanel {}

enum State {
    Loading,
    NoProject,
    Ready(Box<PythonInfo>),
    Failed(SharedString),
}

pub struct PythonPanel {
    project: Project,
    state: State,
    add_input: Entity<Editor>,
    add_kind: DependencyKind,
    /// The uv command running, if any.
    busy: Option<SharedString>,
    /// The last command's output tail, and whether it failed.
    output: Vec<SharedString>,
    failed: bool,
    /// Shown while choosing a Python to pin.
    interpreters: Option<Vec<Interpreter>>,
    focus_handle: FocusHandle,
    _load: Task<()>,
    _operation: Task<()>,
    _subscription: Subscription,
}

impl PythonPanel {
    pub fn new(project: Project, cx: &mut Context<Self>) -> Self {
        let add_input = cx.new(Editor::single_line);
        let subscription = cx.subscribe(&add_input, |_, _, _: &EditorEvent, cx| cx.notify());
        let mut panel = Self {
            project,
            state: State::Loading,
            add_input,
            add_kind: DependencyKind::Main,
            busy: None,
            output: Vec::new(),
            failed: false,
            interpreters: None,
            focus_handle: cx.focus_handle(),
            _load: Task::ready(()),
            _operation: Task::ready(()),
            _subscription: subscription,
        };
        panel.refresh(cx);
        panel
    }

    /// Re-reads the project (after `pyproject.toml`, `uv.lock` or
    /// `.python-version` changed).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let project = self.project.clone();
        self._load = cx.spawn(async move |this, cx| {
            let loaded = cx.background_spawn(async move { load(&project) }).await;
            this.update(cx, |this, cx| {
                let sync_needed = matches!(&loaded, Ok(Some(info)) if info.in_sync == Some(false));
                this.state = match loaded {
                    Ok(Some(info)) => State::Ready(Box::new(info)),
                    Ok(None) => State::NoProject,
                    Err(err) => State::Failed(err.to_string().into()),
                };
                cx.emit(PythonPanelEvent::SyncNeeded(sync_needed));
                cx.notify();
            })
            .ok();
        });
    }

    fn uv(&self) -> Option<String> {
        match &self.state {
            State::Ready(info) => info.uv.clone(),
            _ => None,
        }
    }

    /// Runs a uv command off the main thread, then re-reads the project.
    fn run(
        &mut self,
        label: impl Into<SharedString>,
        args: Vec<String>,
        changes_environment: bool,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        let Some(uv) = self.uv() else {
            self.output = vec!["uv is not installed where the project lives".into()];
            self.failed = true;
            return cx.notify();
        };
        let label = label.into();
        self.busy = Some(label.clone());
        self.output.clear();
        self.failed = false;
        cx.notify();
        let project = self.project.clone();
        self._operation = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { run_uv(&project, &uv, &args) })
                .await;
            this.update(cx, |this, cx| {
                this.busy = None;
                match result {
                    Ok(output) => {
                        this.failed = output.code != Some(0);
                        this.output = output_lines(&output)
                            .into_iter()
                            .map(SharedString::from)
                            .collect();
                        if this.output.is_empty() {
                            this.output.push(format!("{label}: done").into());
                        }
                    }
                    Err(err) => {
                        this.failed = true;
                        this.output = vec![err.to_string().into()];
                    }
                }
                if changes_environment {
                    cx.emit(PythonPanelEvent::EnvironmentChanged);
                }
                this.refresh(cx);
                cx.notify();
            })
            .ok();
        });
    }

    pub fn sync(&mut self, cx: &mut Context<Self>) {
        self.run("uv sync", python::uv::sync(), true, cx);
    }

    fn add(&mut self, _: &editor::Newline, _: &mut Window, cx: &mut Context<Self>) {
        let requirement = self.add_input.read(cx).text().trim().to_owned();
        if requirement.is_empty() {
            return;
        }
        self.add_input
            .update(cx, |input, cx| input.set_text("", cx));
        let args = python::uv::add(&requirement, &self.add_kind);
        self.run(format!("uv add {requirement}"), args, true, cx);
    }

    /// Where new dependencies go: the project's sections, then `dev`.
    fn kinds(&self) -> Vec<DependencyKind> {
        let mut kinds = vec![DependencyKind::Main, DependencyKind::Group("dev".into())];
        if let State::Ready(info) = &self.state {
            kinds.extend(
                info.project
                    .groups
                    .keys()
                    .map(|group| DependencyKind::Group(group.clone())),
            );
            kinds.extend(
                info.project
                    .optional
                    .keys()
                    .map(|extra| DependencyKind::Optional(extra.clone())),
            );
        }
        kinds.dedup();
        kinds
    }

    fn cycle_kind(&mut self, cx: &mut Context<Self>) {
        let kinds = self.kinds();
        let next = kinds
            .iter()
            .position(|kind| *kind == self.add_kind)
            .map_or(0, |index| (index + 1) % kinds.len());
        self.add_kind = kinds[next].clone();
        cx.notify();
    }

    fn choose_python(&mut self, cx: &mut Context<Self>) {
        if self.interpreters.take().is_some() {
            return cx.notify();
        }
        let (Some(uv), project) = (self.uv(), self.project.clone()) else {
            return;
        };
        self._operation = cx.spawn(async move |this, cx| {
            let listed = cx
                .background_spawn(async move {
                    let output = run_uv(&project, &uv, &python::uv::python_list())?;
                    python::parse_python_list(&String::from_utf8_lossy(&output.stdout))
                })
                .await;
            this.update(cx, |this, cx| {
                match listed {
                    Ok(interpreters) => this.interpreters = Some(interpreters),
                    Err(err) => {
                        this.failed = true;
                        this.output = vec![err.to_string().into()];
                    }
                }
                cx.notify();
            })
            .ok();
        });
    }

    fn pin(&mut self, version: String, cx: &mut Context<Self>) {
        self.interpreters = None;
        self.run(
            format!("uv python pin {version}"),
            python::uv::pin(&version),
            false,
            cx,
        );
    }

    fn render_header(&self, info: &PythonInfo) -> impl IntoElement {
        let title = match (&info.name, &info.version) {
            (Some(name), Some(version)) => format!("{name} {version}"),
            (Some(name), None) => name.clone(),
            _ => "Python project".into(),
        };
        let environment = match (&info.environment_python, &info.interpreter) {
            (Some(version), Some(_)) => format!("Python {version} · .venv"),
            (None, Some(path)) => path.clone(),
            _ => "No environment yet: Sync creates .venv".into(),
        };
        let mut details = Vec::new();
        if let Some(requires) = &info.requires_python {
            details.push(format!("requires {requires}"));
        }
        if let Some(pinned) = &info.pinned {
            details.push(format!("pinned {pinned}"));
        }
        if let Some(locked) = info.locked_packages {
            details.push(format!("{locked} locked"));
        }
        div()
            .px_2()
            .py_1()
            .flex()
            .flex_col()
            .child(div().text_sm().text_color(rgb(theme::TEXT)).child(title))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(environment),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(details.join(" · ")),
            )
    }

    fn button(id: &'static str, label: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .px_2()
            .rounded_sm()
            .border_1()
            .border_color(rgb(theme::BORDER))
            .text_xs()
            .text_color(rgb(theme::TEXT))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(theme::HOVER_ROW)))
            .child(label.into())
    }

    fn section(title: &str) -> gpui::Div {
        div()
            .px_2()
            .pt_2()
            .text_xs()
            .text_color(rgb(theme::MUTED))
            .child(title.to_uppercase())
    }

    fn row(id: impl Into<gpui::ElementId>) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .h(px(22.))
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .text_sm()
            .hover(|style| style.bg(rgb(theme::HOVER_ROW)))
    }

    fn render_ready(&self, info: &PythonInfo, cx: &mut Context<Self>) -> gpui::AnyElement {
        let busy = self.busy.is_some();
        let actions = div()
            .px_2()
            .flex()
            .flex_wrap()
            .gap_1()
            .child(
                Self::button("py-sync", "Sync")
                    .on_click(cx.listener(|this, _, _, cx| this.sync(cx))),
            )
            .child(
                Self::button("py-lock", "Lock").on_click(cx.listener(|this, _, _, cx| {
                    this.run("uv lock", python::uv::lock(false), false, cx)
                })),
            )
            .child(
                Self::button("py-upgrade", "Upgrade all").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.run("uv lock --upgrade", python::uv::lock(true), false, cx)
                    },
                )),
            )
            .child(
                Self::button("py-pin", "Python…")
                    .on_click(cx.listener(|this, _, _, cx| this.choose_python(cx))),
            )
            .child(
                Self::button("py-refresh", "⟳")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            );
        let status = match (info.in_sync, &info.uv) {
            (_, None) => Some(("uv is not installed where the project lives", theme::ERROR)),
            (Some(false), _) => Some((
                "Environment out of date: Sync to update .venv",
                theme::WARNING,
            )),
            (Some(true), _) => Some(("Environment up to date", theme::MUTED)),
            (None, _) => None,
        };

        let interpreters = self.interpreters.as_ref().map(|interpreters| {
            div()
                .flex()
                .flex_col()
                .child(Self::section("Pin a Python version"))
                .children(interpreters.iter().enumerate().map(|(index, interpreter)| {
                    let version = interpreter.version.clone();
                    let detail = interpreter
                        .path
                        .clone()
                        .unwrap_or_else(|| "download".into());
                    Self::row(("py-python", index))
                        .cursor_pointer()
                        .child(
                            div()
                                .text_color(rgb(theme::TEXT))
                                .child(format!("{} {}", interpreter.implementation, version)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(rgb(theme::MUTED))
                                .child(detail),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.pin(version.clone(), cx)))
                }))
        });

        let run = |id: &'static str, label: String, request: RunRequest| {
            Self::row(id)
                .cursor_pointer()
                .child(div().text_color(rgb(theme::ACCENT_TEXT)).child("▶"))
                .child(div().text_color(rgb(theme::TEXT)).child(label))
                .on_click(
                    cx.listener(move |_, _, _, cx| cx.emit(PythonPanelEvent::Run(request.clone()))),
                )
        };
        let scripts = info
            .project
            .scripts
            .keys()
            .enumerate()
            .map(|(index, name)| {
                let request = RunRequest::Script(name.clone());
                Self::row(("py-script", index))
                    .cursor_pointer()
                    .child(div().text_color(rgb(theme::ACCENT_TEXT)).child("▶"))
                    .child(div().text_color(rgb(theme::TEXT)).child(name.clone()))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::MUTED))
                            .child("script"),
                    )
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(PythonPanelEvent::Run(request.clone()))
                    }))
                    .into_any_element()
            });

        let mut kinds: Vec<&DependencyKind> = Vec::new();
        for dependency in &info.dependencies {
            if !kinds.contains(&&dependency.kind) {
                kinds.push(&dependency.kind);
            }
        }
        let dependencies = kinds.into_iter().flat_map(|kind| {
            let header = Self::section(&kind.label()).into_any_element();
            let rows = info
                .dependencies
                .iter()
                .enumerate()
                .filter(move |(_, dependency)| dependency.kind == *kind)
                .map(|(index, dependency)| {
                    let name = dependency.requirement.name.clone();
                    let (remove_name, remove_kind) = (name.clone(), dependency.kind.clone());
                    let upgrade_name = name.clone();
                    Self::row(("py-dep", index))
                        .child(div().text_color(rgb(theme::TEXT)).child(name.clone()))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(rgb(theme::MUTED))
                                .child(dependency.requirement.specifier.clone()),
                        )
                        .children(dependency.locked.clone().map(|version| {
                            div()
                                .text_xs()
                                .text_color(rgb(theme::ACCENT_TEXT))
                                .child(version)
                        }))
                        .child(
                            div()
                                .id(("py-dep-upgrade", index))
                                .px_1()
                                .text_color(rgb(theme::MUTED))
                                .hover(|style| style.text_color(rgb(theme::TEXT)))
                                .cursor_pointer()
                                .child("↑")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.run(
                                        format!("uv lock --upgrade-package {upgrade_name}"),
                                        python::uv::upgrade_package(&upgrade_name),
                                        false,
                                        cx,
                                    )
                                })),
                        )
                        .child(
                            div()
                                .id(("py-dep-remove", index))
                                .px_1()
                                .text_color(rgb(theme::MUTED))
                                .hover(|style| style.text_color(rgb(theme::ERROR)))
                                .cursor_pointer()
                                .child("×")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.run(
                                        format!("uv remove {remove_name}"),
                                        python::uv::remove(&remove_name, &remove_kind),
                                        true,
                                        cx,
                                    )
                                })),
                        )
                        .into_any_element()
                });
            std::iter::once(header).chain(rows).collect::<Vec<_>>()
        });

        div()
            .flex()
            .flex_col()
            .child(self.render_header(info))
            .child(actions)
            .children(status.map(|(text, color)| {
                div()
                    .px_2()
                    .pt_1()
                    .text_xs()
                    .text_color(rgb(color))
                    .child(text)
            }))
            .children(self.busy.clone().map(|label| {
                div()
                    .px_2()
                    .pt_1()
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(format!("{label}…"))
            }))
            .when(!self.output.is_empty() && !busy, |panel| {
                panel.child(
                    div()
                        .mx_2()
                        .mt_1()
                        .p_1()
                        .rounded_sm()
                        .bg(rgb(theme::BACKGROUND))
                        .font_family(theme::MONO_FONT)
                        .text_xs()
                        .text_color(rgb(if self.failed {
                            theme::ERROR
                        } else {
                            theme::MUTED
                        }))
                        .children(self.output.iter().map(|line| div().child(line.clone()))),
                )
            })
            .children(interpreters)
            .child(Self::section("Run"))
            .children(scripts)
            .child(run(
                "py-run-file",
                "Current file".into(),
                RunRequest::CurrentFile,
            ))
            .child(run("py-tests", "All tests".into(), RunRequest::Tests))
            .child(run(
                "py-test-file",
                "Tests in current file".into(),
                RunRequest::TestFile,
            ))
            .child(run(
                "py-test-cursor",
                "Test at cursor".into(),
                RunRequest::TestAtCursor,
            ))
            .child(Self::section("Add dependency"))
            .child(
                div()
                    .px_2()
                    .py_1()
                    .flex()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h(px(26.))
                            .px_2()
                            .py_1()
                            .border_1()
                            .border_color(rgb(theme::BORDER))
                            .rounded_sm()
                            .bg(rgb(theme::BACKGROUND))
                            .child(self.add_input.clone()),
                    )
                    .child(
                        Self::button("py-add-kind", self.add_kind.label())
                            .on_click(cx.listener(|this, _, _, cx| this.cycle_kind(cx))),
                    ),
            )
            .children(dependencies)
            .into_any_element()
    }
}

impl Focusable for PythonPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PythonPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.state {
            State::Loading => div()
                .p_2()
                .text_sm()
                .text_color(rgb(theme::MUTED))
                .child("Reading the project…")
                .into_any_element(),
            State::NoProject => div()
                .p_2()
                .text_sm()
                .text_color(rgb(theme::MUTED))
                .child("No pyproject.toml in this folder. `uv init` makes one.")
                .into_any_element(),
            State::Failed(error) => div()
                .p_2()
                .text_sm()
                .text_color(rgb(theme::ERROR))
                .child(error.clone())
                .into_any_element(),
            State::Ready(info) => {
                let info = info.clone();
                self.render_ready(&info, cx)
            }
        };
        div()
            .key_context("PythonPanel")
            .on_action(cx.listener(Self::add))
            .track_focus(&self.focus_handle)
            .id("python-panel")
            .size_full()
            .overflow_y_scroll()
            .bg(rgb(theme::PANEL))
            .child(body)
    }
}

/// A path inside the project, as uv and pytest take it (root-relative,
/// `/`-separated), or as is when outside.
pub fn project_relative(project: &Project, path: &Path) -> String {
    match path.strip_prefix(project.root()) {
        Ok(relative) => relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}
