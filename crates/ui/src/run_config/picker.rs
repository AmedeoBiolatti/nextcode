//! "Select Run Configuration" modal: configurations by source, filtered as
//! typed; disabled ones show why. The last row edits the configurations.

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, MouseButton,
    SharedString, Subscription, Window, actions, div, prelude::*, px,
};
use run::{Configuration, Source};

use crate::{
    editor::{self, Editor, EditorEvent},
    selectable::selectable,
    theme,
};

actions!(configuration_picker, [Dismiss]);

const CONTEXT: &str = "ConfigurationPicker";
const ROW_HEIGHT: f32 = 40.;
const VISIBLE_ROWS: usize = 12;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Dismiss, Some(CONTEXT))]);
}

pub enum ConfigurationPickerEvent {
    Select(String),
    Edit,
    Dismissed,
}

impl EventEmitter<ConfigurationPickerEvent> for ConfigurationPicker {}

#[derive(Clone)]
struct Row {
    name: SharedString,
    detail: SharedString,
    disabled: Option<SharedString>,
}

/// Rows matching the query, then "Edit Configurations…".
enum Entry {
    Configuration(usize),
    Edit,
}

pub struct ConfigurationPicker {
    query: Entity<Editor>,
    rows: Vec<Row>,
    entries: Vec<Entry>,
    current: Option<String>,
    selected: usize,
    _subscription: Subscription,
}

impl ConfigurationPicker {
    pub fn new(configs: &[Configuration], current: Option<String>, cx: &mut Context<Self>) -> Self {
        let query = cx.new(Editor::single_line);
        let subscription = cx.subscribe(&query, |this, _, event, cx| {
            if let EditorEvent::Edited = event {
                this.filter(cx);
            }
        });
        let rows = configs
            .iter()
            .map(|config| {
                let source = match config.source {
                    Source::Automatic => "auto",
                    Source::LaunchJson => "launch.json",
                    Source::RunJson => "run.json",
                };
                Row {
                    name: config.name.clone().into(),
                    detail: format!("{source} · {}", config.summary()).into(),
                    disabled: config.disabled.clone().map(Into::into),
                }
            })
            .collect();
        let mut picker = Self {
            query,
            rows,
            entries: Vec::new(),
            current,
            selected: 0,
            _subscription: subscription,
        };
        picker.filter(cx);
        picker.selected = picker
            .entries
            .iter()
            .position(|entry| match entry {
                Entry::Configuration(index) => {
                    picker.current.as_deref() == Some(picker.rows[*index].name.as_ref())
                }
                Entry::Edit => false,
            })
            .unwrap_or(0);
        picker
    }

    fn filter(&mut self, cx: &mut Context<Self>) {
        let needle = self.query.read(cx).text().trim().to_lowercase();
        self.entries = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                row.name.to_lowercase().contains(&needle)
                    || row.detail.to_lowercase().contains(&needle)
            })
            .map(|(index, _)| Entry::Configuration(index))
            .collect();
        self.entries.push(Entry::Edit);
        self.selected = 0;
        cx.notify();
    }

    fn select_prev(&mut self, _: &editor::Up, _: &mut Window, cx: &mut Context<Self>) {
        self.selected = self.selected.saturating_sub(1);
        cx.notify();
    }

    fn select_next(&mut self, _: &editor::Down, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
            cx.notify();
        }
    }

    fn confirm(&mut self, _: &editor::Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.choose(self.selected, cx);
    }

    fn choose(&mut self, index: usize, cx: &mut Context<Self>) {
        match self.entries.get(index) {
            Some(Entry::Configuration(row)) => {
                let row = &self.rows[*row];
                if row.disabled.is_none() {
                    cx.emit(ConfigurationPickerEvent::Select(row.name.to_string()));
                }
            }
            Some(Entry::Edit) => cx.emit(ConfigurationPickerEvent::Edit),
            None => {}
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ConfigurationPickerEvent::Dismissed);
    }
}

impl Focusable for ConfigurationPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for ConfigurationPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let first = (self.selected + 1).saturating_sub(VISIBLE_ROWS);
        let rows = self
            .entries
            .iter()
            .enumerate()
            .skip(first)
            .take(VISIBLE_ROWS)
            .map(|(index, entry)| {
                let row = div()
                    .id(index)
                    .min_h(px(ROW_HEIGHT))
                    .px_3()
                    .py_1()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .cursor_pointer()
                    .when(index == self.selected, |row| row.bg(theme::active_row()))
                    .when(index != self.selected, |row| {
                        row.hover(|style| style.bg(theme::hover_row()))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.choose(index, cx)));
                match entry {
                    Entry::Configuration(row_index) => {
                        let data = &self.rows[*row_index];
                        let current = self.current.as_deref() == Some(data.name.as_ref());
                        row.child(
                            div()
                                .flex()
                                .gap_2()
                                .text_color(if data.disabled.is_some() {
                                    theme::muted()
                                } else {
                                    theme::text()
                                })
                                .child(
                                    div()
                                        .flex_1()
                                        .truncate()
                                        .child(selectable("name", data.name.clone())),
                                )
                                .when(current, |line| {
                                    line.child(
                                        div()
                                            .text_xs()
                                            .text_color(theme::muted())
                                            .child(selectable("current", "selected")),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .text_xs()
                                .truncate()
                                .text_color(if data.disabled.is_some() {
                                    theme::warning()
                                } else {
                                    theme::muted()
                                })
                                .child(selectable(
                                    "detail",
                                    match &data.disabled {
                                        Some(reason) => format!("disabled: {reason}").into(),
                                        None => data.detail.clone(),
                                    },
                                )),
                        )
                    }
                    Entry::Edit => row.child(
                        div()
                            .text_color(theme::accent_text())
                            .child("Edit Configurations…"),
                    ),
                }
            });
        div()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .on_mouse_down_out(
                cx.listener(|this, _, window, cx| this.dismiss(&Dismiss, window, cx)),
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
                    .h(px(36.))
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(self.query.clone()),
            )
            .children(rows)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    }
}
