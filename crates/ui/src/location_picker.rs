//! Modal list of code locations (multiple definitions, references).

use std::{collections::HashMap, path::PathBuf};

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, KeyBinding, MouseButton, ScrollStrategy,
    SharedString, UniformListScrollHandle, Window, actions, div, prelude::*, px, rgb, uniform_list,
};
use language::Location;

use crate::{project::Project, theme};

actions!(location_picker, [SelectPrev, SelectNext, Confirm, Dismiss]);

const CONTEXT: &str = "LocationPicker";
const ROW_HEIGHT: f32 = 28.;

pub fn bind_keys(cx: &mut App) {
    let ctx = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", SelectPrev, ctx),
        KeyBinding::new("down", SelectNext, ctx),
        KeyBinding::new("enter", Confirm, ctx),
        KeyBinding::new("escape", Dismiss, ctx),
    ]);
}

pub enum LocationPickerEvent {
    Open(Location),
    Dismissed,
}

impl EventEmitter<LocationPickerEvent> for LocationPicker {}

struct Entry {
    location: Location,
    /// `file.py:12`.
    name: SharedString,
    /// Containing directory, relative to the project root when inside it.
    dir: SharedString,
    /// The target line's text, trimmed.
    preview: SharedString,
}

pub struct LocationPicker {
    title: SharedString,
    entries: Vec<Entry>,
    selected: usize,
    focus_handle: FocusHandle,
    scroll: UniformListScrollHandle,
}

impl LocationPicker {
    pub fn new(
        title: SharedString,
        locations: Vec<Location>,
        project: &Project,
        cx: &mut Context<Self>,
    ) -> Self {
        // One read per file; missing files just lose their preview.
        let mut files: HashMap<PathBuf, Option<String>> = HashMap::new();
        let entries = locations
            .into_iter()
            .map(|location| {
                let text = files
                    .entry(location.path.clone())
                    .or_insert_with(|| project.read_text(&location.path).ok());
                let line = location.range.start.line as usize;
                let preview = text
                    .as_deref()
                    .and_then(|text| text.lines().nth(line))
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let shown = location
                    .path
                    .strip_prefix(project.root())
                    .unwrap_or(&location.path);
                let file_name = shown.file_name().unwrap_or_default().to_string_lossy();
                Entry {
                    name: format!("{file_name}:{}", line + 1).into(),
                    dir: shown
                        .parent()
                        .map(|dir| dir.display().to_string())
                        .unwrap_or_default()
                        .into(),
                    preview: preview.into(),
                    location,
                }
            })
            .collect();
        Self {
            title,
            entries,
            selected: 0,
            focus_handle: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
        }
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected = index;
        self.scroll.scroll_to_item(index, ScrollStrategy::Center);
        cx.notify();
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected.checked_sub(1) {
            self.select(index, cx);
        }
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected + 1 < self.entries.len() {
            self.select(self.selected + 1, cx);
        }
    }

    fn confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        self.open(self.selected, cx);
    }

    fn open(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(entry) = self.entries.get(index) {
            cx.emit(LocationPickerEvent::Open(entry.location.clone()));
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(LocationPickerEvent::Dismissed);
    }
}

impl Focusable for LocationPicker {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for LocationPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.entries.len().min(14);
        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::dismiss))
            .on_mouse_down_out(
                cx.listener(|this, _, window, cx| this.dismiss(&Dismiss, window, cx)),
            )
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .w(px(760.))
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
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(theme::BORDER))
                    .text_color(rgb(theme::MUTED))
                    .child(format!("{} ({})", self.title, self.entries.len())),
            )
            .child(
                uniform_list(
                    "locations",
                    self.entries.len(),
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|index| {
                                let entry = &this.entries[index];
                                div()
                                    .id(index)
                                    .h(px(ROW_HEIGHT))
                                    .w_full()
                                    .px_3()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .overflow_hidden()
                                    .cursor_pointer()
                                    .when(index == this.selected, |row| {
                                        row.bg(rgb(theme::ACTIVE_ROW))
                                    })
                                    .when(index != this.selected, |row| {
                                        row.hover(|style| style.bg(rgb(theme::HOVER_ROW)))
                                    })
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| this.open(index, cx)),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_color(rgb(theme::TEXT))
                                            .child(entry.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .font_family(theme::MONO_FONT)
                                            .text_color(rgb(theme::TEXT))
                                            .child(entry.preview.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex_shrink()
                                            .max_w(px(280.))
                                            .truncate()
                                            .text_xs()
                                            .text_color(rgb(theme::MUTED))
                                            .child(entry.dir.clone()),
                                    )
                            })
                            .collect()
                    }),
                )
                .track_scroll(self.scroll.clone())
                .h(px(ROW_HEIGHT * rows as f32)),
            )
    }
}
