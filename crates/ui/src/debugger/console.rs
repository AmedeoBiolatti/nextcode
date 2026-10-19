//! The Debug Console (bottom dock): the session's output (stdout, stderr
//! and adapter messages colored by category) and a REPL input that
//! evaluates in the selected stack frame, with Up/Down history.

use std::ops::Range;

use gpui::{
    App, Context, Entity, FocusHandle, Focusable, Hsla, KeyBinding, ScrollStrategy, Subscription,
    UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};

use super::{
    ClearConsole, NextConsoleInput, PreviousConsoleInput, SubmitConsoleInput,
    session::{DebugSession, DebugSessionEvent, OutputCategory},
};
use crate::{editor::Editor, pinpoint::Pin, selectable::selectable, theme};

const ROW_HEIGHT: f32 = 18.;
const HISTORY_LIMIT: usize = 100;

pub fn bind_keys(cx: &mut App) {
    let input = Some("DebugConsoleInput > Editor");
    cx.bind_keys([
        KeyBinding::new("enter", SubmitConsoleInput, input),
        KeyBinding::new("up", PreviousConsoleInput, input),
        KeyBinding::new("down", NextConsoleInput, input),
    ]);
}

pub struct DebugConsole {
    focus_handle: FocusHandle,
    session: Option<Entity<DebugSession>>,
    input: Entity<Editor>,
    history: Vec<String>,
    /// Position while walking the history with Up/Down.
    history_index: Option<usize>,
    scroll: UniformListScrollHandle,
    /// Keep the newest output in view (until the user scrolls up).
    follow: bool,
    _session_subscription: Option<Subscription>,
}

fn color(category: OutputCategory) -> Hsla {
    match category {
        OutputCategory::Stdout | OutputCategory::Result => theme::text(),
        OutputCategory::Stderr => theme::error(),
        OutputCategory::Important => theme::warning(),
        OutputCategory::Console => theme::muted(),
        OutputCategory::Input => theme::accent_text(),
    }
}

impl DebugConsole {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            session: None,
            input: cx.new(Editor::single_line),
            history: Vec::new(),
            history_index: None,
            scroll: UniformListScrollHandle::new(),
            follow: true,
            _session_subscription: None,
        }
    }

    pub fn set_session(&mut self, session: Option<Entity<DebugSession>>, cx: &mut Context<Self>) {
        self._session_subscription = session.as_ref().map(|session| {
            cx.subscribe(session, |this, session, event, cx| {
                // `Changed` follows the output (once per batch of adapter
                // messages); `Output` itself is for headless listeners.
                if let DebugSessionEvent::Changed = event {
                    if this.follow {
                        let count = session.read(cx).output().len();
                        if count > 0 {
                            this.scroll
                                .scroll_to_item(count - 1, ScrollStrategy::Bottom);
                        }
                    }
                    cx.notify();
                }
            })
        });
        self.session = session;
        self.follow = true;
        cx.notify();
    }

    pub fn focus_input(&self, window: &mut Window, cx: &App) {
        window.focus(&self.input.focus_handle(cx));
    }

    fn submit(&mut self, _: &SubmitConsoleInput, _: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text();
        if text.trim().is_empty() {
            return;
        }
        if self.history.last() != Some(&text) {
            self.history.push(text.clone());
            if self.history.len() > HISTORY_LIMIT {
                self.history.remove(0);
            }
        }
        self.history_index = None;
        self.input.update(cx, |input, cx| input.set_text("", cx));
        self.follow = true;
        if let Some(session) = &self.session {
            session.update(cx, |session, cx| session.repl(text, cx));
        }
    }

    fn previous(&mut self, _: &PreviousConsoleInput, _: &mut Window, cx: &mut Context<Self>) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.history_index {
            Some(index) => index.saturating_sub(1),
            None => self.history.len() - 1,
        };
        self.history_index = Some(index);
        let text = self.history[index].clone();
        self.input.update(cx, |input, cx| input.set_text(&text, cx));
    }

    fn next(&mut self, _: &NextConsoleInput, _: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.history_index else {
            return;
        };
        let text = if index + 1 < self.history.len() {
            self.history_index = Some(index + 1);
            self.history[index + 1].clone()
        } else {
            self.history_index = None;
            String::new()
        };
        self.input.update(cx, |input, cx| input.set_text(&text, cx));
    }

    fn clear(&mut self, _: &ClearConsole, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.update(cx, |session, cx| session.clear_output(cx));
        }
    }
}

impl Focusable for DebugConsole {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DebugConsole {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self
            .session
            .as_ref()
            .map_or(0, |session| session.read(cx).output().len());
        let output = if count == 0 {
            div()
                .flex_1()
                .p_2()
                .text_sm()
                .text_color(theme::muted())
                .child(selectable(
                    "empty",
                    "Program output and debugger messages show here; type an expression below while paused.",
                ))
                .into_any_element()
        } else {
            uniform_list(
                "debug-console",
                count,
                cx.processor(|this, range: Range<usize>, _, cx| {
                    let Some(session) = &this.session else {
                        return Vec::new();
                    };
                    let lines = session.read(cx).output();
                    // One element per line: the list is virtualized (up to
                    // the session's output cap), so a selection is per line.
                    range
                        .filter_map(|index| Some((index, lines.get(index)?)))
                        .map(|(index, line)| {
                            div()
                                .h(px(ROW_HEIGHT))
                                .px_2()
                                .whitespace_nowrap()
                                .font_family(theme::MONO_FONT)
                                .text_sm()
                                .text_color(color(line.category))
                                .child(selectable(("line", index), line.text.clone()))
                                .pin(format!("line:{}", index + 1))
                                .into_any_element()
                        })
                        .collect()
                }),
            )
            .track_scroll(self.scroll.clone())
            .on_scroll_wheel(cx.listener(|this, _, _, cx| {
                // Scrolling up stops following; reaching the end resumes.
                let handle = this.scroll.0.borrow();
                let offset = handle.base_handle.offset();
                let max = handle.base_handle.max_offset();
                drop(handle);
                this.follow = -offset.y >= max.height - px(2.);
                cx.notify();
            }))
            .flex_1()
            .min_h_0()
            .pin("output")
            .into_any_element()
        };
        div()
            .track_focus(&self.focus_handle)
            .key_context("DebugConsole")
            .on_action(cx.listener(Self::clear))
            .size_full()
            .flex()
            .flex_col()
            .child(output)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(theme::border())
                    .child(div().text_sm().text_color(theme::accent_text()).child("›"))
                    .child(
                        div()
                            .key_context("DebugConsoleInput")
                            .on_action(cx.listener(Self::submit))
                            .on_action(cx.listener(Self::previous))
                            .on_action(cx.listener(Self::next))
                            .flex_1()
                            .h(px(20.))
                            .font_family(theme::MONO_FONT)
                            .text_sm()
                            .child(self.input.clone())
                            .pin("input")
                            .label("Evaluate Expression"),
                    )
                    .child(
                        div()
                            .id("clear-console")
                            .px_1()
                            .text_xs()
                            .text_color(theme::muted())
                            .cursor_pointer()
                            .hover(|style| style.text_color(theme::text()))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.clear(&ClearConsole, window, cx)
                            }))
                            .child("Clear")
                            .pin("clear")
                            .label("Clear Console"),
                    ),
            )
    }
}
