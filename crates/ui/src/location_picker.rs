//! Modal list of code locations (multiple definitions, references).

use std::{collections::HashMap, path::PathBuf};

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, KeyBinding, MouseButton, ScrollStrategy,
    SharedString, UniformListScrollHandle, Window, actions, div, prelude::*, px, uniform_list,
};
use language::Location;

use crate::{
    pinpoint::{self, Pin},
    project::Project,
    selectable::selectable,
    theme,
};

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
    /// Containing directory, relative to the project root when inside it
    /// (`root › dir` when the project has several folders).
    dir: SharedString,
    /// The target line's text, trimmed.
    preview: SharedString,
    /// Its pinpoint id: `item:<file key>:<line>:<column>` (1-based).
    pin: SharedString,
}

pub struct LocationPicker {
    title: SharedString,
    entries: Vec<Entry>,
    selected: usize,
    focus_handle: FocusHandle,
    scroll: UniformListScrollHandle,
}

impl LocationPicker {
    #[cfg(test)]
    pub fn new(
        title: SharedString,
        locations: Vec<Location>,
        project: &Project,
        cx: &mut Context<Self>,
    ) -> Self {
        let labels = vec![None; locations.len()];
        Self::with_labels(title, locations, labels, project, cx)
    }

    /// Like `new`, with the name (`file:line`) and preview of each location
    /// given where `labels` has them (a notebook cell's), read otherwise.
    pub fn with_labels(
        title: SharedString,
        locations: Vec<Location>,
        labels: Vec<Option<(String, String)>>,
        project: &Project,
        cx: &mut Context<Self>,
    ) -> Self {
        // One read per file; missing files just lose their preview.
        let mut files: HashMap<PathBuf, Option<String>> = HashMap::new();
        let entries = locations
            .into_iter()
            .zip(labels.into_iter().chain(std::iter::repeat(None)))
            .map(|(location, label)| {
                let (_, dir) = project.display_parts(&location.path);
                let start = location.range.start;
                let pin = format!(
                    "item:{}:{}:{}",
                    pinpoint::file_key(project, &location.path),
                    start.line + 1,
                    start.character + 1
                )
                .into();
                if let Some((name, preview)) = label {
                    return Entry {
                        name: name.into(),
                        dir: dir.into(),
                        preview: preview.into(),
                        pin,
                        location,
                    };
                }
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
                let (file_name, _) = project.display_parts(&location.path);
                Entry {
                    name: format!("{file_name}:{}", line + 1).into(),
                    dir: dir.into(),
                    pin,
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
            .bg(theme::panel())
            .border_1()
            .border_color(theme::border())
            .rounded_md()
            .shadow_lg()
            .text_sm()
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme::border())
                    .text_color(theme::muted())
                    .child(selectable(
                        "title",
                        format!("{} ({})", self.title, self.entries.len()),
                    ))
                    .pin("title")
                    .interactive(false),
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
                                    .when(index == this.selected, |row| row.bg(theme::active_row()))
                                    .when(index != this.selected, |row| {
                                        row.hover(|style| style.bg(theme::hover_row()))
                                    })
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| this.open(index, cx)),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_color(theme::text())
                                            .child(selectable("name", entry.name.clone())),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .font_family(theme::MONO_FONT)
                                            .text_color(theme::text())
                                            .child(selectable("preview", entry.preview.clone())),
                                    )
                                    .child(
                                        div()
                                            .flex_shrink()
                                            .max_w(px(280.))
                                            .truncate()
                                            .text_xs()
                                            .text_color(theme::muted())
                                            .child(selectable("dir", entry.dir.clone())),
                                    )
                                    .pin(entry.pin.clone())
                                    .label(entry.name.clone())
                            })
                            .collect()
                    }),
                )
                .track_scroll(self.scroll.clone())
                .h(px(ROW_HEIGHT * rows as f32)),
            )
            .pin("location-picker")
            .label(self.title.clone())
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use lsp::types::{Position, Range};

    use super::*;

    #[gpui::test]
    fn references_are_labelled_with_their_folder_when_there_are_several(cx: &mut TestAppContext) {
        let base = std::env::temp_dir().join(format!("next_code_locations_{}", std::process::id()));
        let (api, web) = (base.join("api"), base.join("web"));
        std::fs::create_dir_all(web.join("src")).unwrap();
        std::fs::create_dir_all(&api).unwrap();
        std::fs::write(web.join("src").join("lib.rs"), "fn a() {}\n    call();\n").unwrap();
        std::fs::write(api.join("main.rs"), "call();\n").unwrap();
        let at = |path: PathBuf, line| Location {
            path,
            range: Range::new(Position::new(line, 0), Position::new(line, 4)),
        };
        let locations = vec![
            at(web.join("src").join("lib.rs"), 1),
            at(api.join("main.rs"), 0),
        ];
        let project = Project::local_folders(&[&api, &web]);
        let picker = cx.new(|cx| LocationPicker::new("References".into(), locations, &project, cx));
        picker.read_with(cx, |picker, _| {
            let shown: Vec<(&str, &str, &str)> = picker
                .entries
                .iter()
                .map(|entry| {
                    (
                        entry.name.as_ref(),
                        entry.dir.as_ref(),
                        entry.preview.as_ref(),
                    )
                })
                .collect();
            assert_eq!(
                shown,
                [
                    ("lib.rs:2", "web › src", "call();"),
                    ("main.rs:1", "api", "call();")
                ]
            );
        });
        std::fs::remove_dir_all(base).ok();
    }
}
