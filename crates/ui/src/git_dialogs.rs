//! Small git dialogs shown over the workspace: a text input (new branch,
//! rename, checkout revision, …) and PyCharm's Push dialog (outgoing
//! commits and the target, then Push).

use git::{PushTarget, Repository};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, MouseButton,
    SharedString, Subscription, Task, Window, actions, div, prelude::*, px,
};

use crate::{
    editor::{self, Editor, EditorEvent},
    project_picker,
    selectable::selectable,
    theme,
};

actions!(git_dialog, [Confirm]);

/// Both dialogs share the project picker's Escape binding.
const CONTEXT: &str = "ProjectPicker GitDialog";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", Confirm, Some("GitDialog")),
        KeyBinding::new("secondary-enter", Confirm, Some("GitDialog")),
        KeyBinding::new("secondary-enter", Confirm, Some("GitDialog > Editor")),
    ]);
}

pub(crate) fn frame() -> gpui::Div {
    div()
        .w(px(520.))
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .bg(theme::panel())
        .border_1()
        .border_color(theme::border())
        .rounded_md()
        .shadow_lg()
        .text_sm()
        .text_color(theme::text())
}

/// A dialog button; `primary` in the accent colour. Also the workflow
/// inputs form's.
pub(crate) fn button(
    id: &'static str,
    label: SharedString,
    primary: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .when(primary, |button| {
            button.bg(theme::accent()).text_color(theme::on_accent())
        })
        .when(!primary, |button| {
            button
                .border_1()
                .border_color(theme::border())
                .hover(|style| style.bg(theme::hover_row()))
        })
        .child(label)
}

pub enum InputDialogEvent {
    Confirmed { text: String, checked: bool },
    Dismissed,
}

impl EventEmitter<InputDialogEvent> for InputDialog {}

/// Asks for one value (a branch name, a revision, a message).
pub struct InputDialog {
    title: SharedString,
    input: Entity<Editor>,
    multi_line: bool,
    confirm_label: &'static str,
    /// An option under the input (e.g. "Checkout branch") and its state.
    checkbox: Option<(&'static str, bool)>,
    error: Option<SharedString>,
    /// An empty answer is fine (e.g. a stash message).
    allow_empty: bool,
    _subscription: Subscription,
}

impl InputDialog {
    pub fn new(
        title: impl Into<SharedString>,
        initial: &str,
        multi_line: bool,
        confirm_label: &'static str,
        checkbox: Option<(&'static str, bool)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            let mut editor = if multi_line {
                Editor::text_area(cx)
            } else {
                Editor::single_line(cx)
            };
            editor.set_text(initial, cx);
            editor.select_all_text(cx);
            editor
        });
        window.focus(&input.focus_handle(cx));
        let subscription = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if let EditorEvent::Edited = event {
                this.error = None;
                cx.notify();
            }
        });
        Self {
            title: title.into(),
            input,
            multi_line,
            confirm_label,
            checkbox,
            error: None,
            allow_empty: false,
            _subscription: subscription,
        }
    }

    /// Accepts an empty value.
    pub fn allow_empty(mut self) -> Self {
        self.allow_empty = true;
        self
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text();
        let text = if self.multi_line {
            text.trim_end().to_owned()
        } else {
            text.trim().to_owned()
        };
        if text.is_empty() && !self.allow_empty {
            self.error = Some("Enter a value".into());
            return cx.notify();
        }
        let checked = self.checkbox.is_some_and(|(_, checked)| checked);
        cx.emit(InputDialogEvent::Confirmed { text, checked });
    }

    fn confirm_action(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        self.confirm(cx);
    }

    /// Enter in the single-line input.
    fn newline(&mut self, _: &editor::Newline, _: &mut Window, cx: &mut Context<Self>) {
        if !self.multi_line {
            self.confirm(cx);
        }
    }

    fn dismiss(&mut self, _: &project_picker::Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(InputDialogEvent::Dismissed);
    }
}

impl Focusable for InputDialog {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for InputDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let input_height = if self.multi_line {
            px(theme::EDITOR_LINE_HEIGHT * 6. + 10.)
        } else {
            px(30.)
        };
        frame()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::confirm_action))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::dismiss))
            .on_mouse_down_out(
                cx.listener(|this, _, window, cx| {
                    this.dismiss(&project_picker::Dismiss, window, cx)
                }),
            )
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(selectable("title", self.title.clone())),
            )
            .child(
                div()
                    .h(input_height)
                    .px_2()
                    .py_1()
                    .border_1()
                    .border_color(theme::accent())
                    .rounded_sm()
                    .bg(theme::background())
                    .child(self.input.clone()),
            )
            .children(self.checkbox.map(|(label, checked)| {
                div()
                    .id("dialog-checkbox")
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .cursor_pointer()
                    .child(
                        div()
                            .size(px(13.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_sm()
                            .border_1()
                            .border_color(if checked {
                                theme::accent()
                            } else {
                                theme::muted()
                            })
                            .when(checked, |boxed| boxed.bg(theme::accent()))
                            .text_color(theme::on_accent())
                            .text_size(px(10.))
                            .child(if checked { "✓" } else { "" }),
                    )
                    .child(selectable("checkbox-label", label))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some((_, checked)) = &mut this.checkbox {
                            *checked = !*checked;
                        }
                        cx.notify();
                    }))
            }))
            .children(self.error.clone().map(|error| {
                div()
                    .text_xs()
                    .text_color(theme::error())
                    .child(selectable("error", error))
            }))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        button("dialog-cancel", "Cancel".into(), false).on_click(cx.listener(
                            |this, _, window, cx| {
                                this.dismiss(&project_picker::Dismiss, window, cx)
                            },
                        )),
                    )
                    .child(
                        button("dialog-confirm", self.confirm_label.into(), true)
                            .on_click(cx.listener(|this, _, _, cx| this.confirm(cx))),
                    ),
            )
    }
}

pub enum PushDialogEvent {
    Pushed(String),
    Dismissed,
}

impl EventEmitter<PushDialogEvent> for PushDialog {}

enum PushState {
    Loading,
    Ready {
        target: PushTarget,
        commits: Vec<String>,
    },
    Pushing {
        target: PushTarget,
        commits: Vec<String>,
    },
    Failed(SharedString),
}

/// PyCharm's Push dialog: the commits a push of `branch` would send and
/// where to; Push runs it (setting the upstream for a new branch).
pub struct PushDialog {
    repository: Repository,
    branch: String,
    state: PushState,
    focus_handle: FocusHandle,
    _task: Task<()>,
}

impl PushDialog {
    pub fn new(repository: Repository, branch: String, cx: &mut Context<Self>) -> Self {
        let mut dialog = Self {
            repository,
            branch,
            state: PushState::Loading,
            focus_handle: cx.focus_handle(),
            _task: Task::ready(()),
        };
        dialog.load(cx);
        dialog
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let (repository, branch) = (self.repository.clone(), self.branch.clone());
        self._task = cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    let target = repository.push_target(&branch)?;
                    let commits = repository.outgoing(&branch, &target)?;
                    Ok::<_, std::io::Error>((target, commits))
                })
                .await;
            this.update(cx, |this, cx| {
                this.state = match loaded {
                    Ok((target, commits)) => PushState::Ready { target, commits },
                    Err(err) => PushState::Failed(err.to_string().into()),
                };
                cx.notify();
            })
            .ok();
        });
    }

    fn push(&mut self, cx: &mut Context<Self>) {
        let PushState::Ready { target, commits } =
            std::mem::replace(&mut self.state, PushState::Loading)
        else {
            return;
        };
        let (repository, branch) = (self.repository.clone(), self.branch.clone());
        let pushed = (target.clone(), branch.clone());
        self.state = PushState::Pushing {
            target: target.clone(),
            commits,
        };
        cx.notify();
        self._task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { repository.push_branch(&branch, &target) })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    let (target, branch) = pushed;
                    cx.emit(PushDialogEvent::Pushed(format!(
                        "Pushed {branch} to {}/{}",
                        target.remote, target.branch
                    )));
                }
                Err(err) => {
                    this.state = PushState::Failed(err.to_string().into());
                    cx.notify();
                }
            })
            .ok();
        });
    }

    fn confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        self.push(cx);
    }

    fn dismiss(&mut self, _: &project_picker::Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PushDialogEvent::Dismissed);
    }
}

impl Focusable for PushDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PushDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (target, commits, pushing) = match &self.state {
            PushState::Ready { target, commits } => (Some(target), commits.as_slice(), false),
            PushState::Pushing { target, commits } => (Some(target), commits.as_slice(), true),
            PushState::Loading | PushState::Failed(_) => (None, [].as_slice(), false),
        };
        let header = match target {
            Some(target) => format!(
                "Push Commits: {} → {}/{}{}",
                self.branch,
                target.remote,
                target.branch,
                if target.new { "  (new)" } else { "" }
            ),
            None => format!("Push Commits: {}", self.branch),
        };
        let list = div()
            .id("push-commits")
            .max_h(px(260.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .border_1()
            .border_color(theme::border())
            .rounded_sm()
            .bg(theme::background())
            .p_1()
            .when(target.is_some() && commits.is_empty(), |list| {
                list.child(div().p_1().text_color(theme::muted()).child(selectable(
                    "up-to-date",
                    "No commits to push; the branch is up to date.",
                )))
            })
            .children(commits.iter().enumerate().map(|(index, commit)| {
                let (hash, subject) = commit.split_once(' ').unwrap_or((commit, ""));
                div()
                    .flex()
                    .gap_2()
                    .px_1()
                    .child(
                        div()
                            .flex_none()
                            .font_family(theme::MONO_FONT)
                            .text_color(theme::muted())
                            .child(selectable(("hash", index), hash.to_owned())),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(selectable(("subject", index), subject.to_owned())),
                    )
            }));
        let can_push = matches!(&self.state, PushState::Ready { .. });
        frame()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .on_mouse_down_out(
                cx.listener(|this, _, window, cx| {
                    this.dismiss(&project_picker::Dismiss, window, cx)
                }),
            )
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(selectable("title", header)),
            )
            .when(matches!(self.state, PushState::Loading), |dialog| {
                dialog.child(
                    div()
                        .text_color(theme::muted())
                        .child(selectable("loading", "Reading outgoing commits…")),
                )
            })
            .when(target.is_some(), |dialog| dialog.child(list))
            .children(match &self.state {
                PushState::Failed(error) => Some(
                    div()
                        .text_xs()
                        .text_color(theme::error())
                        .child(selectable("error", error.clone())),
                ),
                _ => None,
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        button("push-cancel", "Cancel".into(), false).on_click(cx.listener(
                            |this, _, window, cx| {
                                this.dismiss(&project_picker::Dismiss, window, cx)
                            },
                        )),
                    )
                    .child(
                        button(
                            "push-confirm",
                            if pushing { "Pushing…" } else { "Push" }.into(),
                            can_push,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.push(cx))),
                    ),
            )
    }
}
