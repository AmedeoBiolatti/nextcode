//! PyCharm's Unstash Changes dialog: the stash list (newest first), the
//! selected stash's files (double-click shows a file's diff), Apply or Pop
//! (optionally reinstating the index; conflicts go to the Commit window
//! like a merge's), and Drop (asks).

use git::{ApplyOutcome, Change, Repository, Stash, StashFile};
use gpui::{
    App, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, KeyBinding, MouseButton,
    PromptLevel, SharedString, Task, Window, actions, div, prelude::*, px,
};

use crate::{
    commit_panel::{Check, change_color, checkbox},
    diff_view::DiffSource,
    git_dialogs::{Confirm, button, frame},
    git_log::relative_time,
    project_picker,
    selectable::selectable,
    theme,
};

actions!(stash_list, [SelectPrevious, SelectNext]);

const CONTEXT: &str = "ProjectPicker GitDialog StashList";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("up", SelectPrevious, Some("StashList")),
        KeyBinding::new("down", SelectNext, Some("StashList")),
    ]);
}

pub enum StashListEvent {
    /// A stash was applied or popped; `conflicts`: files to resolve.
    Applied {
        message: String,
        conflicts: bool,
    },
    Dropped(String),
    OpenDiff {
        repository: Repository,
        source: DiffSource,
    },
    Dismissed,
}

impl EventEmitter<StashListEvent> for StashListDialog {}

/// "Popped stash@{0} (message)" or the conflicts left.
pub(crate) fn unstash_message(stash: &str, outcome: &ApplyOutcome, pop: bool) -> String {
    match outcome {
        ApplyOutcome::Done { .. } if pop => format!("Popped {stash}"),
        ApplyOutcome::Done { .. } => format!("Applied {stash}"),
        ApplyOutcome::Conflicts { paths } => format!(
            "Unstashing {stash} left conflicts in {}: resolve them under Merge Conflicts (the \
             stash is kept)",
            paths.join(", ")
        ),
    }
}

pub struct StashListDialog {
    repository: Repository,
    /// `None` while loading.
    stashes: Option<Vec<Stash>>,
    selected: usize,
    /// Files of the selected stash; `None` while loading.
    files: Option<Vec<StashFile>>,
    selected_file: Option<usize>,
    reinstate_index: bool,
    busy: bool,
    /// A confirmation is open: clicks on it are not "outside".
    prompting: bool,
    error: Option<SharedString>,
    focus_handle: FocusHandle,
    _load: Task<()>,
    _files: Task<()>,
    _operation: Task<()>,
}

impl StashListDialog {
    pub fn new(repository: Repository, cx: &mut Context<Self>) -> Self {
        let mut dialog = Self {
            repository,
            stashes: None,
            selected: 0,
            files: None,
            selected_file: None,
            reinstate_index: false,
            busy: false,
            prompting: false,
            error: None,
            focus_handle: cx.focus_handle(),
            _load: Task::ready(()),
            _files: Task::ready(()),
            _operation: Task::ready(()),
        };
        dialog.load(cx);
        dialog
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let repository = self.repository.clone();
        self._load = cx.spawn(async move |this, cx| {
            let stashes = cx
                .background_spawn(async move { repository.stash_list() })
                .await;
            this.update(cx, |this, cx| {
                match stashes {
                    Ok(stashes) => {
                        this.selected = this.selected.min(stashes.len().saturating_sub(1));
                        this.stashes = Some(stashes);
                        this.load_files(cx);
                    }
                    Err(err) => {
                        this.stashes = Some(Vec::new());
                        this.error = Some(err.to_string().into());
                    }
                }
                cx.notify();
            })
            .ok();
        });
    }

    fn selected_stash(&self) -> Option<&Stash> {
        self.stashes.as_ref()?.get(self.selected)
    }

    fn load_files(&mut self, cx: &mut Context<Self>) {
        self.files = None;
        self.selected_file = None;
        let Some(index) = self.selected_stash().map(|stash| stash.index) else {
            self._files = Task::ready(());
            return;
        };
        let repository = self.repository.clone();
        self._files = cx.spawn(async move |this, cx| {
            let files = cx
                .background_spawn(async move { repository.stash_changes(index) })
                .await;
            this.update(cx, |this, cx| {
                match files {
                    Ok(files) => this.files = Some(files),
                    Err(err) => {
                        this.files = Some(Vec::new());
                        this.error = Some(err.to_string().into());
                    }
                }
                cx.notify();
            })
            .ok();
        });
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        let count = self.stashes.as_ref().map_or(0, Vec::len);
        if count == 0 {
            return;
        }
        let index = index.min(count - 1);
        if index != self.selected || self.files.is_none() {
            self.selected = index;
            self.error = None;
            self.load_files(cx);
        }
        cx.notify();
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected.saturating_sub(1), cx);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected + 1, cx);
    }

    /// Opens the diff of file `file` of the selected stash (the first one
    /// without a choice).
    fn view_diff(&mut self, file: Option<usize>, cx: &mut Context<Self>) {
        let Some(file) = self
            .files
            .as_ref()
            .and_then(|files| files.get(file.or(self.selected_file).unwrap_or(0)))
        else {
            return;
        };
        cx.emit(StashListEvent::OpenDiff {
            repository: self.repository.clone(),
            source: DiffSource::Revisions {
                old: file.old.clone(),
                new: file.new.clone(),
                change: file.change.clone(),
            },
        });
    }

    fn apply(&mut self, pop: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(index) = self.selected_stash().map(|stash| stash.index) else {
            return;
        };
        let reinstate_index = self.reinstate_index;
        let repository = self.repository.clone();
        self.busy = true;
        self.error = None;
        cx.notify();
        self._operation = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(
                    async move { repository.stash_apply(index, pop, reinstate_index) },
                )
                .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(outcome) => {
                        let conflicts = matches!(outcome, ApplyOutcome::Conflicts { .. });
                        let stash = format!("stash@{{{index}}}");
                        cx.emit(StashListEvent::Applied {
                            message: unstash_message(&stash, &outcome, pop),
                            conflicts,
                        });
                    }
                    Err(err) => {
                        this.error = Some(err.to_string().into());
                        cx.notify();
                    }
                }
            })
            .ok();
        });
    }

    fn drop_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(stash) = self.selected_stash() else {
            return;
        };
        let (index, message) = (stash.index, stash.message.clone());
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Drop stash@{{{index}}}?"),
            Some(&format!("'{message}' is deleted. This cannot be undone.")),
            &["Drop", "Cancel"],
            cx,
        );
        self.prompting = true;
        let repository = self.repository.clone();
        self._operation = cx.spawn(async move |this, cx| {
            let confirmed = answer.await == Ok(0);
            this.update(cx, |this, _| this.prompting = false).ok();
            if !confirmed {
                return;
            }
            let result = cx
                .background_spawn(async move { repository.stash_drop(index) })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => cx.emit(StashListEvent::Dropped(format!(
                        "Dropped stash@{{{index}}} ({message})"
                    ))),
                    Err(err) => this.error = Some(err.to_string().into()),
                }
                this.load(cx);
                cx.notify();
            })
            .ok();
        });
    }

    fn confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        self.apply(false, cx);
    }

    fn dismiss(&mut self, _: &project_picker::Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(StashListEvent::Dismissed);
    }

    fn render_stashes(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let stashes = self.stashes.as_deref().unwrap_or_default();
        div()
            .id("stash-list")
            .h(px(150.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .border_1()
            .border_color(theme::border())
            .rounded_sm()
            .bg(theme::background())
            .p_1()
            .when(self.stashes.is_none(), |list| {
                list.child(
                    div()
                        .p_1()
                        .text_color(theme::muted())
                        .child(selectable("stash-loading", "Reading the stash list…")),
                )
            })
            .when(self.stashes.is_some() && stashes.is_empty(), |list| {
                list.child(
                    div()
                        .p_1()
                        .text_color(theme::muted())
                        .child(selectable("stash-empty", "There are no stashes.")),
                )
            })
            .children(stashes.iter().enumerate().map(|(row, stash)| {
                let selected = row == self.selected;
                div()
                    .id(("stash-row", row))
                    .flex()
                    .gap_2()
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(selected, |row| row.bg(theme::active_row()))
                    .when(!selected, |row| {
                        row.hover(|style| style.bg(theme::hover_row()))
                    })
                    .child(
                        div()
                            .flex_none()
                            .font_family(theme::MONO_FONT)
                            .text_color(theme::muted())
                            .child(format!("stash@{{{}}}", stash.index)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(stash.message.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(format!(
                                "{} · {}",
                                stash.branch.as_deref().unwrap_or("detached"),
                                relative_time(stash.date)
                            )),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.select(row, cx)))
            }))
    }

    fn render_files(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let files = self.files.as_deref().unwrap_or_default();
        div()
            .id("stash-files")
            .h(px(130.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .border_1()
            .border_color(theme::border())
            .rounded_sm()
            .bg(theme::background())
            .p_1()
            .children(files.iter().enumerate().map(|(row, file)| {
                let selected = self.selected_file == Some(row);
                let path = &file.change.path;
                let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
                let change = file.change.change;
                div()
                    .id(("stash-file", row))
                    .flex()
                    .gap_2()
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(selected, |row| row.bg(theme::active_row()))
                    .when(!selected, |row| {
                        row.hover(|style| style.bg(theme::hover_row()))
                    })
                    .child(
                        div()
                            .flex_none()
                            .text_color(change_color(change))
                            .when(change == Change::Deleted, |name| name.line_through())
                            .child(name.to_owned()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(dir.to_owned()),
                    )
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        this.selected_file = Some(row);
                        if event.click_count() >= 2 {
                            this.view_diff(Some(row), cx);
                        }
                        cx.notify();
                    }))
            }))
    }
}

impl Focusable for StashListDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for StashListDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_stash = self.selected_stash().is_some();
        let files_title = match self.selected_stash() {
            Some(stash) => format!("Files in stash@{{{}}}", stash.index),
            None => "Files".to_owned(),
        };
        let enabled = has_stash && !self.busy;
        let action = |id: &'static str, label: &'static str, primary: bool| {
            button(id, label.into(), primary && enabled)
                .when(!enabled, |button| button.text_color(theme::muted()))
        };
        frame()
            .w(px(600.))
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_next))
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                if !this.prompting {
                    this.dismiss(&project_picker::Dismiss, window, cx)
                }
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(selectable("title", "Unstash Changes")),
            )
            .child(self.render_stashes(cx))
            .child(
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(selectable("files-title", files_title)),
            )
            .child(self.render_files(cx))
            .child(
                div()
                    .id("stash-reinstate-index")
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .cursor_pointer()
                    .child(checkbox(if self.reinstate_index {
                        Check::Checked
                    } else {
                        Check::Unchecked
                    }))
                    .child(selectable("reinstate-label", "Reinstate index"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reinstate_index = !this.reinstate_index;
                        cx.notify();
                    })),
            )
            .children(self.error.clone().map(|error| {
                div()
                    .text_xs()
                    .text_color(theme::error())
                    .child(selectable("error", error))
            }))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        action("stash-view", "View Diff", false)
                            .on_click(cx.listener(|this, _, _, cx| this.view_diff(None, cx))),
                    )
                    .child(action("stash-drop", "Drop…", false).on_click(
                        cx.listener(|this, _, window, cx| this.drop_selected(window, cx)),
                    ))
                    .child(div().flex_1())
                    .child(
                        button("stash-cancel", "Cancel".into(), false).on_click(cx.listener(
                            |this, _, window, cx| {
                                this.dismiss(&project_picker::Dismiss, window, cx)
                            },
                        )),
                    )
                    .child(
                        action("stash-pop", "Pop", false)
                            .on_click(cx.listener(|this, _, _, cx| this.apply(true, cx))),
                    )
                    .child(
                        action(
                            "stash-apply",
                            if self.busy { "Applying…" } else { "Apply" },
                            true,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.apply(false, cx))),
                    ),
            )
    }
}
