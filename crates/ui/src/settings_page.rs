//! The Settings page: every user setting of `settings.json` with a
//! control, by category or found by search. A change is written to the
//! file at once (keeping everything else in it) and applies everywhere,
//! like an edit of the file; a value equal to the default is removed
//! instead of written, so the file holds only what differs. Settings the
//! page has no control for (language servers' commands, formatters, debug
//! adapters) stay in the file, a click away.

use std::collections::HashMap;

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding, MouseButton,
    ScrollHandle, SharedString, Subscription, Window, actions, div, prelude::*, px,
};
use serde_json::Value;
use syntax::Language;

use crate::{
    editor::{self, Editor, EditorEvent},
    file_icons,
    pinpoint::Pin,
    selectable::selectable,
    settings, theme,
};

pub mod schema;

use schema::{Category, Control, Setting};

actions!(settings_page, [Dismiss]);

const CONTEXT: &str = "SettingsPage";
const WIDTH: f32 = 940.;
const HEIGHT: f32 = 640.;
const SIDEBAR_WIDTH: f32 = 210.;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Dismiss, Some(CONTEXT))]);
}

pub enum SettingsPageEvent {
    Close,
    /// Open `settings.json` in an editor.
    OpenJson,
}

impl EventEmitter<SettingsPageEvent> for SettingsPage {}

pub struct SettingsPage {
    focus: FocusHandle,
    search: Entity<Editor>,
    category: Category,
    /// The language the Languages category shows.
    language: Language,
    /// Text inputs by setting key: a number's value, a list's new item.
    inputs: HashMap<String, Entity<Editor>>,
    /// Why the last change of a key failed (bad number, unwritable file).
    errors: HashMap<String, SharedString>,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl SettingsPage {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(Editor::single_line);
        window.focus(&search.focus_handle(cx));
        let subscription = cx.subscribe(&search, |this, _, event, cx| {
            if let EditorEvent::Edited = event {
                this.scroll.set_offset(Default::default());
                cx.notify();
            }
        });
        Self {
            focus: cx.focus_handle(),
            search,
            category: Category::Appearance,
            language: Language::Python,
            inputs: HashMap::new(),
            errors: HashMap::new(),
            scroll: ScrollHandle::new(),
            _subscriptions: vec![subscription],
        }
    }

    fn dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(SettingsPageEvent::Close);
    }

    fn query(&self, cx: &App) -> String {
        self.search.read(cx).text().trim().to_lowercase()
    }

    /// Writes `value` for `setting` (removing the key when it is the
    /// default or `None`) and applies it.
    fn write(&mut self, setting: &Setting, value: Option<Value>, cx: &mut Context<Self>) {
        let value = value.filter(|value| *value != setting.default);
        match theme::write_setting(&setting.key, value, cx) {
            Ok(()) => {
                self.errors.remove(&setting.key);
            }
            Err(err) => {
                self.errors.insert(setting.key.clone(), err.into());
            }
        }
        cx.notify();
    }

    /// The text input of `key`, created on first use. Enter or leaving it
    /// commits a number; Enter adds a list's item.
    fn input(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) -> Entity<Editor> {
        if let Some(input) = self.inputs.get(key) {
            return input.clone();
        }
        let input = cx.new(Editor::single_line);
        let blur_key = key.to_owned();
        let subscription = cx.on_blur(&input.focus_handle(cx), window, move |this, _, cx| {
            this.commit(&blur_key, false, cx)
        });
        self._subscriptions.push(subscription);
        self.inputs.insert(key.to_owned(), input.clone());
        input
    }

    /// Enter in one of the page's inputs.
    fn enter(&mut self, _: &editor::Newline, window: &mut Window, cx: &mut Context<Self>) {
        let focused = self
            .inputs
            .iter()
            .find(|(_, input)| input.focus_handle(cx).is_focused(window))
            .map(|(key, _)| key.clone());
        if let Some(key) = focused {
            self.commit(&key, true, cx);
        }
    }

    /// The setting shown under `key` (global, or the Languages category's).
    fn setting(&self, key: &str) -> Option<Setting> {
        schema::settings()
            .into_iter()
            .chain(
                Language::ALL
                    .into_iter()
                    .flat_map(|language| schema::language_settings(language, &settings::current())),
            )
            .find(|setting| setting.key == key)
    }

    /// A number input's value (when it changed), or a list input's new
    /// item (on Enter only).
    fn commit(&mut self, key: &str, enter: bool, cx: &mut Context<Self>) {
        let (Some(setting), Some(input)) = (self.setting(key), self.inputs.get(key).cloned())
        else {
            return;
        };
        let text = input.read(cx).text().trim().to_owned();
        let raw = settings::current().raw.clone();
        let current = schema::get(&raw, key)
            .cloned()
            .unwrap_or_else(|| setting.default.clone());
        match &setting.control {
            Control::Number { min, max } => {
                match text.parse::<u64>() {
                    Ok(number) if (*min..=*max).contains(&number) => {
                        if Value::from(number) != current {
                            self.write(&setting, Some(number.into()), cx);
                        } else {
                            self.errors.remove(key);
                        }
                    }
                    _ => {
                        self.errors.insert(
                            key.to_owned(),
                            format!("Enter a whole number from {min} to {max}").into(),
                        );
                    }
                }
                cx.notify();
            }
            Control::List { .. } if enter && !text.is_empty() => {
                let mut items = current.as_array().cloned().unwrap_or_default();
                if !items.iter().any(|item| item.as_str() == Some(&text)) {
                    items.push(text.into());
                    self.write(&setting, Some(Value::Array(items)), cx);
                }
                input.update(cx, |input, cx| input.set_text("", cx));
            }
            _ => {}
        }
    }

    fn render_toggle(
        &self,
        setting: &Setting,
        on: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let target = setting.clone();
        div()
            .id(SharedString::from(format!("toggle:{}", setting.key)))
            .flex_none()
            .w(px(34.))
            .h(px(20.))
            .p(px(3.))
            .flex()
            .when(on, |track| track.justify_end())
            .rounded_full()
            .cursor_pointer()
            .bg(if on { theme::accent() } else { theme::border() })
            .child(div().size(px(14.)).rounded_full().bg(if on {
                theme::on_accent()
            } else {
                theme::muted()
            }))
            .on_click(
                cx.listener(move |this, _, _, cx| this.write(&target, Some((!on).into()), cx)),
            )
            .pin("toggle")
            .label(if on { "On" } else { "Off" })
            .into_any_element()
    }

    /// Segmented buttons; with no default (a language's override), a
    /// leading "Default" removes the key.
    fn render_choice(
        &self,
        setting: &Setting,
        choices: &[(Value, &'static str)],
        set: Option<&Value>,
        effective: &Value,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let inherit = setting.default.is_null();
        // "Default · On": what the override follows when unset.
        let default_label: SharedString = match setting.inherited {
            Some(true) => "Default · On".into(),
            Some(false) => "Default · Off".into(),
            None => "Default".into(),
        };
        let options = inherit.then(|| (None, default_label)).into_iter().chain(
            choices
                .iter()
                .map(|(value, label)| (Some(value.clone()), SharedString::from(*label))),
        );
        div()
            .flex_none()
            .flex()
            .p(px(2.))
            .gap(px(2.))
            .rounded_md()
            .bg(theme::background())
            .border_1()
            .border_color(theme::border())
            .children(options.enumerate().map(|(index, (value, label))| {
                let active = match &value {
                    None => set.is_none(),
                    Some(value) if inherit => set == Some(value),
                    Some(value) => effective == value,
                };
                let target = setting.clone();
                div()
                    .id(SharedString::from(format!(
                        "choice:{}:{index}",
                        setting.key
                    )))
                    .px_2p5()
                    .py(px(2.))
                    .rounded_sm()
                    .text_xs()
                    .cursor_pointer()
                    .text_color(if active {
                        theme::on_accent()
                    } else {
                        theme::text()
                    })
                    .when(active, |button| button.bg(theme::accent()))
                    .when(!active, |button| {
                        button.hover(|style| style.bg(theme::hover_row()))
                    })
                    .child(label.clone())
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.write(&target, value.clone(), cx)),
                    )
                    .pin(format!(
                        "choice:{}",
                        label.split(' ').next().unwrap_or_default().to_lowercase()
                    ))
                    .label(label)
            }))
            .into_any_element()
    }

    fn render_number(
        &mut self,
        setting: &Setting,
        effective: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let input = self.input(&setting.key, window, cx);
        // Show the value in effect unless the user is typing.
        if !input.focus_handle(cx).is_focused(window) && !self.errors.contains_key(&setting.key) {
            let text = effective.to_string();
            if input.read(cx).text() != text {
                input.update(cx, |input, cx| input.set_text(&text, cx));
            }
        }
        let invalid = self.errors.contains_key(&setting.key);
        div()
            .flex_none()
            .w(px(96.))
            .h(px(26.))
            .px_1p5()
            .flex()
            .items_center()
            .rounded_md()
            .bg(theme::background())
            .border_1()
            .border_color(if invalid {
                theme::error()
            } else {
                theme::border()
            })
            .child(div().flex_1().h(px(20.)).child(input))
            .pin("number")
            .label("Value")
            .into_any_element()
    }

    fn render_list(
        &mut self,
        setting: &Setting,
        effective: &Value,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let items: Vec<String> = effective
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let input = self.input(&setting.key, window, cx);
        let empty = input.read(cx).text().is_empty();
        div()
            .mt_2()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1p5()
            .children(items.iter().enumerate().map(|(index, item)| {
                let target = setting.clone();
                let remaining: Vec<Value> = items
                    .iter()
                    .enumerate()
                    .filter(|(other, _)| *other != index)
                    .map(|(_, item)| item.as_str().into())
                    .collect();
                div()
                    .h(px(24.))
                    .pl_2()
                    .pr_1()
                    .flex()
                    .items_center()
                    .gap_1()
                    .rounded_md()
                    .bg(theme::hover_row())
                    .border_1()
                    .border_color(theme::border())
                    .text_xs()
                    .font_family(theme::MONO_FONT)
                    .child(selectable(("item", index), item.clone()))
                    .child(
                        div()
                            .id(("remove", index))
                            .size(px(16.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_sm()
                            .cursor_pointer()
                            .hover(|style| style.bg(theme::border()))
                            .child(file_icons::icon("close", theme::muted()).size(px(10.)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.write(&target, Some(Value::Array(remaining.clone())), cx)
                            }))
                            .pin("remove")
                            .label("Remove"),
                    )
                    .pin(format!("item:{item}"))
            }))
            .child(
                div()
                    .relative()
                    .w(px(260.))
                    .h(px(26.))
                    .px_1p5()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .bg(theme::background())
                    .border_1()
                    .border_color(theme::border())
                    .child(div().flex_1().h(px(20.)).child(input))
                    .when(empty, |field| {
                        field.child(
                            div()
                                .absolute()
                                .left(px(8.))
                                .text_xs()
                                .text_color(theme::muted())
                                .child(placeholder),
                        )
                    })
                    .pin("add")
                    .label(placeholder),
            )
            .into_any_element()
    }

    fn render_theme(
        &self,
        setting: &Setting,
        effective: &Value,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let current = effective.as_str().unwrap_or(theme::DEFAULT).to_owned();
        div()
            .mt_2()
            .flex()
            .flex_wrap()
            .gap_2()
            .children(theme::names().into_iter().enumerate().map(|(index, name)| {
                let active = name == current;
                let target = setting.clone();
                let value = Value::from(name.clone());
                div()
                    .id(("theme", index))
                    .h(px(28.))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(if active {
                        theme::accent()
                    } else {
                        theme::border()
                    })
                    .when(active, |chip| chip.bg(theme::accent().opacity(0.15)))
                    .when(!active, |chip| {
                        chip.hover(|style| style.bg(theme::hover_row()))
                    })
                    .cursor_pointer()
                    .text_sm()
                    .when(active, |chip| {
                        chip.child(file_icons::icon("check", theme::accent_text()).size(px(12.)))
                    })
                    .child(name.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        // Written even when it is the default: the theme
                        // is a choice people expect to see in the file.
                        if let Err(err) = theme::select(value.as_str().unwrap_or_default(), cx) {
                            this.errors.insert(target.key.clone(), err.into());
                        }
                        cx.notify();
                    }))
                    .pin(format!("theme:{name}"))
                    .label(name)
            }))
            .into_any_element()
    }

    fn render_setting(
        &mut self,
        setting: &Setting,
        raw: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let set = schema::get(raw, &setting.key).cloned();
        let effective = set.clone().unwrap_or_else(|| setting.default.clone());
        // Set to something else than the default (an explicit default
        // can still be reset, which tidies the file).
        let modified = set.as_ref().is_some_and(|set| *set != setting.default);
        let (inline, below): (Option<gpui::AnyElement>, Option<gpui::AnyElement>) =
            match &setting.control {
                Control::Toggle => (
                    Some(self.render_toggle(setting, effective.as_bool().unwrap_or(false), cx)),
                    None,
                ),
                Control::Choice(choices) => (
                    Some(self.render_choice(setting, choices, set.as_ref(), &effective, cx)),
                    None,
                ),
                Control::Number { .. } => (
                    Some(self.render_number(setting, &effective, window, cx)),
                    None,
                ),
                Control::List { placeholder } => (
                    None,
                    Some(self.render_list(setting, &effective, placeholder, window, cx)),
                ),
                Control::Theme => (None, Some(self.render_theme(setting, &effective, cx))),
            };
        let reset = set.is_some().then(|| {
            let target = setting.clone();
            div()
                .id(SharedString::from(format!("reset:{}", setting.key)))
                .flex_none()
                .flex()
                .items_center()
                .gap_1()
                .px_1()
                .rounded_sm()
                .text_xs()
                .text_color(theme::muted())
                .cursor_pointer()
                .hover(|style| style.text_color(theme::text()).bg(theme::hover_row()))
                .child(file_icons::icon("rollback", theme::muted()).size(px(12.)))
                .child("Reset")
                .on_click(cx.listener(move |this, _, _, cx| this.write(&target, None, cx)))
                .pin("reset")
                .label("Reset to default")
        });
        let error = self.errors.get(&setting.key).cloned();
        div()
            .relative()
            .py_3()
            .pl_4()
            .pr_1()
            .border_b_1()
            .border_color(theme::border())
            // A bar marks settings changed from their default.
            .when(modified, |row| {
                row.child(
                    div()
                        .absolute()
                        .left_0()
                        .top(px(14.))
                        .bottom(px(14.))
                        .w(px(3.))
                        .rounded_full()
                        .bg(theme::accent()),
                )
            })
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap_4()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(theme::text())
                                            .child(selectable("title", setting.title.clone())),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_family(theme::MONO_FONT)
                                            .text_color(theme::muted().opacity(0.8))
                                            .child(selectable("key", setting.key.clone())),
                                    )
                                    .children(reset),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(selectable("description", setting.description.clone())),
                            ),
                    )
                    .children(inline),
            )
            .children(below)
            .children(error.map(|error| {
                div()
                    .mt_1()
                    .text_xs()
                    .text_color(theme::error())
                    .child(selectable("error", error))
            }))
            .pin(format!("setting:{}", setting.key))
            .label(setting.title.clone())
            .into_any_element()
    }

    fn render_sidebar(&self, searching: bool, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex_none()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .p_2()
            .flex()
            .flex_col()
            .gap_0p5()
            .border_r_1()
            .border_color(theme::border())
            .children(Category::ALL.into_iter().map(|category| {
                let active = !searching && category == self.category;
                let color = if active {
                    theme::text()
                } else {
                    theme::muted()
                };
                div()
                    .id(category.title())
                    .h(px(30.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded_md()
                    .cursor_pointer()
                    .text_sm()
                    .text_color(color)
                    .when(active, |row| row.bg(theme::active_row()))
                    .when(!active, |row| {
                        row.hover(|style| style.bg(theme::hover_row()).text_color(theme::text()))
                    })
                    .child(file_icons::icon(category.icon(), color).size(px(15.)))
                    .child(category.title())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.category = category;
                        this.scroll.set_offset(Default::default());
                        this.search.update(cx, |search, cx| search.set_text("", cx));
                        cx.notify();
                    }))
                    .pin(format!("category:{}", category.title().to_lowercase()))
                    .label(category.title())
            }))
            .pin("categories")
    }

    /// The Languages category's language chips.
    fn render_languages(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .pb_3()
            .flex()
            .flex_wrap()
            .gap_1p5()
            .children(Language::ALL.into_iter().map(|language| {
                let active = language == self.language;
                div()
                    .id(language.id())
                    .h(px(24.))
                    .px_2p5()
                    .flex()
                    .items_center()
                    .rounded_full()
                    .border_1()
                    .border_color(if active {
                        theme::accent()
                    } else {
                        theme::border()
                    })
                    .when(active, |chip| {
                        chip.bg(theme::accent()).text_color(theme::on_accent())
                    })
                    .when(!active, |chip| {
                        chip.hover(|style| style.bg(theme::hover_row()))
                    })
                    .cursor_pointer()
                    .text_xs()
                    .child(language.name())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.language = language;
                        cx.notify();
                    }))
                    .pin(format!("language:{}", language.id()))
                    .label(language.name())
            }))
            .pin("languages")
    }

    fn section_title(title: &'static str, detail: Option<String>) -> impl IntoElement {
        div()
            .pt_1()
            .pb_2()
            .flex()
            .items_baseline()
            .gap_2()
            .child(
                div()
                    .text_lg()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme::text())
                    .child(title),
            )
            .children(detail.map(|detail| div().text_xs().text_color(theme::muted()).child(detail)))
    }
}

impl Focusable for SettingsPage {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SettingsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let raw = settings::current().raw.clone();
        let query = self.query(cx);
        let searching = !query.is_empty();
        // What the content column lists: search results grouped by
        // category, or the selected category.
        let mut sections: Vec<(Category, Vec<Setting>)> = Vec::new();
        if searching {
            for category in Category::ALL {
                let found: Vec<Setting> = schema::settings()
                    .into_iter()
                    .chain(schema::language_settings(
                        self.language,
                        &settings::current(),
                    ))
                    .filter(|setting| setting.category == category && setting.matches(&query))
                    .collect();
                if !found.is_empty() {
                    sections.push((category, found));
                }
            }
        } else if self.category == Category::Languages {
            sections.push((
                self.category,
                schema::language_settings(self.language, &settings::current()),
            ));
        } else {
            let settings = schema::settings()
                .into_iter()
                .filter(|setting| setting.category == self.category)
                .collect();
            sections.push((self.category, settings));
        }
        let found: usize = sections.iter().map(|(_, settings)| settings.len()).sum();
        // `settings.json` that does not parse: the page shows defaults and
        // writes would be refused, so say why.
        let problem = theme::problem().filter(|problem| problem.starts_with("Settings ignored"));
        let mut content: Vec<gpui::AnyElement> = Vec::new();
        if let Some(problem) = problem {
            content.push(
                div()
                    .mb_3()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme::error())
                    .bg(theme::error().opacity(0.1))
                    .text_xs()
                    .text_color(theme::text())
                    .child(selectable(
                        "problem",
                        format!("{problem}. Fix the file to change settings here."),
                    ))
                    .pin("problem")
                    .into_any_element(),
            );
        }
        if searching && sections.is_empty() {
            content.push(
                div()
                    .pt_8()
                    .flex()
                    .justify_center()
                    .text_sm()
                    .text_color(theme::muted())
                    .child(selectable(
                        "no-results",
                        format!("No settings match \"{query}\""),
                    ))
                    .into_any_element(),
            );
        }
        for (category, settings) in &sections {
            let detail = (*category == Category::Languages)
                .then(|| format!("{}: overrides of the global settings", self.language.name()));
            content.push(Self::section_title(category.title(), detail).into_any_element());
            if *category == Category::Languages && !searching {
                content.push(self.render_languages(cx).into_any_element());
            }
            for setting in settings {
                content.push(self.render_setting(setting, &raw, window, cx));
            }
            content.push(div().h(px(18.)).into_any_element());
        }
        let search_empty = self.search.read(cx).text().is_empty();
        let path = settings::Settings::path()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::enter))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .occlude()
            .w(px(WIDTH))
            .h(px(HEIGHT))
            .flex()
            .flex_col()
            .bg(theme::panel())
            .text_color(theme::text())
            .border_1()
            .border_color(theme::border())
            .rounded_lg()
            .shadow_lg()
            .overflow_hidden()
            .child(
                // Header: title, search, raw file, close.
                div()
                    .flex_none()
                    .h(px(52.))
                    .px_4()
                    .flex()
                    .items_center()
                    .gap_3()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(file_icons::icon("gear", theme::text()).size(px(18.)))
                    .child(
                        div()
                            .w(px(SIDEBAR_WIDTH - 50.))
                            .text_lg()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Settings"),
                    )
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .h(px(30.))
                            .pl(px(30.))
                            .pr_2()
                            .flex()
                            .items_center()
                            .rounded_md()
                            .bg(theme::background())
                            .border_1()
                            .border_color(theme::border())
                            .child(
                                div()
                                    .absolute()
                                    .left(px(9.))
                                    .top(px(7.))
                                    .child(file_icons::icon("search", theme::muted()).size(px(14.))),
                            )
                            .child(div().flex_1().h(px(20.)).child(self.search.clone()))
                            .when(search_empty, |field| {
                                field.child(
                                    div()
                                        .absolute()
                                        .left(px(30.))
                                        .text_sm()
                                        .text_color(theme::muted())
                                        .child("Search settings"),
                                )
                            })
                            .pin("search")
                            .label("Search settings"),
                    )
                    .when(searching, |header| {
                        header.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(theme::muted())
                                .child(match found {
                                    1 => "1 setting".to_owned(),
                                    n => format!("{n} settings"),
                                }),
                        )
                    })
                    .child(
                        div()
                            .id("open-json")
                            .flex_none()
                            .h(px(28.))
                            .px_2p5()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .rounded_md()
                            .border_1()
                            .border_color(theme::border())
                            .text_sm()
                            .cursor_pointer()
                            .hover(|style| style.bg(theme::hover_row()))
                            .child(file_icons::icon("code", theme::text()).size(px(14.)))
                            .child("Edit JSON")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SettingsPageEvent::OpenJson)))
                            .pin("open-json")
                            .label("Edit settings.json"),
                    )
                    .child(
                        div()
                            .id("close")
                            .flex_none()
                            .size(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|style| style.bg(theme::hover_row()))
                            .child(file_icons::icon("close", theme::muted()).size(px(14.)))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SettingsPageEvent::Close)))
                            .pin("close")
                            .label("Close (Escape)"),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.render_sidebar(searching, cx))
                    .child(
                        div()
                            .id("settings-content")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .bg(theme::background())
                            .px_6()
                            .py_4()
                            .children(content)
                            .pin("content"),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .h(px(30.))
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .border_t_1()
                    .border_color(theme::border())
                    .text_xs()
                    .text_color(theme::muted())
                    .child(selectable(
                        "note",
                        "Changes apply at once. A project's .next_code/settings.json can override them.",
                    ))
                    .child(div().min_w_0().truncate().child(selectable("path", path))),
            )
            .pin("settings-page")
            .label("Settings")
    }
}
