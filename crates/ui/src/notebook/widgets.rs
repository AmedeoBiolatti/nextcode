//! ipywidgets views: the models of `notebook::widgets` (kept by the
//! document, synchronised with the kernel over comms) drawn as native
//! controls. Sliders (int, float, log, ranges), number and text fields,
//! checkboxes, toggle buttons, dropdowns, radio buttons, selects, buttons,
//! labels, HTML (through the HTML renderer), images, progress bars, boxes,
//! accordions, tabs and Output widgets (their outputs through the output
//! renderer). `interact` is a box of these.
//!
//! Using a control sends an [`OutputAction::Widget`] event; the view
//! applies it to the document ([`apply_event`]), which updates the model
//! at once and sends the comm message. Models without a comm (the
//! notebook's saved state, or after the kernel went) render disabled.
//!
//! The controls are plain functions over values and callbacks
//! ([`slider`], [`checkbox`], [`dropdown`], [`button`], [`text_field`],
//! [`radio`]), so other front ends (marimo's `mo.ui`) reuse them.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App, Bounds, CursorStyle, DefiniteLength, DispatchPhase, ElementId, Entity,
    Focusable, FontWeight, Hitbox, HitboxBehavior, Hsla, ImageFormat, KeyBinding, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, SharedString, Subscription, WeakEntity,
    Window, actions, anchored, canvas, deferred, div, prelude::*, px, relative,
};
use notebook::widgets::{WidgetModel, WidgetStore, view_model_id};
use serde_json::{Value, json};

use super::{
    document::NotebookDocument,
    html::{cached_image, parsed, render_html, text_on},
    output::{OnAction, OutputAction, OutputCache, OutputOptions, render_output_inner},
};
use crate::{
    editor::{Editor, EditorEvent},
    selectable::selectable,
    theme,
};

actions!(notebook_widgets, [SubmitWidgetInput]);

const INPUT_CONTEXT: &str = "NotebookWidgetInput";

/// Bound after the editor's bindings: Enter in a widget's text field
/// submits it.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new(
        "enter",
        SubmitWidgetInput,
        Some("NotebookWidgetInput > Editor"),
    )]);
}

/// What using a control does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WidgetEvent {
    /// Sets `key` of `model` to `value`; the kernel hears of it when
    /// `send`.
    Update {
        model: String,
        key: String,
        value: Value,
        send: bool,
    },
    /// A `custom` message (a Button's click).
    Custom { model: String, content: Value },
    /// Sends `key`'s current value (the end of a slider drag).
    Commit { model: String, key: String },
}

/// Applies a widget event to the document.
pub fn apply_event(document: &WeakEntity<NotebookDocument>, event: WidgetEvent, cx: &mut App) {
    document
        .update(cx, |document, cx| match event {
            WidgetEvent::Update {
                model,
                key,
                value,
                send,
            } => document.widget_update(&model, &key, value, send, cx),
            WidgetEvent::Custom { model, content } => document.widget_custom(&model, content),
            WidgetEvent::Commit { model, key } => {
                if let Some(value) = document
                    .widgets()
                    .get(&model)
                    .and_then(|model| model.get(&key))
                    .cloned()
                {
                    document.widget_update(&model, &key, value, true, cx);
                }
            }
        })
        .ok();
}

/// Where widget events go outside of rendering (text field editors).
pub type WidgetSink = Rc<dyn Fn(WidgetEvent, &mut App)>;

/// A control's callback with the new value.
pub type Handler<T> = Rc<dyn Fn(T, &mut Window, &mut App)>;
/// A control's callback without a value (a click, a drag's end).
pub type Callback = Rc<dyn Fn(&mut Window, &mut App)>;
/// A slider's change: the new model value and whether to send it now.
pub type SliderChange = Rc<dyn Fn(Value, bool, &mut Window, &mut App)>;

/// What rendering widgets needs: the models and the text fields' editors.
#[derive(Clone, Copy)]
pub struct WidgetRender<'a> {
    pub store: &'a WidgetStore,
    pub inputs: &'a WidgetInputs,
}

/// How a text field's text becomes the model's value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputKind {
    Text,
    Int { min: Option<f64>, max: Option<f64> },
    Float { min: Option<f64>, max: Option<f64> },
}

impl InputKind {
    /// The value for `text`; `None` when it is not a number of the kind.
    pub fn parse(self, text: &str) -> Option<Value> {
        let clamp = |value: f64, min: Option<f64>, max: Option<f64>| {
            let value = min.map_or(value, |min| value.max(min));
            max.map_or(value, |max| value.min(max))
        };
        match self {
            InputKind::Text => Some(Value::String(text.to_owned())),
            InputKind::Int { min, max } => {
                let value: f64 = text.trim().parse::<i64>().ok()? as f64;
                Some(json!(clamp(value, min, max) as i64))
            }
            InputKind::Float { min, max } => {
                let value: f64 = text.trim().parse().ok()?;
                value.is_finite().then(|| json!(clamp(value, min, max)))
            }
        }
    }

    fn text(self, value: Option<&Value>) -> String {
        match value {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Number(number)) => match self {
                InputKind::Int { .. } => number
                    .as_f64()
                    .map_or_else(|| number.to_string(), |n| format!("{}", n.round() as i64)),
                _ => number.to_string(),
            },
            _ => String::new(),
        }
    }
}

/// A text field a model needs.
#[derive(Debug, Clone, PartialEq)]
pub struct InputSpec {
    pub model: String,
    pub text: String,
    pub multiline: bool,
    pub continuous: bool,
    pub kind: InputKind,
}

/// The kind of field a model shows, if it is a text-like widget.
fn input_kind(model: &WidgetModel) -> Option<(InputKind, bool)> {
    let bounds = (model.f64("min"), model.f64("max"));
    Some(match model.name.as_str() {
        "TextModel" | "ComboboxModel" => (InputKind::Text, false),
        "TextareaModel" => (InputKind::Text, true),
        "IntTextModel" => (
            InputKind::Int {
                min: None,
                max: None,
            },
            false,
        ),
        "BoundedIntTextModel" => (
            InputKind::Int {
                min: bounds.0,
                max: bounds.1,
            },
            false,
        ),
        "FloatTextModel" => (
            InputKind::Float {
                min: None,
                max: None,
            },
            false,
        ),
        "BoundedFloatTextModel" => (
            InputKind::Float {
                min: bounds.0,
                max: bounds.1,
            },
            false,
        ),
        _ => return None,
    })
}

/// The text fields of the widgets reachable from `roots`.
pub fn input_specs(store: &WidgetStore, roots: &[&str]) -> Vec<InputSpec> {
    let mut specs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut stack: Vec<String> = roots.iter().map(|root| (*root).to_owned()).collect();
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) || seen.len() > 10_000 {
            continue;
        }
        let Some(model) = store.get(&id) else {
            continue;
        };
        if let Some((kind, multiline)) = input_kind(model) {
            specs.push(InputSpec {
                text: kind.text(model.get("value")),
                model: id.clone(),
                multiline,
                continuous: model
                    .get("continuous_update")
                    .and_then(Value::as_bool)
                    .unwrap_or(matches!(kind, InputKind::Text)),
                kind,
            });
        }
        stack.extend(model.references("children").into_iter().map(str::to_owned));
        for output in &model.outputs {
            if let Some(id) = view_model_id(output) {
                stack.push(id.to_owned());
            }
        }
    }
    specs
}

struct Input {
    editor: Entity<Editor>,
    /// Set while the model's value is written into the editor (that edit
    /// is not the user's).
    syncing: Rc<Cell<bool>>,
    _subscription: Subscription,
}

/// The editors of text-like widgets by model id (one set per view).
#[derive(Default)]
pub struct WidgetInputs {
    inputs: HashMap<String, Input>,
}

impl WidgetInputs {
    pub fn editor(&self, model: &str) -> Option<&Entity<Editor>> {
        self.inputs.get(model).map(|input| &input.editor)
    }

    /// Creates the editors `specs` need and shows values the kernel
    /// changed in editors without focus.
    pub fn ensure(
        &mut self,
        specs: Vec<InputSpec>,
        sink: &WidgetSink,
        window: &Window,
        cx: &mut App,
    ) {
        for spec in specs {
            if let Some(input) = self.inputs.get(&spec.model) {
                let editor = input.editor.clone();
                let focused = editor.focus_handle(cx).is_focused(window);
                if !focused && editor.read(cx).text() != spec.text {
                    input.syncing.set(true);
                    editor.update(cx, |editor, cx| editor.set_text(&spec.text, cx));
                    input.syncing.set(false);
                }
                continue;
            }
            let multiline = spec.multiline;
            let editor = cx.new(|cx| {
                let mut editor = if multiline {
                    Editor::text_area(cx)
                } else {
                    Editor::single_line(cx)
                };
                editor.set_text(&spec.text, cx);
                editor
            });
            let syncing = Rc::new(Cell::new(false));
            let subscription = {
                let syncing = syncing.clone();
                let sink = sink.clone();
                let model = spec.model.clone();
                let (kind, continuous) = (spec.kind, spec.continuous);
                cx.subscribe(&editor, move |editor, event: &EditorEvent, cx| {
                    if !matches!(event, EditorEvent::Edited) || syncing.get() || !continuous {
                        return;
                    }
                    let text = editor.read(cx).text();
                    if let Some(value) = kind.parse(&text) {
                        sink(
                            WidgetEvent::Update {
                                model: model.clone(),
                                key: "value".into(),
                                value,
                                send: true,
                            },
                            cx,
                        );
                    }
                })
            };
            self.inputs.insert(
                spec.model,
                Input {
                    editor,
                    syncing,
                    _subscription: subscription,
                },
            );
        }
    }

    /// Drops editors of models the store no longer has.
    pub fn prune(&mut self, store: &WidgetStore) {
        self.inputs.retain(|model, _| store.get(model).is_some());
    }
}

thread_local! {
    /// The slider being dragged: (model, handle, last send).
    static DRAG: RefCell<Option<(String, usize, Instant)>> = const { RefCell::new(None) };
    /// The dropdown whose list is open (by element key).
    static OPEN: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// How often a continuous slider drag sends its value.
const DRAG_SEND_INTERVAL: Duration = Duration::from_millis(60);
/// Width of a description label.
const LABEL_WIDTH: f32 = 96.;
/// Default width of a slider's track or a progress bar.
const TRACK_WIDTH: f32 = 220.;
const ROW_HEIGHT: f32 = 26.;

/// A labelled row: `description` in a fixed column, then `control`.
fn labelled(description: &str, control: impl IntoElement) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .min_h(px(ROW_HEIGHT))
        .when(!description.is_empty(), |row| {
            row.child(
                div()
                    .flex_none()
                    .w(px(LABEL_WIDTH))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_sm()
                    .child(SharedString::from(description.to_owned())),
            )
        })
        .child(control)
}

fn emit(on_action: &OnAction, event: WidgetEvent, window: &mut Window, cx: &mut App) {
    on_action(OutputAction::Widget(event), window, cx);
}

/// A slider's numbers. Log sliders work on exponents: `min`/`max`/`step`
/// are exponents of `base`, values are powers.
#[derive(Debug, Clone, PartialEq)]
pub struct Slider {
    pub min: f64,
    pub max: f64,
    pub step: f64,
    /// One value, or two for a range.
    pub values: Vec<f64>,
    pub integer: bool,
    pub log_base: Option<f64>,
    /// Sends while dragging (else at the end).
    pub continuous: bool,
    pub disabled: bool,
    pub readout: Option<String>,
    pub width: Pixels,
}

impl Slider {
    fn position(&self, value: f64) -> f64 {
        match self.log_base {
            Some(base) if value > 0. => value.log(base),
            Some(_) => self.min,
            None => value,
        }
    }

    /// Where `value` sits along the track (0-1).
    fn fraction(&self, value: f64) -> f32 {
        let span = self.max - self.min;
        if span <= 0. {
            return 0.;
        }
        ((self.position(value) - self.min) / span).clamp(0., 1.) as f32
    }

    /// The value at `fraction` of the track, on a step.
    pub fn value_at(&self, fraction: f32) -> f64 {
        let span = self.max - self.min;
        let raw = self.min + f64::from(fraction.clamp(0., 1.)) * span;
        let stepped = if self.step > 0. {
            (self.min + ((raw - self.min) / self.step).round() * self.step)
                .clamp(self.min, self.max)
        } else {
            raw
        };
        let value = match self.log_base {
            Some(base) => base.powf(stepped),
            None => stepped,
        };
        if self.integer { value.round() } else { value }
    }

    fn json(&self, value: f64) -> Value {
        if self.integer {
            json!(value as i64)
        } else {
            // Steps of 0.1 should not show 0.30000000000000004.
            json!((value * 1e12).round() / 1e12)
        }
    }

    /// The model value with `handle` moved to `value`.
    fn with(&self, handle: usize, value: f64) -> Value {
        if self.values.len() == 2 {
            let mut values = self.values.clone();
            values[handle] = value;
            if values[0] > values[1] {
                values.swap(0, 1);
            }
            Value::Array(values.iter().map(|v| self.json(*v)).collect())
        } else {
            self.json(value)
        }
    }
}

/// A horizontal slider: `on_change(value, send)` with the new model value
/// (a number, or `[low, high]`), `send` false while a non-continuous drag
/// goes on; `on_commit` at the end of a drag.
pub fn slider(
    key: SharedString,
    spec: Slider,
    on_change: SliderChange,
    on_commit: Callback,
) -> AnyElement {
    let width = spec.width;
    let fractions: Vec<f32> = spec.values.iter().map(|v| spec.fraction(*v)).collect();
    let (low, high) = match fractions[..] {
        [low, high] => (low, high),
        [value] => (0., value),
        _ => (0., 0.),
    };
    let color = if spec.disabled {
        theme::muted()
    } else {
        theme::accent()
    };
    let handles = fractions.iter().map(|fraction| {
        div()
            .absolute()
            .top(px(3.))
            .left(width * *fraction - px(7.))
            .size(px(14.))
            .rounded_full()
            .bg(color)
            .border_2()
            .border_color(theme::background())
    });
    let readout = spec.readout.clone();
    let disabled = spec.disabled;
    let spec = Rc::new(spec);
    let selector = format!("{key}-track");
    let track = div()
        .relative()
        .flex_none()
        .w(width)
        .h(px(20.))
        .debug_selector(move || selector)
        .child(
            div()
                .absolute()
                .top(px(8.))
                .left_0()
                .w(width)
                .h(px(4.))
                .rounded_full()
                .bg(theme::border()),
        )
        .child(
            div()
                .absolute()
                .top(px(8.))
                .left(width * low)
                .w(width * (high - low))
                .h(px(4.))
                .rounded_full()
                .bg(color),
        )
        .children(handles)
        .when(!disabled, |track| {
            track.child(
                canvas(
                    |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                    move |bounds, hitbox, window, _| {
                        slider_listeners(key, spec, bounds, hitbox, on_change, on_commit, window)
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
        });
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(track)
        .when_some(readout, |row, readout| {
            row.child(
                div()
                    .flex_none()
                    .min_w(px(36.))
                    .text_sm()
                    .font_family(theme::MONO_FONT)
                    .child(readout),
            )
        })
        .into_any_element()
}

fn slider_listeners(
    key: SharedString,
    spec: Rc<Slider>,
    bounds: Bounds<Pixels>,
    hitbox: Hitbox,
    on_change: SliderChange,
    on_commit: Callback,
    window: &mut Window,
) {
    window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
    let fraction_at = move |x: Pixels| {
        if bounds.size.width <= px(0.) {
            0.
        } else {
            (x - bounds.left()) / bounds.size.width
        }
    };
    {
        let (key, spec, on_change) = (key.clone(), spec.clone(), on_change.clone());
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble
                || event.button != MouseButton::Left
                || !hitbox.is_hovered(window)
            {
                return;
            }
            let value = spec.value_at(fraction_at(event.position.x));
            // The nearest handle moves.
            let handle = if spec.values.len() == 2 {
                usize::from((value - spec.values[1]).abs() < (value - spec.values[0]).abs())
            } else {
                0
            };
            DRAG.with(|drag| *drag.borrow_mut() = Some((key.to_string(), handle, Instant::now())));
            on_change(spec.with(handle, value), spec.continuous, window, cx);
            cx.stop_propagation();
        });
    }
    {
        let (key, spec, on_change) = (key.clone(), spec.clone(), on_change.clone());
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || event.pressed_button != Some(MouseButton::Left) {
                return;
            }
            let Some((handle, send)) = DRAG.with(|drag| {
                let mut drag = drag.borrow_mut();
                let (model, handle, last) = drag.as_mut()?;
                if *model != key.as_ref() {
                    return None;
                }
                let send = spec.continuous && last.elapsed() >= DRAG_SEND_INTERVAL;
                if send {
                    *last = Instant::now();
                }
                Some((*handle, send))
            }) else {
                return;
            };
            let value = spec.value_at(fraction_at(event.position.x));
            on_change(spec.with(handle, value), send, window, cx);
        });
    }
    window.on_mouse_event(move |_: &MouseUpEvent, phase, window, cx| {
        if phase != DispatchPhase::Bubble {
            return;
        }
        let ours = DRAG.with(|drag| {
            let mut drag = drag.borrow_mut();
            let ours = drag
                .as_ref()
                .is_some_and(|(model, _, _)| *model == key.as_ref());
            if ours {
                *drag = None;
            }
            ours
        });
        if ours {
            on_commit(window, cx);
        }
    });
}

/// A checkbox with `label` after it.
pub fn checkbox(
    id: impl Into<ElementId>,
    checked: bool,
    label: &str,
    disabled: bool,
    on_toggle: Handler<bool>,
) -> AnyElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .min_h(px(ROW_HEIGHT))
        .when(!disabled, |row| row.cursor_pointer())
        .child(
            div()
                .flex_none()
                .size(px(16.))
                .rounded_sm()
                .border_1()
                .border_color(if checked {
                    theme::accent()
                } else {
                    theme::border()
                })
                .when(checked, |checkbox| {
                    checkbox
                        .bg(theme::accent())
                        .text_color(theme::on_accent())
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .child("✓")
                }),
        )
        .when(!label.is_empty(), |row| {
            row.child(div().text_sm().child(SharedString::from(label.to_owned())))
        })
        .when(!disabled, |row| {
            row.on_click(move |_, window, cx| on_toggle(!checked, window, cx))
        })
        .into_any_element()
}

/// A push button; `active` draws it pressed (toggle buttons), `color` is
/// its background (Jupyter's `button_style`).
pub fn button(
    id: impl Into<ElementId>,
    label: &str,
    active: bool,
    color: Option<Hsla>,
    disabled: bool,
    on_click: Callback,
) -> AnyElement {
    let background = color.or_else(|| active.then(theme::accent));
    let id = id.into();
    let selector = id.to_string();
    div()
        .id(id)
        .debug_selector(move || selector)
        .flex_none()
        .px_3()
        .h(px(ROW_HEIGHT))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .text_sm()
        .border_1()
        .border_color(theme::border())
        .map(|button| match background {
            Some(background) => button.bg(background).text_color(text_on(Some(background))),
            None => button.bg(theme::panel()),
        })
        .when(!disabled, |button| {
            button
                .cursor_pointer()
                .hover(|style| style.opacity(0.85))
                .on_click(move |_, window, cx| on_click(window, cx))
        })
        .child(SharedString::from(if label.is_empty() {
            " ".to_owned()
        } else {
            label.to_owned()
        }))
        .into_any_element()
}

/// A dropdown: the selected label; a click opens the list of `options`
/// under it. `key` identifies it while open.
pub fn dropdown(
    key: SharedString,
    options: Vec<SharedString>,
    selected: Option<usize>,
    disabled: bool,
    width: Pixels,
    on_select: Handler<usize>,
) -> AnyElement {
    let open = !disabled && OPEN.with(|open| open.borrow().as_deref() == Some(key.as_ref()));
    let label = selected
        .and_then(|index| options.get(index).cloned())
        .unwrap_or_default();
    let toggle_key = key.clone();
    let field = div()
        .id(ElementId::Name(format!("{key}-field").into()))
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .w(width)
        .h(px(ROW_HEIGHT))
        .px_2()
        .rounded_sm()
        .border_1()
        .border_color(if open {
            theme::accent()
        } else {
            theme::border()
        })
        .bg(theme::background())
        .text_sm()
        .child(div().overflow_hidden().whitespace_nowrap().child(label))
        .child(div().text_color(theme::muted()).child("▾"))
        .when(!disabled, |field| {
            field.cursor_pointer().on_click(move |_, window, _| {
                OPEN.with(|open| {
                    let mut open = open.borrow_mut();
                    *open = if open.as_deref() == Some(toggle_key.as_ref()) {
                        None
                    } else {
                        Some(toggle_key.to_string())
                    };
                });
                window.refresh();
            })
        });
    let menu = open.then(|| {
        let rows = options.into_iter().enumerate().map(|(index, option)| {
            let on_select = on_select.clone();
            div()
                .id(index)
                .px_2()
                .py(px(3.))
                .text_sm()
                .cursor_pointer()
                .when(Some(index) == selected, |row| row.bg(theme::active_row()))
                .hover(|style| style.bg(theme::hover_row()))
                .child(option)
                .on_click(move |_, window, cx| {
                    OPEN.with(|open| *open.borrow_mut() = None);
                    on_select(index, window, cx);
                    window.refresh();
                })
        });
        deferred(
            anchored().child(
                div()
                    .id(ElementId::Name(format!("{key}-menu").into()))
                    .occlude()
                    .mt_1()
                    .min_w(width)
                    .max_h(px(260.))
                    .overflow_y_scroll()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .border_color(theme::border())
                    .bg(theme::panel())
                    .shadow_md()
                    .children(rows)
                    .on_mouse_down_out(|_, window, _| {
                        OPEN.with(|open| *open.borrow_mut() = None);
                        window.refresh();
                    }),
            ),
        )
        .with_priority(1)
    });
    div()
        .flex()
        .flex_col()
        .child(field)
        .children(menu)
        .into_any_element()
}

/// Radio buttons, one per row.
pub fn radio(
    id: impl Into<ElementId>,
    options: Vec<SharedString>,
    selected: Option<usize>,
    disabled: bool,
    on_select: Handler<usize>,
) -> AnyElement {
    div()
        .id(id)
        .flex()
        .flex_col()
        .children(options.into_iter().enumerate().map(|(index, option)| {
            let on_select = on_select.clone();
            let chosen = Some(index) == selected;
            div()
                .id(index)
                .flex()
                .items_center()
                .gap_2()
                .h(px(22.))
                .text_sm()
                .child(
                    div()
                        .flex_none()
                        .size(px(14.))
                        .rounded_full()
                        .border_1()
                        .border_color(if chosen {
                            theme::accent()
                        } else {
                            theme::border()
                        })
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(chosen, |dot| {
                            dot.child(div().size(px(8.)).rounded_full().bg(theme::accent()))
                        }),
                )
                .child(option)
                .when(!disabled, |row| {
                    row.cursor_pointer()
                        .on_click(move |_, window, cx| on_select(index, window, cx))
                })
        }))
        .into_any_element()
}

/// A text field over `editor`; Enter calls `on_submit` with its text.
pub fn text_field(
    editor: &Entity<Editor>,
    width: Pixels,
    multiline: bool,
    disabled: bool,
    on_submit: Handler<String>,
) -> AnyElement {
    let submit_editor = editor.clone();
    div()
        .key_context(INPUT_CONTEXT)
        .on_action(move |_: &SubmitWidgetInput, window, cx| {
            let text = submit_editor.read(cx).text();
            on_submit(text, window, cx);
        })
        .flex_none()
        .w(width)
        .when(multiline, |field| field.h(px(ROW_HEIGHT * 3.)))
        .when(!multiline, |field| field.h(px(ROW_HEIGHT)))
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(theme::border())
        .bg(theme::background())
        .overflow_hidden()
        .font_family(theme::MONO_FONT)
        .text_size(px(theme::EDITOR_FONT_SIZE))
        .when(disabled, |field| field.opacity(0.6))
        .child(editor.clone())
        .into_any_element()
}

/// A progress bar filled to `fraction`.
pub fn progress(fraction: f32, color: Hsla, width: Pixels) -> AnyElement {
    div()
        .flex_none()
        .w(width)
        .h(px(10.))
        .rounded_full()
        .bg(theme::border())
        .child(
            div()
                .h_full()
                .w(width * fraction.clamp(0., 1.))
                .rounded_full()
                .bg(color),
        )
        .into_any_element()
}

/// Jupyter's `button_style`/`bar_style` colours.
fn style_color(style: &str) -> Option<Hsla> {
    let rgb = |hex: u32| -> Hsla { gpui::rgb(hex).into() };
    Some(match style {
        "primary" => theme::accent(),
        "success" => rgb(0x4caf50),
        "info" => rgb(0x00bcd4),
        "warning" => rgb(0xff9800),
        "danger" => rgb(0xf44336),
        _ => return None,
    })
}

/// A number as the slider readout format (`d`, `.2f`, `.3e`, `.1%`)
/// shows it.
pub fn format_number(value: f64, format: &str, integer: bool) -> String {
    let format = format.trim();
    let precision = |rest: &str| {
        rest.trim_start_matches('.')
            .trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == '%')
            .parse::<usize>()
            .ok()
    };
    if integer || format.ends_with('d') {
        return format!("{}", value.round() as i64);
    }
    if let Some(rest) = format.strip_suffix('f') {
        return format!("{value:.*}", precision(rest).unwrap_or(2));
    }
    if let Some(rest) = format.strip_suffix('e') {
        return format!("{value:.*e}", precision(rest).unwrap_or(2));
    }
    if let Some(rest) = format.strip_suffix('%') {
        return format!("{:.*}%", precision(rest).unwrap_or(0), value * 100.);
    }
    let text = format!("{value:.4}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// A layout's `width` (`300px`, `50%`) when it sets one.
fn layout_width(store: &WidgetStore, model: &WidgetModel) -> Option<DefiniteLength> {
    let layout = store.get(model.reference("layout")?)?;
    let width = layout.str("width").trim();
    if let Some(pixels) = width.strip_suffix("px") {
        return pixels.trim().parse::<f32>().ok().map(|w| px(w).into());
    }
    if let Some(percent) = width.strip_suffix('%') {
        return percent
            .trim()
            .parse::<f32>()
            .ok()
            .map(|p| relative(p / 100.));
    }
    None
}

fn layout_hidden(store: &WidgetStore, model: &WidgetModel) -> bool {
    model
        .reference("layout")
        .and_then(|layout| store.get(layout))
        .is_some_and(|layout| {
            layout.str("display") == "none" || layout.str("visibility") == "hidden"
        })
}

/// A pixel width from the layout (minus the description column), else
/// `default`.
fn control_width(store: &WidgetStore, model: &WidgetModel, default: f32) -> Pixels {
    match layout_width(store, model) {
        Some(DefiniteLength::Absolute(gpui::AbsoluteLength::Pixels(width))) => {
            let label = if model.str("description").is_empty() {
                0.
            } else {
                LABEL_WIDTH + 8.
            };
            (width - px(label)).max(px(40.))
        }
        _ => px(default),
    }
}

/// Whether a model has a native view.
pub fn is_supported(model: &WidgetModel) -> bool {
    let module_ok = matches!(
        model.module.as_str(),
        "@jupyter-widgets/controls" | "@jupyter-widgets/output" | "@jupyter-widgets/base" | ""
    );
    module_ok
        && (input_kind(model).is_some()
            || matches!(
                model.name.as_str(),
                "IntSliderModel"
                    | "FloatSliderModel"
                    | "FloatLogSliderModel"
                    | "IntRangeSliderModel"
                    | "FloatRangeSliderModel"
                    | "CheckboxModel"
                    | "ToggleButtonModel"
                    | "ToggleButtonsModel"
                    | "DropdownModel"
                    | "RadioButtonsModel"
                    | "SelectModel"
                    | "SelectMultipleModel"
                    | "ButtonModel"
                    | "LabelModel"
                    | "HTMLModel"
                    | "HTMLMathModel"
                    | "ImageModel"
                    | "IntProgressModel"
                    | "FloatProgressModel"
                    | "BoxModel"
                    | "HBoxModel"
                    | "VBoxModel"
                    | "GridBoxModel"
                    | "AccordionModel"
                    | "TabModel"
                    | "StackModel"
                    | "OutputModel"
            ))
}

/// One output's widget view (`model_id` from its widget-view mime), or
/// the text fallback with a note saying why.
pub(super) fn widget_output(
    model_id: Option<&str>,
    plain: Option<&str>,
    cache: &mut OutputCache,
    options: &OutputOptions,
    on_action: &OnAction,
    cx: &App,
) -> AnyElement {
    let fallback = |cache: &mut OutputCache, note: String| {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .children(
                plain.map(|plain| super::output::plain_block(plain, cache, options, on_action)),
            )
            .child(super::output::note(note))
            .into_any_element()
    };
    let (Some(model_id), Some(render)) = (model_id, options.widgets) else {
        return fallback(
            cache,
            "Widgets are not available here; showing the text.".into(),
        );
    };
    let Some(model) = render.store.get(model_id) else {
        return fallback(
            cache,
            "The widget is gone (its kernel ended or it was closed); run the cell again to show it."
                .into(),
        );
    };
    if !is_supported(model) {
        return fallback(
            cache,
            format!(
                "The {} widget ({}) has no native view; showing its text.",
                model.name.trim_end_matches("Model"),
                if model.module.is_empty() {
                    "unknown module"
                } else {
                    &model.module
                }
            ),
        );
    }
    let live = model.live;
    let element = render_model(model_id, render, cache, options, on_action, cx, 0);
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(element)
        .when(!live, |frame| {
            frame.child(super::output::note(
                "Saved widget state: run the cell to use the widget.",
            ))
        })
        .into_any_element()
}

fn render_model(
    id: &str,
    render: WidgetRender,
    cache: &mut OutputCache,
    options: &OutputOptions,
    on_action: &OnAction,
    cx: &App,
    depth: usize,
) -> AnyElement {
    let store = render.store;
    let Some(model) = store.get(id) else {
        return div().into_any_element();
    };
    if depth > 32 || layout_hidden(store, model) {
        return div().into_any_element();
    }
    if !is_supported(model) {
        return super::output::note(format!(
            "[{} widget: no native view]",
            model.name.trim_end_matches("Model")
        ))
        .into_any_element();
    }
    let disabled = model.bool("disabled") || !model.live;
    let description = model.str("description");
    let key: SharedString = format!("widget-{id}").into();
    let element_id = ElementId::Name(key.clone());
    let update = {
        let on_action = on_action.clone();
        let model = id.to_owned();
        move |key: &'static str| {
            let on_action = on_action.clone();
            let model = model.clone();
            move |value: Value, send: bool, window: &mut Window, cx: &mut App| {
                emit(
                    &on_action,
                    WidgetEvent::Update {
                        model: model.clone(),
                        key: key.into(),
                        value,
                        send,
                    },
                    window,
                    cx,
                );
            }
        }
    };
    let options_labels = || -> Vec<SharedString> {
        model
            .get("_options_labels")
            .and_then(Value::as_array)
            .map(|labels| {
                labels
                    .iter()
                    .map(|label| SharedString::from(label.as_str().unwrap_or_default().to_owned()))
                    .collect()
            })
            .unwrap_or_default()
    };
    let index = model
        .get("index")
        .and_then(Value::as_u64)
        .map(|index| index as usize);
    let children = |cache: &mut OutputCache| -> Vec<AnyElement> {
        model
            .references("children")
            .into_iter()
            .map(|child| render_model(child, render, cache, options, on_action, cx, depth + 1))
            .collect()
    };
    let element: AnyElement = match model.name.as_str() {
        "IntSliderModel"
        | "FloatSliderModel"
        | "FloatLogSliderModel"
        | "IntRangeSliderModel"
        | "FloatRangeSliderModel" => {
            let integer = model.name.starts_with("Int");
            let log_base =
                (model.name == "FloatLogSliderModel").then(|| model.f64("base").unwrap_or(10.));
            let values: Vec<f64> = match model.get("value") {
                Some(Value::Array(values)) => values.iter().filter_map(Value::as_f64).collect(),
                Some(value) => value.as_f64().into_iter().collect(),
                None => Vec::new(),
            };
            let format = model.str("readout_format");
            let readout = model
                .get("readout")
                .and_then(Value::as_bool)
                .unwrap_or(true)
                .then(|| {
                    values
                        .iter()
                        .map(|value| format_number(*value, format, integer))
                        .collect::<Vec<_>>()
                        .join(" – ")
                });
            let spec = Slider {
                min: model.f64("min").unwrap_or(0.),
                max: model.f64("max").unwrap_or(100.),
                step: model.f64("step").unwrap_or(if integer { 1. } else { 0.1 }),
                values: if values.is_empty() { vec![0.] } else { values },
                integer,
                log_base,
                continuous: model
                    .get("continuous_update")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                disabled,
                readout,
                width: control_width(store, model, TRACK_WIDTH + 60.) - px(60.),
            };
            let on_change = Rc::new(update("value"));
            let on_commit = {
                let on_action = on_action.clone();
                let model = id.to_owned();
                Rc::new(move |window: &mut Window, cx: &mut App| {
                    emit(
                        &on_action,
                        WidgetEvent::Commit {
                            model: model.clone(),
                            key: "value".into(),
                        },
                        window,
                        cx,
                    )
                })
            };
            labelled(description, slider(key, spec, on_change, on_commit)).into_any_element()
        }
        "TextModel"
        | "ComboboxModel"
        | "TextareaModel"
        | "IntTextModel"
        | "BoundedIntTextModel"
        | "FloatTextModel"
        | "BoundedFloatTextModel" => {
            let (kind, multiline) = input_kind(model).expect("text-like");
            let width = control_width(store, model, if multiline { 320. } else { 200. });
            match render.inputs.editor(id) {
                Some(editor) => {
                    let on_change = update("value");
                    let on_submit =
                        Rc::new(move |text: String, window: &mut Window, cx: &mut App| {
                            if let Some(value) = kind.parse(&text) {
                                on_change(value, true, window, cx);
                            }
                        });
                    labelled(
                        description,
                        text_field(editor, width, multiline, disabled, on_submit),
                    )
                    .into_any_element()
                }
                // Not created yet (the next frame has it): the value.
                None => labelled(
                    description,
                    div()
                        .w(width)
                        .text_sm()
                        .child(SharedString::from(kind.text(model.get("value")))),
                )
                .into_any_element(),
            }
        }
        "CheckboxModel" => {
            let on_change = update("value");
            let toggle = Rc::new(move |checked: bool, window: &mut Window, cx: &mut App| {
                on_change(Value::Bool(checked), true, window, cx)
            });
            let indent = model.get("indent").and_then(Value::as_bool).unwrap_or(true);
            div()
                .flex()
                .items_center()
                .gap_2()
                .when(indent, |row| {
                    row.child(div().flex_none().w(px(LABEL_WIDTH)))
                })
                .child(checkbox(
                    element_id,
                    model.bool("value"),
                    description,
                    disabled,
                    toggle,
                ))
                .into_any_element()
        }
        "ToggleButtonModel" => {
            let on_change = update("value");
            let value = model.bool("value");
            let color = style_color(model.str("button_style"));
            button(
                element_id,
                description,
                value,
                color.filter(|_| value),
                disabled,
                Rc::new(move |window, cx| on_change(Value::Bool(!value), true, window, cx)),
            )
        }
        "ToggleButtonsModel" => {
            let labels = options_labels();
            let on_change = Rc::new(update("index"));
            labelled(
                description,
                div().id(element_id).flex().flex_wrap().gap_1().children(
                    labels.iter().enumerate().map(|(position, label)| {
                        let on_change = on_change.clone();
                        button(
                            position,
                            label,
                            Some(position) == index,
                            None,
                            disabled,
                            Rc::new(move |window, cx| on_change(json!(position), true, window, cx)),
                        )
                    }),
                ),
            )
            .into_any_element()
        }
        "DropdownModel" => {
            let on_change = update("index");
            labelled(
                description,
                dropdown(
                    key,
                    options_labels(),
                    index,
                    disabled,
                    control_width(store, model, 200.),
                    Rc::new(move |position, window, cx| {
                        on_change(json!(position), true, window, cx)
                    }),
                ),
            )
            .into_any_element()
        }
        "RadioButtonsModel" => {
            let on_change = update("index");
            labelled(
                description,
                radio(
                    element_id,
                    options_labels(),
                    index,
                    disabled,
                    Rc::new(move |position, window, cx| {
                        on_change(json!(position), true, window, cx)
                    }),
                ),
            )
            .items_start()
            .into_any_element()
        }
        "SelectModel" | "SelectMultipleModel" => {
            let multiple = model.name == "SelectMultipleModel";
            let selected: Vec<usize> = match model.get("index") {
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(Value::as_u64)
                    .map(|i| i as usize)
                    .collect(),
                Some(value) => value.as_u64().map(|i| i as usize).into_iter().collect(),
                None => Vec::new(),
            };
            let rows = model.f64("rows").unwrap_or(5.).max(1.) as f32;
            let on_change = Rc::new(update("index"));
            let width = control_width(store, model, 200.);
            labelled(
                description,
                div()
                    .id(element_id)
                    .w(width)
                    .max_h(px(rows * 22. + 4.))
                    .overflow_y_scroll()
                    .py(px(1.))
                    .rounded_sm()
                    .border_1()
                    .border_color(theme::border())
                    .bg(theme::background())
                    .children(
                        options_labels()
                            .into_iter()
                            .enumerate()
                            .map(|(position, label)| {
                                let on_change = on_change.clone();
                                let chosen = selected.contains(&position);
                                let selected = selected.clone();
                                div()
                                    .id(position)
                                    .h(px(22.))
                                    .px_2()
                                    .text_sm()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .when(chosen, |row| {
                                        row.bg(theme::accent()).text_color(theme::on_accent())
                                    })
                                    .when(!disabled, |row| {
                                        row.cursor_pointer()
                                            .when(!chosen, |row| {
                                                row.hover(|style| style.bg(theme::hover_row()))
                                            })
                                            .on_click(move |_, window, cx| {
                                                let value = if multiple {
                                                    let mut next: Vec<usize> = selected.clone();
                                                    match next.iter().position(|i| *i == position) {
                                                        Some(at) => {
                                                            next.remove(at);
                                                        }
                                                        None => next.push(position),
                                                    }
                                                    next.sort_unstable();
                                                    json!(next)
                                                } else {
                                                    json!(position)
                                                };
                                                on_change(value, true, window, cx)
                                            })
                                    })
                                    .child(label)
                            }),
                    ),
            )
            .items_start()
            .into_any_element()
        }
        "ButtonModel" => {
            let on_action = on_action.clone();
            let model_id = id.to_owned();
            button(
                element_id,
                description,
                false,
                style_color(model.str("button_style")),
                disabled,
                Rc::new(move |window, cx| {
                    emit(
                        &on_action,
                        WidgetEvent::Custom {
                            model: model_id.clone(),
                            content: json!({"event": "click"}),
                        },
                        window,
                        cx,
                    )
                }),
            )
        }
        "LabelModel" => labelled(
            description,
            div()
                .text_sm()
                .child(selectable(element_id, model.str("value").to_owned())),
        )
        .into_any_element(),
        "HTMLModel" | "HTMLMathModel" => {
            let value = model.str("value");
            let body = match parsed(value, cx).as_ref() {
                Ok(blocks) => render_html(element_id, blocks),
                Err(_) => div()
                    .text_sm()
                    .child(selectable(element_id, value.to_owned()))
                    .into_any_element(),
            };
            labelled(description, div().min_w_0().child(body)).into_any_element()
        }
        "ImageModel" => {
            let format = match model.str("format") {
                "jpeg" | "jpg" => Some(ImageFormat::Jpeg),
                "gif" => Some(ImageFormat::Gif),
                "svg+xml" | "svg" => Some(ImageFormat::Svg),
                "webp" => Some(ImageFormat::Webp),
                "png" | "" => Some(ImageFormat::Png),
                _ => None,
            };
            let image = format
                .zip(model.buffer("value"))
                .and_then(|(format, data)| cached_image(format, data, true));
            let size = |key: &str| {
                model
                    .str(key)
                    .trim()
                    .trim_end_matches("px")
                    .parse::<f32>()
                    .ok()
            };
            match image {
                Some(image) => {
                    let size = match (size("width"), size("height"), image.size) {
                        (Some(w), Some(h), _) => Some((w, h)),
                        (Some(w), None, Some((iw, ih))) => Some((w, w * ih / iw)),
                        (None, Some(h), Some((iw, ih))) => Some((h * iw / ih, h)),
                        _ => None,
                    };
                    super::output::image_element(&image, size)
                }
                None => super::output::note("[image]").into_any_element(),
            }
        }
        "IntProgressModel" | "FloatProgressModel" => {
            let (min, max) = (
                model.f64("min").unwrap_or(0.),
                model.f64("max").unwrap_or(100.),
            );
            let value = model.f64("value").unwrap_or(min);
            let fraction = if max > min {
                ((value - min) / (max - min)) as f32
            } else {
                0.
            };
            let color = style_color(model.str("bar_style")).unwrap_or_else(theme::accent);
            labelled(
                description,
                progress(fraction, color, control_width(store, model, TRACK_WIDTH)),
            )
            .into_any_element()
        }
        "BoxModel" | "HBoxModel" | "GridBoxModel" => div()
            .id(element_id)
            .flex()
            .flex_wrap()
            .items_start()
            .gap_2()
            .children(children(cache))
            .into_any_element(),
        "VBoxModel" => div()
            .id(element_id)
            .flex()
            .flex_col()
            .gap_1()
            .children(children(cache))
            .into_any_element(),
        "AccordionModel" | "TabModel" | "StackModel" => {
            let titles: Vec<String> = model
                .get("titles")
                .and_then(Value::as_array)
                .map(|titles| {
                    titles
                        .iter()
                        .map(|title| title.as_str().unwrap_or_default().to_owned())
                        .collect()
                })
                .unwrap_or_default();
            let selected = model
                .get("selected_index")
                .and_then(Value::as_u64)
                .map(|index| index as usize);
            let child_ids = model.references("children");
            let on_change = Rc::new(update("selected_index"));
            let title = |position: usize| -> SharedString {
                titles
                    .get(position)
                    .filter(|title| !title.is_empty())
                    .cloned()
                    .unwrap_or_else(|| format!("{}", position + 1))
                    .into()
            };
            match model.name.as_str() {
                "AccordionModel" => div()
                    .id(element_id)
                    .flex()
                    .flex_col()
                    .border_1()
                    .border_color(theme::border())
                    .rounded_sm()
                    .children(child_ids.iter().enumerate().map(|(position, child)| {
                        let open = selected == Some(position);
                        let on_change = on_change.clone();
                        div()
                            .id(position)
                            .flex()
                            .flex_col()
                            .when(position > 0, |section| {
                                section.border_t_1().border_color(theme::border())
                            })
                            .child(
                                div()
                                    .id("title")
                                    .px_2()
                                    .h(px(ROW_HEIGHT))
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .bg(theme::panel())
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(if open { "▾" } else { "▸" })
                                    .child(title(position))
                                    .when(!disabled, |row| {
                                        row.cursor_pointer().on_click(move |_, window, cx| {
                                            let next =
                                                if open { Value::Null } else { json!(position) };
                                            on_change(next, true, window, cx)
                                        })
                                    }),
                            )
                            .when(open, |section| {
                                section.child(div().p_2().child(render_model(
                                    child,
                                    render,
                                    cache,
                                    options,
                                    on_action,
                                    cx,
                                    depth + 1,
                                )))
                            })
                    }))
                    .into_any_element(),
                _ => {
                    let tabs = model.name == "TabModel";
                    div()
                        .id(element_id)
                        .flex()
                        .flex_col()
                        .gap_1()
                        .when(tabs, |frame| {
                            frame.child(
                                div()
                                    .flex()
                                    .border_b_1()
                                    .border_color(theme::border())
                                    .children((0..child_ids.len()).map(|position| {
                                        let chosen = selected == Some(position);
                                        let on_change = on_change.clone();
                                        div()
                                            .id(position)
                                            .px_3()
                                            .py_1()
                                            .text_sm()
                                            .when(chosen, |tab| {
                                                tab.border_b_2()
                                                    .border_color(theme::accent())
                                                    .font_weight(FontWeight::SEMIBOLD)
                                            })
                                            .when(!disabled, |tab| {
                                                tab.cursor_pointer().on_click(
                                                    move |_, window, cx| {
                                                        on_change(json!(position), true, window, cx)
                                                    },
                                                )
                                            })
                                            .child(title(position))
                                    })),
                            )
                        })
                        .children(selected.and_then(|position| child_ids.get(position)).map(
                            |child| {
                                render_model(
                                    child,
                                    render,
                                    cache,
                                    options,
                                    on_action,
                                    cx,
                                    depth + 1,
                                )
                            },
                        ))
                        .into_any_element()
                }
            }
        }
        "OutputModel" => div()
            .id(element_id)
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .children(model.outputs.iter().enumerate().map(|(position, output)| {
                render_output_inner(
                    ElementId::NamedInteger(key.clone(), position as u64),
                    output,
                    cache,
                    options,
                    on_action,
                    cx,
                )
            }))
            .into_any_element(),
        _ => super::output::note(format!("[{}]", model.name)).into_any_element(),
    };
    let width = layout_width(store, model);
    div()
        .min_w_0()
        .when_some(width, |frame, width| frame.w(width))
        .when(disabled && model.live, |frame| frame.opacity(0.6))
        .child(element)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::output::render_output;
    use gpui::{Modifiers, TestAppContext, point};
    use notebook::Output;

    fn open(store: &mut WidgetStore, id: &str, name: &str, module: &str, state: Value) {
        let mut state = state.as_object().cloned().unwrap();
        state.insert("_model_name".into(), name.into());
        state.insert("_model_module".into(), module.into());
        store.apply(
            "comm_open",
            &json!({"comm_id": id, "target_name": "jupyter.widget",
                    "data": {"state": state, "buffer_paths": []}}),
            &[],
            None,
        );
    }

    struct Widgets {
        store: WidgetStore,
        inputs: WidgetInputs,
        cache: OutputCache,
        output: Output,
        events: Rc<RefCell<Vec<WidgetEvent>>>,
    }

    impl Render for Widgets {
        fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
            let events = self.events.clone();
            div().size_full().child(render_output(
                "output",
                &self.output,
                &mut self.cache,
                OutputOptions {
                    max_lines: 10,
                    expanded: false,
                    widgets: Some(WidgetRender {
                        store: &self.store,
                        inputs: &self.inputs,
                    }),
                    marimo_inputs: None,
                },
                move |action, _, _| {
                    if let OutputAction::Widget(event) = action {
                        events.borrow_mut().push(event);
                    }
                },
                cx,
            ))
        }
    }

    fn controls(live: bool) -> Widgets {
        let mut store = WidgetStore::new();
        let controls = "@jupyter-widgets/controls";
        open(
            &mut store,
            "slider",
            "IntSliderModel",
            controls,
            json!({"value": 10, "min": 0, "max": 20, "description": "x"}),
        );
        open(
            &mut store,
            "button",
            "ButtonModel",
            controls,
            json!({"description": "Go", "button_style": "primary"}),
        );
        open(
            &mut store,
            "fruit",
            "DropdownModel",
            controls,
            json!({"_options_labels": ["a", "b", "c"], "index": 1}),
        );
        open(
            &mut store,
            "check",
            "CheckboxModel",
            controls,
            json!({"value": true, "description": "on"}),
        );
        open(
            &mut store,
            "html",
            "HTMLModel",
            controls,
            json!({"value": "<b>bold</b> text"}),
        );
        open(
            &mut store,
            "bar",
            "IntProgressModel",
            controls,
            json!({"value": 3, "max": 10}),
        );
        open(
            &mut store,
            "out",
            "OutputModel",
            "@jupyter-widgets/output",
            json!({"msg_id": ""}),
        );
        store.capture(
            "out",
            "stream",
            &json!({"name": "stdout", "text": "captured\n"}),
        );
        open(
            &mut store,
            "map",
            "LeafletMapModel",
            "jupyter-leaflet",
            json!({}),
        );
        open(
            &mut store,
            "box",
            "VBoxModel",
            controls,
            json!({"children": ["IPY_MODEL_slider", "IPY_MODEL_button", "IPY_MODEL_fruit",
                                "IPY_MODEL_check", "IPY_MODEL_html", "IPY_MODEL_bar",
                                "IPY_MODEL_out", "IPY_MODEL_map"]}),
        );
        if !live {
            store.kernel_gone();
        }
        Widgets {
            store,
            inputs: WidgetInputs::default(),
            cache: OutputCache::default(),
            output: Output::DisplayData {
                data: notebook::outputs::bundle_from_json(&json!({
                    "application/vnd.jupyter.widget-view+json": {"model_id": "box", "version_major": 2},
                    "text/plain": "VBox()",
                })),
                metadata: Default::default(),
                display_id: None,
            },
            events: Rc::default(),
        }
    }

    #[gpui::test]
    fn sliders_drag_and_buttons_click(cx: &mut TestAppContext) {
        let widgets = controls(true);
        let events = widgets.events.clone();
        let (_, cx) = cx.add_window_view(|_, _| widgets);
        cx.run_until_parked();
        let track = cx
            .debug_bounds("widget-slider-track")
            .expect("the slider renders");
        let at =
            |fraction: f32| point(track.left() + track.size.width * fraction, track.center().y);
        cx.simulate_mouse_down(at(0.75), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(at(1.5), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(at(1.5), MouseButton::Left, Modifiers::none());
        let update = |value: Value, send: bool| WidgetEvent::Update {
            model: "slider".into(),
            key: "value".into(),
            value,
            send,
        };
        {
            let events = events.borrow();
            assert_eq!(events[0], update(json!(15), true));
            // Past the end: the maximum (sent at once or by the commit).
            assert!(matches!(&events[1], WidgetEvent::Update { value, .. } if *value == json!(20)));
            assert_eq!(
                events.last(),
                Some(&WidgetEvent::Commit {
                    model: "slider".into(),
                    key: "value".into()
                })
            );
        }
        events.borrow_mut().clear();
        let button = cx
            .debug_bounds("widget-button")
            .expect("the button renders");
        cx.simulate_click(button.center(), Modifiers::none());
        assert_eq!(
            *events.borrow(),
            [WidgetEvent::Custom {
                model: "button".into(),
                content: json!({"event": "click"})
            }]
        );
    }

    #[gpui::test]
    fn saved_state_renders_without_controls(cx: &mut TestAppContext) {
        let widgets = controls(false);
        let events = widgets.events.clone();
        let (_, cx) = cx.add_window_view(|_, _| widgets);
        cx.run_until_parked();
        let track = cx.debug_bounds("widget-slider-track").expect("still drawn");
        cx.simulate_click(track.center(), Modifiers::none());
        let button = cx.debug_bounds("widget-button").unwrap();
        cx.simulate_click(button.center(), Modifiers::none());
        assert!(events.borrow().is_empty(), "{:?}", events.borrow());
    }

    #[test]
    fn sliders_snap_to_steps_and_ranges_keep_order() {
        let spec = Slider {
            min: 0.,
            max: 10.,
            step: 0.5,
            values: vec![2., 6.],
            integer: false,
            log_base: None,
            continuous: true,
            disabled: false,
            readout: None,
            width: px(100.),
        };
        assert_eq!(spec.value_at(0.33), 3.5);
        assert_eq!(spec.value_at(-1.), 0.);
        assert_eq!(spec.value_at(2.), 10.);
        assert_eq!(spec.fraction(5.), 0.5);
        // Moving the low handle past the high one swaps them.
        assert_eq!(spec.with(0, 8.), json!([6.0, 8.0]));
        let log = Slider {
            min: -1.,
            max: 3.,
            step: 1.,
            values: vec![10.],
            log_base: Some(10.),
            ..spec.clone()
        };
        assert_eq!(log.fraction(10.), 0.5);
        assert_eq!(log.value_at(1.), 1000.);
        let int = Slider {
            integer: true,
            step: 1.,
            values: vec![3.],
            ..spec
        };
        assert_eq!(int.with(0, int.value_at(0.42)), json!(4));
    }

    #[test]
    fn readouts_follow_the_format() {
        assert_eq!(format_number(1.23456, ".2f", false), "1.23");
        assert_eq!(format_number(12.0, "d", false), "12");
        assert_eq!(format_number(7.6, "", true), "8");
        assert_eq!(format_number(0.25, ".1%", false), "25.0%");
        assert_eq!(format_number(1500., ".1e", false), "1.5e3");
        assert_eq!(format_number(0.30000000000000004, "", false), "0.3");
    }

    #[test]
    fn text_fields_parse_by_kind() {
        let bounded = InputKind::Int {
            min: Some(0.),
            max: Some(10.),
        };
        assert_eq!(bounded.parse("12"), Some(json!(10)));
        assert_eq!(bounded.parse("x"), None);
        assert_eq!(
            InputKind::Float {
                min: None,
                max: None
            }
            .parse(" 2.5 "),
            Some(json!(2.5))
        );
        assert_eq!(InputKind::Text.parse("hi"), Some(json!("hi")));
        assert_eq!(bounded.text(Some(&json!(3))), "3");
    }

    #[test]
    fn text_fields_are_found_through_boxes() {
        let mut store = WidgetStore::new();
        let open = |store: &mut WidgetStore, id: &str, name: &str, state: Value| {
            let mut state = state.as_object().cloned().unwrap();
            state.insert("_model_name".into(), name.into());
            state.insert("_model_module".into(), "@jupyter-widgets/controls".into());
            store.apply(
                "comm_open",
                &json!({"comm_id": id, "target_name": "jupyter.widget",
                        "data": {"state": state, "buffer_paths": []}}),
                &[],
                None,
            );
        };
        open(&mut store, "t", "TextModel", json!({"value": "hello"}));
        open(
            &mut store,
            "n",
            "BoundedIntTextModel",
            json!({"value": 3, "min": 0, "max": 5, "continuous_update": false}),
        );
        open(
            &mut store,
            "box",
            "HBoxModel",
            json!({"children": ["IPY_MODEL_t", "IPY_MODEL_n"]}),
        );
        let mut specs = input_specs(&store, &["box"]);
        specs.sort_by(|a, b| a.model.cmp(&b.model));
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].text, "3");
        assert!(!specs[0].continuous);
        assert_eq!(specs[1].text, "hello");
        assert!(specs[1].continuous, "Text sends as you type by default");
        assert!(is_supported(store.get("box").unwrap()));
    }
}
