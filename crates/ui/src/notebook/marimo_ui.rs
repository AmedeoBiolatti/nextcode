//! marimo's `mo.ui` elements in a marimo cell's output
//! (`notebook::outputs::MARIMO_UI_MIME`, described by the bridge's helper)
//! drawn with the notebook's native controls (`widgets`): slider, checkbox,
//! switch, dropdown, radio, button, and number/text fields when the view
//! gives them editors. Markdown around them (`mo.md(f"… {slider} …")`)
//! renders as Markdown. Using one sends [`OutputAction::MarimoUi`] with the
//! value in marimo's front-end form; the view hands it to
//! `NotebookDocument::set_ui_value`. Other elements show their value as text.

use std::rc::Rc;

use gpui::{AnyElement, App, ElementId, SharedString, div, prelude::*, px};
use serde_json::{Value, json};

use super::{
    markdown::{MarkdownImages, markdown},
    output::{OnAction, OutputAction, OutputOptions, note},
    widgets::{self, InputKind, InputSpec, Slider},
};

/// Width of a label column and of controls.
const LABEL_WIDTH: f32 = 96.;
const CONTROL_WIDTH: f32 = 220.;

/// The editor key of a text-like element.
pub fn input_key(object_id: &str) -> String {
    format!("marimo:{object_id}")
}

/// Text fields the elements in `parts` need (number and text elements).
pub fn input_specs(parts: &[Value]) -> Vec<InputSpec> {
    parts
        .iter()
        .filter_map(|part| part.get("element"))
        .filter_map(|element| {
            let id = element["id"].as_str()?;
            let kind = input_kind(element)?;
            let text = match &element["value"] {
                Value::String(text) => text.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            Some(InputSpec {
                model: input_key(id),
                text,
                multiline: false,
                continuous: false,
                kind,
            })
        })
        .collect()
}

fn input_kind(element: &Value) -> Option<InputKind> {
    let args = &element["args"];
    match element["kind"].as_str()? {
        "text" => Some(InputKind::Text),
        "number" => {
            let (min, max) = (args["start"].as_f64(), args["stop"].as_f64());
            let integer = [&args["start"], &args["stop"], &args["step"]]
                .iter()
                .all(|value| value.is_null() || value.is_i64());
            Some(if integer {
                InputKind::Int { min, max }
            } else {
                InputKind::Float { min, max }
            })
        }
        _ => None,
    }
}

pub fn render(
    parts: &[Value],
    options: &OutputOptions,
    on_action: &OnAction,
    cx: &App,
) -> AnyElement {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .children(parts.iter().enumerate().map(|(index, part)| {
            if let Some(element) = part.get("element") {
                render_element(element, options, on_action)
            } else if let Some(text) = part["markdown"].as_str() {
                markdown(("marimo-md", index), text, MarkdownImages::default(), cx)
            } else {
                div()
                    .text_sm()
                    .child(SharedString::from(
                        part["text"].as_str().unwrap_or_default().to_owned(),
                    ))
                    .into_any_element()
            }
        }))
        .into_any_element()
}

fn labelled(label: &str, control: AnyElement) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .when(!label.is_empty(), |row| {
            row.child(
                div()
                    .flex_none()
                    .w(px(LABEL_WIDTH))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_sm()
                    .child(SharedString::from(label.to_owned())),
            )
        })
        .child(control)
        .into_any_element()
}

fn render_element(element: &Value, options: &OutputOptions, on_action: &OnAction) -> AnyElement {
    let id = element["id"].as_str().unwrap_or_default().to_owned();
    let label = element["label"].as_str().unwrap_or_default();
    let args = &element["args"];
    let value = &element["value"];
    let disabled = args["disabled"].as_bool() == Some(true);
    let send = {
        let (on_action, id) = (on_action.clone(), id.clone());
        move |value: Option<Value>, send: bool, window: &mut gpui::Window, cx: &mut App| {
            on_action(
                OutputAction::MarimoUi {
                    object_id: id.clone(),
                    value,
                    send,
                },
                window,
                cx,
            )
        }
    };
    let element_id = ElementId::Name(format!("marimo-ui-{id}").into());
    let options_of = || -> Vec<SharedString> {
        args["options"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|option| {
                SharedString::from(
                    option
                        .as_str()
                        .map_or_else(|| option.to_string(), str::to_owned),
                )
            })
            .collect()
    };
    match element["kind"].as_str().unwrap_or_default() {
        "slider" => {
            let steps: Vec<f64> = args["steps"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_f64)
                .collect();
            let (min, max) = match (steps.first(), steps.last()) {
                (Some(first), Some(last)) => (*first, *last),
                _ => (
                    args["start"].as_f64().unwrap_or(0.),
                    args["stop"].as_f64().unwrap_or(100.),
                ),
            };
            let integer = [&args["start"], &args["stop"], &args["step"]]
                .iter()
                .all(|value| value.is_null() || value.is_i64());
            let current = value.as_f64().unwrap_or(min);
            let spec = Slider {
                min,
                max,
                step: args["step"]
                    .as_f64()
                    .unwrap_or(if integer { 1. } else { 0. }),
                values: vec![current],
                integer,
                log_base: None,
                continuous: false,
                disabled,
                readout: Some(widgets::format_number(current, "", integer)),
                width: px(CONTROL_WIDTH),
            };
            let change = send.clone();
            let commit = send.clone();
            labelled(
                label,
                widgets::slider(
                    SharedString::from(format!("marimo-ui-{id}")),
                    spec,
                    Rc::new(move |value, _, window, cx| change(Some(value), false, window, cx)),
                    Rc::new(move |window, cx| commit(None, true, window, cx)),
                ),
            )
        }
        "checkbox" | "switch" => {
            let checked = value.as_bool() == Some(true);
            widgets::checkbox(
                element_id,
                checked,
                label,
                disabled,
                Rc::new(move |checked, window, cx| send(Some(json!(checked)), true, window, cx)),
            )
        }
        "button" => {
            let clicks = value.as_i64().unwrap_or(0);
            widgets::button(
                element_id,
                if label.is_empty() { "Click" } else { label },
                false,
                None,
                disabled,
                Rc::new(move |window, cx| send(Some(json!(clicks + 1)), true, window, cx)),
            )
        }
        "dropdown" => {
            let mut options = options_of();
            let none = args["allow-select-none"].as_bool() == Some(true);
            if none {
                options.insert(0, SharedString::from("--"));
            }
            let chosen = value
                .as_array()
                .and_then(|values| values.first())
                .and_then(Value::as_str);
            let selected = match chosen {
                Some(chosen) => options.iter().position(|option| option.as_ref() == chosen),
                None if none => Some(0),
                None => None,
            };
            let labels = options.clone();
            labelled(
                label,
                widgets::dropdown(
                    SharedString::from(format!("marimo-ui-{id}")),
                    options,
                    selected,
                    disabled,
                    px(CONTROL_WIDTH),
                    Rc::new(move |index, window, cx| {
                        let value = if none && index == 0 {
                            json!([])
                        } else {
                            json!([labels[index].to_string()])
                        };
                        send(Some(value), true, window, cx)
                    }),
                ),
            )
        }
        "radio" => {
            let options = options_of();
            let selected = value
                .as_str()
                .and_then(|chosen| options.iter().position(|option| option.as_ref() == chosen));
            let labels = options.clone();
            labelled(
                label,
                widgets::radio(
                    element_id,
                    options,
                    selected,
                    disabled,
                    Rc::new(move |index, window, cx| {
                        send(Some(json!(labels[index].to_string())), true, window, cx)
                    }),
                ),
            )
        }
        kind @ ("number" | "text") => {
            let editor = options
                .marimo_inputs
                .and_then(|inputs| inputs.editor(&input_key(&id)));
            let Some(editor) = editor else {
                return note(format!("{label} {kind}: {value}")).into_any_element();
            };
            let input = input_kind(element).unwrap_or(InputKind::Text);
            labelled(
                label,
                widgets::text_field(
                    editor,
                    px(CONTROL_WIDTH),
                    false,
                    disabled,
                    Rc::new(move |text, window, cx| {
                        if let Some(value) = input.parse(&text) {
                            send(Some(value), true, window, cx)
                        }
                    }),
                ),
            )
        }
        kind => note(format!(
            "{label} ({kind}): {value} — this marimo element is shown in marimo (Open in marimo)"
        ))
        .into_any_element(),
    }
}
