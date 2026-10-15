//! Modal for branches: switch to a local or remote branch (or a tag),
//! create a branch from HEAD or a chosen commit, and delete local branches.

use git::{Ref, RefKind, Repository};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, MouseButton, PromptLevel,
    SharedString, Subscription, Task, Window, div, prelude::*, px,
};

use crate::{
    editor::{self, Editor, EditorEvent},
    git_panel::short_revision,
    project_picker, theme,
};

const ROW_HEIGHT: f32 = 26.;
const SHOWN: usize = 14;

pub enum BranchPickerEvent {
    /// The repository changed (switched, created or deleted a branch).
    Changed(String),
    Dismissed,
}

impl EventEmitter<BranchPickerEvent> for BranchPicker {}

#[derive(Clone)]
enum Choice {
    Create(String),
    Switch(Ref),
}

pub struct BranchPicker {
    repository: Repository,
    /// Where new branches start; `None` is HEAD.
    start: Option<String>,
    query: Entity<Editor>,
    refs: Vec<Ref>,
    rows: Vec<Choice>,
    selected: usize,
    error: Option<SharedString>,
    busy: bool,
    _task: Task<()>,
    _subscription: Subscription,
}

impl BranchPicker {
    pub fn new(repository: Repository, start: Option<String>, cx: &mut Context<Self>) -> Self {
        let query = cx.new(Editor::single_line);
        let subscription = cx.subscribe(&query, |this, _, event, cx| {
            if let EditorEvent::Edited = event {
                this.error = None;
                this.filter(cx);
            }
        });
        let mut picker = Self {
            repository,
            start,
            query,
            refs: Vec::new(),
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
            let refs = cx.background_spawn(async move { repository.refs() }).await;
            this.update(cx, |this, cx| {
                match refs {
                    Ok(refs) => this.refs = refs,
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
        if !query.is_empty() && !self.refs.iter().any(|r| r.name == query) {
            rows.push(Choice::Create(query.to_owned()));
        }
        let mut refs: Vec<&Ref> = self
            .refs
            .iter()
            .filter(|r| r.name.to_lowercase().contains(&needle))
            .collect();
        refs.sort_by_key(|r| (r.kind as u8, !r.head));
        rows.extend(refs.into_iter().cloned().map(Choice::Switch));
        self.rows = rows;
        self.selected = 0;
        cx.notify();
    }

    /// Runs a branch operation off the main thread; success closes the picker.
    fn run(
        &mut self,
        cx: &mut Context<Self>,
        done: String,
        operation: impl FnOnce(&Repository) -> std::io::Result<()> + Send + 'static,
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
                    Ok(()) => cx.emit(BranchPickerEvent::Changed(done)),
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
            Some(Choice::Create(name)) => {
                let start = self.start.clone();
                self.run(cx, format!("Switched to new branch {name}"), move |repo| {
                    repo.create_branch(&name, start.as_deref(), true)
                });
            }
            Some(Choice::Switch(branch)) => {
                if branch.head {
                    return cx.emit(BranchPickerEvent::Dismissed);
                }
                self.run(cx, format!("Switched to {}", branch.name), move |repo| {
                    repo.switch_branch(&branch)
                });
            }
            None => {}
        }
    }

    fn delete(&mut self, branch: Ref, window: &mut Window, cx: &mut Context<Self>) {
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete branch {}?", branch.name),
            Some("“Force delete” also deletes it when it has commits no other branch has."),
            &["Delete", "Force Delete", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            let force = match answer.await {
                Ok(0) => false,
                Ok(1) => true,
                _ => return,
            };
            this.update(cx, |this, cx| {
                let name = branch.name.clone();
                this.run(cx, format!("Deleted branch {name}"), move |repo| {
                    repo.delete_branch(&name, force)
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
        cx.emit(BranchPickerEvent::Dismissed);
    }

    fn start_label(&self) -> String {
        self.start.as_deref().map_or_else(
            || "HEAD".to_owned(),
            |start| short_revision(start).to_owned(),
        )
    }
}

impl Focusable for BranchPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for BranchPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = match &self.start {
            Some(_) => format!("Create a branch at {} (type a name)", self.start_label()),
            None => "Switch branch, or type a name to create one".to_owned(),
        };
        let start = self.start_label();
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
            .w(px(560.))
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
                    .child(title),
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
                        let (name, detail, deletable) = match choice {
                            Choice::Create(name) => (
                                format!("Create branch “{name}”"),
                                format!("from {start}, and switch to it"),
                                None,
                            ),
                            Choice::Switch(r) => {
                                let kind = match r.kind {
                                    RefKind::Branch if r.head => "current branch",
                                    RefKind::Branch => "branch",
                                    RefKind::Remote => "remote branch · checks out a local copy",
                                    RefKind::Tag => "tag · detached",
                                };
                                let deletable =
                                    (r.kind == RefKind::Branch && !r.head).then(|| r.clone());
                                (r.name.clone(), format!("{kind} · {}", r.target), deletable)
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
                            .child(div().text_xs().text_color(theme::muted()).child(detail))
                            .children(deletable.map(|branch| {
                                div()
                                    .id(("delete-branch", index))
                                    .px_1()
                                    .text_color(theme::muted())
                                    .hover(|style| style.text_color(theme::error()))
                                    .child("×")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.delete(branch.clone(), window, cx)
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
