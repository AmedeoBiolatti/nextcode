//! Create a local project and open it as a project tab.

use crate::{
    commit_panel::{Check, checkbox},
    editor::{Editor, EditorEvent},
    git_dialogs::button,
    pinpoint::Pin,
    project::{Location, Project},
    selectable::selectable,
    theme,
};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, MouseButton,
    PathPromptOptions, SharedString, Subscription, Window, actions, div, prelude::*, px,
};
use std::{path::PathBuf, time::Duration};

mod files;
use files::Template;

actions!(
    new_project,
    [Submit, Dismiss, NextField, PreviousField, Activate]
);
const CONTEXT: &str = "NewProject";
const TEMPLATE_START: usize = 3;
const UV_FIELD: usize = TEMPLATE_START + Template::ALL.len();
const CANCEL_FIELD: usize = UV_FIELD + 1;
const CREATE_FIELD: usize = CANCEL_FIELD + 1;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", Dismiss, Some(CONTEXT)),
        KeyBinding::new("enter", Submit, Some("NewProject > Editor")),
        KeyBinding::new("tab", NextField, Some("NewProject > Editor")),
        KeyBinding::new("shift-tab", PreviousField, Some("NewProject > Editor")),
        KeyBinding::new("tab", NextField, Some("NewProjectControl")),
        KeyBinding::new("shift-tab", PreviousField, Some("NewProjectControl")),
        KeyBinding::new("enter", Activate, Some("NewProjectControl")),
        KeyBinding::new("space", Activate, Some("NewProjectControl")),
    ]);
}

pub enum NewProjectEvent {
    Created(Location),
    Dismissed,
}
impl EventEmitter<NewProjectEvent> for NewProject {}

pub struct NewProject {
    name: Entity<Editor>,
    parent: Entity<Editor>,
    template: Template,
    use_uv: bool,
    // Name, parent, Browse, templates, uv, Cancel, Create.
    handles: Vec<FocusHandle>,
    error: Option<SharedString>,
    busy: bool,
    _subscriptions: Vec<Subscription>,
}

impl NewProject {
    pub fn new(parent: PathBuf, cx: &mut Context<Self>) -> Self {
        let name = cx.new(Editor::single_line);
        let parent = cx.new(|cx| {
            let mut editor = Editor::single_line(cx);
            editor.set_text(&parent.to_string_lossy(), cx);
            editor
        });
        let mut handles = vec![name.focus_handle(cx), parent.focus_handle(cx)];
        handles.extend((2..=CREATE_FIELD).map(|_| cx.focus_handle()));
        let subscriptions = [&name, &parent]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |this: &mut Self, _, event, cx| {
                    if matches!(event, EditorEvent::Edited) {
                        this.error = None;
                        cx.notify();
                    }
                })
            })
            .collect();
        Self {
            name,
            parent,
            template: Template::Empty,
            use_uv: false,
            handles,
            error: None,
            busy: false,
            _subscriptions: subscriptions,
        }
    }

    fn parent_path(&self, cx: &App) -> PathBuf {
        let text = self.parent.read(cx).text();
        let text = text.trim();
        if (text == "~" || text.starts_with("~/") || text.starts_with("~\\"))
            && let Some(home) = dirs::home_dir()
        {
            return if text == "~" {
                home
            } else {
                home.join(&text[2..])
            };
        }
        PathBuf::from(text)
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        if !self.busy {
            cx.emit(NewProjectEvent::Dismissed);
        }
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose Parent Folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| match paths.await {
            Ok(Ok(Some(paths))) => {
                if let Some(path) = paths.first() {
                    this.update_in(cx, |this, window, cx| {
                        this.parent
                            .update(cx, |input, cx| input.set_text(&path.to_string_lossy(), cx));
                        this.error = None;
                        window.focus(&this.handles[0]);
                        cx.notify();
                    })
                    .ok();
                }
            }
            Ok(Err(error)) => {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("Could not open the folder picker: {error}").into());
                    cx.notify();
                })
                .ok();
            }
            _ => {}
        })
        .detach();
    }

    fn submit(&mut self, _: &Submit, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let name = self.name.read(cx).text();
        let parent = self.parent_path(cx);
        if let Err(error) = files::destination(&parent, &name) {
            self.error = Some(error.into());
            cx.notify();
            return;
        }
        self.busy = true;
        self.error = None;
        let template = self.template;
        let use_uv = self.use_uv && template == Template::Python;
        let task = cx.background_spawn(async move {
            if !use_uv {
                return files::create(&parent, &name, template);
            }
            let uv = worktree::find_executable("uv", &parent).ok_or_else(|| {
                "uv was not found. Install uv, or uncheck Initialize with uv to create a plain Python project.".to_owned()
            })?;
            files::create_with_uv(&parent, &name, |root, package| {
                let project = Project::local(root.to_path_buf());
                let mut args = python::uv::init(package);
                args.push(root.to_string_lossy().into_owned());
                let output = project.run_program(
                    &uv.to_string_lossy(), &args,
                    &[("NO_COLOR", "1"), ("UV_NO_PROGRESS", "1")],
                    None, Duration::from_secs(30),
                ).map_err(|error| format!("Could not run uv init: {error}"))?;
                if output.code == Some(0) { return Ok(()); }
                let message = String::from_utf8_lossy(&output.stderr);
                let message: String = message.trim().chars().take(2000).collect();
                Err(if message.is_empty() {
                    format!("uv init failed (exit code {:?}).", output.code)
                } else { format!("uv init failed: {message}") })
            })
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(path) => cx.emit(NewProjectEvent::Created(Location::Local(path))),
                    Err(error) => this.error = Some(error.into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn move_focus(&mut self, by: isize, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .handles
            .iter()
            .position(|handle| handle.contains_focused(window, cx))
            .unwrap_or(0);
        let mut next = (current as isize + by).rem_euclid(self.handles.len() as isize) as usize;
        if next == UV_FIELD && self.template != Template::Python {
            next = (next as isize + by).rem_euclid(self.handles.len() as isize) as usize;
        }
        window.focus(&self.handles[next]);
        cx.notify();
    }
    fn next_field(&mut self, _: &NextField, window: &mut Window, cx: &mut Context<Self>) {
        self.move_focus(1, window, cx);
    }
    fn previous_field(&mut self, _: &PreviousField, window: &mut Window, cx: &mut Context<Self>) {
        self.move_focus(-1, window, cx);
    }
    fn activate(&mut self, _: &Activate, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        match self
            .handles
            .iter()
            .position(|handle| handle.contains_focused(window, cx))
        {
            Some(2) => self.browse(window, cx),
            Some(index) if (TEMPLATE_START..UV_FIELD).contains(&index) => {
                self.template = Template::ALL[index - TEMPLATE_START];
                cx.notify();
            }
            Some(UV_FIELD) if self.template == Template::Python => self.toggle_uv(cx),
            Some(CANCEL_FIELD) => self.dismiss(&Dismiss, window, cx),
            Some(CREATE_FIELD) => self.submit(&Submit, window, cx),
            _ => {}
        }
    }

    fn toggle_uv(&mut self, cx: &mut Context<Self>) {
        if !self.busy {
            self.use_uv = !self.use_uv;
            self.error = None;
            cx.notify();
        }
    }

    fn input(
        &self,
        id: &'static str,
        label: &'static str,
        editor: Entity<Editor>,
    ) -> impl IntoElement {
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_color(theme::muted()).child(label))
            .child(
                div()
                    .h(px(30.))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(theme::border())
                    .bg(theme::background())
                    .child(editor)
                    .pin(id)
                    .label(label),
            )
    }
}

impl Focusable for NewProject {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.handles[0].clone()
    }
}

impl Render for NewProject {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let path = files::destination(&self.parent_path(cx), &self.name.read(cx).text()).ok();
        div()
            .id("new-project")
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::next_field))
            .on_action(cx.listener(Self::previous_field))
            .on_action(cx.listener(Self::activate))
            .on_mouse_down_out(
                cx.listener(|this, _, window, cx| this.dismiss(&Dismiss, window, cx)),
            )
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .w_full()
            .max_w(px(560.))
            .max_h((window.viewport_size().height - px(100.)).max(px(100.)))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .bg(theme::panel())
            .border_1()
            .border_color(theme::border())
            .rounded_md()
            .shadow_lg()
            .text_sm()
            .text_color(theme::text())
            .child(
                div()
                    .flex_none()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(div().text_lg().child("New Project"))
                    .child(self.input("name", "Project name", self.name.clone()))
                    .child(
                        div()
                            .flex()
                            .items_end()
                            .gap_2()
                            .child(self.input("parent", "Parent folder", self.parent.clone()))
                            .child(
                                button("browse", "Browse…".into(), false)
                                    .key_context("NewProjectControl")
                                    .track_focus(&self.handles[2])
                                    .when(self.handles[2].is_focused(window), |button| {
                                        button.bg(theme::active_row())
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        window.focus(&this.handles[2]);
                                        this.browse(window, cx);
                                    }))
                                    .pin("browse")
                                    .label("Choose parent folder"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(div().text_color(theme::muted()).child("Starter"))
                            .child(
                                div().flex().flex_wrap().gap_2().children(
                                    Template::ALL.into_iter().enumerate().map(
                                        |(index, template)| {
                                            let handle = &self.handles[index + TEMPLATE_START];
                                            button(
                                                template.label(),
                                                template.label().into(),
                                                self.template == template,
                                            )
                                            .key_context("NewProjectControl")
                                            .track_focus(handle)
                                            .border_1()
                                            .border_color(if handle.is_focused(window) {
                                                theme::accent_text()
                                            } else {
                                                theme::border()
                                            })
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                if !this.busy {
                                                    this.template = template;
                                                    window.focus(
                                                        &this.handles[index + TEMPLATE_START],
                                                    );
                                                    cx.notify();
                                                }
                                            }))
                                            .pin(template.label())
                                            .label(template.label())
                                        },
                                    ),
                                ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(self.template.description()),
                            ),
                    )
                    .when(self.template == Template::Python, |dialog| {
                        dialog.child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .id("initialize-uv")
                                .key_context("NewProjectControl")
                                .track_focus(&self.handles[UV_FIELD])
                                .h(px(26.))
                                .px_1()
                                .flex()
                                .items_center()
                                .gap_2()
                                .rounded_sm()
                                .border_1()
                                .border_color(if self.handles[UV_FIELD].is_focused(window) {
                                    theme::accent()
                                } else {
                                    gpui::transparent_black()
                                })
                                .cursor_pointer()
                                .child(checkbox(if self.use_uv {
                                    Check::Checked
                                } else {
                                    Check::Unchecked
                                }))
                                .child("Initialize with uv")
                                .on_click(cx.listener(
                                    |this, event: &gpui::ClickEvent, window, cx| {
                                        if event.is_keyboard() {
                                            return;
                                        }
                                        window.focus(&this.handles[UV_FIELD]);
                                        this.toggle_uv(cx);
                                    },
                                ))
                                .pin("initialize-uv")
                                .label("Initialize with uv"),
                        )
                        .child(div().text_xs().text_color(theme::muted()).child(
                            "Use uv init to create the Python starter and pin its Python version.",
                        )),
                )
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted())
                            .overflow_hidden()
                            .child(selectable(
                                "destination",
                                path.map(|p| format!("Create in {}", p.display()))
                                    .unwrap_or_else(|| {
                                        "Enter a name and choose a parent folder.".into()
                                    }),
                            ))
                            .pin("destination"),
                    )
                    .children(self.error.clone().map(|error| {
                        div()
                            .text_color(theme::error())
                            .child(selectable("error", error))
                            .pin("error")
                    }))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                button("cancel", "Cancel".into(), false)
                                    .key_context("NewProjectControl")
                                    .track_focus(&self.handles[CANCEL_FIELD])
                                    .when(self.handles[CANCEL_FIELD].is_focused(window), |button| {
                                        button.bg(theme::active_row())
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.dismiss(&Dismiss, window, cx)
                                    }))
                                    .pin("cancel"),
                            )
                            .child(
                                button(
                                    "create",
                                    if self.busy {
                                        "Creating…"
                                    } else {
                                        "Create Project"
                                    }
                                    .into(),
                                    true,
                                )
                                .key_context("NewProjectControl")
                                .track_focus(&self.handles[CREATE_FIELD])
                                .border_1()
                                .border_color(if self.handles[CREATE_FIELD].is_focused(window) {
                                    theme::accent_text()
                                } else {
                                    theme::accent()
                                })
                                .when(self.busy, |button| button.opacity(0.5))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.submit(&Submit, window, cx)
                                }))
                                .pin("create"),
                            ),
                    ),
            )
            .pin("new-project")
            .label("New Project")
    }
}
