//! "Open Workflow…" modal: the workflows found (the user's, then each open
//! project's), filtered as typed; Enter opens the chosen one's tab.

use std::path::PathBuf;

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, MouseButton,
    Subscription, Window, actions, div, prelude::*, px,
};

use super::{Discovered, Source, Workflows};
use crate::{
    editor::{self, Editor, EditorEvent},
    theme,
};

actions!(workflow_picker, [Dismiss]);

const CONTEXT: &str = "WorkflowPicker";
const ROW_HEIGHT: f32 = 46.;
const VISIBLE_ROWS: usize = 10;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Dismiss, Some(CONTEXT))]);
}

pub enum WorkflowPickerEvent {
    Open { source: Source, path: PathBuf },
    Dismissed,
}

impl EventEmitter<WorkflowPickerEvent> for WorkflowPicker {}

pub struct WorkflowPicker {
    workflows: Entity<Workflows>,
    query: Entity<Editor>,
    /// The workflows matching the query.
    rows: Vec<Discovered>,
    selected: usize,
    _subscriptions: [Subscription; 2],
}

impl WorkflowPicker {
    /// Lists the workflows as last read and reads them again.
    pub fn new(workflows: Entity<Workflows>, cx: &mut Context<Self>) -> Self {
        let query = cx.new(Editor::single_line);
        let edited = cx.subscribe(&query, |this, _, event, cx| {
            if let EditorEvent::Edited = event {
                this.filter(cx);
            }
        });
        let listed = cx.observe(&workflows, |this, _, cx| this.filter(cx));
        workflows.update(cx, |workflows, cx| workflows.refresh(cx).detach());
        let mut picker = Self {
            workflows,
            query,
            rows: Vec::new(),
            selected: 0,
            _subscriptions: [edited, listed],
        };
        picker.filter(cx);
        picker
    }

    fn filter(&mut self, cx: &mut Context<Self>) {
        let needle = self.query.read(cx).text().trim().to_lowercase();
        let selected = self.rows.get(self.selected).map(|row| row.id.clone());
        self.rows = self
            .workflows
            .read(cx)
            .discovered()
            .iter()
            .filter(|found| {
                needle.is_empty()
                    || found.name.to_lowercase().contains(&needle)
                    || found.source.label().to_lowercase().contains(&needle)
            })
            .cloned()
            .collect();
        // The list refreshes under the cursor: keep the row chosen.
        self.selected = selected
            .and_then(|id| self.rows.iter().position(|row| row.id == id))
            .unwrap_or(0);
        cx.notify();
    }

    fn select_prev(&mut self, _: &editor::Up, _: &mut Window, cx: &mut Context<Self>) {
        self.selected = self.selected.saturating_sub(1);
        cx.notify();
    }

    fn select_next(&mut self, _: &editor::Down, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected + 1 < self.rows.len() {
            self.selected += 1;
            cx.notify();
        }
    }

    fn confirm(&mut self, _: &editor::Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.choose(self.selected, cx);
    }

    fn choose(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(found) = self.rows.get(index) {
            cx.emit(WorkflowPickerEvent::Open {
                source: found.source.clone(),
                path: found.path.clone(),
            });
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(WorkflowPickerEvent::Dismissed);
    }
}

impl Focusable for WorkflowPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for WorkflowPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let first = (self.selected + 1).saturating_sub(VISIBLE_ROWS);
        div()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .on_mouse_down_out(
                cx.listener(|this, _, window, cx| this.dismiss(&Dismiss, window, cx)),
            )
            .w(px(520.))
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
                    .enumerate()
                    .skip(first)
                    .take(VISIBLE_ROWS)
                    .map(|(index, found)| {
                        div()
                            .id(index)
                            .h(px(ROW_HEIGHT))
                            .px_3()
                            .flex()
                            .flex_col()
                            .justify_center()
                            .cursor_pointer()
                            .text_color(theme::text())
                            .when(index == self.selected, |row| row.bg(theme::active_row()))
                            .when(index != self.selected, |row| {
                                row.hover(|style| style.bg(theme::hover_row()))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| this.choose(index, cx)))
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .child(found.name.clone()),
                                    )
                                    .when(found.parsed.is_err(), |row| {
                                        row.child(
                                            div()
                                                .text_xs()
                                                .text_color(theme::error())
                                                .child("invalid"),
                                        )
                                    }),
                            )
                            .child(div().text_xs().text_color(theme::muted()).truncate().child(
                                format!("{} · {}", found.source.label(), found.path.display()),
                            ))
                    }),
            )
            .when(self.rows.is_empty(), |picker| {
                picker.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(
                            "No workflows. Run > New Workflow… creates one in the user folder; \
                             projects can have .next_code/workflows/*.jsonc.",
                        ),
                )
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    }
}
