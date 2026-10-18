//! The Log's commit context menu and the operations behind it, plus the
//! "Reset Current Branch to Here" dialog.

use std::io;

use git::{ApplyOutcome, Repository, ResetMode};
use gpui::{
    App, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable, KeyBinding, MouseButton,
    Pixels, Point, SharedString, Window, actions, anchored, deferred, div, prelude::*, px,
};

use super::{CopyRevision, GitLogEvent, GitLogPanel};
use crate::{
    commit_panel::short_revision,
    pinpoint::{Pin, kebab},
    selectable::selectable,
    theme,
};

const DIALOG_CONTEXT: &str = "GitResetDialog";

actions!(git_log, [ResetUp, ResetDown, ResetConfirm, ResetCancel]);

pub(super) fn bind_keys(cx: &mut App) {
    let ctx = Some(DIALOG_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", ResetUp, ctx),
        KeyBinding::new("down", ResetDown, ctx),
        KeyBinding::new("enter", ResetConfirm, ctx),
        KeyBinding::new("escape", ResetCancel, ctx),
    ]);
}

/// The open right-click menu of the commit list.
pub(super) struct CommitMenu {
    position: Point<Pixels>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CommitAction {
    CopyRevision,
    CreatePatch,
    CherryPick,
    CheckoutRevision,
    CompareWithLocal,
    Reset,
    Revert,
    UndoCommit,
    EditMessage,
    NewBranch,
    NewTag,
}

/// Menu sections, separated by lines.
const MENU: [&[CommitAction]; 4] = [
    &[
        CommitAction::CopyRevision,
        CommitAction::CreatePatch,
        CommitAction::CherryPick,
    ],
    &[
        CommitAction::CheckoutRevision,
        CommitAction::CompareWithLocal,
    ],
    &[
        CommitAction::Reset,
        CommitAction::Revert,
        CommitAction::UndoCommit,
        CommitAction::EditMessage,
    ],
    &[CommitAction::NewBranch, CommitAction::NewTag],
];

impl CommitAction {
    fn label(self) -> &'static str {
        match self {
            Self::CopyRevision => "Copy Revision Number",
            Self::CreatePatch => "Create Patch…",
            Self::CherryPick => "Cherry-Pick",
            Self::CheckoutRevision => "Checkout Revision",
            Self::CompareWithLocal => "Compare with Local",
            Self::Reset => "Reset Current Branch to Here…",
            Self::Revert => "Revert Commit",
            Self::UndoCommit => "Undo Commit",
            Self::EditMessage => "Edit Commit Message…",
            Self::NewBranch => "New Branch…",
            Self::NewTag => "New Tag…",
        }
    }
}

impl GitLogPanel {
    pub(super) fn open_menu(
        &mut self,
        index: usize,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.popup = None;
        if self.selected.binary_search(&index).is_err() {
            self.anchor = Some(index);
            self.set_selection(vec![index], cx);
        }
        self.menu = Some(CommitMenu { position });
        cx.notify();
    }

    /// Whether `action` applies to the current selection.
    pub(super) fn enabled(&self, action: CommitAction) -> bool {
        let commits = self.selected_commits();
        let single = commits.len() == 1;
        let any_merge = commits.iter().any(|c| c.parents.len() > 1);
        let is_head = single
            && self
                .head
                .as_ref()
                .is_some_and(|head| head.id == commits[0].id);
        match action {
            _ if commits.is_empty() => false,
            CommitAction::CopyRevision | CommitAction::Revert => true,
            CommitAction::CreatePatch | CommitAction::CherryPick => !any_merge,
            CommitAction::CheckoutRevision
            | CommitAction::CompareWithLocal
            | CommitAction::Reset
            | CommitAction::NewBranch
            | CommitAction::NewTag => single,
            CommitAction::EditMessage => is_head,
            CommitAction::UndoCommit => {
                is_head
                    && !commits[0].parents.is_empty()
                    && self.head.as_ref().is_some_and(|head| !head.pushed)
            }
        }
    }

    pub(super) fn run_action(&mut self, action: CommitAction, cx: &mut Context<Self>) {
        self.menu = None;
        cx.notify();
        if !self.enabled(action) {
            return;
        }
        let Some(repository) = self.repository() else {
            return;
        };
        let commits: Vec<git::Commit> = self.selected_commits().into_iter().cloned().collect();
        // Newest first, as listed.
        let ids: Vec<String> = commits.iter().map(|c| c.id.clone()).collect();
        let oldest_first: Vec<String> = ids.iter().rev().cloned().collect();
        let first = &commits[0];
        let short = first.short_id().to_owned();
        match action {
            CommitAction::CopyRevision => self.copy_ids(cx),
            CommitAction::CreatePatch => self.create_patch(oldest_first, cx),
            CommitAction::CherryPick => {
                let count = ids.len();
                self.apply(
                    move |repository| repository.cherry_pick(&oldest_first),
                    move |outcome| match outcome {
                        ApplyOutcome::Done { empty: 0 } => {
                            format!("Cherry-picked {}", plural(count, "commit"))
                        }
                        ApplyOutcome::Done { empty } => format!(
                            "Cherry-picked {}; {} already applied",
                            plural(count - empty, "commit"),
                            plural(*empty, "commit")
                        ),
                        ApplyOutcome::Conflicts { paths } => conflict_message("Cherry-pick", paths),
                    },
                    cx,
                );
            }
            CommitAction::Revert => {
                let count = ids.len();
                self.apply(
                    move |repository| repository.revert(&ids),
                    move |outcome| match outcome {
                        ApplyOutcome::Done { .. } => {
                            format!("Reverted {}", plural(count, "commit"))
                        }
                        ApplyOutcome::Conflicts { paths } => conflict_message("Revert", paths),
                    },
                    cx,
                );
            }
            CommitAction::CheckoutRevision => {
                let id = first.id.clone();
                self.operation(
                    move |repository| {
                        repository.checkout_revision(&id)?;
                        Ok(format!("Checked out {short} (detached HEAD)"))
                    },
                    cx,
                );
            }
            CommitAction::CompareWithLocal => cx.emit(GitLogEvent::CompareWithLocal {
                repository,
                base: first.id.clone(),
            }),
            CommitAction::Reset => cx.emit(GitLogEvent::Reset {
                id: first.id.clone(),
                subject: first.subject.clone(),
                branch: self.head.as_ref().and_then(|head| head.branch.clone()),
            }),
            CommitAction::UndoCommit => {
                let subject = first.subject.clone();
                self.operation(
                    move |repository| {
                        repository.undo_last_commit()?;
                        Ok(format!("Undid commit “{subject}”; its changes are kept"))
                    },
                    cx,
                );
            }
            CommitAction::EditMessage => {
                let message = self
                    .details
                    .as_ref()
                    .and_then(|details| details.message.clone())
                    .unwrap_or_else(|| first.subject.clone());
                cx.emit(GitLogEvent::EditMessage { message });
            }
            CommitAction::NewBranch => cx.emit(GitLogEvent::NewBranch {
                repository,
                start: first.id.clone(),
            }),
            CommitAction::NewTag => cx.emit(GitLogEvent::NewTag {
                at: first.id.clone(),
            }),
        }
    }

    fn copy_ids(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<&str> = self
            .selected_commits()
            .into_iter()
            .map(|c| c.id.as_str())
            .collect();
        if !ids.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(ids.join(" ")));
        }
    }

    pub(super) fn copy_revision(
        &mut self,
        _: &CopyRevision,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.copy_ids(cx);
    }

    /// Runs `op` off the UI thread; reports its message (or error), then
    /// tells the workspace the repository changed.
    fn operation(
        &mut self,
        op: impl FnOnce(&Repository) -> io::Result<String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        self.apply(op, |message: &String| message.clone(), cx);
    }

    /// Like [`Self::operation`] for operations with an outcome; a
    /// conflicted cherry-pick or revert offers Abort.
    fn apply<T: IsConflict + Send + 'static>(
        &mut self,
        op: impl FnOnce(&Repository) -> io::Result<T> + Send + 'static,
        describe: impl FnOnce(&T) -> String + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repository() else {
            return;
        };
        self.report = Some(("Working…".into(), false));
        self.conflict = false;
        cx.notify();
        self._operation = cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { op(&repository) }).await;
            this.update(cx, |this, cx| {
                let message = match result {
                    Ok(outcome) => {
                        this.conflict = outcome.is_conflict();
                        let message = describe(&outcome);
                        this.report = Some((message.clone().into(), this.conflict));
                        message
                    }
                    Err(err) => {
                        let message = err.to_string();
                        this.report = Some((message.clone().into(), true));
                        message
                    }
                };
                // The workspace refreshes everything showing the
                // repository, this log included.
                cx.emit(GitLogEvent::Changed(message));
                cx.notify();
            })
            .ok();
        });
    }

    /// Abandons a conflicted cherry-pick or revert.
    pub(super) fn abort(&mut self, cx: &mut Context<Self>) {
        self.operation(
            |repository| {
                repository.abort(repository.operation_state()?)?;
                Ok("Aborted; the branch is back where it was".into())
            },
            cx,
        );
    }

    /// Creates tag `name` at `at` (the New Tag dialog's answer).
    pub fn create_tag(&mut self, name: String, at: String, cx: &mut Context<Self>) {
        self.operation(
            move |repository| {
                let name = name.trim();
                repository.create_tag(name, &at, None)?;
                Ok(format!("Created tag {name} at {}", short_revision(&at)))
            },
            cx,
        );
    }

    /// Replaces HEAD's message (the Edit Commit Message dialog's answer).
    pub fn reword_head(&mut self, message: String, cx: &mut Context<Self>) {
        self.operation(
            move |repository| {
                let summary = repository.reword_head(&message)?;
                Ok(format!("Edited the message: {summary}"))
            },
            cx,
        );
    }

    /// Moves the current branch to `id` (the reset dialog's answer).
    pub fn reset(&mut self, id: String, mode: ResetMode, cx: &mut Context<Self>) {
        self.operation(
            move |repository| {
                repository.reset(&id, mode)?;
                Ok(format!(
                    "Reset ({}) to {}",
                    mode.label().to_lowercase(),
                    short_revision(&id)
                ))
            },
            cx,
        );
    }

    /// Asks where to save the selected commits' patch, then writes it.
    fn create_patch(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        let Some(repository) = self.repository() else {
            return;
        };
        let name = format!("{}.patch", short_revision(&ids[ids.len() - 1]));
        let directory = dirs::home_dir().unwrap_or_default();
        let chosen = cx.prompt_for_new_path(&directory, Some(&name));
        self._operation = cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = chosen.await else {
                return;
            };
            let count = ids.len();
            let written = cx
                .background_spawn(async move {
                    let patch = repository.create_patch(&ids)?;
                    std::fs::write(&path, patch)?;
                    Ok::<_, io::Error>(path)
                })
                .await;
            this.update(cx, |this, cx| {
                this.conflict = false;
                this.report = Some(match written {
                    Ok(path) => (
                        format!(
                            "Saved the patch of {} to {}",
                            plural(count, "commit"),
                            path.display()
                        )
                        .into(),
                        false,
                    ),
                    Err(err) => (format!("Could not create the patch: {err}").into(), true),
                });
                cx.notify();
            })
            .ok();
        });
    }

    pub(super) fn render_menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let menu = self.menu.as_ref()?;
        let mut children: Vec<gpui::AnyElement> = Vec::new();
        for (section, actions) in MENU.iter().enumerate() {
            if section > 0 {
                children.push(
                    div()
                        .my_1()
                        .h(px(1.))
                        .bg(theme::border())
                        .into_any_element(),
                );
            }
            for &action in *actions {
                let enabled = self.enabled(action);
                children.push(
                    div()
                        .id(action.label())
                        .px_3()
                        .py_1()
                        .rounded_sm()
                        .when(enabled, |item| {
                            item.cursor_pointer()
                                .hover(|style| style.bg(theme::active_row()))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.run_action(action, cx)),
                                )
                        })
                        .when(!enabled, |item| item.text_color(theme::muted()))
                        .child(action.label())
                        .pin(format!("item:{}", kebab(action.label())))
                        .into_any_element(),
                );
            }
        }
        Some(deferred(
            anchored().position(menu.position).child(
                div()
                    .id("log-commit-menu")
                    .occlude()
                    .min_w(px(240.))
                    .p_1()
                    .flex()
                    .flex_col()
                    .text_sm()
                    .text_color(theme::text())
                    .bg(theme::panel())
                    .border_1()
                    .border_color(theme::border())
                    .rounded_md()
                    .shadow_lg()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.menu = None;
                        cx.notify();
                    }))
                    .children(children)
                    .pin("menu"),
            ),
        ))
    }
}

/// Outcomes that may leave conflicts to resolve.
trait IsConflict {
    fn is_conflict(&self) -> bool;
}

impl IsConflict for ApplyOutcome {
    fn is_conflict(&self) -> bool {
        matches!(self, ApplyOutcome::Conflicts { .. })
    }
}

impl IsConflict for String {
    fn is_conflict(&self) -> bool {
        false
    }
}

fn plural(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

fn conflict_message(operation: &str, paths: &[String]) -> String {
    let shown: Vec<&str> = paths.iter().take(3).map(String::as_str).collect();
    let more = paths.len() - shown.len();
    format!(
        "{operation} stopped on conflicts in {}{}: resolve them and commit in the Commit window, or Abort",
        shown.join(", "),
        if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        }
    )
}

pub enum ResetDialogEvent {
    Confirmed(ResetMode),
    Dismissed,
}

impl EventEmitter<ResetDialogEvent> for ResetDialog {}

/// PyCharm's "Git Reset" dialog: the commit, and the mode as radio buttons.
pub struct ResetDialog {
    title: SharedString,
    mode: ResetMode,
    focus_handle: FocusHandle,
}

impl ResetDialog {
    pub fn new(id: &str, subject: &str, branch: Option<&str>, cx: &mut Context<Self>) -> Self {
        let target = branch.map_or("HEAD".to_owned(), |branch| format!("'{branch}'"));
        Self {
            title: format!("Reset {target} to {} “{subject}”", short_revision(id)).into(),
            mode: ResetMode::Mixed,
            focus_handle: cx.focus_handle(),
        }
    }

    fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        let index = ResetMode::ALL
            .iter()
            .position(|&mode| mode == self.mode)
            .unwrap_or(0) as isize;
        let next = (index + by).clamp(0, ResetMode::ALL.len() as isize - 1);
        self.mode = ResetMode::ALL[next as usize];
        cx.notify();
    }
}

impl Focusable for ResetDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ResetDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded_sm()
                .border_1()
                .cursor_pointer()
                .when(primary, |button| {
                    button
                        .bg(theme::accent())
                        .border_color(theme::accent())
                        .text_color(theme::on_accent())
                })
                .when(!primary, |button| {
                    button
                        .border_color(theme::border())
                        .hover(|style| style.bg(theme::hover_row()))
                })
                .child(label)
        };
        div()
            .key_context(DIALOG_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &ResetUp, _, cx| this.step(-1, cx)))
            .on_action(cx.listener(|this, _: &ResetDown, _, cx| this.step(1, cx)))
            .on_action(cx.listener(|this, _: &ResetConfirm, _, cx| {
                cx.emit(ResetDialogEvent::Confirmed(this.mode))
            }))
            .on_action(
                cx.listener(|_, _: &ResetCancel, _, cx| cx.emit(ResetDialogEvent::Dismissed)),
            )
            .on_mouse_down_out(cx.listener(|_, _, _, cx| cx.emit(ResetDialogEvent::Dismissed)))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .w(px(520.))
            .p_4()
            .flex()
            .flex_col()
            .gap_2()
            .bg(theme::panel())
            .border_1()
            .border_color(theme::border())
            .rounded_md()
            .shadow_lg()
            .text_sm()
            .text_color(theme::text())
            .child(
                div()
                    .font_weight(gpui::FontWeight::BOLD)
                    .child(selectable("title", "Git Reset")),
            )
            .child(
                div()
                    .text_color(theme::muted())
                    .child(selectable("subtitle", self.title.clone())),
            )
            .children(ResetMode::ALL.into_iter().map(|mode| {
                let checked = mode == self.mode;
                div()
                    .id(mode.label())
                    .flex()
                    .gap_2()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.mode = mode;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .w(px(16.))
                            .flex_none()
                            .text_color(if checked {
                                theme::accent_text()
                            } else {
                                theme::muted()
                            })
                            .child(if checked { "◉" } else { "○" }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(selectable("label", mode.label()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(selectable("description", mode.description())),
                            ),
                    )
                    .pin(format!("mode:{}", kebab(mode.label())))
                    .label(mode.label())
            }))
            .child(
                div()
                    .mt_2()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        button("reset-cancel", "Cancel", false)
                            .on_click(
                                cx.listener(|_, _, _, cx| cx.emit(ResetDialogEvent::Dismissed)),
                            )
                            .pin("cancel"),
                    )
                    .child(
                        button("reset-confirm", "Reset", true)
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.emit(ResetDialogEvent::Confirmed(this.mode))
                            }))
                            .pin("reset"),
                    ),
            )
            .pin("reset-dialog")
    }
}
