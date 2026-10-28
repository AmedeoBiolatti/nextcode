//! The notebook's interpreter picker (the toolbar's kernel label): the
//! default interpreter, the virtualenvs of the notebook's folder and of
//! the project, Pythons uv knows, the kernelspecs the bridge lists, or a
//! typed path.

use std::path::{Path, PathBuf};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, MouseButton,
    SharedString, Subscription, Task, Window, actions, div, prelude::*, px,
};

use super::{
    NotebookDocument,
    kernel::{KernelLaunch, default_launch},
};
use crate::{
    editor::{self, Editor, EditorEvent},
    pinpoint::Pin,
    project::Project,
    selectable::selectable,
    theme,
};

actions!(interpreter_picker, [Dismiss]);

const CONTEXT: &str = "InterpreterPicker";
const ROW_HEIGHT: f32 = 46.;
const VISIBLE_ROWS: usize = 10;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Dismiss, Some(CONTEXT))]);
}

pub enum InterpreterPickerEvent {
    Picked(KernelLaunch),
    Dismissed,
}

impl EventEmitter<InterpreterPickerEvent> for InterpreterPicker {}

#[derive(Clone)]
struct Entry {
    label: SharedString,
    detail: SharedString,
    launch: KernelLaunch,
}

pub struct InterpreterPicker {
    query: Entity<Editor>,
    entries: Vec<Entry>,
    /// Entries matching the query (indices), after the typed-path row when
    /// there is one.
    rows: Vec<usize>,
    /// The query is a path: its row comes first.
    typed: Option<KernelLaunch>,
    selected: usize,
    current: Option<KernelLaunch>,
    cwd: PathBuf,
    /// Interpreters and kernelspecs still being listed.
    pending: usize,
    errors: Vec<SharedString>,
    _tasks: Vec<Task<()>>,
    _subscription: Subscription,
}

impl InterpreterPicker {
    pub fn new(document: Entity<NotebookDocument>, cx: &mut Context<Self>) -> Self {
        let query = cx.new(Editor::single_line);
        let subscription = cx.subscribe(&query, |this, _, event, cx| {
            if let EditorEvent::Edited = event {
                this.filter(cx);
            }
        });
        let (project, path, current) = {
            let document = document.read(cx);
            (
                document.project().clone(),
                document.path().to_path_buf(),
                document.launch().cloned(),
            )
        };
        let cwd = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let listing = {
            let (project, path) = (project.clone(), path.clone());
            cx.spawn(async move |this, cx| {
                let entries = cx
                    .background_spawn(async move { interpreters(&project, &path) })
                    .await;
                this.update(cx, |this, cx| {
                    this.pending -= 1;
                    this.add(entries, cx);
                })
                .ok();
            })
        };
        let specs = document.update(cx, |document, cx| document.kernel_specs(cx));
        let base = current.clone();
        let kernelspecs = cx.spawn(async move |this, cx| {
            let specs = specs.await;
            this.update(cx, |this, cx| {
                this.pending -= 1;
                match specs {
                    Ok(specs) => {
                        let python = base
                            .as_ref()
                            .or_else(|| this.entries.first().map(|entry| &entry.launch))
                            .map(|launch| launch.python.clone())
                            .unwrap_or_else(|| vec!["python3".into()]);
                        let entries = specs
                            .into_iter()
                            .map(|spec| Entry {
                                label: format!("Kernel: {}", spec.display_name).into(),
                                detail: format!("kernelspec {} ({})", spec.name, spec.language)
                                    .into(),
                                launch: KernelLaunch {
                                    python: python.clone(),
                                    kernel_name: Some(spec.name),
                                    cwd: this.cwd.clone(),
                                },
                            })
                            .collect();
                        this.add(entries, cx);
                    }
                    Err(err) => {
                        this.errors
                            .push(format!("Kernelspecs unavailable: {err}").into());
                        cx.notify();
                    }
                }
            })
            .ok();
        });
        Self {
            query,
            entries: Vec::new(),
            rows: Vec::new(),
            typed: None,
            selected: 0,
            current,
            cwd,
            pending: 2,
            errors: Vec::new(),
            _tasks: vec![listing, kernelspecs],
            _subscription: subscription,
        }
    }

    fn add(&mut self, entries: Vec<Entry>, cx: &mut Context<Self>) {
        for entry in entries {
            if !self
                .entries
                .iter()
                .any(|known| known.launch == entry.launch)
            {
                self.entries.push(entry);
            }
        }
        self.filter(cx);
    }

    fn filter(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text();
        let query = query.trim();
        self.typed = (query.contains('/') || query.contains('\\')).then(|| KernelLaunch {
            python: vec![query.to_owned()],
            kernel_name: None,
            cwd: self.cwd.clone(),
        });
        let needle = query.to_lowercase();
        self.rows = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.label.to_lowercase().contains(&needle)
                    || entry.detail.to_lowercase().contains(&needle)
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
        cx.notify();
    }

    fn row_count(&self) -> usize {
        self.rows.len() + usize::from(self.typed.is_some())
    }

    fn launch_at(&self, row: usize) -> Option<KernelLaunch> {
        match (&self.typed, row) {
            (Some(typed), 0) => Some(typed.clone()),
            (Some(_), row) => Some(self.entries[*self.rows.get(row - 1)?].launch.clone()),
            (None, row) => Some(self.entries[*self.rows.get(row)?].launch.clone()),
        }
    }

    fn select_prev(&mut self, _: &editor::Up, _: &mut Window, cx: &mut Context<Self>) {
        self.selected = self.selected.saturating_sub(1);
        cx.notify();
    }

    fn select_next(&mut self, _: &editor::Down, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected + 1 < self.row_count() {
            self.selected += 1;
            cx.notify();
        }
    }

    fn confirm(&mut self, _: &editor::Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.choose(self.selected, cx);
    }

    fn choose(&mut self, row: usize, cx: &mut Context<Self>) {
        if let Some(launch) = self.launch_at(row) {
            cx.emit(InterpreterPickerEvent::Picked(launch));
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(InterpreterPickerEvent::Dismissed);
    }
}

/// The default interpreter, virtualenvs, and the Pythons uv has installed
/// (blocks).
fn interpreters(project: &Project, notebook: &Path) -> Vec<Entry> {
    let default = default_launch(project, notebook);
    let cwd = default.cwd.clone();
    let mut entries = vec![Entry {
        label: "Default".into(),
        detail: default.label().into(),
        launch: default,
    }];
    let python = |path: PathBuf| KernelLaunch {
        python: vec![path.to_string_lossy().into_owned()],
        kernel_name: None,
        cwd: cwd.clone(),
    };
    if let Some(dir) = notebook.parent()
        && let Ok(Some(path)) = project.python_interpreter_in(dir)
    {
        entries.push(Entry {
            label: "Virtualenv of the notebook's folder".into(),
            detail: path.to_string_lossy().into_owned().into(),
            launch: python(path),
        });
    }
    if let Ok(Some(path)) = project.python_interpreter() {
        entries.push(Entry {
            label: "Project virtualenv".into(),
            detail: path.to_string_lossy().into_owned().into(),
            launch: python(path),
        });
    }
    if let Ok(Some(uv)) = project.find_executable("uv")
        && let Ok(output) =
            crate::python_env::run_uv(project, &uv.to_string_lossy(), &python::uv::python_list())
        && let Ok(found) = python::parse_python_list(&String::from_utf8_lossy(&output.stdout))
    {
        for interpreter in found {
            let Some(path) = interpreter.path else {
                continue;
            };
            entries.push(Entry {
                label: format!("{} {}", interpreter.implementation, interpreter.version).into(),
                detail: path.clone().into(),
                launch: python(PathBuf::from(path)),
            });
        }
    }
    for name in ["python3", "python"] {
        if let Ok(Some(path)) = project.find_executable(name) {
            entries.push(Entry {
                label: format!("{name} on PATH").into(),
                detail: path.to_string_lossy().into_owned().into(),
                launch: python(path),
            });
            break;
        }
    }
    entries
}

impl Focusable for InterpreterPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for InterpreterPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let first = (self.selected + 1).saturating_sub(VISIBLE_ROWS);
        let rows: Vec<(SharedString, SharedString, bool, String)> = self
            .typed
            .iter()
            .map(|typed| {
                (
                    format!("Use {}", typed.python.join(" ")).into(),
                    SharedString::from("the interpreter at this path"),
                    false,
                    "item:typed".to_owned(),
                )
            })
            .chain(self.rows.iter().map(|&index| {
                let entry = &self.entries[index];
                (
                    entry.label.clone(),
                    entry.detail.clone(),
                    self.current.as_ref() == Some(&entry.launch),
                    // Labels repeat (two venvs of one Python): the detail
                    // (its path) tells them apart.
                    format!("item:{}@{}", entry.label, entry.detail),
                )
            }))
            .collect();
        let status = if self.pending > 0 {
            Some(SharedString::from("Looking for interpreters…"))
        } else if rows.is_empty() {
            Some(SharedString::from("No interpreters found; type a path"))
        } else {
            None
        };
        div()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .on_mouse_down_out(
                cx.listener(|this, _, window, cx| this.dismiss(&Dismiss, window, cx)),
            )
            .w(px(460.))
            .flex()
            .flex_col()
            .bg(theme::panel())
            .border_1()
            .border_color(theme::border())
            .rounded_md()
            .shadow_lg()
            .text_sm()
            .child(
                div()
                    .px_3()
                    .pt_2()
                    .text_xs()
                    .text_color(theme::muted())
                    .child("Notebook interpreter (type to filter, or a path)"),
            )
            .child(
                div()
                    .h(px(36.))
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(self.query.clone())
                    .pin("input")
                    .label("Interpreter"),
            )
            .children(
                rows.into_iter()
                    .enumerate()
                    .skip(first)
                    .take(VISIBLE_ROWS)
                    .map(|(index, (label, detail, current, pin))| {
                        let name = label.clone();
                        div()
                            .id(index)
                            .h(px(ROW_HEIGHT))
                            .px_3()
                            .flex()
                            .flex_col()
                            .justify_center()
                            .cursor_pointer()
                            .when(index == self.selected, |row| row.bg(theme::active_row()))
                            .when(index != self.selected, |row| {
                                row.hover(|style| style.bg(theme::hover_row()))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| this.choose(index, cx)))
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(div().flex_1().text_sm().truncate().child(label))
                                    .when(current, |row| {
                                        row.child(
                                            div()
                                                .text_xs()
                                                .text_color(theme::muted())
                                                .child("current"),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .truncate()
                                    .child(detail),
                            )
                            .pin(pin)
                            .label(name)
                    }),
            )
            .children(status.map(|status| {
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(status)
                    .pin("status")
                    .interactive(false)
            }))
            .children(self.errors.iter().enumerate().map(|(index, error)| {
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(theme::error())
                    .child(selectable(("picker-error", index), error.clone()))
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .pin("interpreter-picker")
            .label("Notebook interpreter")
    }
}
