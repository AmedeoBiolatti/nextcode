//! The command palette: every command available where focus was when it
//! opened, filtered by what is typed, run with Enter.

use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, MouseButton,
    ScrollStrategy, SharedString, Subscription, UniformListScrollHandle, Window, actions, div,
    prelude::*, px, rgb, uniform_list,
};

use crate::{
    commands,
    editor::{self, Editor, EditorEvent},
    theme,
};

actions!(command_palette, [Dismiss]);

const CONTEXT: &str = "CommandPalette";
const ROW_HEIGHT: f32 = 28.;
const VISIBLE_ROWS: usize = 14;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Dismiss, Some(CONTEXT))]);
}

pub struct Command {
    pub action: Box<dyn Action>,
    pub label: SharedString,
    pub category: SharedString,
    pub keys: Option<SharedString>,
    /// Lowercase `category label name`, for matching.
    haystack: String,
}

impl Command {
    pub fn new(action: Box<dyn Action>, keys: Option<String>) -> Self {
        let name = action.name();
        let label = commands::label(name);
        let category = commands::category(name);
        let haystack = format!("{category} {label} {name}").to_lowercase();
        Self {
            action,
            label: label.into(),
            category: category.into(),
            keys: keys.map(Into::into),
            haystack,
        }
    }
}

pub enum CommandPaletteEvent {
    Run(Box<dyn Action>),
    Dismissed,
}

impl EventEmitter<CommandPaletteEvent> for CommandPalette {}

pub struct CommandPalette {
    query: Entity<Editor>,
    commands: Vec<Command>,
    /// Indices into `commands` matching the query, in display order.
    matches: Vec<usize>,
    selected: usize,
    scroll: UniformListScrollHandle,
    _subscription: Subscription,
}

/// Whether every word of `query` occurs in `haystack`.
fn matches(haystack: &str, query: &str) -> bool {
    query
        .split_whitespace()
        .all(|word| haystack.contains(&word.to_lowercase()))
}

impl CommandPalette {
    /// `commands` sorted for display (by category, then label).
    pub fn new(mut commands: Vec<Command>, cx: &mut Context<Self>) -> Self {
        commands.sort_by(|a, b| (&a.category, &a.label).cmp(&(&b.category, &b.label)));
        let query = cx.new(Editor::single_line);
        let subscription = cx.subscribe(&query, |this, _, event, cx| {
            if let EditorEvent::Edited = event {
                this.filter(cx);
            }
        });
        let mut palette = Self {
            query,
            commands,
            matches: Vec::new(),
            selected: 0,
            scroll: UniformListScrollHandle::new(),
            _subscription: subscription,
        };
        palette.filter(cx);
        palette
    }

    fn filter(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text();
        self.matches = self
            .commands
            .iter()
            .enumerate()
            .filter(|(_, command)| matches(&command.haystack, &query))
            .map(|(index, _)| index)
            .collect();
        self.select(0, cx);
    }

    fn select(&mut self, row: usize, cx: &mut Context<Self>) {
        self.selected = row;
        self.scroll.scroll_to_item(row, ScrollStrategy::Center);
        cx.notify();
    }

    fn select_prev(&mut self, _: &editor::Up, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected.saturating_sub(1), cx);
    }

    fn select_next(&mut self, _: &editor::Down, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected + 1 < self.matches.len() {
            self.select(self.selected + 1, cx);
        }
    }

    fn confirm(&mut self, _: &editor::Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.run(self.selected, cx);
    }

    fn run(&mut self, row: usize, cx: &mut Context<Self>) {
        if let Some(&index) = self.matches.get(row) {
            cx.emit(CommandPaletteEvent::Run(
                self.commands[index].action.boxed_clone(),
            ));
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(CommandPaletteEvent::Dismissed);
    }
}

impl Focusable for CommandPalette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

#[cfg(test)]
impl CommandPalette {
    /// Action names and shortcuts, in display order.
    pub fn commands(&self) -> impl Iterator<Item = (&'static str, Option<String>)> {
        self.commands.iter().map(|command| {
            (
                command.action.name(),
                command.keys.as_ref().map(|keys| keys.to_string()),
            )
        })
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.matches.len().clamp(1, VISIBLE_ROWS);
        let list = if self.matches.is_empty() {
            div()
                .h(px(ROW_HEIGHT))
                .px_3()
                .flex()
                .items_center()
                .text_color(rgb(theme::MUTED))
                .child("No matching commands")
                .into_any_element()
        } else {
            uniform_list(
                "command-palette-rows",
                self.matches.len(),
                cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                    range
                        .map(|row| {
                            let command = &this.commands[this.matches[row]];
                            let selected = row == this.selected;
                            div()
                                .id(row)
                                .h(px(ROW_HEIGHT))
                                .px_3()
                                .flex()
                                .items_center()
                                .gap_2()
                                .cursor_pointer()
                                .when(selected, |row| row.bg(rgb(theme::ACTIVE_ROW)))
                                .when(!selected, |row| {
                                    row.hover(|style| style.bg(rgb(theme::HOVER_ROW)))
                                })
                                .on_click(cx.listener(move |this, _, _, cx| this.run(row, cx)))
                                .child(
                                    div()
                                        .text_color(rgb(theme::MUTED))
                                        .child(format!("{}:", command.category)),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .truncate()
                                        .text_color(rgb(theme::TEXT))
                                        .child(command.label.clone()),
                                )
                                .children(command.keys.clone().map(|keys| {
                                    div()
                                        .px_1()
                                        .rounded_sm()
                                        .bg(rgb(theme::BACKGROUND))
                                        .text_xs()
                                        .text_color(rgb(theme::MUTED))
                                        .child(keys)
                                }))
                        })
                        .collect()
                }),
            )
            .track_scroll(self.scroll.clone())
            .h(px(ROW_HEIGHT * rows as f32))
            .into_any_element()
        };
        div()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .on_mouse_down_out(
                cx.listener(|this, _, window, cx| this.dismiss(&Dismiss, window, cx)),
            )
            .w(px(640.))
            .flex()
            .flex_col()
            .bg(rgb(theme::PANEL))
            .border_1()
            .border_color(rgb(theme::BORDER))
            .rounded_md()
            .shadow_lg()
            .text_sm()
            .child(
                div()
                    .h(px(36.))
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(theme::BORDER))
                    .child(self.query.clone()),
            )
            .child(list)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_typed_word_must_match_in_any_order() {
        let haystack = "git branches… git::branches";
        assert!(matches(haystack, ""));
        assert!(matches(haystack, "branch git"));
        assert!(matches(haystack, "GIT"));
        assert!(!matches(haystack, "git push"));
    }
}
