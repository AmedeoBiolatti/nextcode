//! The Workflows panel (bottom dock, next to the Terminal): the workflows
//! found (user and project ones) on the left; the selected one's steps on
//! the right with their state, elapsed time, project, last output line and
//! actions, plus Run All / Stop All / Retry Failed / Open File. Steps
//! without a project show their terminal here.

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, Hsla, SharedString, Subscription,
    Window, div, prelude::*, px,
};
use workflow::{Outcome, Ready, Status};

use super::{NewWorkflow, Workflows};
use crate::theme;

pub struct WorkflowsPanel {
    workflows: Entity<Workflows>,
    /// The workflow shown, by id (default: the first).
    selected: Option<String>,
    /// A step without a project whose terminal shows below the steps.
    terminal: Option<(String, usize)>,
    focus_handle: FocusHandle,
    listed: bool,
    /// The steps (label, status) of the last render (tests).
    #[cfg(test)]
    pub(super) shown: Vec<(String, String)>,
    _observe: Subscription,
}

/// `12s`, `3m 05s`, `1h 02m`.
fn elapsed_text(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}

fn status_look(status: &Status) -> (&'static str, Hsla) {
    match status {
        Status::Pending => ("○", theme::muted()),
        Status::Waiting => ("◦", theme::muted()),
        Status::Starting => ("◌", theme::warning()),
        Status::Running => ("▶", theme::accent()),
        Status::Ready => ("✓", theme::git_added()),
        Status::Failed(_) => ("✗", theme::error()),
        Status::Stopped => ("■", theme::muted()),
    }
}

fn outcome_text(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Idle => "",
        Outcome::Running => "Running",
        Outcome::Ready => "All ready",
        Outcome::Failed => "Failed",
        Outcome::Stopped => "Stopped",
    }
}

fn button(id: impl Into<gpui::ElementId>, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .px_2()
        .rounded_sm()
        .border_1()
        .border_color(theme::border())
        .text_xs()
        .text_color(theme::text())
        .cursor_pointer()
        .hover(|style| style.bg(theme::hover_row()))
        .child(label)
}

impl WorkflowsPanel {
    pub fn new(workflows: Entity<Workflows>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&workflows, |_, _, cx| cx.notify());
        Self {
            workflows,
            selected: None,
            terminal: None,
            focus_handle: cx.focus_handle(),
            listed: false,
            #[cfg(test)]
            shown: Vec::new(),
            _observe: observe,
        }
    }

    /// The selected workflow's id.
    fn selected_id(&self, cx: &App) -> Option<String> {
        let workflows = self.workflows.read(cx);
        self.selected
            .as_ref()
            .filter(|id| workflows.find(id).is_some())
            .cloned()
            .or_else(|| workflows.discovered().first().map(|found| found.id.clone()))
    }

    /// The selected workflow's steps as (label, status).
    #[cfg(test)]
    fn summary(&self, cx: &App) -> Vec<(String, String)> {
        let Some(id) = self.selected_id(cx) else {
            return Vec::new();
        };
        let workflows = self.workflows.read(cx);
        let (Some(found), active) = (workflows.find(&id), workflows.run_of(&id)) else {
            return Vec::new();
        };
        let Ok(workflow) = &found.parsed else {
            return Vec::new();
        };
        workflow
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| {
                let status = active.map_or("pending", |run| run.run.steps()[index].status.name());
                (step.label().to_owned(), status.to_owned())
            })
            .collect()
    }

    fn render_list(&self, selected: Option<&str>, cx: &mut Context<Self>) -> AnyElement {
        let workflows = self.workflows.read(cx);
        let rows: Vec<AnyElement> = workflows
            .discovered()
            .iter()
            .enumerate()
            .map(|(index, found)| {
                let id = found.id.clone();
                let active = selected == Some(found.id.as_str());
                let outcome = workflows.run_of(&found.id).map(|run| run.run.outcome());
                div()
                    .id(("workflow", index))
                    .px_2()
                    .py_1()
                    .cursor_pointer()
                    .when(active, |row| row.bg(theme::active_row()))
                    .hover(|style| style.bg(theme::hover_row()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected = Some(id.clone());
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(found.name.clone()),
                            )
                            .children(
                                found
                                    .parsed
                                    .is_err()
                                    .then(|| div().text_color(theme::error()).child("invalid")),
                            )
                            .children(outcome.filter(|o| *o != Outcome::Idle).map(|outcome| {
                                div()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(outcome_text(outcome))
                            })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted())
                            .truncate()
                            .child(found.source.label()),
                    )
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty().then(|| {
            div()
                .p_2()
                .text_xs()
                .text_color(theme::muted())
                .child("No workflows. New… creates one in the user folder; projects can have .next_code/workflows/*.jsonc.")
        });
        div()
            .id("workflow-list")
            .w(px(240.))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(theme::border())
            .children(rows)
            .children(empty)
            .into_any_element()
    }

    fn render_detail(&self, id: &str, now: Instant, cx: &mut Context<Self>) -> AnyElement {
        let workflows = self.workflows.read(cx);
        let Some(found) = workflows.find(id) else {
            return div().into_any_element();
        };
        let active = workflows.run_of(id);
        let outcome = active.map_or(Outcome::Idle, |run| run.run.outcome());
        let running = active.is_some_and(|run| {
            run.run.is_active() || run.run.steps().iter().any(|step| step.command_live())
        });
        let any_failed = active.is_some_and(|run| {
            run.run
                .steps()
                .iter()
                .any(|step| matches!(step.status, Status::Failed(_)))
        });
        let id_owned = id.to_owned();
        let toolbar = div()
            .px_2()
            .py_1()
            .flex()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(found.name.clone())
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted())
                            .truncate()
                            .child(found.path.display().to_string()),
                    ),
            )
            .children((!outcome_text(outcome).is_empty()).then(|| {
                div()
                    .text_xs()
                    .text_color(match outcome {
                        Outcome::Ready => theme::git_added(),
                        Outcome::Failed => theme::error(),
                        _ => theme::muted(),
                    })
                    .child(outcome_text(outcome))
            }))
            .child(button("workflow-run-all", "Run All").on_click({
                let id = id_owned.clone();
                cx.listener(move |this, _, _, cx| {
                    this.workflows.update(cx, |workflows, cx| {
                        workflows.run_workflow(id.clone(), cx).detach()
                    });
                })
            }))
            .when(running, |toolbar| {
                toolbar.child(button("workflow-stop-all", "Stop All").on_click({
                    let id = id_owned.clone();
                    cx.listener(move |this, _, _, cx| {
                        this.workflows
                            .update(cx, |workflows, cx| workflows.stop(&id, cx));
                    })
                }))
            })
            .when(any_failed, |toolbar| {
                toolbar.child(button("workflow-retry", "Retry Failed").on_click({
                    let id = id_owned.clone();
                    cx.listener(move |this, _, _, cx| {
                        this.workflows
                            .update(cx, |workflows, cx| workflows.retry_failed(&id, cx));
                    })
                }))
            })
            .child(button("workflow-open", "Open File").on_click({
                let id = id_owned.clone();
                cx.listener(move |this, _, _, cx| {
                    this.workflows
                        .update(cx, |workflows, cx| workflows.open_file(&id, cx));
                })
            }));

        let workflow = match &found.parsed {
            Ok(workflow) => workflow.clone(),
            Err(error) => {
                return div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .child(toolbar)
                    .child(
                        div()
                            .p_2()
                            .text_sm()
                            .text_color(theme::error())
                            .child(format!("This workflow cannot run:\n{error}")),
                    )
                    .into_any_element();
            }
        };
        let rows: Vec<AnyElement> = workflow
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| {
                let state = active.map(|run| &run.run.steps()[index]);
                let status = state.map_or(Status::Pending, |state| state.status.clone());
                let (icon, color) = status_look(&status);
                let elapsed = state
                    .and_then(|state| state.elapsed(now))
                    .map(elapsed_text)
                    .unwrap_or_default();
                let place = match active.and_then(|run| run.location(index)) {
                    Some(location) => crate::shell::tab_title(location),
                    None => match &step.project {
                        Some(_) => "project".to_owned(),
                        None => "this machine".to_owned(),
                    },
                };
                // What it waits for, why it failed, else its last output.
                let mut note = match &status {
                    Status::Failed(reason) => reason.clone(),
                    _ => state.map(|state| state.detail.clone()).unwrap_or_default(),
                };
                if let Some(state) = state {
                    if let Some(check) = &state.check_output
                        && matches!(status, Status::Failed(_))
                    {
                        note = format!("{note} — {check}");
                    }
                    let last = state.last_line();
                    if !last.is_empty() && !matches!(status, Status::Failed(_)) {
                        note = if note.is_empty() {
                            last.to_owned()
                        } else {
                            format!("{note} · {last}")
                        };
                    }
                    if let (Status::Ready, Some(code)) = (&status, state.exit)
                        && step.ready != Ready::Exit
                    {
                        let code = code.map_or("ended".to_owned(), |code| format!("exited {code}"));
                        note = format!("{code} · {note}");
                    }
                }
                let has_terminal = active.is_some_and(|run| run.terminal(index).is_some());
                let waiting_manual =
                    status == Status::Running && matches!(step.ready, Ready::Manual(_));
                let failed = matches!(status, Status::Failed(_));
                let id = id_owned.clone();
                let project_step = step.project.is_some();
                let actions = div()
                    .flex()
                    .flex_none()
                    .gap_1()
                    .when(has_terminal, |actions| {
                        let id = id.clone();
                        actions.child(button(("step-terminal", index), "Terminal").on_click(
                            cx.listener(move |this, _, _, cx| {
                                if project_step {
                                    this.workflows.update(cx, |workflows, cx| {
                                        workflows.show_terminal(&id, index, cx)
                                    });
                                } else {
                                    this.terminal = Some((id.clone(), index));
                                    cx.notify();
                                }
                            }),
                        ))
                    })
                    .when(waiting_manual, |actions| {
                        let id = id.clone();
                        actions.child(button(("step-continue", index), "Continue").on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.workflows.update(cx, |workflows, cx| {
                                    workflows.continue_step(&id, index, cx);
                                });
                            }),
                        ))
                    })
                    .when(state.is_some(), |actions| {
                        let id = id.clone();
                        let label = if failed { "Retry" } else { "Restart" };
                        actions.child(button(("step-restart", index), label).on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.workflows.update(cx, |workflows, cx| {
                                    workflows.restart_step(&id, index, cx);
                                });
                            },
                        )))
                    });
                div()
                    .id(("workflow-step", index))
                    .h(px(26.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .hover(|style| style.bg(theme::hover_row()))
                    .child(div().w(px(16.)).flex_none().text_color(color).child(icon))
                    .child(
                        div()
                            .w(px(180.))
                            .flex_none()
                            .truncate()
                            .child(step.label().to_owned()),
                    )
                    .child(
                        div()
                            .w(px(70.))
                            .flex_none()
                            .text_xs()
                            .text_color(color)
                            .child(status.name()),
                    )
                    .child(
                        div()
                            .w(px(56.))
                            .flex_none()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(elapsed),
                    )
                    .child(
                        div()
                            .w(px(170.))
                            .flex_none()
                            .text_xs()
                            .text_color(theme::muted())
                            .truncate()
                            .child(place),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(if failed {
                                theme::error()
                            } else {
                                theme::muted()
                            })
                            .child(note),
                    )
                    .child(actions)
                    .into_any_element()
            })
            .collect();
        let terminal = self.terminal.as_ref().and_then(|(shown, step)| {
            (shown == id)
                .then(|| active?.terminal(*step).cloned())
                .flatten()
                .map(|view| (*step, view))
        });
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(toolbar)
            .child(
                div()
                    .id("workflow-steps")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .children(terminal.map(|(step, view)| {
                div()
                    .h(px(200.))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .border_t_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .px_2()
                            .flex()
                            .items_center()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(
                                div().flex_1().child(format!(
                                    "{} (this machine)",
                                    workflow.steps[step].label()
                                )),
                            )
                            .child(button("workflow-terminal-close", "Hide").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.terminal = None;
                                    cx.notify();
                                }),
                            )),
                    )
                    .child(div().flex_1().min_h_0().child(view))
            }))
            .into_any_element()
    }
}

impl Focusable for WorkflowsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkflowsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.listed {
            self.listed = true;
            cx.defer_in(window, |this, _, cx| {
                this.workflows
                    .update(cx, |workflows, cx| workflows.refresh(cx).detach());
            });
        }
        let selected = self.selected_id(cx);
        #[cfg(test)]
        {
            self.shown = self.summary(cx);
        }
        let header = div()
            .px_2()
            .h(px(26.))
            .flex()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(theme::border())
            .text_xs()
            .text_color(theme::muted())
            .child(div().flex_1().child("Workflows"))
            .child(
                button("workflows-refresh", "⟳").on_click(cx.listener(|this, _, _, cx| {
                    this.workflows
                        .update(cx, |workflows, cx| workflows.refresh(cx).detach());
                })),
            )
            .child(button("workflows-new", "New…").on_click(
                cx.listener(|_, _, window, cx| window.dispatch_action(Box::new(NewWorkflow), cx)),
            ));
        let detail: AnyElement = match &selected {
            Some(id) => self.render_detail(id, Instant::now(), cx),
            None => div()
                .p_3()
                .text_color(theme::muted())
                .child(SharedString::from(
                    "Workflows start a development environment: checks, commands in \
                     several projects' terminals, readiness, browser links and manual steps.",
                ))
                .into_any_element(),
        };
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .bg(theme::panel())
            .text_sm()
            .text_color(theme::text())
            .child(
                div().flex().flex_col().h_full().child(header).child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .child(self.render_list(selected.as_deref(), cx)),
                ),
            )
            .child(detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_times_read_naturally() {
        assert_eq!(elapsed_text(Duration::from_secs(7)), "7s");
        assert_eq!(elapsed_text(Duration::from_secs(65)), "1m 05s");
        assert_eq!(elapsed_text(Duration::from_secs(3720)), "1h 02m");
    }
}
