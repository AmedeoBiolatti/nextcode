//! Stash and shelf flows of the workspace (PyCharm's): Git > Stash
//! Changes… (message, Include untracked files), Unstash Changes… (the
//! stash list dialog), Shelve Changes… (a name) and Show Shelf (the Commit
//! window's Shelf tab). The Commit window's context menu asks for the same
//! dialogs with its files.

use git::Repository;

use super::*;
use crate::{
    commit_panel::Tab,
    git_dialogs::InputDialog,
    shelf_view::ProjectFiles,
    stash_dialogs::{StashListDialog, StashListEvent},
};

actions!(
    git,
    [StashChanges, UnstashChanges, ShelveChanges, ShowShelf]
);

fn files(count: usize) -> String {
    match count {
        1 => "1 file".to_owned(),
        n => format!("{n} files"),
    }
}

impl Workspace {
    pub(super) fn on_stash_actions<E: InteractiveElement>(
        &self,
        element: E,
        cx: &mut Context<Self>,
    ) -> E {
        element
            .on_action(cx.listener(|this, _: &StashChanges, window, cx| {
                if let Some(repository) = this.repository_or_notice(cx) {
                    this.stash_dialog(repository, None, false, window, cx)
                }
            }))
            .on_action(
                cx.listener(|this, _: &UnstashChanges, window, cx| {
                    this.open_stash_list(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &ShelveChanges, _, cx| {
                this.show_panel(PanelId::Commit, cx);
                this.commit.update(cx, |commit, cx| {
                    let (repo, entries) = commit.chosen_entries().unwrap_or_default();
                    commit.request_shelve(repo, entries, cx)
                });
            }))
            .on_action(cx.listener(|this, _: &ShowShelf, _, cx| {
                this.show_panel(PanelId::Commit, cx);
                this.commit
                    .update(cx, |commit, cx| commit.select_tab(Tab::Shelf, cx));
            }))
    }

    /// PyCharm's Stash dialog: an optional message and Include untracked
    /// files; `paths` limits the stash to those files of `repository` (all
    /// its changes otherwise).
    pub(super) fn stash_dialog(
        &mut self,
        repository: Repository,
        paths: Option<Vec<String>>,
        include_untracked: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = match paths.as_deref() {
            Some([path]) => format!("Stash Changes in {path} — message (optional)"),
            Some(paths) => format!(
                "Stash Changes in {} — message (optional)",
                files(paths.len())
            ),
            None => "Stash Changes — message (optional)".to_owned(),
        };
        let dialog = cx.new(|cx| {
            InputDialog::new(
                title,
                "",
                false,
                "Create Stash",
                Some(("Include untracked files", include_untracked)),
                window,
                cx,
            )
            .allow_empty()
        });
        self.open_input_dialog(
            dialog,
            move |this, message, untracked, _, cx| {
                let paths = paths.clone();
                this.run_git(
                    repository.clone(),
                    "Stashed changes".into(),
                    move |repo| {
                        repo.stash_push(&message, untracked, paths.as_deref())
                            .map(|()| None)
                    },
                    cx,
                );
            },
            window,
            cx,
        );
    }

    /// Asks for the shelf's name (prefilled with `name`), then shelves
    /// `paths` of `repository` and rolls them back.
    pub(super) fn shelve_dialog(
        &mut self,
        repository: Repository,
        paths: Vec<String>,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dialog = cx.new(|cx| {
            InputDialog::new(
                format!("Shelve Changes in {} — name", files(paths.len())),
                &name,
                false,
                "Shelve",
                None,
                window,
                cx,
            )
        });
        let project = self.project.clone();
        self.open_input_dialog(
            dialog,
            move |this, name, _, _, cx| {
                let (paths, project) = (paths.clone(), project.clone());
                this.run_git(
                    repository.clone(),
                    String::new(),
                    move |repo| {
                        let shelf =
                            repo.shelve(&ProjectFiles::new(&project, repo), &paths, &name)?;
                        Ok(Some(format!(
                            "Shelved {} as '{}' (Shelf tab)",
                            files(shelf.files.len()),
                            shelf.message
                        )))
                    },
                    cx,
                );
            },
            window,
            cx,
        );
    }

    /// Unstash Changes…: the stash list as the git modal. Applying closes
    /// it; dropping and viewing diffs keep it open.
    fn open_stash_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository_or_notice(cx) else {
            return;
        };
        let dialog = cx.new(|cx| StashListDialog::new(repository, cx));
        let subscription =
            cx.subscribe_in(
                &dialog,
                window,
                |this, dialog, event, window, cx| match event {
                    StashListEvent::Applied { message, conflicts } => {
                        this.git_modal = None;
                        this.focus_active(window, cx);
                        if *conflicts {
                            this.show_panel(PanelId::Commit, cx);
                            this.commit
                                .update(cx, |commit, cx| commit.select_tab(Tab::Changes, cx));
                        }
                        this.repository_changed(Some(message.clone()), cx);
                        cx.notify();
                    }
                    StashListEvent::Dropped(message) => {
                        this.repository_changed(Some(message.clone()), cx)
                    }
                    StashListEvent::OpenDiff { repository, source } => {
                        this.open_diff(repository.clone(), source.clone(), window, cx);
                        window.focus(&dialog.focus_handle(cx));
                    }
                    StashListEvent::Dismissed => {
                        this.git_modal = None;
                        this.focus_active(window, cx);
                        cx.notify();
                    }
                },
            );
        window.focus(&dialog.focus_handle(cx));
        self.git_modal = Some((dialog.into(), subscription));
        cx.notify();
    }
}
