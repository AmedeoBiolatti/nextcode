//! Form controls drawn from values and callbacks: sliders (with log and
//! range variants), checkboxes, buttons, dropdowns, radio buttons, text
//! fields and progress bars. Notebook widgets (ipywidgets, marimo's
//! `mo.ui`) and extension views draw their controls with them.

use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App, Bounds, CursorStyle, DispatchPhase, ElementId, Entity, Focusable, Hitbox,
    HitboxBehavior, Hsla, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, SharedString, Window, actions, anchored, canvas, deferred, div, prelude::*, px,
};
use serde_json::{Value, json};

use crate::{editor::Editor, pinpoint::Pin, theme};

actions!(
    controls,
    [
        /// Enter in a text field.
        Submit
    ]
);

const INPUT_CONTEXT: &str = "ControlInput";

/// Bound after the editor's bindings: Enter in a text field submits it.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new(
        "enter",
        Submit,
        Some("ControlInput > Editor"),
    )]);
}

/// A control's callback with the new value.
pub type Handler<T> = Rc<dyn Fn(T, &mut Window, &mut App)>;
/// A control's callback without a value (a click, a drag's end).
pub type Callback = Rc<dyn Fn(&mut Window, &mut App)>;
/// A slider's change: the new model value and whether to send it now.
pub type SliderChange = Rc<dyn Fn(Value, bool, &mut Window, &mut App)>;

thread_local! {
    /// The slider being dragged: (model, handle, last send).
    static DRAG: RefCell<Option<(String, usize, Instant)>> = const { RefCell::new(None) };
    /// The dropdown whose list is open (by element key).
    static OPEN: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// How often a continuous slider drag sends its value.
const DRAG_SEND_INTERVAL: Duration = Duration::from_millis(60);
/// Width of a description label.
pub(crate) const LABEL_WIDTH: f32 = 96.;
/// Default width of a slider's track or a progress bar.
pub(crate) const TRACK_WIDTH: f32 = 220.;
pub(crate) const ROW_HEIGHT: f32 = 26.;

/// A labelled row: `description` in a fixed column, then `control`.
pub(crate) fn labelled(description: &str, control: impl IntoElement) -> gpui::Div {
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
    checkbox_with_color(id, checked, label, disabled, None, on_toggle)
}

/// A checkbox with an optional swatch between its box and label.
pub fn checkbox_with_color(
    id: impl Into<ElementId>,
    checked: bool,
    label: &str,
    disabled: bool,
    color: Option<Hsla>,
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
        .when_some(color, |row, color| {
            row.child(div().flex_none().size(px(8.)).bg(color))
        })
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
            Some(background) => button
                .bg(background)
                .text_color(theme::text_on(Some(background))),
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
    dropdown_with_prefix(key, options, selected, disabled, width, None, on_select)
}

/// A dropdown whose closed field can include a label prefix.
pub fn dropdown_with_prefix(
    key: SharedString,
    options: Vec<SharedString>,
    selected: Option<usize>,
    disabled: bool,
    width: Pixels,
    prefix: Option<SharedString>,
    on_select: Handler<usize>,
) -> AnyElement {
    let open = !disabled && OPEN.with(|open| open.borrow().as_deref() == Some(key.as_ref()));
    let label = selected
        .and_then(|index| options.get(index).cloned())
        .unwrap_or_default();
    let label = prefix.map_or(label.clone(), |prefix| format!("{prefix}: {label}").into());
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
                .child(option.clone())
                .on_click(move |_, window, cx| {
                    OPEN.with(|open| *open.borrow_mut() = None);
                    on_select(index, window, cx);
                    window.refresh();
                })
                .pin(format!("option:{option}"))
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
                    })
                    .pin("menu"),
            ),
        )
        .with_priority(1)
    });
    div()
        .flex()
        .flex_col()
        .child(field.pin("field"))
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
                .child(option.clone())
                .when(!disabled, |row| {
                    row.cursor_pointer()
                        .on_click(move |_, window, cx| on_select(index, window, cx))
                })
                .pin(format!("option:{option}"))
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
    text_field_with_prefix(editor, width, multiline, disabled, None, None, on_submit)
}

/// A text field with an optional label inside its border.
pub fn text_field_with_prefix(
    editor: &Entity<Editor>,
    width: Pixels,
    multiline: bool,
    disabled: bool,
    prefix: Option<SharedString>,
    placeholder: Option<SharedString>,
    on_submit: Handler<String>,
) -> AnyElement {
    let submit_editor = editor.clone();
    div()
        .key_context(INPUT_CONTEXT)
        .on_action(move |_: &Submit, window, cx| {
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
        .when(prefix.is_some(), |field| {
            field.flex().items_center().gap_2()
        })
        .when_some(prefix.clone(), |field, prefix| {
            field.child(div().flex_none().text_sm().child(format!("{prefix}:")))
        })
        .child(if prefix.is_none() && placeholder.is_none() {
            editor.clone().into_any_element()
        } else {
            div()
                .relative()
                .when(prefix.is_some(), |area| area.flex_1().min_w_0())
                .child(editor.clone())
                .when_some(placeholder, |area, placeholder| {
                    area.child(
                        div()
                            .absolute()
                            .left(px(2.))
                            .top(px(0.))
                            .text_color(theme::muted())
                            .child(placeholder)
                            .on_mouse_down(gpui::MouseButton::Left, {
                                let editor = editor.clone();
                                move |_, window, cx| {
                                    window.focus(&editor.focus_handle(cx));
                                    cx.stop_propagation();
                                }
                            }),
                    )
                })
                .into_any_element()
        })
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
