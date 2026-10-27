//! "Open Recent" modal: recent projects plus whatever location is typed
//! (a local folder, `Distro:/linux/path`, `\\wsl.localhost\...`, a built-in
//! extension's `<scheme>://host/path`, or any of those after
//! `devcontainer:`), and the machines the extensions' connectors suggest
//! (`builtins::Connector::machines`) for what is typed.

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, MouseButton,
    SharedString, Subscription, Task, Window, actions, div, prelude::*, px,
};

use crate::{
    builtins::{self, Machine},
    editor::{self, Editor, EditorEvent},
    pinpoint::Pin,
    project::Location,
    recent,
    selectable::selectable,
    theme,
};

actions!(project_picker, [Dismiss]);

const CONTEXT: &str = "ProjectPicker";
const ROW_HEIGHT: f32 = 28.;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Dismiss, Some(CONTEXT))]);
}

pub enum ProjectPickerEvent {
    Open(Location),
    Dismissed,
}

impl EventEmitter<ProjectPickerEvent> for ProjectPicker {}

pub struct ProjectPicker {
    query: Entity<Editor>,
    recent: Vec<Location>,
    /// Schemes of the connectors that are on (`ssh`).
    schemes: Vec<&'static str>,
    /// What those connectors suggest, with their schemes; filled by a
    /// thread.
    machines: Vec<(&'static str, Machine)>,
    /// Rows: a typed location (if any) first, then matching recents, then
    /// matching machines.
    rows: Vec<Row>,
    selected: usize,
    _subscription: Subscription,
    _machines: Task<()>,
}

struct Row {
    action: Action,
    label: SharedString,
    /// Shown faint after the label.
    detail: Option<SharedString>,
    pin: SharedString,
}

enum Action {
    Open(Location),
    /// Types this, for the path to be added (a machine's home folder).
    Fill(String),
}

impl ProjectPicker {
    /// The picker with `query` typed.
    pub fn new(query: &str, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(Editor::single_line);
        editor.update(cx, |editor, cx| editor.set_text(query, cx));
        let subscription = cx.subscribe(&editor, |this, _, event, cx| {
            if let EditorEvent::Edited = event {
                this.filter(cx);
            }
        });
        let connectors = builtins::connectors(cx);
        let schemes = connectors
            .iter()
            .map(|connector| connector.scheme)
            .collect();
        // Listing machines can take a moment (`tailscale status`).
        let machines = (!connectors.is_empty()).then(|| {
            cx.spawn(async move |this, cx| {
                let machines = cx
                    .background_spawn(async move {
                        connectors
                            .iter()
                            .flat_map(|connector| {
                                (connector.machines)()
                                    .into_iter()
                                    .map(|machine| (connector.scheme, machine))
                            })
                            .collect()
                    })
                    .await;
                this.update(cx, |this, cx| {
                    this.machines = machines;
                    this.filter(cx);
                })
                .ok();
            })
        });
        let mut picker = Self {
            query: editor,
            recent: recent::load(),
            schemes,
            machines: Vec::new(),
            rows: Vec::new(),
            selected: 0,
            _subscription: subscription,
            _machines: machines.unwrap_or_else(|| Task::ready(())),
        };
        picker.filter(cx);
        picker
    }

    fn filter(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text();
        let needle = query.trim().to_lowercase();
        let mut rows: Vec<Row> = Vec::new();
        if let Some(typed) = Location::parse(&query)
            && !self.recent.contains(&typed)
        {
            rows.push(Row {
                label: format!("Open {typed}").into(),
                detail: None,
                pin: format!("item:{typed}").into(),
                action: Action::Open(typed),
            });
        }
        rows.extend(
            self.recent
                .iter()
                .filter(|location| location.to_string().to_lowercase().contains(&needle))
                .map(|location| Row {
                    label: location.to_string().into(),
                    detail: None,
                    pin: format!("item:{location}").into(),
                    action: Action::Open(location.clone()),
                }),
        );
        rows.extend(
            self.machines
                .iter()
                .filter(|(scheme, machine)| machine_matches(scheme, machine, &needle))
                .map(|(scheme, machine)| {
                    let url = format!("{scheme}://{}", machine.host);
                    Row {
                        action: Action::Fill(format!("{url}/~/")),
                        label: url.clone().into(),
                        detail: Some(machine.detail.clone().into()),
                        pin: format!("machine:{url}").into(),
                    }
                }),
        );
        self.rows = rows;
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
        self.open(self.selected, cx);
    }

    fn open(&mut self, index: usize, cx: &mut Context<Self>) {
        match self.rows.get(index).map(|row| &row.action) {
            Some(Action::Open(location)) => cx.emit(ProjectPickerEvent::Open(location.clone())),
            Some(Action::Fill(text)) => {
                let text = text.clone();
                self.query.update(cx, |query, cx| query.set_text(&text, cx));
            }
            None => {}
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ProjectPickerEvent::Dismissed);
    }
}

/// Whether `machine` is suggested for `needle` (lowercase): a typed
/// `<scheme>://` lists its machines, narrowed by the host typed after it
/// until a path follows; other text lists those it is part of.
fn machine_matches(scheme: &str, machine: &Machine, needle: &str) -> bool {
    let host = machine.host.to_lowercase();
    match needle
        .strip_prefix(scheme)
        .and_then(|rest| rest.strip_prefix("://"))
    {
        Some(typed) => !typed.contains('/') && host.contains(typed),
        None => {
            !needle.is_empty()
                && (host.contains(needle) || machine.detail.to_lowercase().contains(needle))
        }
    }
}

impl Focusable for ProjectPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for ProjectPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let hint: SharedString = if self.rows.is_empty() {
            let remote: String = self
                .schemes
                .iter()
                .map(|scheme| format!(", {scheme}://host/path"))
                .collect();
            format!(
                "Type a folder, Distro:/linux/path, \\\\wsl.localhost\\Distro\\path{remote}, \
                 or devcontainer:<folder>"
            )
            .into()
        } else if matches!(
            self.rows.get(self.selected),
            Some(Row {
                action: Action::Fill(_),
                ..
            })
        ) {
            "Enter types the machine's home folder: add a path, or Enter again to open it".into()
        } else {
            "Enter opens in a new window (or switches to it)".into()
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
                    .label("Project location"),
            )
            .children(self.rows.iter().enumerate().take(14).map(|(index, row)| {
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
                    .on_click(cx.listener(move |this, _, _, cx| this.open(index, cx)))
                    .child(selectable("label", row.label.clone()))
                    .children(row.detail.clone().map(|detail| {
                        div()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(selectable("detail", detail))
                    }))
                    .pin(row.pin.clone())
                    .label(row.label.clone())
            }))
            .child(
                div()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(theme::border())
                    .text_xs()
                    .text_color(theme::muted())
                    .child(selectable("hint", hint))
                    .pin("hint")
                    .interactive(false),
            )
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .pin("project-picker")
            .label("Open Recent")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machines_are_listed_for_their_scheme_and_for_what_they_contain() {
        let machine = Machine {
            host: "Box.tail1.ts.net".into(),
            detail: "box · Tailscale · online".into(),
        };
        let matches = |needle: &str| machine_matches("ssh", &machine, needle);
        assert!(matches("ssh://"));
        assert!(matches("ssh://box"));
        assert!(!matches("ssh://other"));
        // A path follows: the typed location is what opens.
        assert!(!matches("ssh://box.tail1.ts.net/~"));
        assert!(matches("tail1"));
        assert!(matches("tailscale"));
        // Open Recent as it was until something is typed.
        assert!(!matches(""));
        assert!(!matches("/home/me"));
    }
}
