//! The Git tool window in the workspace: `Alt+9`, Git > Show Git Log and
//! Show History for Current File, and the dialogs its Log asks for.

use gpui::InteractiveElement;

use super::*;
use crate::{
    git_dialogs::InputDialog,
    git_log::{GitLogEvent, ResetDialog, ResetDialogEvent},
};

impl Workspace {
    pub(super) fn on_log_actions<E: InteractiveElement>(
        &self,
        element: E,
        cx: &mut Context<Self>,
    ) -> E {
        element
            .on_action(cx.listener(Self::toggle_git_log))
            .on_action(cx.listener(|this, _: &git_commands::ShowLog, window, cx| {
                this.show_git_log(window, cx)
            }))
            .on_action(cx.listener(Self::show_file_history))
    }

    /// Shows the Git window and focuses its log.
    fn show_git_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_panel(PanelId::GitLog, cx);
        self.git_log.read(cx).focus(window);
    }

    /// `Alt+9`: shows and focuses the Git window, or hides it when it has
    /// focus (PyCharm's tool window toggle).
    fn toggle_git_log(
        &mut self,
        _: &docks::ToggleGitLog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let visible = self
            .layout
            .position_of(PanelId::GitLog)
            .is_some_and(|position| {
                let dock = self.layout.dock(position);
                dock.open && dock.active == Some(PanelId::GitLog)
            });
        if visible && self.git_log.read(cx).contains_focus(window, cx) {
            self.toggle_panel(PanelId::GitLog, cx);
            return self.focus_active(window, cx);
        }
        self.show_git_log(window, cx);
    }

    /// The log filtered to the active tab's file.
    fn show_file_history(
        &mut self,
        _: &git_commands::ShowFileHistory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repository) = self.repository_or_notice(cx) else {
            return;
        };
        let relative = self.active_tab().and_then(|tab| {
            let inside = tab.path.strip_prefix(self.project.root()).ok()?;
            let parts: Vec<String> = inside
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect();
            (!parts.is_empty()).then(|| format!("{}{}", repository.prefix(), parts.join("/")))
        });
        let Some(path) = relative else {
            self.notice = Some("Open a file to see its history".into());
            cx.notify();
            return;
        };
        self.git_log
            .update(cx, |log, cx| log.show_path_history(path, cx));
        self.show_git_log(window, cx);
    }

    pub(super) fn on_git_log_event(
        &mut self,
        log: &Entity<GitLogPanel>,
        event: &GitLogEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            GitLogEvent::OpenDiff { repository, source } => {
                self.open_diff(repository.clone(), source.clone(), window, cx)
            }
            GitLogEvent::CompareWithLocal { repository, base } => self.open_compare(
                repository.clone(),
                base.clone(),
                git::Side::Worktree,
                window,
                cx,
            ),
            GitLogEvent::NewBranch { repository, start } => {
                self.new_branch_dialog(repository.clone(), Some(start.clone()), window, cx)
            }
            GitLogEvent::NewTag { at } => {
                let title = format!(
                    "Create New Tag on {}",
                    crate::commit_panel::short_revision(at)
                );
                let dialog =
                    cx.new(|cx| InputDialog::new(title, "", false, "Create", None, window, cx));
                let (log, at) = (log.clone(), at.clone());
                self.open_input_dialog(
                    dialog,
                    move |_, name, _, _, cx| {
                        let at = at.clone();
                        log.update(cx, |log, cx| log.create_tag(name, at, cx));
                    },
                    window,
                    cx,
                );
            }
            GitLogEvent::EditMessage { message } => {
                let dialog = cx.new(|cx| {
                    InputDialog::new("Edit Commit Message", message, true, "OK", None, window, cx)
                });
                let log = log.clone();
                self.open_input_dialog(
                    dialog,
                    move |_, message, _, _, cx| {
                        log.update(cx, |log, cx| log.reword_head(message, cx));
                    },
                    window,
                    cx,
                );
            }
            GitLogEvent::Reset {
                id,
                subject,
                branch,
            } => {
                let dialog = cx.new(|cx| ResetDialog::new(id, subject, branch.as_deref(), cx));
                let (log, id) = (log.clone(), id.clone());
                let subscription =
                    cx.subscribe_in(&dialog, window, move |this, _, event, window, cx| {
                        this.git_modal = None;
                        this.focus_active(window, cx);
                        if let ResetDialogEvent::Confirmed(mode) = event {
                            let id = id.clone();
                            log.update(cx, |log, cx| log.reset(id, *mode, cx));
                        }
                        cx.notify();
                    });
                window.focus(&dialog.focus_handle(cx));
                self.git_modal = Some((dialog.into(), subscription));
                cx.notify();
            }
            GitLogEvent::Changed(message) => self.repository_changed(Some(message.clone()), cx),
        }
    }
}
