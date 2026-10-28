//! Notes panel: every sticky note of the project grouped by file, with
//! filters (open/resolved, to me/to an agent); a click opens the file at
//! the note with its thread. Notes whose lines are gone (orphaned) are
//! listed too, so none is ever lost.

use std::collections::HashMap;

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FontWeight, Subscription, Task, Window, div,
    prelude::*, px,
};
use notes::{AnchorState, AuthorKind, Note};

use crate::{
    note_store::{self, NoteStore},
    pinpoint::{self, Pin},
    selectable::selectable,
    theme,
};

pub enum NotesPanelEvent {
    /// Open folder `root`'s `file` at note `id`.
    Open {
        root: usize,
        file: String,
        id: String,
    },
}

impl EventEmitter<NotesPanelEvent> for NotesPanel {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    Open,
    Resolved,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToFilter {
    Anyone,
    /// Addressed to the person (or to anyone, by an agent).
    Me,
    Agent,
}

pub struct NotesPanel {
    store: Entity<NoteStore>,
    status: StatusFilter,
    to: ToFilter,
    /// Where notes are in their files on disk, by (folder, id): 1-based
    /// line and state; computed in the background per store generation.
    positions: HashMap<(usize, String), (usize, AnchorState)>,
    positions_generation: Option<u64>,
    _positions_task: Option<Task<()>>,
    _observe: Subscription,
}

#[derive(Debug, Clone, PartialEq)]
enum Row {
    File {
        root: usize,
        file: String,
        label: String,
        count: usize,
    },
    Note {
        root: usize,
        file: String,
        id: String,
        line: usize,
        state: AnchorState,
        open: bool,
        agent: bool,
        author: String,
        to: Option<String>,
        text: String,
        replies: usize,
        cell: Option<usize>,
    },
}

impl NotesPanel {
    pub fn new(store: Entity<NoteStore>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&store, |this, _, cx| {
            this.refresh_positions(cx);
            cx.notify();
        });
        let mut panel = Self {
            store,
            status: StatusFilter::Open,
            to: ToFilter::Anyone,
            positions: HashMap::new(),
            positions_generation: None,
            _positions_task: None,
            _observe: observe,
        };
        panel.refresh_positions(cx);
        panel
    }

    pub fn store(&self) -> &Entity<NoteStore> {
        &self.store
    }

    /// Re-anchors every note against its file as saved, in the background,
    /// to show current lines and orphaned notes.
    fn refresh_positions(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        if self.positions_generation == Some(store.generation()) {
            return;
        }
        self.positions_generation = Some(store.generation());
        let project = store.project().clone();
        let mut by_file: HashMap<(usize, String), Vec<Note>> = HashMap::new();
        for (root, note) in store.all() {
            if note.cell.is_none() {
                by_file
                    .entry((root, note.file.clone()))
                    .or_default()
                    .push(note.clone());
            }
        }
        self._positions_task = Some(cx.spawn(async move |this, cx| {
            let positions = cx
                .background_spawn(async move {
                    let mut positions = HashMap::new();
                    for ((root, file), notes) in by_file {
                        let text = project.read_text(&project.resolve_in(root, &file)).ok();
                        for note in notes {
                            let found = match &text {
                                Some(text) => note.reanchor(text),
                                None => notes::Anchored {
                                    start_line: note.anchor.start_line,
                                    end_line: note.anchor.end_line,
                                    state: AnchorState::Orphaned,
                                },
                            };
                            positions.insert((root, note.id), (found.start_line, found.state));
                        }
                    }
                    positions
                })
                .await;
            this.update(cx, |this, cx| {
                this.positions = positions;
                cx.notify();
            })
            .ok();
        }));
    }

    fn rows(&self, cx: &App) -> Vec<Row> {
        let store = self.store.read(cx);
        let project = store.project();
        let user = store.user();
        let mut notes: Vec<(usize, &Note)> = store
            .all()
            .filter(|(_, note)| match self.status {
                StatusFilter::Open => note.is_open(),
                StatusFilter::Resolved => !note.is_open(),
                StatusFilter::All => true,
            })
            .filter(|(_, note)| match self.to {
                ToFilter::Anyone => true,
                ToFilter::Me => {
                    note_store::awaits(note, user) || {
                        note.to.is_some() && user.is_addressed_by(note.to.as_deref())
                    }
                }
                ToFilter::Agent => {
                    note.to.as_deref() == Some("agent")
                        || note
                            .to
                            .as_deref()
                            .is_some_and(|to| to != "user" && !user.is_addressed_by(Some(to)))
                }
            })
            .collect();
        // An open editor's position (unsaved edits included), else the
        // file's as saved.
        let position = |root: usize, note: &Note| {
            store
                .live_position(root, &note.id)
                .or_else(|| self.positions.get(&(root, note.id.clone())).copied())
                .unwrap_or((note.anchor.start_line, AnchorState::Exact))
        };
        notes.sort_by_key(|(root, note)| {
            (*root, note.file.clone(), note.cell, position(*root, note).0)
        });
        let mut rows = Vec::new();
        for (root, note) in notes {
            let new_file = !matches!(rows.last(), Some(Row::Note { root: r, file, .. }) if *r == root && *file == note.file);
            if new_file {
                let label = if project.is_multi_root() {
                    crate::project::root_label(&project.roots()[root].name, &note.file)
                } else {
                    note.file.clone()
                };
                rows.push(Row::File {
                    root,
                    file: note.file.clone(),
                    label,
                    count: 0,
                });
            }
            if let Some(Row::File { count, .. }) = rows
                .iter_mut()
                .rev()
                .find(|row| matches!(row, Row::File { .. }))
            {
                *count += 1;
            }
            let (line, state) = position(root, note);
            let first = note.thread.first();
            rows.push(Row::Note {
                root,
                file: note.file.clone(),
                id: note.id.clone(),
                line,
                state,
                open: note.is_open(),
                agent: first.is_some_and(|m| m.author.kind == AuthorKind::Agent),
                author: first.map(|m| m.author.name.clone()).unwrap_or_default(),
                to: note.to.clone(),
                text: note.text().lines().next().unwrap_or_default().to_owned(),
                replies: note.thread.len().saturating_sub(1),
                cell: note.cell,
            });
        }
        rows
    }

    fn filter_button(
        &self,
        id: &'static str,
        label: &'static str,
        active: bool,
        on_click: impl Fn(&mut Self) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .px_2()
            .py(px(1.))
            .rounded_sm()
            .cursor_pointer()
            .text_xs()
            .when(active, |button| {
                button.bg(theme::active_row()).text_color(theme::text())
            })
            .when(!active, |button| button.text_color(theme::muted()))
            .hover(|style| style.bg(theme::hover_row()))
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                on_click(this);
                cx.notify();
            }))
            .pin(format!("filter:{id}"))
            .label(label)
            .into_any_element()
    }
}

impl Render for NotesPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows(cx);
        let store = self.store.read(cx);
        let error = store.error().cloned();
        let show_resolved = store.show_resolved();
        let project = store.project().clone();
        let filters = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme::border())
            .child(self.filter_button(
                "open",
                "Open",
                self.status == StatusFilter::Open,
                |this| this.status = StatusFilter::Open,
                cx,
            ))
            .child(self.filter_button(
                "resolved",
                "Resolved",
                self.status == StatusFilter::Resolved,
                |this| this.status = StatusFilter::Resolved,
                cx,
            ))
            .child(self.filter_button(
                "all",
                "All",
                self.status == StatusFilter::All,
                |this| this.status = StatusFilter::All,
                cx,
            ))
            .child(div().w(px(12.)))
            .child(self.filter_button(
                "anyone",
                "Anyone",
                self.to == ToFilter::Anyone,
                |this| this.to = ToFilter::Anyone,
                cx,
            ))
            .child(self.filter_button(
                "to-me",
                "To me",
                self.to == ToFilter::Me,
                |this| this.to = ToFilter::Me,
                cx,
            ))
            .child(self.filter_button(
                "to-agent",
                "To agents",
                self.to == ToFilter::Agent,
                |this| this.to = ToFilter::Agent,
                cx,
            ))
            .child(div().flex_1())
            .child(
                div()
                    .id("show-resolved")
                    .px_2()
                    .text_xs()
                    .cursor_pointer()
                    .text_color(theme::muted())
                    .hover(|style| style.text_color(theme::text()))
                    .child(if show_resolved {
                        "☑ Resolved in editors"
                    } else {
                        "☐ Resolved in editors"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.store.update(cx, |store, cx| {
                            let show = !store.show_resolved();
                            store.set_show_resolved(show, cx);
                        });
                    }))
                    .pin("show-resolved")
                    .label("Show resolved notes in editors"),
            );
        let body = if rows.is_empty() {
            div()
                .p_3()
                .text_sm()
                .text_color(theme::muted())
                .child(selectable(
                    "empty",
                    "No notes here. Add one in an editor with Add Note (Ctrl+K Ctrl+N), \
                     or let an agent run `next_code notes add`.",
                ))
                .into_any_element()
        } else {
            div()
                .id("notes-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .children(rows.into_iter().enumerate().map(|(index, row)| match row {
                    Row::File {
                        root,
                        file,
                        label,
                        count,
                    } => {
                        let key = pinpoint::file_key(&project, &project.resolve_in(root, &file));
                        div()
                            .id(index)
                            .h(px(24.))
                            .px_2()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_sm()
                            .text_color(theme::text())
                            .child(selectable("path", label.clone()))
                            .child(div().text_xs().text_color(theme::muted()).child(format!(
                                "{count} note{}",
                                if count == 1 { "" } else { "s" }
                            )))
                            .pin(format!("file:{key}"))
                            .label(label)
                            .into_any_element()
                    }
                    Row::Note {
                        root,
                        file,
                        id,
                        line,
                        state,
                        open,
                        agent,
                        author,
                        to,
                        text,
                        replies,
                        cell,
                    } => {
                        let place = match cell {
                            Some(cell) => format!("cell {cell}, L{line}"),
                            None => format!("L{line}"),
                        };
                        let badge = match (state, open) {
                            (AnchorState::Orphaned, _) => Some(("lines deleted", theme::warning())),
                            (_, false) => Some(("resolved", theme::muted())),
                            _ => None,
                        };
                        let label = format!("{author}: {text}");
                        div()
                            .id(index)
                            .pl(px(24.))
                            .pr_2()
                            .py(px(3.))
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_sm()
                            .cursor_pointer()
                            .hover(|style| style.bg(theme::hover_row()))
                            .on_click(cx.listener({
                                let id = id.clone();
                                move |_, _, _, cx| {
                                    cx.emit(NotesPanelEvent::Open {
                                        root,
                                        file: file.clone(),
                                        id: id.clone(),
                                    })
                                }
                            }))
                            .child(
                                div()
                                    .w(px(48.))
                                    .flex_none()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(place),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(if agent {
                                        theme::accent_text()
                                    } else {
                                        theme::text()
                                    })
                                    .child(format!("{}{author}", if agent { "◆ " } else { "" })),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(if open { theme::text() } else { theme::muted() })
                                    .child(selectable("text", text)),
                            )
                            .when(replies > 0, |row| {
                                row.child(
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(theme::muted())
                                        .child(format!(
                                            "{replies} repl{}",
                                            if replies == 1 { "y" } else { "ies" }
                                        )),
                                )
                            })
                            .children(badge.map(|(badge, color)| {
                                div().flex_none().text_xs().text_color(color).child(badge)
                            }))
                            .child(
                                div()
                                    .flex_none()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(format!("→ {}", to.as_deref().unwrap_or("anyone"))),
                            )
                            .pin(format!("note:{id}"))
                            .label(label)
                            .into_any_element()
                    }
                }))
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(filters)
            .children(error.map(|error| {
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(theme::error())
                    .child(selectable("error", error))
            }))
            .child(body)
    }
}
