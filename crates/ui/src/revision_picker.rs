//! Modal for choosing a revision: a branch, remote branch or tag, any typed
//! revision (commit id, `HEAD~3`, …), or optionally the working tree.

use git::{Ref, RefKind, Repository, Side};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, MouseButton, SharedString,
    Subscription, Task, Window, div, prelude::*, px,
};

use crate::{
    editor::{self, Editor, EditorEvent},
    pinpoint::Pin,
    project_picker,
    selectable::selectable,
    theme,
};

const ROW_HEIGHT: f32 = 26.;
const SHOWN: usize = 14;

pub enum RevisionPickerEvent {
    Picked(Side),
    Dismissed,
}

impl EventEmitter<RevisionPickerEvent> for RevisionPicker {}

#[derive(Clone)]
enum Choice {
    Worktree,
    Ref(Ref),
    /// Whatever was typed; resolved before it is accepted.
    Typed(String),
}

pub struct RevisionPicker {
    repository: Repository,
    title: &'static str,
    allow_worktree: bool,
    query: Entity<Editor>,
    refs: Vec<Ref>,
    rows: Vec<Choice>,
    selected: usize,
    error: Option<SharedString>,
    _load: Task<()>,
    _subscription: Subscription,
}

impl RevisionPicker {
    pub fn new(
        repository: Repository,
        title: &'static str,
        allow_worktree: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(Editor::single_line);
        let subscription = cx.subscribe(&query, |this, _, event, cx| {
            if let EditorEvent::Edited = event {
                this.error = None;
                this.filter(cx);
            }
        });
        let loader = repository.clone();
        let load = cx.spawn(async move |this, cx| {
            let refs = cx.background_spawn(async move { loader.refs() }).await;
            this.update(cx, |this, cx| {
                match refs {
                    Ok(refs) => this.refs = refs,
                    Err(err) => this.error = Some(err.to_string().into()),
                }
                this.filter(cx);
            })
            .ok();
        });
        let mut picker = Self {
            repository,
            title,
            allow_worktree,
            query,
            refs: Vec::new(),
            rows: Vec::new(),
            selected: 0,
            error: None,
            _load: load,
            _subscription: subscription,
        };
        picker.filter(cx);
        picker
    }

    fn filter(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text();
        let query = query.trim();
        let needle = query.to_lowercase();
        let mut rows = Vec::new();
        if self.allow_worktree && "working tree".contains(&needle) {
            rows.push(Choice::Worktree);
        }
        if !query.is_empty() && !self.refs.iter().any(|r| r.name == query) {
            rows.push(Choice::Typed(query.to_owned()));
        }
        // Local branches first, then remote branches, then tags.
        let mut refs: Vec<&Ref> = self
            .refs
            .iter()
            .filter(|r| r.name.to_lowercase().contains(&needle))
            .collect();
        refs.sort_by_key(|r| (r.kind as u8, !r.head));
        rows.extend(refs.into_iter().cloned().map(Choice::Ref));
        self.rows = rows;
        self.selected = 0;
        cx.notify();
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

    fn choose(&mut self, index: usize, cx: &mut Context<Self>) {
        match self.rows.get(index).cloned() {
            Some(Choice::Worktree) => cx.emit(RevisionPickerEvent::Picked(Side::Worktree)),
            Some(Choice::Ref(r)) => cx.emit(RevisionPickerEvent::Picked(Side::Revision(r.name))),
            Some(Choice::Typed(revision)) => {
                let repository = self.repository.clone();
                self._load = cx.spawn(async move |this, cx| {
                    let check = revision.clone();
                    let resolved = cx
                        .background_spawn(async move { repository.resolve(&check) })
                        .await;
                    this.update(cx, |this, cx| match resolved {
                        Ok(_) => cx.emit(RevisionPickerEvent::Picked(Side::Revision(revision))),
                        Err(err) => {
                            this.error = Some(err.to_string().into());
                            cx.notify();
                        }
                    })
                    .ok();
                });
            }
            None => {}
        }
    }

    fn dismiss(&mut self, _: &project_picker::Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(RevisionPickerEvent::Dismissed);
    }

    fn label(choice: &Choice) -> (String, String) {
        match choice {
            Choice::Worktree => ("Working tree".into(), "including unstaged changes".into()),
            Choice::Typed(revision) => (
                format!("Use “{revision}”"),
                "commit, branch or expression".into(),
            ),
            Choice::Ref(r) => {
                let kind = match r.kind {
                    RefKind::Branch if r.head => "current branch",
                    RefKind::Branch => "branch",
                    RefKind::Remote => "remote branch",
                    RefKind::Tag => "tag",
                };
                (r.name.clone(), format!("{kind} · {}", r.target))
            }
        }
    }
}

impl Focusable for RevisionPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for RevisionPicker {
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
                    .child(selectable("title", self.title)),
            )
            .child(
                div()
                    .h(px(36.))
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(self.query.clone())
                    .pin("input")
                    .label("Revision"),
            )
            .children(
                self.rows
                    .iter()
                    .take(SHOWN)
                    .enumerate()
                    .map(|(index, choice)| {
                        let (name, detail) = Self::label(choice);
                        let pin = format!("item:{name}");
                        let label = name.clone();
                        div()
                            .id(index)
                            .h(px(ROW_HEIGHT))
                            .px_3()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .cursor_pointer()
                            .when(index == self.selected, |row| row.bg(theme::active_row()))
                            .when(index != self.selected, |row| {
                                row.hover(|style| style.bg(theme::hover_row()))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| this.choose(index, cx)))
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .child(selectable("name", name)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(selectable("detail", detail)),
                            )
                            .pin(pin)
                            .label(label)
                    }),
            )
            .children(self.error.clone().map(|error| {
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(theme::error())
                    .child(selectable("error", error))
                    .pin("error")
                    .interactive(false)
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .pin("revision-picker")
            .label(self.title)
    }
}
