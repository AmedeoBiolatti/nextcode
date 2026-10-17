//! The Ports panel (bottom dock, like VS Code's): forwarded ports with their
//! local address, process, origin and connections, and the actions on them.

use devcontainer::ports::ForwardPort as Target;
use gpui::{
    App, ClipboardItem, Context, Entity, FocusHandle, Focusable, KeyBinding, SharedString,
    Subscription, Window, actions, div, prelude::*, px,
};

use super::{Origin, Ports};
use crate::{
    editor::{Editor, EditorEvent},
    selectable::selectable,
    theme,
};

actions!(ports, [ForwardPort, ConfirmInput, CancelInput]);

const INPUT_CONTEXT: &str = "PortInput";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", ConfirmInput, Some("PortInput > Editor")),
        KeyBinding::new("escape", CancelInput, Some(INPUT_CONTEXT)),
    ]);
}

/// What the input row is for.
#[derive(Clone, PartialEq)]
enum Editing {
    Forward,
    LocalPort(Target),
}

pub struct PortsPanel {
    ports: Entity<Ports>,
    input: Entity<Editor>,
    editing: Option<Editing>,
    /// The last action's outcome; `true` for an error.
    message: Option<(SharedString, bool)>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

const COLUMNS: [(&str, f32); 5] = [
    ("Port", 150.),
    ("Forwarded Address", 160.),
    ("Running Process", 0.),
    ("Origin", 130.),
    ("Conns", 50.),
];

impl PortsPanel {
    pub fn new(ports: Entity<Ports>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(Editor::single_line);
        let subscriptions = vec![
            cx.observe(&ports, |_, _, cx| cx.notify()),
            cx.subscribe(&input, |_, _, _: &EditorEvent, cx| cx.notify()),
        ];
        Self {
            ports,
            input,
            editing: None,
            message: None,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    /// Shows the input for Forward a Port and focuses it.
    pub fn start_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.edit(Editing::Forward, "", window, cx);
    }

    /// The forwarded-ports model the panel shows.
    pub fn ports(&self) -> &Entity<Ports> {
        &self.ports
    }

    fn edit(&mut self, editing: Editing, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = Some(editing);
        self.message = None;
        self.input.update(cx, |input, cx| {
            input.set_text(text, cx);
            input.select_all_text(cx);
        });
        window.focus(&self.input.focus_handle(cx));
        cx.notify();
    }

    fn confirm(&mut self, _: &ConfirmInput, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.clone() else {
            return;
        };
        let text = self.input.read(cx).text().trim().to_owned();
        let result = match &editing {
            Editing::Forward => match Target::parse(&text) {
                Some(target) => {
                    let label = target.to_string();
                    self.ports
                        .update(cx, |ports, cx| {
                            ports.forward(target, Origin::User, None, cx)
                        })
                        .map(|local| format!("Forwarded {label} to localhost:{local}"))
                }
                None => Err("Enter a port (8000) or host:port (db:5432)".into()),
            },
            Editing::LocalPort(target) => match text.parse::<u16>() {
                Ok(local) if local != 0 => self
                    .ports
                    .update(cx, |ports, cx| ports.change_local_port(target, local, cx))
                    .map(|local| format!("{target} is now on localhost:{local}")),
                _ => Err("Enter a local port number".into()),
            },
        };
        match result {
            Ok(message) => {
                self.message = Some((message.into(), false));
                self.editing = None;
                window.focus(&self.focus_handle);
            }
            Err(error) => self.message = Some((error.into(), true)),
        }
        cx.notify();
    }

    fn cancel(&mut self, _: &CancelInput, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        window.focus(&self.focus_handle);
        cx.notify();
    }

    fn button(id: impl Into<gpui::ElementId>, label: &'static str) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .px_2()
            .rounded_sm()
            .border_1()
            .border_color(theme::border())
            .text_xs()
            .text_color(theme::text())
            .cursor_pointer()
            .hover(|style| style.bg(theme::hover_row()))
            .child(label)
    }

    fn cell(width: f32) -> gpui::Div {
        let cell = div().px_2().truncate();
        if width > 0. {
            cell.w(px(width)).flex_none()
        } else {
            cell.flex_1().min_w_0()
        }
    }

    fn render_input(&self, editing: &Editing, cx: &mut Context<Self>) -> gpui::Div {
        let prompt = match editing {
            Editing::Forward => "Port or host:port".to_owned(),
            Editing::LocalPort(target) => format!("Local port for {target}"),
        };
        div()
            .key_context(INPUT_CONTEXT)
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .px_2()
            .py_1()
            .flex()
            .items_center()
            .gap_2()
            .text_sm()
            .child(
                div()
                    .text_color(theme::muted())
                    .child(selectable("prompt", prompt)),
            )
            .child(
                div()
                    .w(px(220.))
                    .h(px(24.))
                    .px_2()
                    .py_px()
                    .border_1()
                    .border_color(theme::accent())
                    .rounded_sm()
                    .bg(theme::background())
                    .child(self.input.clone()),
            )
            .child(Self::button("port-input-ok", "Forward").on_click(
                cx.listener(|this, _, window, cx| this.confirm(&ConfirmInput, window, cx)),
            ))
            .child(
                Self::button("port-input-cancel", "Cancel").on_click(
                    cx.listener(|this, _, window, cx| this.cancel(&CancelInput, window, cx)),
                ),
            )
    }
}

impl Focusable for PortsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PortsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ports = self.ports.read(cx);
        let base = div()
            .track_focus(&self.focus_handle)
            .id("ports-panel")
            .size_full()
            .flex()
            .flex_col()
            .overflow_y_scroll()
            .bg(theme::panel())
            .text_sm()
            .text_color(theme::text());
        if !ports.available() {
            return base
                .child(div().p_3().text_color(theme::muted()).child(selectable(
                    "message",
                    "Port forwarding is for WSL and container projects.",
                )))
                .into_any_element();
        }

        let toolbar = div()
            .px_2()
            .py_1()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Self::button("forward-port", "Forward a Port…")
                    .on_click(cx.listener(|this, _, window, cx| this.start_forward(window, cx))),
            )
            .children(
                self.message
                    .clone()
                    .or_else(|| ports.error().map(|error| (error.to_owned().into(), true)))
                    .map(|(text, error)| {
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(if error {
                                theme::error()
                            } else {
                                theme::muted()
                            })
                            .child(selectable("message", text))
                    }),
            );

        let header = div()
            .h(px(22.))
            .flex()
            .items_center()
            .text_xs()
            .text_color(theme::muted())
            .border_b_1()
            .border_color(theme::border())
            .children(COLUMNS.iter().enumerate().map(|(index, &(title, width))| {
                Self::cell(width).child(selectable(("column", index), title))
            }))
            .child(div().w(px(220.)).flex_none());

        let rows: Vec<gpui::AnyElement> = ports
            .forwarded()
            .iter()
            .enumerate()
            .map(|(index, forwarded)| {
                let target = forwarded.target.clone();
                let port = match &forwarded.attributes.label {
                    Some(label) => format!("{label} ({target})"),
                    None => target.to_string(),
                };
                let process = if target.host.is_some() {
                    String::new()
                } else {
                    ports
                        .listener(target.port)
                        .map(|listener| {
                            let command = listener.command.clone().unwrap_or_default();
                            match listener.pid {
                                Some(pid) => format!("{command} ({pid})"),
                                None => command,
                            }
                        })
                        .unwrap_or_default()
                };
                let (url, address) = (forwarded.url(), forwarded.address());
                let (open_url, copy, stop, change) =
                    (url.clone(), address.clone(), target.clone(), target.clone());
                div()
                    .id(("port-row", index))
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .hover(|style| style.bg(theme::hover_row()))
                    .child(Self::cell(COLUMNS[0].1).child(selectable("port", port)))
                    .child(
                        Self::cell(COLUMNS[1].1).child(
                            div()
                                .id(("port-address", index))
                                .text_color(theme::accent_text())
                                .cursor_pointer()
                                .hover(|style| style.underline())
                                .child(selectable("address", address))
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                        ),
                    )
                    .child(
                        Self::cell(COLUMNS[2].1)
                            .text_color(theme::muted())
                            .child(selectable("process", process)),
                    )
                    .child(
                        Self::cell(COLUMNS[3].1)
                            .text_color(theme::muted())
                            .child(selectable("origin", forwarded.origin.label())),
                    )
                    .child(Self::cell(COLUMNS[4].1).child(selectable(
                        "connections",
                        forwarded.connections().to_string(),
                    )))
                    .child(
                        div()
                            .w(px(220.))
                            .flex_none()
                            .flex()
                            .gap_1()
                            .child(
                                Self::button(("port-open", index), "Open")
                                    .on_click(move |_, _, cx| cx.open_url(&open_url)),
                            )
                            .child(Self::button(("port-copy", index), "Copy").on_click(
                                cx.listener(move |this, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                                    this.message = Some((format!("Copied {copy}").into(), false));
                                    cx.notify();
                                }),
                            ))
                            .child(Self::button(("port-local", index), "Port…").on_click(
                                cx.listener(move |this, _, window, cx| {
                                    let current = this
                                        .ports
                                        .read(cx)
                                        .find(&change)
                                        .map(|f| f.local_port().to_string())
                                        .unwrap_or_default();
                                    this.edit(
                                        Editing::LocalPort(change.clone()),
                                        &current,
                                        window,
                                        cx,
                                    );
                                }),
                            ))
                            .child(Self::button(("port-stop", index), "Stop").on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.ports.update(cx, |ports, cx| ports.stop(&stop, cx));
                                    this.message =
                                        Some((format!("Stopped forwarding {stop}").into(), false));
                                    cx.notify();
                                }),
                            )),
                    )
                    .into_any_element()
            })
            .collect();
        let empty = rows.is_empty().then(|| {
            div().p_3().text_color(theme::muted()).child(selectable(
                "empty",
                "No forwarded ports. Forward a port, or start a server: ports that start \
                 listening are forwarded automatically.",
            ))
        });
        let editing = self.editing.clone();
        base.child(toolbar)
            .children(editing.map(|editing| self.render_input(&editing, cx)))
            .child(header)
            .children(rows)
            .children(empty)
            .into_any_element()
    }
}
