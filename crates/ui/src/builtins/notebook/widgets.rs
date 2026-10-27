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
//! The controls themselves are `crate::controls`, which marimo's `mo.ui`
//! (`marimo_ui`) uses too.

use std::{cell::Cell, collections::HashMap, rc::Rc};

use gpui::{
    AnyElement, App, DefiniteLength, ElementId, Entity, Focusable, FontWeight, Hsla, ImageFormat,
    Pixels, SharedString, Subscription, WeakEntity, Window, div, prelude::*, px, relative,
};
use notebook::widgets::{WidgetModel, WidgetStore, view_model_id};
use serde_json::{Value, json};

use super::{
    document::NotebookDocument,
    html::{cached_image, parsed, render_html},
    output::{OnAction, OutputAction, OutputCache, OutputOptions, render_output_inner},
};
use crate::{
    controls::{
        LABEL_WIDTH, ROW_HEIGHT, Slider, TRACK_WIDTH, button, checkbox, dropdown, labelled,
        progress, radio, slider, text_field,
    },
    editor::{Editor, EditorEvent},
    pinpoint::Pin,
    selectable::selectable,
    theme,
};

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
    let name = model.name.as_str();
    let (min, max) = if name.starts_with("Bounded") {
        (model.f64("min"), model.f64("max"))
    } else {
        (None, None)
    };
    Some(match name {
        "TextModel" | "ComboboxModel" => (InputKind::Text, false),
        "TextareaModel" => (InputKind::Text, true),
        "IntTextModel" | "BoundedIntTextModel" => (InputKind::Int { min, max }, false),
        "FloatTextModel" | "BoundedFloatTextModel" => (InputKind::Float { min, max }, false),
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

fn emit(on_action: &OnAction, event: WidgetEvent, window: &mut Window, cx: &mut App) {
    on_action(OutputAction::Widget(event), window, cx);
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
                        .pin(format!("option:{label}"))
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
                                    .child(label.clone())
                                    .pin(format!("option:{label}"))
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
                    let size = crate::images::fit(size("width"), size("height"), image.size);
                    crate::images::image_element(&image, size)
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
                                    })
                                    .pin(format!("section:{}", title(position))),
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
                                            .pin(format!("tab:{}", title(position)))
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
        .pin(if description.is_empty() {
            format!("widget:{id}")
        } else {
            format!("widget:{description}")
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::builtins::notebook::output::render_output;
    use gpui::{Modifiers, MouseButton, TestAppContext, point};
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
