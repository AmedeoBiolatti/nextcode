//! Modal for git worktrees: open one in its own window, add one for a new
//! or existing branch (next to the repository), or remove one.

use git::{Ref, RefKind, Repository, Worktree};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, MouseButton, PromptLevel,
    SharedString, Subscription, Task, Window, div, prelude::*, px,
};

use crate::{
    editor::{self, Editor, EditorEvent},
    project_picker, theme,
};

const ROW_HEIGHT: f32 = 26.;
const SHOWN: usize = 14;

pub enum WorktreePickerEvent {
    /// Open the worktree at this path (as git prints it) in a window.
    Open(String),
    /// Worktrees were added or removed.
    Changed(String),
    Dismissed,
}

impl EventEmitter<WorktreePickerEvent> for WorktreePicker {}

#[derive(Clone)]
enum Choice {
    Open(Worktree),
    /// Worktree for `branch` at `path`; `create` makes the branch first.
    Add {
        branch: String,
        path: String,
        create: bool,
    },
}

pub struct WorktreePicker {
    repository: Repository,
    query: Entity<Editor>,
    worktrees: Vec<Worktree>,
    branches: Vec<Ref>,
    rows: Vec<Choice>,
    selected: usize,
    error: Option<SharedString>,
    busy: bool,
    _task: Task<()>,
    _subscription: Subscription,
}

impl WorktreePicker {
    pub fn new(repository: Repository, cx: &mut Context<Self>) -> Self {
        let query = cx.new(Editor::single_line);
        let subscription = cx.subscribe(&query, |this, _, event, cx| {
            if let EditorEvent::Edited = event {
                this.error = None;
                this.filter(cx);
            }
        });
        let mut picker = Self {
            repository,
            query,
            worktrees: Vec::new(),
            branches: Vec::new(),
            rows: Vec::new(),
            selected: 0,
            error: None,
            busy: false,
            _task: Task::ready(()),
            _subscription: subscription,
        };
        picker.load(cx);
        picker
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let repository = self.repository.clone();
        self._task = cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    Ok::<_, std::io::Error>((repository.worktrees()?, repository.refs()?))
                })
                .await;
            this.update(cx, |this, cx| {
                match loaded {
                    Ok((worktrees, refs)) => {
                        this.worktrees = worktrees;
                        this.branches = refs
                            .into_iter()
                            .filter(|r| r.kind == RefKind::Branch)
                            .collect();
                    }
                    Err(err) => this.error = Some(err.to_string().into()),
                }
                this.filter(cx);
            })
            .ok();
        });
    }

    fn filter(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text();
        let query = query.trim();
        let needle = query.to_lowercase();
        let mut rows = Vec::new();
        if !query.is_empty() {
            let used = self
                .worktrees
                .iter()
                .any(|w| w.branch.as_deref() == Some(query));
            if !used {
                let exists = self.branches.iter().any(|b| b.name == query);
                rows.push(Choice::Add {
                    branch: query.to_owned(),
                    path: self.repository.default_worktree_path(query),
                    create: !exists,
                });
            }
        }
        rows.extend(
            self.worktrees
                .iter()
                .filter(|w| {
                    w.path.to_lowercase().contains(&needle)
                        || w.branch
                            .as_deref()
                            .is_some_and(|b| b.to_lowercase().contains(&needle))
                })
                .cloned()
                .map(Choice::Open),
        );
        self.rows = rows;
        self.selected = 0;
        cx.notify();
    }

    fn run(
        &mut self,
        cx: &mut Context<Self>,
        operation: impl FnOnce(&Repository) -> std::io::Result<WorktreePickerEvent> + Send + 'static,
    ) {
        let repository = self.repository.clone();
        self.busy = true;
        self.error = None;
        cx.notify();
        self._task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { operation(&repository) })
                .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(event) => cx.emit(event),
                    Err(err) => {
                        this.error = Some(err.to_string().into());
                        this.load(cx);
                    }
                }
                cx.notify();
            })
            .ok();
        });
    }

    fn choose(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        match self.rows.get(index).cloned() {
            Some(Choice::Open(worktree)) if worktree.prunable => {
                self.error = Some("That worktree's directory is gone.".into());
                cx.notify();
            }
            Some(Choice::Open(worktree)) => cx.emit(WorktreePickerEvent::Open(worktree.path)),
            Some(Choice::Add {
                branch,
                path,
                create,
            }) => self.run(cx, move |repo| {
                repo.add_worktree(&path, &branch, create, None)?;
                // Open the new worktree right away.
                Ok(WorktreePickerEvent::Open(path))
            }),
            None => {}
        }
    }

    fn remove(&mut self, worktree: Worktree, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Remove the worktree at {}?", worktree.path),
            Some(
                "Its directory is deleted; its branch is kept. “Force Remove” also discards \
                 uncommitted changes in it.",
            ),
            &["Remove", "Force Remove", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            let force = match answer.await {
                Ok(0) => false,
                Ok(1) => true,
                _ => return,
            };
            this.update(cx, |this, cx| {
                let path = worktree.path.clone();
                this.run(cx, move |repo| {
                    repo.remove_worktree(&path, force)?;
                    Ok(WorktreePickerEvent::Changed(format!(
                        "Removed worktree {path}"
                    )))
                });
            })
            .ok();
        })
        .detach();
    }

    fn select_prev(&mut self, _: &editor::Up, _: &mut Window, cx: &mut Context<Self>) {
        self.selected = self.selected.saturating_sub(1);
        cx.notify();
    }

    fn select_next(&mut self, _: &editor::Down, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected + 1 < self.rows.len().min(SHOWN) {
            self.selected += 1;
            cx.notify();
        }
    }

    fn confirm(&mut self, _: &editor::Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.choose(self.selected, cx);
    }

    fn dismiss(&mut self, _: &project_picker::Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(WorktreePickerEvent::Dismissed);
    }
}

impl Focusable for WorktreePicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for WorktreePicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            // Shares the project picker's Escape binding.
            .key_context("ProjectPicker")
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .on_mouse_down_out(
                cx.listener(|this, _, window, cx| {
                    this.dismiss(&project_picker::Dismiss, window, cx)
                }),
            )
            .w(px(640.))
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
                    .child("Open a worktree, or type a branch name to add one"),
            )
            .child(
                div()
                    .h(px(36.))
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(self.query.clone()),
            )
            .children(
                self.rows
                    .iter()
                    .take(SHOWN)
                    .enumerate()
                    .map(|(index, choice)| {
                        let (name, detail, removable) = match choice {
                            Choice::Add {
                                branch,
                                path,
                                create,
                            } => (
                                format!(
                                    "Add worktree for {} “{branch}”",
                                    if *create { "new branch" } else { "branch" }
                                ),
                                path.clone(),
                                None,
                            ),
                            Choice::Open(worktree) => {
                                let branch = worktree.branch.clone().unwrap_or_else(|| {
                                    if worktree.bare {
                                        "bare".into()
                                    } else {
                                        "detached".into()
                                    }
                                });
                                let mut detail = branch;
                                if worktree.current {
                                    detail.push_str(" · this window");
                                }
                                if worktree.prunable {
                                    detail.push_str(" · missing");
                                }
                                if worktree.locked {
                                    detail.push_str(" · locked");
                                }
                                let removable =
                                    (!worktree.current && !worktree.bare).then(|| worktree.clone());
                                (worktree.path.clone(), detail, removable)
                            }
                        };
                        div()
                            .id(index)
                            .h(px(ROW_HEIGHT))
                            .px_3()
                            .flex()
                            .items_center()
                            .gap_3()
                            .cursor_pointer()
                            .when(index == self.selected, |row| row.bg(theme::active_row()))
                            .when(index != self.selected, |row| {
                                row.hover(|style| style.bg(theme::hover_row()))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| this.choose(index, cx)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(theme::text())
                                    .child(name),
                            )
                            .child(
                                div()
                                    .max_w(px(260.))
                                    .truncate()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(detail),
                            )
                            .children(removable.map(|worktree| {
                                div()
                                    .id(("remove-worktree", index))
                                    .px_1()
                                    .text_color(theme::muted())
                                    .hover(|style| style.text_color(theme::error()))
                                    .child("×")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.remove(worktree.clone(), window, cx)
                                    }))
                            }))
                    }),
            )
            .when(self.busy, |picker| {
                picker.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(theme::muted())
                        .child("Working…"),
                )
            })
            .children(self.error.clone().map(|error| {
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(theme::error())
                    .child(error)
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    }
}
