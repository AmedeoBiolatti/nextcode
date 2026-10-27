//! "Select Theme" modal: built-in and user themes, filtered as typed.

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, MouseButton,
    SharedString, Subscription, Window, actions, div, prelude::*, px,
};

use crate::{
    editor::{self, Editor, EditorEvent},
    pinpoint::{self, Pin},
    selectable::selectable,
    theme,
};

actions!(theme_picker, [Dismiss]);

const CONTEXT: &str = "ThemePicker";
const ROW_HEIGHT: f32 = 28.;
const VISIBLE_ROWS: usize = 14;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Dismiss, Some(CONTEXT))]);
}

pub enum ThemePickerEvent {
    Select(SharedString),
    Dismissed,
}

impl EventEmitter<ThemePickerEvent> for ThemePicker {}

pub struct ThemePicker {
    query: Entity<Editor>,
    themes: Vec<SharedString>,
    current: SharedString,
    /// Themes matching the query.
    rows: Vec<SharedString>,
    selected: usize,
    _subscription: Subscription,
}

impl ThemePicker {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let query = cx.new(Editor::single_line);
        let subscription = cx.subscribe(&query, |this, _, event, cx| {
            if let EditorEvent::Edited = event {
                this.filter(cx);
            }
        });
        let mut picker = Self {
            query,
            themes: theme::names().into_iter().map(Into::into).collect(),
            current: theme::current().name.clone().into(),
            rows: Vec::new(),
            selected: 0,
            _subscription: subscription,
        };
        picker.filter(cx);
        picker.selected = picker
            .rows
            .iter()
            .position(|name| *name == picker.current)
            .unwrap_or(0);
        picker
    }

    fn filter(&mut self, cx: &mut Context<Self>) {
        let needle = self.query.read(cx).text().trim().to_lowercase();
        self.rows = self
            .themes
            .iter()
            .filter(|name| name.to_lowercase().contains(&needle))
            .cloned()
            .collect();
        self.selected = 0;
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
        if let Some(name) = self.rows.get(index) {
            cx.emit(ThemePickerEvent::Select(name.clone()));
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ThemePickerEvent::Dismissed);
    }
}

impl Focusable for ThemePicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for ThemePicker {
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
            .w(px(420.))
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
                    .child(self.query.clone())
                    .pin("input")
                    .label("Theme name"),
            )
            .children(
                self.rows
                    .iter()
                    .enumerate()
                    .skip(first)
                    .take(VISIBLE_ROWS)
                    .map(|(index, name)| {
                        div()
                            .id(index)
                            .h(px(ROW_HEIGHT))
                            .px_3()
                            .flex()
                            .items_center()
                            .gap_2()
                            .cursor_pointer()
                            .text_color(theme::text())
                            .when(index == self.selected, |row| row.bg(theme::active_row()))
                            .when(index != self.selected, |row| {
                                row.hover(|style| style.bg(theme::hover_row()))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| this.choose(index, cx)))
                            .child(div().flex_1().child(name.clone()))
                            .when(*name == self.current, |row| {
                                row.child(
                                    div().text_xs().text_color(theme::muted()).child("current"),
                                )
                            })
                            .pin(format!("item:{}", pinpoint::kebab(name)))
                            .label(name.clone())
                    }),
            )
            .when(self.rows.is_empty(), |picker| {
                picker.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(selectable("empty", "No matching themes"))
                        .pin("empty")
                        .interactive(false),
                )
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .pin("theme-picker")
            .label("Select Theme")
    }
}
