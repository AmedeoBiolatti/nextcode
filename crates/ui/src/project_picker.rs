//! "Open Recent" modal: recent projects plus whatever location is typed
//! (a local folder, `Distro:/linux/path`, `\\wsl.localhost\...`, or any of
//! those after `devcontainer:`).

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, MouseButton,
    SharedString, Subscription, Window, actions, div, prelude::*, px, rgb,
};

use crate::{
    editor::{self, Editor, EditorEvent},
    project::Location,
    recent, theme,
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
    /// Rows: a typed location (if any) first, then matching recents.
    rows: Vec<(Location, SharedString)>,
    selected: usize,
    _subscription: Subscription,
}

impl ProjectPicker {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let query = cx.new(Editor::single_line);
        let subscription = cx.subscribe(&query, |this, _, event, cx| {
            if let EditorEvent::Edited = event {
                this.filter(cx);
            }
        });
        let mut picker = Self {
            query,
            recent: recent::load(),
            rows: Vec::new(),
            selected: 0,
            _subscription: subscription,
        };
        picker.filter(cx);
        picker
    }

    fn filter(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).text();
        let needle = query.trim().to_lowercase();
        let mut rows: Vec<(Location, SharedString)> = Vec::new();
        if let Some(typed) = Location::parse(&query)
            && !self.recent.contains(&typed)
        {
            rows.push((typed.clone(), format!("Open {typed}").into()));
        }
        rows.extend(
            self.recent
                .iter()
                .filter(|location| location.to_string().to_lowercase().contains(&needle))
                .map(|location| (location.clone(), location.to_string().into())),
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
        if let Some((location, _)) = self.rows.get(index) {
            cx.emit(ProjectPickerEvent::Open(location.clone()));
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(ProjectPickerEvent::Dismissed);
    }
}

impl Focusable for ProjectPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for ProjectPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let hint = if self.rows.is_empty() {
            "Type a folder, Distro:/linux/path, \\\\wsl.localhost\\Distro\\path, or devcontainer:<folder>"
        } else {
            "Enter opens in a new window (or switches to it)"
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
            .children(
                self.rows
                    .iter()
                    .enumerate()
                    .take(14)
                    .map(|(index, (_, label))| {
                        div()
                            .id(index)
                            .h(px(ROW_HEIGHT))
                            .px_3()
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .text_color(rgb(theme::TEXT))
                            .when(index == self.selected, |row| row.bg(rgb(theme::ACTIVE_ROW)))
                            .when(index != self.selected, |row| {
                                row.hover(|style| style.bg(rgb(theme::HOVER_ROW)))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| this.open(index, cx)))
                            .child(label.clone())
                    }),
            )
            .child(
                div()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(rgb(theme::BORDER))
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(hint),
            )
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
    }
}
