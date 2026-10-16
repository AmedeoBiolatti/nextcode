//! PyCharm-style git flows of the workspace: the branches popup (opened
//! from the title bar's branch widget), its branch commands, the New
//! Branch / Rename / Checkout Revision dialogs, the Push dialog and Update
//! Project.

use std::io;

use git::{Outcome, Ref, RefKind, Repository};
use gpui::{Pixels, Point, anchored, deferred};

use super::*;
use crate::{
    branches_popup::{BranchesPopup, BranchesPopupEvent, PopupCommand},
    commit_panel,
    git_dialogs::{self, InputDialog, InputDialogEvent, PushDialog, PushDialogEvent},
    settings::{self, UpdateMethod},
};

actions!(git, [NewBranch, CheckoutRevision, Update, Rollback]);

pub(super) fn bind_keys(cx: &mut App) {
    commit_panel::bind_keys(cx);
    git_dialogs::bind_keys(cx);
    crate::stash_dialogs::bind_keys(cx);
}

/// The open branches popup; `position` is where the title-bar widget was
/// clicked (else it opens at the workspace's top left).
pub(super) struct BranchesPopupState {
    popup: Entity<BranchesPopup>,
    position: Option<Point<Pixels>>,
    _subscription: Subscription,
}

/// What the title bar's branch widget shows.
pub struct BranchWidget {
    /// The branch, or a short commit id when detached.
    pub label: SharedString,
    pub ahead: u32,
    pub behind: u32,
    /// A merge, rebase, … in progress (shown after the branch).
    pub operation: Option<&'static str>,
}

impl Workspace {
    pub(super) fn on_git_actions<E: InteractiveElement>(
        &self,
        element: E,
        cx: &mut Context<Self>,
    ) -> E {
        element
            .on_action(cx.listener(|this, _: &NewBranch, window, cx| {
                if let Some(repository) = this.repository_or_notice(cx) {
                    this.new_branch_dialog(repository, None, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CheckoutRevision, window, cx| {
                this.checkout_revision_dialog(window, cx)
            }))
            .on_action(cx.listener(|this, _: &Update, _, cx| this.update_project(cx)))
            .on_action(cx.listener(|this, _: &Rollback, window, cx| {
                this.show_panel(PanelId::Commit, cx);
                this.commit
                    .update(cx, |commit, cx| commit.rollback_selection(window, cx));
            }))
    }

    /// The branch widget's content, once the repository is read.
    pub fn branch_widget(&self, cx: &App) -> Option<BranchWidget> {
        let commit = self.commit.read(cx);
        let status = commit.status()?;
        let label = match (&status.branch, &status.head) {
            (Some(branch), _) => branch.clone(),
            (None, Some(head)) => commit_panel::short_revision(head).to_owned(),
            (None, None) => "HEAD".to_owned(),
        };
        let operation = match commit.operation() {
            git::OperationState::None => None,
            git::OperationState::Merging => Some("Merging"),
            git::OperationState::Rebasing => Some("Rebasing"),
            git::OperationState::CherryPicking => Some("Cherry-picking"),
            git::OperationState::Reverting => Some("Reverting"),
        };
        Some(BranchWidget {
            label: label.into(),
            ahead: status.ahead,
            behind: status.behind,
            operation,
        })
    }

    pub fn open_branches_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_branches_popup_at(None, window, cx);
    }

    /// Opens (or, if open, closes) the branches popup below `position`.
    pub fn open_branches_popup_at(
        &mut self,
        position: Option<Point<Pixels>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.branches_popup.take().is_some() {
            self.focus_active(window, cx);
            return cx.notify();
        }
        let Some(repository) = self.repository_or_notice(cx) else {
            return;
        };
        let popup = cx.new(|cx| BranchesPopup::new(repository, window, cx));
        let subscription = cx.subscribe_in(&popup, window, |this, _, event, window, cx| {
            this.branches_popup = None;
            match event {
                BranchesPopupEvent::Run(command) => {
                    this.run_popup_command(command.clone(), window, cx)
                }
                BranchesPopupEvent::Dismissed => this.focus_active(window, cx),
            }
            cx.notify();
        });
        self.branches_popup = Some(BranchesPopupState {
            popup,
            position,
            _subscription: subscription,
        });
        cx.notify();
    }

    pub(super) fn render_branches_popup(
        &self,
        _: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let state = self.branches_popup.as_ref()?;
        let popup = state.popup.clone();
        Some(match state.position {
            Some(position) => {
                deferred(anchored().position(position).child(popup)).into_any_element()
            }
            None => div()
                .absolute()
                .top(px(4.))
                .left(px(8.))
                .child(popup)
                .into_any_element(),
        })
    }

    fn current_branch(&self, cx: &App) -> Option<String> {
        self.commit.read(cx).status()?.branch.clone()
    }

    fn run_popup_command(
        &mut self,
        command: PopupCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repository_or_notice(cx) else {
            return;
        };
        self.focus_active(window, cx);
        let current = self.current_branch(cx);
        match command {
            PopupCommand::UpdateProject => self.update_project(cx),
            PopupCommand::Commit => {
                self.show_panel(PanelId::Commit, cx);
                self.commit.update(cx, |commit, cx| {
                    commit.refresh(cx);
                    commit.focus(window, cx);
                });
            }
            PopupCommand::Push(branch) if branch.is_empty() => {
                self.open_push_dialog(repository, window, cx)
            }
            PopupCommand::Push(branch) => self.open_push_dialog_for(repository, branch, window, cx),
            PopupCommand::NewBranch(start) => self.new_branch_dialog(repository, start, window, cx),
            PopupCommand::CheckoutRevision => self.checkout_revision_dialog(window, cx),
            PopupCommand::Checkout(branch) => {
                let done = format!("Checked out {}", branch.name);
                self.run_git(
                    done,
                    move |repo| repo.switch_branch(&branch).map(|()| None),
                    cx,
                );
            }
            PopupCommand::CheckoutAndRebase(branch) => {
                let Some(onto) = current else {
                    return;
                };
                self.run_git_outcome(
                    format!("Rebased {branch} onto {onto}"),
                    move |repo| repo.rebase(&onto, Some(&branch)),
                    cx,
                );
            }
            PopupCommand::Compare(branch) => {
                if let Some(current) = current {
                    self.open_compare(repository, current, git::Side::Revision(branch), window, cx);
                }
            }
            PopupCommand::ShowDiff(branch) => {
                self.open_compare(repository, branch, git::Side::Worktree, window, cx)
            }
            PopupCommand::RebaseOnto(branch) => {
                let what = format!("Rebased {} onto {branch}", current.unwrap_or_default());
                self.run_git_outcome(what, move |repo| repo.rebase(&branch, None), cx);
            }
            PopupCommand::Merge(branch) => {
                let what = format!("Merged {branch} into {}", current.unwrap_or_default());
                self.run_git_outcome(what, move |repo| repo.merge(&branch), cx);
            }
            PopupCommand::Update(branch) => {
                if current.as_deref() == Some(branch.as_str()) {
                    self.update_project(cx);
                } else {
                    self.run_git(
                        format!("Updated {branch}"),
                        move |repo| repo.update_branch(&branch).map(|()| None),
                        cx,
                    );
                }
            }
            PopupCommand::Rename(branch) => {
                let dialog = cx.new(|cx| {
                    InputDialog::new(
                        format!("Rename Branch '{branch}'"),
                        &branch,
                        false,
                        "Rename",
                        None,
                        window,
                        cx,
                    )
                });
                self.open_input_dialog(
                    dialog,
                    move |this, name, _, _, cx| {
                        let old = branch.clone();
                        this.run_git(
                            format!("Renamed {old} to {name}"),
                            move |repo| repo.rename_branch(&old, &name).map(|()| None),
                            cx,
                        );
                    },
                    window,
                    cx,
                );
            }
            PopupCommand::Delete(branch) => self.delete_branch(repository, branch, window, cx),
        }
    }

    /// Deletes a branch; a local one with commits no other branch has
    /// needs a second confirmation (PyCharm's unmerged warning).
    fn delete_branch(
        &mut self,
        repository: Repository,
        branch: Ref,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = branch.name.clone();
        if branch.kind == RefKind::Remote {
            let answer = window.prompt(
                PromptLevel::Warning,
                &format!("Delete remote branch {name}?"),
                Some("The branch is deleted on the remote."),
                &["Delete", "Cancel"],
                cx,
            );
            cx.spawn(async move |this, cx| {
                if answer.await != Ok(0) {
                    return;
                }
                this.update(cx, |this, cx| {
                    this.run_git(
                        format!("Deleted remote branch {name}"),
                        move |repo| repo.delete_remote_branch(&name).map(|()| None),
                        cx,
                    )
                })
                .ok();
            })
            .detach();
            return;
        }
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let attempt = {
                let (repository, name) = (repository.clone(), name.clone());
                cx.background_spawn(async move { repository.delete_branch(&name, false) })
                    .await
            };
            let err = match attempt {
                Ok(()) => {
                    this.update(cx, |this, cx| {
                        this.repository_changed(Some(format!("Deleted branch {name}")), cx)
                    })
                    .ok();
                    return;
                }
                Err(err) => err,
            };
            if !git::not_fully_merged(&err) {
                this.update(cx, |this, cx| this.git_failed(err.to_string(), cx))
                    .ok();
                return;
            }
            let answer = window_handle.update(cx, |_, window, cx| {
                window.prompt(
                    PromptLevel::Warning,
                    &format!("Branch {name} is not fully merged"),
                    Some("It has commits that no other branch has. Delete it anyway?"),
                    &["Delete", "Cancel"],
                    cx,
                )
            });
            let Ok(answer) = answer else {
                return;
            };
            if answer.await != Ok(0) {
                return;
            }
            this.update(cx, |this, cx| {
                this.run_git(
                    format!("Deleted branch {name}"),
                    move |repo| repo.delete_branch(&name, true).map(|()| None),
                    cx,
                )
            })
            .ok();
        })
        .detach();
    }

    /// Update Project: fetch, then merge or rebase the upstream
    /// (`git.update_method`).
    pub(super) fn update_project(&mut self, cx: &mut Context<Self>) {
        let rebase = settings::current().git.update_method == UpdateMethod::Rebase;
        self.run_git_outcome(
            "Project updated".into(),
            move |repo| repo.update(rebase),
            cx,
        );
    }

    fn checkout_revision_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.repository_or_notice(cx).is_none() {
            return;
        }
        let dialog = cx.new(|cx| {
            InputDialog::new(
                "Checkout Tag or Revision",
                "",
                false,
                "Checkout",
                None,
                window,
                cx,
            )
        });
        self.open_input_dialog(
            dialog,
            |this, revision, _, _, cx| {
                this.run_git(
                    format!("Checked out {revision} (detached HEAD)"),
                    move |repo| {
                        let id = repo.resolve(&revision)?;
                        repo.checkout_revision(&id).map(|()| None)
                    },
                    cx,
                )
            },
            window,
            cx,
        );
    }

    /// PyCharm's New Branch dialog: a name, and "Checkout branch".
    pub(super) fn new_branch_dialog(
        &mut self,
        repository: Repository,
        start: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = match &start {
            Some(start) => format!(
                "Create New Branch from '{}'",
                commit_panel::short_revision(start)
            ),
            None => "Create New Branch".to_owned(),
        };
        let dialog = cx.new(|cx| {
            InputDialog::new(
                title,
                "",
                false,
                "Create",
                Some(("Checkout branch", true)),
                window,
                cx,
            )
        });
        let _ = repository;
        self.open_input_dialog(
            dialog,
            move |this, name, checkout, _, cx| {
                let start = start.clone();
                let done = if checkout {
                    format!("Created and checked out {name}")
                } else {
                    format!("Created branch {name}")
                };
                this.run_git(
                    done,
                    move |repo| {
                        repo.create_branch(&name, start.as_deref(), checkout)
                            .map(|()| None)
                    },
                    cx,
                );
            },
            window,
            cx,
        );
    }

    /// Shows `dialog` as the git modal; `on_confirm` gets its text and
    /// checkbox state. Either answer closes it.
    pub(super) fn open_input_dialog(
        &mut self,
        dialog: Entity<InputDialog>,
        on_confirm: impl Fn(&mut Workspace, String, bool, &mut Window, &mut Context<Workspace>)
        + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let subscription = cx.subscribe_in(&dialog, window, move |this, _, event, window, cx| {
            this.git_modal = None;
            this.focus_active(window, cx);
            if let InputDialogEvent::Confirmed { text, checked } = event {
                on_confirm(this, text.clone(), *checked, window, cx);
            }
            cx.notify();
        });
        window.focus(&dialog.focus_handle(cx));
        self.git_modal = Some((dialog.into(), subscription));
        cx.notify();
    }

    /// The Push dialog for the current branch.
    pub(super) fn open_push_dialog(
        &mut self,
        repository: Repository,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.current_branch(cx) {
            Some(branch) => self.open_push_dialog_for(repository, branch, window, cx),
            None => {
                self.notice = Some("Cannot push a detached HEAD; create a branch first".into());
                cx.notify();
            }
        }
    }

    fn open_push_dialog_for(
        &mut self,
        repository: Repository,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dialog = cx.new(|cx| PushDialog::new(repository, branch, cx));
        let subscription = cx.subscribe_in(&dialog, window, |this, _, event, window, cx| {
            this.git_modal = None;
            this.focus_active(window, cx);
            if let PushDialogEvent::Pushed(message) = event {
                this.repository_changed(Some(message.clone()), cx);
            }
            cx.notify();
        });
        window.focus(&dialog.focus_handle(cx));
        self.git_modal = Some((dialog.into(), subscription));
        cx.notify();
    }

    /// Runs a git operation in the background, then re-reads git state
    /// and reports `done` (or the operation's own message) in the Commit
    /// window; failures are reported there too.
    pub(super) fn run_git(
        &mut self,
        done: String,
        operation: impl FnOnce(&Repository) -> io::Result<Option<String>> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        self.run_git_with(
            move |repo| Ok((operation(repo)?.unwrap_or(done), false)),
            cx,
        );
    }

    /// A merge, rebase or update: conflicts show the Commit window, where
    /// they are listed under Merge Conflicts.
    fn run_git_outcome(
        &mut self,
        done: String,
        operation: impl FnOnce(&Repository) -> io::Result<Outcome> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        self.run_git_with(
            move |repo| {
                Ok(match operation(repo)? {
                    Outcome::Done => (done, false),
                    Outcome::Conflicts => (
                        "Stopped on conflicts: resolve them under Merge Conflicts".into(),
                        true,
                    ),
                })
            },
            cx,
        );
    }

    fn run_git_with(
        &mut self,
        operation: impl FnOnce(&Repository) -> io::Result<(String, bool)> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repository_or_notice(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { operation(&repository) })
                .await;
            this.update(cx, |this, cx| match result {
                Ok((message, show)) => {
                    if show {
                        this.show_panel(PanelId::Commit, cx);
                    }
                    this.repository_changed(Some(message), cx);
                }
                Err(err) => this.git_failed(err.to_string(), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Reports a failed operation in the Commit window (shown) and
    /// re-reads git state, which the operation may have changed.
    fn git_failed(&mut self, error: String, cx: &mut Context<Self>) {
        self.show_panel(PanelId::Commit, cx);
        self.repository_changed(None, cx);
        self.commit
            .update(cx, |commit, cx| commit.report_error(error, cx));
    }
}
