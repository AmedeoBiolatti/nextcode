//! The form a workflow with `inputs` shows when it runs ([`InputForm`],
//! over the graph of its tab): a row per input (a textbox for strings and
//! numbers, a checkbox for booleans, a dropdown for choices) with its
//! description and what is wrong with its value. It starts with the
//! values the workflow last ran with where they still fit, else the
//! defaults (kept per workflow id in `workflow_inputs.json` in the local
//! data dir). Run (Enter) once every value is valid, Cancel (Escape), Tab
//! and Shift+Tab between fields, Space toggles a checkbox; a dropdown
//! opens with a click, Space, Up or Down, which then move through its
//! options, Enter picks one and Escape closes it.

use std::{io, path::PathBuf, sync::Arc};

use gpui::{
    App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla, KeyBinding,
    MouseButton, MouseDownEvent, Pixels, Point, SharedString, Subscription, Window, actions,
    anchored, deferred, div, prelude::*, px,
};
use serde_json::{Map, Value};
use workflow::{Input, InputKind, InputValue, InputValues, Workflow, inputs};

use crate::{
    commit_panel::{Check, checkbox},
    editor::{Editor, EditorEvent},
    git_dialogs::button,
    pinpoint::Pin,
    selectable::selectable,
    theme,
};

actions!(
    workflow_inputs,
    [
        Submit,
        Cancel,
        NextField,
        PreviousField,
        OptionUp,
        OptionDown,
        Toggle
    ]
);

/// The form.
const CONTEXT: &str = "WorkflowInputs";
/// A checkbox or dropdown of it (textboxes are editors).
const FIELD_CONTEXT: &str = "WorkflowInputField";
const FIELD_HEIGHT: f32 = 26.;

pub(super) fn bind_keys(cx: &mut App) {
    let editor = Some("WorkflowInputs > Editor");
    let field = Some(FIELD_CONTEXT);
    // After the editor's own keys, which these replace in its textboxes.
    cx.bind_keys([
        KeyBinding::new("enter", Submit, editor),
        KeyBinding::new("tab", NextField, editor),
        KeyBinding::new("shift-tab", PreviousField, editor),
        KeyBinding::new("enter", Submit, field),
        KeyBinding::new("tab", NextField, field),
        KeyBinding::new("shift-tab", PreviousField, field),
        KeyBinding::new("up", OptionUp, field),
        KeyBinding::new("down", OptionDown, field),
        KeyBinding::new("space", Toggle, field),
        KeyBinding::new("escape", Cancel, Some(CONTEXT)),
    ]);
}

/// What a field holds while it is edited.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Raw {
    /// A textbox's text.
    Text(String),
    Bool(bool),
    /// The chosen option.
    Choice(usize),
}

/// The form's values, without its views.
#[derive(Debug, Clone)]
pub(super) struct FormState {
    workflow: Arc<Workflow>,
    raw: Vec<Raw>,
}

/// The fields showing `values`.
fn raw_of(inputs: &[Input], values: &InputValues) -> Vec<Raw> {
    inputs
        .iter()
        .map(|input| {
            let value = values.get(&input.id).unwrap_or(&input.default);
            match (&input.kind, value) {
                (InputKind::Boolean, InputValue::Bool(flag)) => Raw::Bool(*flag),
                (InputKind::Choice(options), InputValue::Text(text)) => Raw::Choice(
                    options
                        .iter()
                        .position(|option| option.value == *text)
                        .unwrap_or(0),
                ),
                (_, value) => Raw::Text(value.text()),
            }
        })
        .collect()
}

impl FormState {
    /// Prefilled with what `remembered` holds (the last run's values)
    /// where it still fits, else the defaults.
    pub(super) fn new(workflow: Arc<Workflow>, remembered: Option<&Map<String, Value>>) -> Self {
        let values = inputs::prefill(&workflow.inputs, remembered);
        let raw = raw_of(&workflow.inputs, &values);
        Self { workflow, raw }
    }

    pub(super) fn workflow(&self) -> &Arc<Workflow> {
        &self.workflow
    }

    pub(super) fn inputs(&self) -> &[Input] {
        &self.workflow.inputs
    }

    pub(super) fn raw(&self, index: usize) -> &Raw {
        &self.raw[index]
    }

    /// Every field back to its input's default.
    pub(super) fn reset(&mut self) {
        self.raw = raw_of(
            &self.workflow.inputs,
            &inputs::defaults(&self.workflow.inputs),
        );
    }

    pub(super) fn set(&mut self, index: usize, raw: Raw) {
        self.raw[index] = raw;
    }

    /// Field `index`'s value, or what is wrong with it.
    pub(super) fn value(&self, index: usize) -> Result<InputValue, String> {
        let input = &self.workflow.inputs[index];
        let value = match (&input.kind, &self.raw[index]) {
            (InputKind::Choice(options), Raw::Choice(at)) => {
                InputValue::Text(options[*at].value.clone())
            }
            (_, Raw::Bool(flag)) => InputValue::Bool(*flag),
            (_, Raw::Text(text)) => input.parse_text(text)?,
            (_, Raw::Choice(_)) => return Err(format!("must be a {}", input.type_name())),
        };
        input.check(&value)?;
        Ok(value)
    }

    /// Every value, once all are valid (Run is enabled).
    pub(super) fn values(&self) -> Option<InputValues> {
        (0..self.raw.len())
            .map(|index| {
                self.value(index)
                    .ok()
                    .map(|value| (self.workflow.inputs[index].id.clone(), value))
            })
            .collect::<Option<Vec<_>>>()
            .map(InputValues::from_iter)
    }
}

fn store_path() -> Option<PathBuf> {
    Some(crate::layout::state_root()?.join("workflow_inputs.json"))
}

/// The values workflow `workflow_id` last ran with from the form.
pub(super) fn remembered(workflow_id: &str) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(store_path()?).ok()?;
    let Value::Object(mut all) = serde_json::from_str(&text).ok()? else {
        return None;
    };
    match all.remove(workflow_id)? {
        Value::Object(values) => Some(values),
        _ => None,
    }
}

/// Keeps `values` as what workflow `workflow_id` last ran with.
pub(super) fn remember(workflow_id: &str, values: &InputValues) -> io::Result<()> {
    let path = store_path().ok_or_else(|| io::Error::other("no data directory"))?;
    let mut all = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<Map<String, Value>>(&text).ok())
        .unwrap_or_default();
    all.insert(workflow_id.to_owned(), values.to_json());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(&all).map_err(io::Error::other)?;
    worktree::write_atomic(&path, json.as_bytes())
}

pub(super) enum InputFormEvent {
    /// Run this workflow with these values.
    Run(Arc<Workflow>, InputValues),
    Cancelled,
}

impl EventEmitter<InputFormEvent> for InputForm {}

pub(super) struct InputForm {
    state: FormState,
    /// Per input: its textbox (strings and numbers).
    editors: Vec<Option<Entity<Editor>>>,
    /// Per input: what takes the focus (the textbox's own handle).
    handles: Vec<FocusHandle>,
    /// The open dropdown: its input and the highlighted option.
    open: Option<(usize, usize)>,
    /// Where the click that closed a dropdown from outside it was: that
    /// click on the dropdown itself must not open it again.
    dismissed_at: Option<Point<Pixels>>,
    /// Focus the first field on the next frame.
    focus_first: bool,
    _subscriptions: Vec<Subscription>,
}

/// `required` → `Required`.
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

impl InputForm {
    pub(super) fn new(
        workflow: Arc<Workflow>,
        remembered: Option<&Map<String, Value>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let state = FormState::new(workflow, remembered);
        let mut editors = Vec::with_capacity(state.raw.len());
        let mut handles = Vec::with_capacity(state.raw.len());
        let mut subscriptions = Vec::new();
        for index in 0..state.raw.len() {
            match state.raw(index) {
                Raw::Text(text) => {
                    let editor = cx.new(|cx| {
                        let mut editor = Editor::single_line(cx);
                        editor.set_text(text, cx);
                        editor
                    });
                    subscriptions.push(cx.subscribe(&editor, move |this, editor, event, cx| {
                        if let EditorEvent::Edited = event {
                            let text = editor.read(cx).text();
                            this.state.set(index, Raw::Text(text));
                            cx.notify();
                        }
                    }));
                    handles.push(editor.focus_handle(cx));
                    editors.push(Some(editor));
                }
                Raw::Bool(_) | Raw::Choice(_) => {
                    handles.push(cx.focus_handle());
                    editors.push(None);
                }
            }
        }
        Self {
            state,
            editors,
            handles,
            open: None,
            dismissed_at: None,
            focus_first: true,
            _subscriptions: subscriptions,
        }
    }

    #[cfg(test)]
    pub(super) fn state(&self) -> &FormState {
        &self.state
    }

    /// Types `text` into input `index`'s textbox, replacing its text.
    #[cfg(test)]
    pub(super) fn type_text(&self, index: usize, text: &str, cx: &mut Context<Self>) {
        if let Some(editor) = &self.editors[index] {
            editor.update(cx, |editor, cx| editor.set_text(text, cx));
        }
    }

    /// Run: the values, once all are valid.
    pub(super) fn submit(&mut self, cx: &mut Context<Self>) {
        if let Some(values) = self.state.values() {
            cx.emit(InputFormEvent::Run(self.state.workflow().clone(), values));
        }
    }

    /// Every field back to its default.
    fn reset(&mut self, cx: &mut Context<Self>) {
        self.state.reset();
        self.open = None;
        for (index, editor) in self.editors.iter().enumerate() {
            if let (Some(editor), Raw::Text(text)) = (editor, self.state.raw(index)) {
                let text = text.clone();
                editor.update(cx, |editor, cx| editor.set_text(&text, cx));
            }
        }
        cx.notify();
    }

    pub(super) fn choose(&mut self, index: usize, option: usize, cx: &mut Context<Self>) {
        self.state.set(index, Raw::Choice(option));
        self.open = None;
        cx.notify();
    }

    fn toggle_open(&mut self, index: usize, cx: &mut Context<Self>) {
        self.open = match (self.open, self.state.raw(index)) {
            (Some((open, _)), _) if open == index => None,
            (_, Raw::Choice(current)) => Some((index, *current)),
            _ => None,
        };
        cx.notify();
    }

    fn toggle_bool(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Raw::Bool(flag) = *self.state.raw(index) {
            self.state.set(index, Raw::Bool(!flag));
            cx.notify();
        }
    }

    fn submit_action(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        match self.open {
            Some((index, option)) => self.choose(index, option, cx),
            None => self.submit(cx),
        }
    }

    fn cancel_action(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.open.take().is_some() {
            return cx.notify();
        }
        cx.emit(InputFormEvent::Cancelled);
    }

    fn move_focus(&mut self, by: isize, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.handles.len() as isize;
        if count == 0 {
            return;
        }
        let next = match self.focused_in(window, cx) {
            Some(at) => (at as isize + by).rem_euclid(count),
            None => 0,
        } as usize;
        self.open = None;
        window.focus(&self.handles[next]);
        cx.notify();
    }

    fn focused_in(&self, window: &Window, cx: &App) -> Option<usize> {
        self.handles
            .iter()
            .position(|handle| handle.contains_focused(window, cx))
    }

    fn next_field(&mut self, _: &NextField, window: &mut Window, cx: &mut Context<Self>) {
        self.move_focus(1, window, cx);
    }

    fn previous_field(&mut self, _: &PreviousField, window: &mut Window, cx: &mut Context<Self>) {
        self.move_focus(-1, window, cx);
    }

    /// Up/Down on a dropdown: opens it, then moves through its options.
    fn step_option(&mut self, by: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.focused_in(window, cx) else {
            return;
        };
        let InputKind::Choice(options) = &self.state.inputs()[index].kind else {
            return;
        };
        let count = options.len() as isize;
        self.open = match self.open {
            Some((open, highlighted)) if open == index => Some((
                index,
                (highlighted as isize + by).clamp(0, count - 1) as usize,
            )),
            _ => match self.state.raw(index) {
                Raw::Choice(current) => Some((index, *current)),
                _ => None,
            },
        };
        cx.notify();
    }

    fn option_up(&mut self, _: &OptionUp, window: &mut Window, cx: &mut Context<Self>) {
        self.step_option(-1, window, cx);
    }

    fn option_down(&mut self, _: &OptionDown, window: &mut Window, cx: &mut Context<Self>) {
        self.step_option(1, window, cx);
    }

    fn toggle_action(&mut self, _: &Toggle, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.focused_in(window, cx) else {
            return;
        };
        match self.state.raw(index) {
            Raw::Bool(_) => self.toggle_bool(index, cx),
            Raw::Choice(_) => self.toggle_open(index, cx),
            Raw::Text(_) => {}
        }
    }

    /// The frame of a field: `focused` in the accent colour.
    fn field_frame(focused: bool, invalid: bool) -> gpui::Div {
        div()
            .h(px(FIELD_HEIGHT))
            .w_full()
            .px_2()
            .flex()
            .items_center()
            .rounded_sm()
            .border_1()
            .border_color(if focused {
                theme::accent()
            } else if invalid {
                theme::error()
            } else {
                theme::border()
            })
            .bg(theme::background())
    }

    fn render_textbox(
        &self,
        index: usize,
        editor: &Entity<Editor>,
        focused: bool,
        invalid: bool,
        cx: &App,
    ) -> gpui::AnyElement {
        let input = &self.state.inputs()[index];
        let empty = editor.read(cx).text().is_empty();
        Self::field_frame(focused, invalid)
            .relative()
            .child(div().flex_1().min_w_0().h(px(20.)).child(editor.clone()))
            .when_some(
                input.placeholder.clone().filter(|_| empty),
                |field, hint| {
                    field.child(
                        div()
                            .absolute()
                            .inset_0()
                            .px(px(9.))
                            .flex()
                            .items_center()
                            .text_sm()
                            .text_color(theme::muted())
                            .child(hint),
                    )
                },
            )
            .into_any_element()
    }

    fn render_checkbox(
        &self,
        index: usize,
        checked: bool,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let input = &self.state.inputs()[index];
        div()
            .id(("input-check", index))
            .key_context(FIELD_CONTEXT)
            .track_focus(&self.handles[index])
            .h(px(FIELD_HEIGHT))
            .px_1()
            .flex()
            .items_center()
            .gap_1p5()
            .rounded_sm()
            .border_1()
            .border_color(if focused {
                theme::accent()
            } else {
                gpui::transparent_black()
            })
            .cursor_pointer()
            .child(checkbox(if checked {
                Check::Checked
            } else {
                Check::Unchecked
            }))
            .child(selectable(
                ("input-check-label", index),
                input.label().to_owned(),
            ))
            // Space comes as the Toggle action (gpui's own keyboard click
            // would toggle it back, and Enter there is Run).
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.is_keyboard() {
                    return;
                }
                window.focus(&this.handles[index]);
                this.toggle_bool(index, cx);
            }))
            .into_any_element()
    }

    fn render_dropdown(
        &self,
        index: usize,
        chosen: usize,
        focused: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let InputKind::Choice(options) = &self.state.inputs()[index].kind else {
            return div().into_any_element();
        };
        let highlighted = self
            .open
            .and_then(|(open, highlighted)| (open == index).then_some(highlighted));
        let list = highlighted.map(|highlighted| {
            let items = options.iter().enumerate().map(|(at, option)| {
                div()
                    .id(("input-option", at))
                    .px_2()
                    .py_px()
                    .rounded_sm()
                    .flex()
                    .gap_2()
                    .cursor_pointer()
                    .when(at == highlighted, |item| item.bg(theme::active_row()))
                    .hover(|style| style.bg(theme::hover_row()))
                    .child(div().w(px(12.)).flex_none().child(if at == chosen {
                        "✓"
                    } else {
                        ""
                    }))
                    .child(option.label().to_owned())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.choose(index, at, cx);
                        window.focus(&this.handles[index]);
                    }))
                    .pin(format!("option:{}", option.label()))
            });
            div().absolute().top(px(FIELD_HEIGHT)).left_0().child(
                deferred(
                    anchored().snap_to_window().child(
                        div()
                            .id("input-options")
                            .occlude()
                            .min_w(px(220.))
                            .max_h(px(240.))
                            .overflow_y_scroll()
                            .p_1()
                            .flex()
                            .flex_col()
                            .text_sm()
                            .text_color(theme::text())
                            .bg(theme::panel())
                            .border_1()
                            .border_color(theme::border())
                            .rounded_md()
                            .shadow_lg()
                            .on_mouse_down_out(cx.listener(
                                |this, event: &MouseDownEvent, _, cx| {
                                    this.open = None;
                                    this.dismissed_at = Some(event.position);
                                    cx.notify();
                                },
                            ))
                            .children(items)
                            .pin("options"),
                    ),
                )
                .with_priority(1),
            )
        });
        Self::field_frame(focused || highlighted.is_some(), false)
            .id(("input-choice", index))
            .key_context(FIELD_CONTEXT)
            .track_focus(&self.handles[index])
            .relative()
            .justify_between()
            .cursor_pointer()
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .child(options[chosen].label().to_owned()),
            )
            .child(div().text_xs().text_color(theme::muted()).child("▾"))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.focus(&this.handles[index]);
                    if this.dismissed_at.take() == Some(event.position) {
                        return;
                    }
                    this.toggle_open(index, cx);
                }),
            )
            .children(list)
            .into_any_element()
    }

    fn render_row(
        &self,
        index: usize,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let input = &self.state.inputs()[index];
        let focused = self.handles[index].contains_focused(window, cx);
        let error = self.state.value(index).err();
        let control = match (self.state.raw(index), &self.editors[index]) {
            (Raw::Text(_), Some(editor)) => {
                self.render_textbox(index, editor, focused, error.is_some(), cx)
            }
            (Raw::Bool(checked), _) => self.render_checkbox(index, *checked, focused, cx),
            (Raw::Choice(chosen), _) => self.render_dropdown(index, *chosen, focused, cx),
            (Raw::Text(_), None) => div().into_any_element(),
        };
        let is_checkbox = matches!(input.kind, InputKind::Boolean);
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .when(!is_checkbox, |row| {
                row.child(
                    div()
                        .flex()
                        .gap_1()
                        .text_sm()
                        .child(selectable(("input-label", index), input.label().to_owned()))
                        .when(input.required, |label| {
                            label.child(div().text_color(theme::muted()).child("*"))
                        }),
                )
            })
            .child(control)
            .when_some(input.description.clone(), |row, description| {
                row.child(
                    div()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(selectable(("input-description", index), description)),
                )
            })
            .when_some(error, |row, error| {
                row.child(
                    div()
                        .text_xs()
                        .text_color(theme::error())
                        .child(selectable(("input-error", index), sentence(&error))),
                )
            })
            .pin(format!("field:{}", input.id))
            .label(input.label().to_owned())
            .into_any_element()
    }
}

impl Focusable for InputForm {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.handles
            .first()
            .cloned()
            .unwrap_or_else(|| cx.focus_handle())
    }
}

impl Render for InputForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_first)
            && let Some(first) = self.handles.first()
        {
            window.focus(first);
        }
        let valid = self.state.values().is_some();
        let rows: Vec<gpui::AnyElement> = (0..self.handles.len())
            .map(|index| self.render_row(index, window, cx))
            .collect();
        let backdrop = Hsla {
            a: 0.55,
            ..theme::background()
        };
        div()
            .id("workflow-inputs")
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::submit_action))
            .on_action(cx.listener(Self::cancel_action))
            .on_action(cx.listener(Self::next_field))
            .on_action(cx.listener(Self::previous_field))
            .on_action(cx.listener(Self::option_up))
            .on_action(cx.listener(Self::option_down))
            .on_action(cx.listener(Self::toggle_action))
            .absolute()
            .inset_0()
            .occlude()
            .bg(backdrop)
            .flex()
            .justify_center()
            .items_start()
            .p_4()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id("workflow-inputs-form")
                    .w(px(420.))
                    .max_w_full()
                    .max_h_full()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_3()
                    .bg(theme::panel())
                    .border_1()
                    .border_color(theme::border())
                    .rounded_md()
                    .shadow_lg()
                    .text_sm()
                    .text_color(theme::text())
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(selectable(
                                        "inputs-title",
                                        SharedString::from(format!(
                                            "Run {}",
                                            self.state.workflow().name
                                        )),
                                    )),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(selectable("inputs-hint", "Inputs for this run")),
                            ),
                    )
                    .children(rows)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .id("inputs-reset")
                                    .text_xs()
                                    .text_color(theme::accent_text())
                                    .cursor_pointer()
                                    .hover(|style| style.underline())
                                    .child("Reset to defaults")
                                    .on_click(cx.listener(|this, _, _, cx| this.reset(cx)))
                                    .pin("reset")
                                    .label("Reset to defaults"),
                            )
                            .child(div().flex_1())
                            .child(
                                button("inputs-cancel", "Cancel".into(), false)
                                    .on_click(
                                        cx.listener(|_, _, _, cx| {
                                            cx.emit(InputFormEvent::Cancelled)
                                        }),
                                    )
                                    .pin("cancel")
                                    .label("Cancel"),
                            )
                            .child(
                                button("inputs-run", "Run".into(), valid)
                                    .when(!valid, |run| {
                                        run.opacity(0.5)
                                            .cursor_default()
                                            .border_1()
                                            .border_color(theme::border())
                                    })
                                    .when(valid, |run| {
                                        run.on_click(cx.listener(|this, _, _, cx| this.submit(cx)))
                                    })
                                    .pin("run")
                                    .label("Run"),
                            ),
                    )
                    .pin("inputs-form"),
            )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn workflow() -> Arc<Workflow> {
        Arc::new(
            workflow::parse(
                "inputs:
  env: {type: choice, options: [dev, staging, prod], default: dev}
  branch: {required: true, placeholder: branch name}
  workers: {type: number, default: 4, min: 1, max: 64, integer: true}
  clean: {type: boolean}
steps:
  a: {run: 'echo ${inputs.env}'}
",
                "f",
            )
            .unwrap(),
        )
    }

    #[test]
    fn the_form_starts_with_remembered_values_that_fit_else_defaults() {
        let remembered = json!({ "env": "prod", "branch": "feature", "workers": 8, "clean": true });
        let state = FormState::new(workflow(), remembered.as_object());
        assert_eq!(
            [state.raw(0), state.raw(1), state.raw(2), state.raw(3)],
            [
                &Raw::Choice(2),
                &Raw::Text("feature".into()),
                &Raw::Text("8".into()),
                &Raw::Bool(true)
            ]
        );
        assert_eq!(
            state.values().unwrap().summary(),
            "env=prod · branch=feature · workers=8 · clean=true"
        );
        // An option removed, a bound lowered, a type changed: those fall
        // back to their defaults, the rest stays.
        let stale = json!({ "env": "qa", "branch": "feature", "workers": 100, "clean": "maybe" });
        let state = FormState::new(workflow(), stale.as_object());
        assert_eq!(
            [state.raw(0), state.raw(1), state.raw(2), state.raw(3)],
            [
                &Raw::Choice(0),
                &Raw::Text("feature".into()),
                &Raw::Text("4".into()),
                &Raw::Bool(false)
            ]
        );
        // Nothing remembered: the defaults; the required branch is empty,
        // so Run waits for it.
        let mut state = FormState::new(workflow(), None);
        assert_eq!(state.raw(1), &Raw::Text(String::new()));
        assert_eq!(state.value(1), Err("required".into()));
        assert!(state.values().is_none(), "Run is disabled");
        state.set(1, Raw::Text("main".into()));
        state.set(2, Raw::Text("65".into()));
        assert_eq!(state.value(2), Err("must be ≤ 64".into()));
        assert!(state.values().is_none());
        state.set(2, Raw::Text("x".into()));
        assert_eq!(state.value(2), Err("`x` is not a number".into()));
        state.set(2, Raw::Text("16".into()));
        state.set(0, Raw::Choice(1));
        assert_eq!(
            state.values().unwrap().summary(),
            "env=staging · branch=main · workers=16 · clean=false"
        );
        state.reset();
        assert_eq!(state.raw(0), &Raw::Choice(0));
        assert_eq!(state.raw(2), &Raw::Text("4".into()));
        assert!(state.values().is_none(), "the branch is empty again");
        assert_eq!(sentence("required"), "Required");
    }

    #[test]
    fn values_are_remembered_per_workflow() {
        let parsed = workflow();
        let values = inputs::resolve(
            &parsed.inputs,
            json!({ "env": "prod", "branch": "x" }).as_object().unwrap(),
        )
        .unwrap();
        remember("first", &values).unwrap();
        remember("second", &inputs::defaults(&parsed.inputs)).unwrap();
        let first = remembered("first").unwrap();
        assert_eq!(
            Value::Object(first),
            json!({ "env": "prod", "branch": "x", "workers": 4, "clean": false })
        );
        assert_eq!(remembered("second").unwrap()["branch"], "");
        assert!(remembered("third").is_none());
        if let Some(state) = crate::layout::state_root() {
            std::fs::remove_dir_all(state).ok();
        }
    }
}
