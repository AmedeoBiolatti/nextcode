//! Native, workspace-local views of validated extension panel trees.
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    rc::Rc,
    sync::Arc,
};

use crate::project::CancelToken;
use extension_api::{InputValue, PanelUpdate, ProcessExit, UiEvent, UiEventKind, Widget};
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding,
    ScrollAnchor, ScrollHandle, SharedString, Window, actions, div, prelude::*, px,
};

use crate::{controls, editor::Editor, pinpoint::Pin, theme};

mod charts;
mod markdown;
#[cfg(test)]
mod tests;

actions!(
    extension_panels,
    [NextControl, PreviousControl, ActivateControl, SubmitControl]
);

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("tab", NextControl, Some("ExtensionPanel")),
        KeyBinding::new("shift-tab", PreviousControl, Some("ExtensionPanel")),
        KeyBinding::new("enter", ActivateControl, Some("ExtensionControl")),
        KeyBinding::new("space", ActivateControl, Some("ExtensionControl")),
        KeyBinding::new("enter", SubmitControl, Some("ExtensionInput > Editor")),
    ]);
}

#[derive(Clone, Debug)]
pub(crate) enum ExtensionViewEvent {
    Background {
        revision: u64,
        process: Option<ProcessExit>,
        direct: bool,
        inputs: BTreeMap<String, InputValue>,
    },
    Invoke {
        revision: u64,
        event: UiEvent,
        inputs: BTreeMap<String, InputValue>,
    },
}

impl ExtensionViewEvent {
    pub(crate) fn revision(&self) -> u64 {
        match self {
            Self::Invoke { revision, .. } | Self::Background { revision, .. } => *revision,
        }
    }
}

/// Owned by the workspace's panel entry, so closing a panel cancels its jobs
/// even if an in-flight invocation still holds the view alive.
pub(crate) struct PanelLifetime(pub CancelToken);
impl Drop for PanelLifetime {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(crate) struct ExtensionView {
    extension: String,
    panel: String,
    title: String,
    revision: u64,
    model: Option<Arc<PanelUpdate>>,
    inputs: BTreeMap<String, InputValue>,
    baselines: BTreeMap<String, InputValue>,
    editors: HashMap<String, Entity<Editor>>,
    controls: HashMap<String, FocusHandle>,
    order: Vec<String>,
    charts: HashMap<String, charts::ChartState>,
    markdown: HashMap<String, Arc<markdown::Markdown>>,
    scroll: ScrollHandle,
    scrolls: HashMap<String, ScrollHandle>,
    anchors: HashMap<String, ScrollAnchor>,
    scroll_paths: HashMap<String, Vec<String>>,
    focus: FocusHandle,
    busy: bool,
    invoking: bool,
    error: Option<String>,
    pub(crate) lifetime: CancelToken,
    processes: HashMap<String, CancelToken>,
    completed: VecDeque<(ProcessExit, bool)>,
    refresh_due: bool,
    refresh_task: Option<gpui::Task<()>>,
}

impl EventEmitter<ExtensionViewEvent> for ExtensionView {}
impl Focusable for ExtensionView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ExtensionView {
    pub(crate) fn new(
        extension: String,
        panel: String,
        title: String,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            extension,
            panel,
            title,
            revision: 0,
            model: None,
            inputs: BTreeMap::new(),
            baselines: BTreeMap::new(),
            editors: HashMap::new(),
            controls: HashMap::new(),
            order: Vec::new(),
            charts: HashMap::new(),
            markdown: HashMap::new(),
            scroll: ScrollHandle::new(),
            scrolls: HashMap::new(),
            anchors: HashMap::new(),
            scroll_paths: HashMap::new(),
            focus: cx.focus_handle(),
            busy: false,
            invoking: false,
            error: None,
            lifetime: CancelToken::new(),
            processes: HashMap::new(),
            completed: VecDeque::new(),
            refresh_due: false,
            refresh_task: None,
        }
    }
    pub(crate) fn extension(&self) -> &str {
        &self.extension
    }
    pub(crate) fn panel(&self) -> &str {
        &self.panel
    }
    pub(crate) fn title(&self) -> &str {
        &self.title
    }
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }
    pub(crate) fn model(&self) -> Option<&PanelUpdate> {
        self.model.as_deref()
    }
    pub(crate) fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.busy = busy;
        self.dispatch_background(cx);
        cx.notify();
    }
    pub(crate) fn fail(&mut self, message: String, cx: &mut Context<Self>) {
        self.busy = false;
        self.invoking = false;
        self.error = Some(message);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn snapshot(
        &self,
        cx: &App,
    ) -> Option<(u64, Arc<PanelUpdate>, BTreeMap<String, InputValue>)> {
        Some((
            self.revision,
            self.model.as_ref()?.clone(),
            self.current_inputs(cx),
        ))
    }

    pub(crate) fn current_inputs(&self, cx: &App) -> BTreeMap<String, InputValue> {
        self.inputs
            .iter()
            .map(|(id, value)| {
                let value = self.editors.get(id).map_or_else(
                    || value.clone(),
                    |editor| InputValue::Text(editor.read(cx).text()),
                );
                (id.clone(), value)
            })
            .collect()
    }

    pub(crate) fn cancel_processes(&mut self) {
        for token in self.processes.values() {
            token.cancel();
        }
        self.processes.clear();
        self.completed.clear();
    }

    pub(crate) fn can_start_processes(
        &self,
        requests: &[extension_api::ProcessRequest],
    ) -> Result<(), String> {
        if self.processes.len() + self.completed.len() + requests.len()
            > extension_api::MAX_PROCESSES
        {
            return Err("At most four processes may belong to a panel".into());
        }
        if requests.iter().any(|r| {
            self.processes.contains_key(&r.id) || self.completed.iter().any(|(p, _)| p.id == r.id)
        }) {
            return Err("Process id is already in use by this panel".into());
        }
        Ok(())
    }

    pub(crate) fn start_process(&mut self, id: String, token: CancelToken) {
        self.processes.insert(id, token);
    }

    pub(crate) fn process_finished(
        &mut self,
        result: ProcessExit,
        expected: &CancelToken,
        direct: bool,
        cx: &mut Context<Self>,
    ) {
        if !self
            .processes
            .get(&result.id)
            .is_some_and(|token| token.same(expected))
        {
            return;
        }
        let token = self.processes.remove(&result.id).unwrap();
        if !token.is_cancelled() {
            self.completed.push_back((result, direct));
        } else {
            self.refresh_task = None;
            self.refresh_due = false;
        }
        self.dispatch_background(cx);
        cx.notify();
    }

    pub(crate) fn finish_invocation(&mut self, cx: &mut Context<Self>) {
        self.invoking = false;
        self.dispatch_background(cx);
    }

    pub(crate) fn defer_background(&mut self, event: ExtensionViewEvent) {
        if let ExtensionViewEvent::Background {
            process, direct, ..
        } = event
        {
            if let Some(process) = process {
                self.completed.push_front((process, direct));
            } else {
                self.refresh_due = true;
            }
            self.invoking = false;
            self.busy = true;
        }
    }

    fn is_busy(&self) -> bool {
        self.busy || self.invoking || !self.processes.is_empty() || !self.completed.is_empty()
    }

    fn dispatch_background(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.invoking || self.lifetime.is_cancelled() {
            return;
        }
        let (process, direct) = if let Some((process, direct)) = self.completed.pop_front() {
            (Some(process), direct)
        } else if self.refresh_due && self.processes.is_empty() {
            self.refresh_due = false;
            (None, false)
        } else {
            return;
        };
        if self.model.is_none() {
            return;
        }
        self.invoking = true;
        cx.emit(ExtensionViewEvent::Background {
            revision: self.revision,
            process,
            direct,
            inputs: self.current_inputs(cx),
        });
    }

    #[cfg(test)]
    pub(crate) fn replace(
        &mut self,
        update: PanelUpdate,
        submitted: Option<&BTreeMap<String, InputValue>>,
        cx: &mut Context<Self>,
    ) {
        self.replace_from(update, submitted, false, cx);
    }

    pub(crate) fn replace_from(
        &mut self,
        update: PanelUpdate,
        submitted_inputs: Option<&BTreeMap<String, InputValue>>,
        preserve_processes: bool,
        cx: &mut Context<Self>,
    ) {
        if update.id != self.panel {
            self.fail("Extension returned a different panel".into(), cx);
            return;
        }
        if let Err(error) = extensions::validate_panel_update(&update) {
            self.fail(format!("Invalid extension panel: {error}"), cx);
            return;
        }
        if !preserve_processes {
            self.cancel_processes();
        }
        self.refresh_task = None;
        self.refresh_due = false;
        if let Some(ms) = update.refresh_ms {
            self.refresh_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(ms))
                    .await;
                this.update(cx, |this, cx| {
                    this.refresh_due = true;
                    this.dispatch_background(cx);
                })
                .ok();
            }));
        }
        let current = self.current_inputs(cx);
        let mut defaults = BTreeMap::new();
        let mut order = Vec::new();
        let mut text_ids = HashSet::new();
        let mut chart_ids = HashSet::new();
        let mut markdown_ids = HashSet::new();
        visit(&update.root, &mut |widget| {
            let id = widget.id();
            match widget {
                Widget::TextInput {
                    value, disabled, ..
                } => {
                    defaults.insert(id.to_owned(), InputValue::Text(value.clone()));
                    text_ids.insert(id.to_owned());
                    if !disabled {
                        order.push(id.to_owned());
                    }
                }
                Widget::Checkbox {
                    value, disabled, ..
                } => {
                    defaults.insert(id.to_owned(), InputValue::Bool(*value));
                    if !disabled {
                        order.push(id.to_owned());
                    }
                }
                Widget::Select {
                    value, disabled, ..
                } => {
                    defaults.insert(id.to_owned(), InputValue::Text(value.clone()));
                    if !disabled {
                        order.push(id.to_owned());
                    }
                }
                Widget::Button { disabled, .. } => {
                    if !disabled {
                        order.push(id.to_owned());
                    }
                }
                Widget::Chart { chart, .. } => {
                    chart_ids.insert(id.to_owned());
                    order.push(id.to_owned());
                    match self.charts.get_mut(id) {
                        Some(state) => state.replace(chart),
                        None => {
                            self.charts
                                .insert(id.to_owned(), charts::ChartState::new(chart));
                        }
                    }
                }
                Widget::Markdown { text, .. } => {
                    markdown_ids.insert(id.to_owned());
                    let replace = self
                        .markdown
                        .get(id)
                        .is_none_or(|parsed| parsed.source() != text);
                    if replace {
                        self.markdown
                            .insert(id.to_owned(), Arc::new(markdown::Markdown::new(text)));
                    }
                }
                _ => {}
            }
        });
        let mut reconciled = reconcile(
            &self.baselines,
            &current,
            defaults.clone(),
            submitted_inputs,
        );
        visit(&update.root, &mut |widget| {
            if let Widget::Select {
                id, value, options, ..
            } = widget
            {
                let valid = matches!(reconciled.get(id), Some(InputValue::Text(selected)) if options.iter().any(|option| &option.value == selected));
                if !valid {
                    reconciled.insert(id.clone(), InputValue::Text(value.clone()));
                }
            }
        });
        self.baselines = defaults;
        self.editors.retain(|id, _| text_ids.contains(id));
        for id in text_ids {
            let Some(InputValue::Text(value)) = reconciled.get(&id) else {
                continue;
            };
            if let Some(editor) = self.editors.get(&id) {
                if !editor.read(cx).buffer().equals(value) {
                    editor.update(cx, |editor, cx| editor.set_text(value, cx));
                }
            } else {
                let editor = cx.new(|cx| {
                    let mut editor = Editor::single_line(cx);
                    editor.set_text(value, cx);
                    editor
                });
                self.editors.insert(id, editor);
            }
        }
        self.controls.retain(|id, _| order.contains(id));
        for id in &order {
            if !self.editors.contains_key(id) {
                self.controls
                    .entry(id.clone())
                    .or_insert_with(|| cx.focus_handle());
            }
        }
        self.charts.retain(|id, _| chart_ids.contains(id));
        self.markdown.retain(|id, _| markdown_ids.contains(id));
        let mut scroll_ids = HashSet::new();
        visit(&update.root, &mut |widget| {
            if let Widget::Scroll { id, .. } = widget {
                scroll_ids.insert(id.clone());
                self.scrolls.entry(id.clone()).or_default();
            }
        });
        self.scrolls.retain(|id, _| scroll_ids.contains(id));
        self.anchors.clear();
        self.scroll_paths.clear();
        prepare_scrolling(
            &update.root,
            &self.scroll,
            &[],
            &self.scrolls,
            &mut self.anchors,
            &mut self.scroll_paths,
        );
        self.inputs = reconciled;
        self.order = order;
        self.model = Some(Arc::new(update));
        self.revision = self.revision.saturating_add(1);
        self.busy = false;
        self.invoking = false;
        self.error = None;
        cx.notify();
    }

    fn emit(&mut self, event: UiEvent, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let Some(model) = &self.model else {
            return;
        };
        let inputs = self.current_inputs(cx);
        if let Err(error) = extensions::validate_panel_event(model, &event, &inputs) {
            self.error = Some(format!("Cannot send extension action: {error}"));
            cx.notify();
            return;
        }
        self.error = None;
        self.invoking = true;
        cx.notify();
        cx.emit(ExtensionViewEvent::Invoke {
            revision: self.revision,
            event,
            inputs,
        });
    }

    fn change(&mut self, id: &str, value: InputValue, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let Some(model) = &self.model else {
            return;
        };
        let event = UiEvent {
            widget: id.to_owned(),
            kind: UiEventKind::Change {
                value: value.clone(),
            },
        };
        let mut inputs = self.current_inputs(cx);
        inputs.insert(id.to_owned(), value.clone());
        if extensions::validate_panel_event(model, &event, &inputs).is_err() {
            return;
        }
        self.inputs.insert(id.to_owned(), value);
        self.emit(event, cx);
        cx.notify();
    }

    fn handle(&self, id: &str, cx: &App) -> Option<FocusHandle> {
        self.editors
            .get(id)
            .map(|editor| editor.focus_handle(cx))
            .or_else(|| self.controls.get(id).cloned())
    }
    fn focus_next(&mut self, reverse: bool, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.order.iter().position(|id| {
            self.handle(id, cx)
                .is_some_and(|handle| handle.is_focused(window))
        });
        if let Some(index) = next_focus(self.order.len(), current, reverse) {
            if let Some(handle) = self.handle(&self.order[index], cx) {
                window.focus(&handle);
            }
            let id = &self.order[index];
            if let Some(parents) = self.scroll_paths.get(id) {
                for parent in parents {
                    if let Some(anchor) = self.anchors.get(parent) {
                        anchor.scroll_to(window, cx);
                    }
                }
            }
            if let Some(anchor) = self.anchors.get(id) {
                anchor.scroll_to(window, cx);
            }
        }
    }
    fn activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        let Some(id) = self
            .order
            .iter()
            .find(|id| {
                self.handle(id, cx)
                    .is_some_and(|handle| handle.is_focused(window))
            })
            .cloned()
        else {
            return;
        };
        let Some(model) = self.model.clone() else {
            return;
        };
        let Some(widget) = find(&model.root, &id) else {
            return;
        };
        match widget {
            Widget::Button { .. } => self.emit(
                UiEvent {
                    widget: id,
                    kind: UiEventKind::Click,
                },
                cx,
            ),
            Widget::TextInput { .. } => self.emit(
                UiEvent {
                    widget: id,
                    kind: UiEventKind::Submit,
                },
                cx,
            ),
            Widget::Checkbox { .. } => {
                let checked = matches!(self.inputs.get(&id), Some(InputValue::Bool(true)));
                self.change(&id, InputValue::Bool(!checked), cx);
            }
            Widget::Select { options, .. } => {
                let current = self.inputs.get(&id);
                let index = options.iter().position(|option| matches!(current, Some(InputValue::Text(value)) if value == &option.value)).map_or(0, |index| (index + 1) % options.len());
                if let Some(option) = options.get(index) {
                    self.change(&id, InputValue::Text(option.value.clone()), cx);
                }
            }
            _ => {}
        }
    }

    fn select_key(&mut self, reverse: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(id) = self
            .order
            .iter()
            .find(|id| {
                self.controls
                    .get(*id)
                    .is_some_and(|handle| handle.is_focused(window))
            })
            .cloned()
        else {
            return false;
        };
        let Some(model) = self.model.clone() else {
            return false;
        };
        let Some(Widget::Select { options, .. }) = find(&model.root, &id) else {
            return false;
        };
        if options.is_empty() {
            return false;
        }
        let current = options.iter().position(|option| matches!(self.inputs.get(&id), Some(InputValue::Text(value)) if value == &option.value));
        let index = next_focus(options.len(), current, reverse).unwrap_or(0);
        self.change(&id, InputValue::Text(options[index].value.clone()), cx);
        true
    }

    fn render_widget(&self, widget: &Widget, cx: &Context<Self>) -> AnyElement {
        let id = widget.id().to_owned();
        let weak = cx.entity().downgrade();
        let element = match widget {
            Widget::Row { children, gap, .. } | Widget::Column { children, gap, .. } => div()
                .id(SharedString::from(id))
                .flex()
                .when(matches!(widget, Widget::Column { .. }), |row| {
                    row.flex_col()
                })
                .when(matches!(widget, Widget::Row { .. }), |row| {
                    row.flex_wrap().items_end()
                })
                .gap(px(f32::from(*gap)))
                .min_w_0()
                .children(children.iter().map(|child| self.render_widget(child, cx)))
                .into_any_element(),
            Widget::Scroll { child, height, .. } => div()
                .id(SharedString::from(id.clone()))
                .anchor_scroll(self.anchors.get(&id).cloned())
                .h(px(f32::from(*height)))
                .flex_none()
                .w_full()
                .overflow_scroll()
                .when_some(self.scrolls.get(&id), |view, scroll| {
                    view.track_scroll(scroll)
                })
                .child(self.render_widget(child, cx))
                .into_any_element(),
            Widget::Space { size, .. } => div()
                .size(px(f32::from(*size)))
                .flex_none()
                .into_any_element(),
            Widget::Label { text, .. } => div()
                .child(SharedString::from(text.clone()))
                .into_any_element(),
            Widget::Markdown { .. } => self
                .markdown
                .get(&id)
                .map(|text| text.render())
                .unwrap_or_else(|| div().into_any_element()),
            Widget::Button { text, disabled, .. } => {
                let callback_id = id.clone();
                let control = controls::button(
                    SharedString::from(format!("{id}-button")),
                    text,
                    false,
                    None,
                    *disabled || self.is_busy(),
                    Rc::new(move |_, cx| {
                        weak.update(cx, |view, cx| {
                            view.emit(
                                UiEvent {
                                    widget: callback_id.clone(),
                                    kind: UiEventKind::Click,
                                },
                                cx,
                            )
                        })
                        .ok();
                    }),
                );
                self.focusable_control(&id, *disabled, control)
            }
            Widget::TextInput {
                label,
                placeholder,
                disabled,
                ..
            } => {
                let content = if *disabled {
                    let value = match self.inputs.get(&id) {
                        Some(InputValue::Text(value)) => value.clone(),
                        _ => String::new(),
                    };
                    div()
                        .px_2()
                        .py_1()
                        .border_1()
                        .border_color(theme::border())
                        .opacity(0.6)
                        .child(SharedString::from(value))
                        .into_any_element()
                } else if let Some(editor) = self.editors.get(&id) {
                    let submit_id = id.clone();
                    let field = controls::text_field(
                        editor,
                        px(260.),
                        false,
                        false,
                        Rc::new(move |_, _, cx| {
                            weak.update(cx, |view, cx| {
                                view.emit(
                                    UiEvent {
                                        widget: submit_id.clone(),
                                        kind: UiEventKind::Submit,
                                    },
                                    cx,
                                )
                            })
                            .ok();
                        }),
                    );
                    div()
                        .key_context("ExtensionInput")
                        .relative()
                        .child(field)
                        .when(
                            editor.read(cx).buffer().is_empty() && !placeholder.is_empty(),
                            |field| {
                                let focus = editor.focus_handle(cx);
                                field.child(
                                    div()
                                        .absolute()
                                        .left(px(6.))
                                        .top(px(2.))
                                        .text_color(theme::muted())
                                        .child(SharedString::from(placeholder.clone()))
                                        .on_mouse_down(
                                            gpui::MouseButton::Left,
                                            move |_, window, cx| {
                                                window.focus(&focus);
                                                cx.stop_propagation();
                                            },
                                        ),
                                )
                            },
                        )
                        .into_any_element()
                } else {
                    div().into_any_element()
                };
                div()
                    .id(SharedString::from(format!("{id}-input")))
                    .anchor_scroll(self.anchors.get(&id).cloned())
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(SharedString::from(label.clone()))
                    .child(content)
                    .into_any_element()
            }
            Widget::Checkbox {
                label, disabled, ..
            } => {
                let checked = matches!(self.inputs.get(&id), Some(InputValue::Bool(true)));
                let callback_id = id.clone();
                let control = controls::checkbox(
                    SharedString::from(format!("{id}-checkbox")),
                    checked,
                    label,
                    *disabled || self.is_busy(),
                    Rc::new(move |checked, _, cx| {
                        weak.update(cx, |view, cx| {
                            view.change(&callback_id, InputValue::Bool(checked), cx)
                        })
                        .ok();
                    }),
                );
                self.focusable_control(&id, *disabled, control)
            }
            Widget::Select {
                label,
                options,
                disabled,
                ..
            } => {
                let selected = options.iter().position(|option| matches!(self.inputs.get(&id), Some(InputValue::Text(value)) if value == &option.value));
                let values: Vec<String> =
                    options.iter().map(|option| option.value.clone()).collect();
                let callback_id = id.clone();
                let control = controls::dropdown(
                    format!("extension-{}-{id}", cx.entity().entity_id()).into(),
                    options
                        .iter()
                        .map(|option| SharedString::from(option.label.clone()))
                        .collect(),
                    selected,
                    *disabled || self.is_busy(),
                    px(260.),
                    Rc::new(move |index, _, cx| {
                        if let Some(value) = values.get(index) {
                            weak.update(cx, |view, cx| {
                                view.change(&callback_id, InputValue::Text(value.clone()), cx)
                            })
                            .ok();
                        }
                    }),
                );
                self.focusable_control(
                    &id,
                    *disabled,
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(SharedString::from(label.clone()))
                        .child(control)
                        .into_any_element(),
                )
            }
            Widget::Progress { label, value, .. } => div()
                .flex()
                .flex_col()
                .gap_1()
                .child(SharedString::from(format!(
                    "{label} ({:.0}%)",
                    value * 100.
                )))
                .child(controls::progress(*value as f32, theme::accent(), px(260.)))
                .into_any_element(),
            Widget::Table { columns, rows, .. } => {
                let row = |cells: &[String], header: bool| {
                    div().flex().children(cells.iter().map(|cell| {
                        div()
                            .w(px(150.))
                            .flex_none()
                            .px_2()
                            .py_1()
                            .border_b_1()
                            .border_color(theme::border())
                            .when(header, |cell| {
                                cell.font_weight(gpui::FontWeight::BOLD).bg(theme::panel())
                            })
                            .child(SharedString::from(cell.clone()))
                    }))
                };
                div()
                    .id(SharedString::from(id))
                    .max_h(px(320.))
                    .overflow_scroll()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(row(columns, true))
                            .children(rows.iter().map(|cells| row(cells, false))),
                    )
                    .into_any_element()
            }
            Widget::Chart {
                title,
                height,
                chart,
                ..
            } => self.render_chart(&id, title, *height, chart, cx),
        };
        let label = match widget {
            Widget::Label { text, .. }
            | Widget::Markdown { text, .. }
            | Widget::Button { text, .. } => text.clone(),
            Widget::TextInput { label, .. }
            | Widget::Checkbox { label, .. }
            | Widget::Select { label, .. }
            | Widget::Progress { label, .. } => label.clone(),
            Widget::Chart { title, .. } => title.clone(),
            Widget::Table { .. } => "Table".into(),
            Widget::Row { .. } => "Row".into(),
            Widget::Column { .. } => "Column".into(),
            Widget::Scroll { .. } => "Scroll".into(),
            Widget::Space { .. } => "Space".into(),
        };
        element
            .pin(format!("widget:{}", widget.id()))
            .label(label)
            .into_any_element()
    }

    fn focusable_control(&self, id: &str, disabled: bool, child: AnyElement) -> AnyElement {
        div()
            .id(SharedString::from(format!("{id}-focus")))
            .anchor_scroll(self.anchors.get(id).cloned())
            .key_context("ExtensionControl")
            .when(disabled || self.is_busy(), |control| control.opacity(0.6))
            .when(!disabled, |control| {
                if let Some(handle) = self.controls.get(id) {
                    let focus = handle.clone();
                    control
                        .track_focus(handle)
                        .on_mouse_down(gpui::MouseButton::Left, move |_, window, _| {
                            window.focus(&focus)
                        })
                        .border_1()
                        .border_color(theme::border())
                        .focus(|style| style.border_color(theme::accent()))
                } else {
                    control
                }
            })
            .child(child)
            .into_any_element()
    }
}

impl Render for ExtensionView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = self
            .model
            .as_ref()
            .map(|model| self.render_widget(&model.root, cx));
        div()
            .id("extension-view")
            .key_context("ExtensionPanel")
            .track_focus(&self.focus)
            .size_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .bg(theme::background())
            .text_color(theme::text())
            .font_family(theme::UI_FONT)
            .text_sm()
            .capture_key_down(cx.listener(|view, event: &gpui::KeyDownEvent, window, cx| {
                // Capture Tab before Editor's indentation binding; retain native input editing otherwise.
                let modifiers = &event.keystroke.modifiers;
                if modifiers.control || modifiers.alt || modifiers.platform {
                    return;
                }
                if event.keystroke.key == "tab" {
                    view.focus_next(modifiers.shift, window, cx);
                    cx.stop_propagation();
                    return;
                }
                let key = event.keystroke.key.as_str();
                if (!modifiers.shift || matches!(key, "+" | "=")) && view.chart_key(key, window, cx)
                {
                    cx.stop_propagation();
                    return;
                }
                if !modifiers.shift
                    && matches!(key, "up" | "down" | "left" | "right")
                    && view.select_key(matches!(key, "up" | "left"), window, cx)
                {
                    cx.stop_propagation();
                }
            }))
            .on_action(
                cx.listener(|view, _: &NextControl, window, cx| view.focus_next(false, window, cx)),
            )
            .on_action(cx.listener(|view, _: &PreviousControl, window, cx| {
                view.focus_next(true, window, cx)
            }))
            .on_action(
                cx.listener(|view, _: &ActivateControl, window, cx| view.activate(window, cx)),
            )
            .on_action(cx.listener(|view, _: &SubmitControl, window, cx| view.activate(window, cx)))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .child(SharedString::from(self.title.clone()))
                    .when(self.is_busy(), |row| {
                        row.child(
                            div()
                                .text_color(theme::muted())
                                .child("Running…")
                                .pin("busy")
                                .label("Running extension"),
                        )
                    }),
            )
            .when_some(self.error.clone(), |view, error| {
                let error: SharedString = error.into();
                view.child(
                    div()
                        .p_2()
                        .border_1()
                        .border_color(theme::git_deleted())
                        .text_color(theme::git_deleted())
                        .child(error.clone())
                        .pin("error")
                        .label(error),
                )
            })
            .when(self.model.is_none() && self.error.is_none(), |view| {
                view.child(
                    div()
                        .text_color(theme::muted())
                        .child("Loading extension panel…"),
                )
            })
            .child(
                div()
                    .id("extension-content")
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_scroll()
                    .track_scroll(&self.scroll)
                    .children(content),
            )
            .pin(format!("extension:{}:{}", self.extension, self.panel))
            .label(self.title.clone())
    }
}

fn visit(widget: &Widget, callback: &mut impl FnMut(&Widget)) {
    callback(widget);
    match widget {
        Widget::Row { children, .. } | Widget::Column { children, .. } => {
            for child in children {
                visit(child, callback);
            }
        }
        Widget::Scroll { child, .. } => visit(child, callback),
        _ => {}
    }
}
fn find<'a>(widget: &'a Widget, id: &str) -> Option<&'a Widget> {
    if widget.id() == id {
        return Some(widget);
    }
    match widget {
        Widget::Row { children, .. } | Widget::Column { children, .. } => {
            children.iter().find_map(|child| find(child, id))
        }
        Widget::Scroll { child, .. } => find(child, id),
        _ => None,
    }
}
fn reconcile(
    previous: &BTreeMap<String, InputValue>,
    current: &BTreeMap<String, InputValue>,
    mut incoming: BTreeMap<String, InputValue>,
    submitted: Option<&BTreeMap<String, InputValue>>,
) -> BTreeMap<String, InputValue> {
    for (id, value) in &mut incoming {
        let baseline = submitted
            .and_then(|inputs| inputs.get(id))
            .or_else(|| previous.get(id));
        if let Some(local) = current.get(id)
            && Some(local) != baseline
            && std::mem::discriminant(local) == std::mem::discriminant(value)
        {
            *value = local.clone();
        }
    }
    incoming
}
fn next_focus(length: usize, current: Option<usize>, reverse: bool) -> Option<usize> {
    if length == 0 {
        return None;
    }
    Some(match (current, reverse) {
        (Some(index), true) => (index + length - 1) % length,
        (Some(index), false) => (index + 1) % length,
        (None, true) => length - 1,
        (None, false) => 0,
    })
}

fn prepare_scrolling(
    widget: &Widget,
    handle: &ScrollHandle,
    parents: &[String],
    scrolls: &HashMap<String, ScrollHandle>,
    anchors: &mut HashMap<String, ScrollAnchor>,
    paths: &mut HashMap<String, Vec<String>>,
) {
    anchors.insert(
        widget.id().to_owned(),
        ScrollAnchor::for_handle(handle.clone()),
    );
    paths.insert(widget.id().to_owned(), parents.to_vec());
    match widget {
        Widget::Row { children, .. } | Widget::Column { children, .. } => {
            for child in children {
                prepare_scrolling(child, handle, parents, scrolls, anchors, paths);
            }
        }
        Widget::Scroll { id, child, .. } => {
            if let Some(scroll) = scrolls.get(id) {
                let mut parents = parents.to_vec();
                parents.push(id.clone());
                prepare_scrolling(child, scroll, &parents, scrolls, anchors, paths);
            }
        }
        _ => {}
    }
}
