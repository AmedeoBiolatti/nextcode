//! The Debug panel (right dock): a toolbar, then collapsible sections for
//! threads and the call stack, variables (lazily expanded), watch
//! expressions, and breakpoints with exception filters.

use std::{collections::HashSet, path::PathBuf};

use gpui::{
    Action, AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    KeyBinding, SharedString, Subscription, Window, div, prelude::*, px,
};

use super::{
    AddWatch, CancelSetVariable, ConfirmSetVariable, Continue, Pause, Restart, StartDebugging,
    StepInto, StepOut, StepOver, Stop,
    session::{Children, DebugSession, SessionState, WatchValue},
    store::DebugStore,
};
use crate::{editor::Editor, selectable::selectable, theme};

const ROW_HEIGHT: f32 = 22.;
const INDENT: f32 = 14.;

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", AddWatch, Some("WatchInput > Editor")),
        KeyBinding::new("enter", ConfirmSetVariable, Some("VariableInput > Editor")),
        KeyBinding::new("escape", CancelSetVariable, Some("VariableInput")),
    ]);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Section {
    CallStack,
    Variables,
    Watch,
    Breakpoints,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Self::CallStack => "Call Stack",
            Self::Variables => "Variables",
            Self::Watch => "Watch",
            Self::Breakpoints => "Breakpoints",
        }
    }
}

pub enum DebugPanelEvent {
    /// Show a breakpoint's line.
    Open { path: PathBuf, line: usize },
}

pub struct DebugPanel {
    focus_handle: FocusHandle,
    store: Entity<DebugStore>,
    session: Option<Entity<DebugSession>>,
    watch_input: Entity<Editor>,
    collapsed: HashSet<Section>,
    /// A variable whose value is being edited (`setVariable`).
    editing: Option<VariableEdit>,
    _session_subscription: Option<Subscription>,
    _store_subscription: Subscription,
}

struct VariableEdit {
    /// The container's reference and the variable's name.
    parent: i64,
    name: String,
    input: Entity<Editor>,
}

impl EventEmitter<DebugPanelEvent> for DebugPanel {}

/// One rendered row of a variables or watch tree.
struct VariableRow {
    depth: usize,
    /// Expansion key (see `DebugSession::child_path`).
    path: String,
    name: String,
    value: String,
    type_name: Option<String>,
    reference: i64,
    /// The container's reference (for `setVariable`).
    parent: i64,
    indexed: Option<u32>,
    expanded: bool,
    kind: RowKind,
}

enum RowKind {
    Variable,
    Loading,
    Message(Hsla),
    /// "Load more" of a long list.
    More(i64),
}

impl DebugPanel {
    pub fn new(store: Entity<DebugStore>, cx: &mut Context<Self>) -> Self {
        let store_subscription = cx.observe(&store, |_, _, cx| cx.notify());
        Self {
            focus_handle: cx.focus_handle(),
            store,
            session: None,
            watch_input: cx.new(Editor::single_line),
            collapsed: HashSet::new(),
            editing: None,
            _session_subscription: None,
            _store_subscription: store_subscription,
        }
    }

    pub fn set_session(&mut self, session: Option<Entity<DebugSession>>, cx: &mut Context<Self>) {
        self._session_subscription = session
            .as_ref()
            .map(|session| cx.observe(session, |_, _, cx| cx.notify()));
        self.session = session;
        cx.notify();
    }

    fn add_watch(&mut self, _: &AddWatch, _: &mut Window, cx: &mut Context<Self>) {
        let expression = self.watch_input.read(cx).text();
        if expression.trim().is_empty() {
            return;
        }
        self.watch_input
            .update(cx, |input, cx| input.set_text("", cx));
        self.store
            .update(cx, |store, cx| store.add_watch(expression, cx));
    }

    /// Double-click on a leaf value: edit it in place.
    fn edit_variable(
        &mut self,
        parent: i64,
        name: String,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let supported = self
            .session
            .as_ref()
            .is_some_and(|session| session.read(cx).capabilities().supports_set_variable);
        if !supported {
            return;
        }
        let input = cx.new(|cx| {
            let mut input = Editor::single_line(cx);
            input.set_text(&value, cx);
            input.select_all_text(cx);
            input
        });
        window.focus(&input.focus_handle(cx));
        self.editing = Some(VariableEdit {
            parent,
            name,
            input,
        });
        cx.notify();
    }

    fn confirm_set_variable(
        &mut self,
        _: &ConfirmSetVariable,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(edit) = self.editing.take() else {
            return;
        };
        let value = edit.input.read(cx).text();
        if let Some(session) = &self.session {
            session.update(cx, |session, cx| {
                session.set_variable(edit.parent, edit.name, value, cx)
            });
        }
        cx.notify();
    }

    fn cancel_set_variable(
        &mut self,
        _: &CancelSetVariable,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editing.take().is_some() {
            cx.notify();
        }
    }

    fn toggle_section(&mut self, section: Section, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&section) {
            self.collapsed.insert(section);
        }
        cx.notify();
    }

    fn render_toolbar(&self, cx: &App) -> impl IntoElement + use<> {
        let state = self.session.as_ref().map(|s| s.read(cx).state());
        let active = state.is_some_and(|state| state != SessionState::Ended);
        let paused = state == Some(SessionState::Stopped);
        let running = state == Some(SessionState::Running);
        let label: SharedString = match self.session.as_ref().map(|s| s.read(cx)) {
            Some(session) if session.is_active() => match session.stop_reason() {
                Some(reason) if paused => format!("Paused ({reason})").into(),
                _ => capitalize(session.state().label()).into(),
            },
            Some(session) => match session.error() {
                Some(_) => "Failed".into(),
                None => "Ended".into(),
            },
            None => "Not started".into(),
        };
        div()
            .flex_none()
            .h(px(30.))
            .px_1()
            .flex()
            .items_center()
            .gap_1()
            .border_b_1()
            .border_color(theme::border())
            .child(if paused || !active {
                tool_button(
                    "debug-continue",
                    "▶",
                    if paused {
                        "Continue (F5)"
                    } else {
                        "Start Debugging (F5)"
                    },
                    true,
                    if paused {
                        Continue.boxed_clone()
                    } else {
                        StartDebugging.boxed_clone()
                    },
                )
            } else {
                tool_button(
                    "debug-pause",
                    "‖",
                    "Pause (F6)",
                    running,
                    Pause.boxed_clone(),
                )
            })
            .child(tool_button(
                "debug-over",
                "↷",
                "Step Over (F10)",
                paused,
                StepOver.boxed_clone(),
            ))
            .child(tool_button(
                "debug-into",
                "↓",
                "Step Into (F11)",
                paused,
                StepInto.boxed_clone(),
            ))
            .child(tool_button(
                "debug-out",
                "↑",
                "Step Out (Shift+F11)",
                paused,
                StepOut.boxed_clone(),
            ))
            .child(tool_button(
                "debug-restart",
                "↻",
                "Restart (Ctrl+Shift+F5)",
                active,
                Restart.boxed_clone(),
            ))
            .child(tool_button(
                "debug-stop",
                "■",
                "Stop (Shift+F5)",
                active,
                Stop.boxed_clone(),
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .pl_1()
                    .text_xs()
                    .text_color(if paused {
                        theme::warning()
                    } else {
                        theme::muted()
                    })
                    .child(selectable("status", label)),
            )
    }

    fn section_header(
        &self,
        section: Section,
        extra: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let collapsed = self.collapsed.contains(&section);
        div()
            .id(SharedString::from(format!("section-{}", section.title())))
            .flex_none()
            .h(px(ROW_HEIGHT + 2.))
            .px_1()
            .flex()
            .items_center()
            .gap_1()
            .bg(theme::panel())
            .border_t_1()
            .border_color(theme::border())
            .cursor_pointer()
            .text_xs()
            .text_color(theme::muted())
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_section(section, cx)))
            .child(div().w(px(12.)).child(if collapsed { "▸" } else { "▾" }))
            .child(
                div()
                    .flex_1()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(selectable("title", section.title().to_uppercase())),
            )
            .children(extra)
    }

    fn render_call_stack(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(session_entity) = self.session.clone() else {
            return vec![message("stack-message", "Not debugging").into_any_element()];
        };
        let session = session_entity.read(cx);
        if session.state() != SessionState::Stopped {
            let text = match session.state() {
                SessionState::Running => "Running",
                SessionState::Starting => "Starting…",
                _ => "Not paused",
            };
            return vec![message("stack-message", text).into_any_element()];
        }
        let mut rows = Vec::new();
        let threads = session.threads().to_vec();
        let current = session.thread_id();
        let several = threads.len() > 1;
        let frames: Vec<_> = session.frames().to_vec();
        let selected = session.selected_frame();
        let locations: Vec<Option<(PathBuf, usize)>> = (0..frames.len())
            .map(|index| session.frame_location(index))
            .collect();
        for thread in threads {
            let is_current = Some(thread.id) == current;
            if several {
                let session = session_entity.clone();
                let id = thread.id;
                rows.push(
                    row(SharedString::from(format!("thread-{id}")), 0)
                        .text_color(if is_current {
                            theme::text()
                        } else {
                            theme::muted()
                        })
                        .on_click(move |_, _, cx| {
                            session.update(cx, |session, cx| session.select_thread(id, cx))
                        })
                        .child(if is_current { "▾" } else { "▸" })
                        .child(selectable("name", thread.name.clone()))
                        .into_any_element(),
                );
            }
            if !is_current {
                continue;
            }
            let depth = usize::from(several);
            for (index, frame) in frames.iter().enumerate() {
                let session = session_entity.clone();
                let location = locations[index]
                    .as_ref()
                    .map(|(path, line)| {
                        let name = path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        format!("{name}:{}", line + 1)
                    })
                    .unwrap_or_default();
                let has_source = locations[index].is_some();
                rows.push(
                    row(SharedString::from(format!("frame-{index}")), depth)
                        .when(index == selected, |row| row.bg(theme::active_row()))
                        .on_click(move |_, _, cx| {
                            session.update(cx, |session, cx| session.select_frame(index, cx))
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(if has_source {
                                    theme::text()
                                } else {
                                    theme::muted()
                                })
                                .child(selectable("name", frame.name.clone())),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(theme::muted())
                                .child(selectable("location", location)),
                        )
                        .into_any_element(),
                );
            }
            if frames.is_empty() {
                rows.push(message("stack-loading", "Loading…").into_any_element());
            }
        }
        if rows.is_empty() {
            rows.push(message("stack-message", "No threads").into_any_element());
        }
        rows
    }

    /// The rows of `reference`'s children (recursively, where expanded).
    fn variable_rows(
        session: &DebugSession,
        reference: i64,
        path: &str,
        depth: usize,
        rows: &mut Vec<VariableRow>,
    ) {
        match session.children(reference) {
            None | Some(Children::Loading) => rows.push(VariableRow {
                depth,
                path: String::new(),
                name: String::new(),
                value: String::new(),
                type_name: None,
                reference: 0,
                parent: reference,
                indexed: None,
                expanded: false,
                kind: RowKind::Loading,
            }),
            Some(Children::Failed(err)) => rows.push(VariableRow {
                depth,
                path: String::new(),
                name: String::new(),
                value: err.clone(),
                type_name: None,
                reference: 0,
                parent: reference,
                indexed: None,
                expanded: false,
                kind: RowKind::Message(theme::error()),
            }),
            Some(Children::Loaded {
                variables,
                next_indexed,
                total_indexed,
            }) => {
                for variable in variables {
                    let child = DebugSession::child_path(path, &variable.name);
                    let expanded = variable.variables_reference > 0 && session.is_expanded(&child);
                    rows.push(VariableRow {
                        depth,
                        path: child.clone(),
                        name: variable.name.clone(),
                        value: variable.value.clone(),
                        type_name: variable.type_name.clone(),
                        reference: variable.variables_reference,
                        parent: reference,
                        indexed: variable.indexed_variables,
                        expanded,
                        kind: RowKind::Variable,
                    });
                    if expanded {
                        Self::variable_rows(
                            session,
                            variable.variables_reference,
                            &child,
                            depth + 1,
                            rows,
                        );
                    }
                }
                if let Some(next) = next_indexed {
                    rows.push(VariableRow {
                        depth,
                        path: String::new(),
                        name: String::new(),
                        value: format!("Load more ({next} of {total_indexed} shown)"),
                        type_name: None,
                        reference: 0,
                        parent: reference,
                        indexed: None,
                        expanded: false,
                        kind: RowKind::More(reference),
                    });
                }
                if variables.is_empty() && next_indexed.is_none() {
                    rows.push(VariableRow {
                        depth,
                        path: String::new(),
                        name: String::new(),
                        value: "(empty)".into(),
                        type_name: None,
                        reference: 0,
                        parent: reference,
                        indexed: None,
                        expanded: false,
                        kind: RowKind::Message(theme::muted()),
                    });
                }
            }
        }
    }

    fn render_variable_row(
        &self,
        session: &Entity<DebugSession>,
        index: usize,
        row: VariableRow,
        prefix: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = SharedString::from(format!("{prefix}-{index}"));
        let element = div()
            .id(id)
            .h(px(ROW_HEIGHT))
            .w_full()
            .pl(px(6. + INDENT * row.depth as f32))
            .pr_2()
            .flex()
            .items_center()
            .gap_1()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_sm();
        match row.kind {
            RowKind::Loading => element
                .child(
                    div()
                        .text_color(theme::muted())
                        .child(selectable("loading", "Loading…")),
                )
                .into_any_element(),
            RowKind::Message(color) => element
                .child(
                    div()
                        .truncate()
                        .text_color(color)
                        .child(selectable("message", row.value)),
                )
                .into_any_element(),
            RowKind::More(reference) => {
                let session = session.clone();
                element
                    .cursor_pointer()
                    .text_color(theme::accent_text())
                    .hover(|style| style.bg(theme::hover_row()))
                    .on_click(move |_, _, cx| {
                        session.update(cx, |session, cx| session.load_more(reference, cx))
                    })
                    .child(row.value)
                    .into_any_element()
            }
            RowKind::Variable => {
                let expandable = row.reference > 0;
                let session = session.clone();
                let (path, reference, indexed) = (row.path.clone(), row.reference, row.indexed);
                element
                    .when(expandable, |element| {
                        element
                            .cursor_pointer()
                            .hover(|style| style.bg(theme::hover_row()))
                            .on_click(move |_, _, cx| {
                                let path = path.clone();
                                session.update(cx, |session, cx| {
                                    session.toggle_expanded(path, reference, indexed, cx)
                                })
                            })
                    })
                    .child(
                        div()
                            .w(px(12.))
                            .flex_none()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(match (expandable, row.expanded) {
                                (false, _) => "",
                                (true, true) => "▾",
                                (true, false) => "▸",
                            }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme::accent_text())
                            .child(selectable("name", row.name.clone())),
                    )
                    .child(div().flex_none().text_color(theme::muted()).child("="))
                    .child(match &self.editing {
                        Some(edit) if edit.parent == row.parent && edit.name == row.name => div()
                            .key_context("VariableInput")
                            .flex_1()
                            .min_w_0()
                            .px_1()
                            .border_1()
                            .border_color(theme::accent())
                            .rounded_sm()
                            .bg(theme::background())
                            .font_family(theme::MONO_FONT)
                            .child(edit.input.clone())
                            .into_any_element(),
                        _ => {
                            let (parent, name, value) =
                                (row.parent, row.name.clone(), row.value.clone());
                            div()
                                .id("value")
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(theme::MONO_FONT)
                                .text_color(theme::text())
                                .when(!expandable, |value_div| {
                                    value_div.on_click(cx.listener(
                                        move |this, event: &gpui::ClickEvent, window, cx| {
                                            if event.click_count() == 2 {
                                                this.edit_variable(
                                                    parent,
                                                    name.clone(),
                                                    value.clone(),
                                                    window,
                                                    cx,
                                                );
                                            }
                                        },
                                    ))
                                })
                                .child({
                                    // A double-click edits the value.
                                    let text = selectable("text", row.value);
                                    if expandable {
                                        text
                                    } else {
                                        text.double_click_through()
                                    }
                                })
                                .into_any_element()
                        }
                    })
                    .children(row.type_name.filter(|t| !t.is_empty()).map(|type_name| {
                        div()
                            .flex_none()
                            .max_w(px(100.))
                            .truncate()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(selectable("type", type_name))
                    }))
                    .into_any_element()
            }
        }
    }

    fn render_variables(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(session_entity) = self.session.clone() else {
            return vec![message("variables-message", "Not debugging").into_any_element()];
        };
        let session = session_entity.read(cx);
        if session.state() != SessionState::Stopped {
            return vec![
                message("variables-message", "Variables show while paused").into_any_element(),
            ];
        }
        if session.scopes().is_empty() {
            return vec![message("variables-loading", "Loading…").into_any_element()];
        }
        let mut rows = Vec::new();
        for scope in session.scopes() {
            let expanded = session.is_expanded(&scope.name);
            rows.push(VariableRow {
                depth: 0,
                path: scope.name.clone(),
                name: scope.name.clone(),
                value: String::new(),
                type_name: None,
                reference: scope.variables_reference,
                parent: 0,
                indexed: scope.indexed_variables,
                expanded,
                kind: RowKind::Variable,
            });
            if expanded {
                Self::variable_rows(
                    session,
                    scope.variables_reference,
                    &scope.name,
                    1,
                    &mut rows,
                );
            }
        }
        rows.into_iter()
            .enumerate()
            .map(|(index, row)| {
                if row.depth == 0 && matches!(row.kind, RowKind::Variable) {
                    // Scope rows: name only.
                    let session = session_entity.clone();
                    let (path, reference, indexed) = (row.path.clone(), row.reference, row.indexed);
                    div()
                        .id(SharedString::from(format!("scope-{index}")))
                        .h(px(ROW_HEIGHT))
                        .pl(px(6.))
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_sm()
                        .cursor_pointer()
                        .hover(|style| style.bg(theme::hover_row()))
                        .on_click(move |_, _, cx| {
                            let path = path.clone();
                            session.update(cx, |session, cx| {
                                session.toggle_expanded(path, reference, indexed, cx)
                            })
                        })
                        .child(
                            div()
                                .w(px(12.))
                                .text_xs()
                                .text_color(theme::muted())
                                .child(if row.expanded { "▾" } else { "▸" }),
                        )
                        .child(
                            div()
                                .text_color(theme::text())
                                .child(selectable("name", row.name)),
                        )
                        .into_any_element()
                } else {
                    self.render_variable_row(&session_entity, index, row, "var", cx)
                }
            })
            .collect()
    }

    fn render_watch(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let watches = self.store.read(cx).watches().to_vec();
        let session = self
            .session
            .clone()
            .filter(|session| session.read(cx).state() == SessionState::Stopped);
        let mut elements = Vec::new();
        for (index, expression) in watches.iter().enumerate() {
            let (value, color, reference) = match session
                .as_ref()
                .and_then(|s| s.read(cx).watch(expression).cloned())
            {
                Some(WatchValue::Value(response)) => {
                    (response.result, theme::text(), response.variables_reference)
                }
                Some(WatchValue::Error(err)) => (err, theme::error(), 0),
                Some(WatchValue::Pending) => ("…".to_owned(), theme::muted(), 0),
                None => ("not available".to_owned(), theme::muted(), 0),
            };
            let path = format!("watch:{expression}");
            let expanded = reference > 0
                && session
                    .as_ref()
                    .is_some_and(|s| s.read(cx).is_expanded(&path));
            let store = self.store.clone();
            let toggle_session = session.clone();
            let toggle_path = path.clone();
            elements.push(
                div()
                    .id(SharedString::from(format!("watch-{index}")))
                    .group("watch-row")
                    .h(px(ROW_HEIGHT))
                    .pl(px(6.))
                    .pr_1()
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_sm()
                    .whitespace_nowrap()
                    .hover(|style| style.bg(theme::hover_row()))
                    .when(reference > 0, |row| {
                        row.cursor_pointer().on_click(move |_, _, cx| {
                            if let Some(session) = &toggle_session {
                                let path = toggle_path.clone();
                                session.update(cx, |session, cx| {
                                    session.toggle_expanded(path, reference, None, cx)
                                });
                            }
                        })
                    })
                    .child(
                        div()
                            .w(px(12.))
                            .flex_none()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(match (reference > 0, expanded) {
                                (false, _) => "",
                                (true, true) => "▾",
                                (true, false) => "▸",
                            }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme::accent_text())
                            .child(selectable("expression", expression.clone())),
                    )
                    .child(div().flex_none().text_color(theme::muted()).child("="))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(theme::MONO_FONT)
                            .text_color(color)
                            .child(selectable("value", value)),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("watch-remove-{index}")))
                            .flex_none()
                            .px_1()
                            .text_color(theme::muted())
                            .cursor_pointer()
                            .hover(|style| style.text_color(theme::error()))
                            .on_click(move |_, _, cx| {
                                cx.stop_propagation();
                                store.update(cx, |store, cx| store.remove_watch(index, cx));
                            })
                            .child("×"),
                    )
                    .into_any_element(),
            );
            if expanded && let Some(session_entity) = &session {
                let mut rows = Vec::new();
                Self::variable_rows(session_entity.read(cx), reference, &path, 1, &mut rows);
                for (row_index, row) in rows.into_iter().enumerate() {
                    elements.push(self.render_variable_row(
                        session_entity,
                        row_index,
                        row,
                        &format!("watch-{index}"),
                        cx,
                    ));
                }
            }
        }
        elements.push(
            div()
                .key_context("WatchInput")
                .on_action(cx.listener(Self::add_watch))
                .mx_1()
                .my_1()
                .h(px(ROW_HEIGHT))
                .px_1()
                .border_1()
                .border_color(theme::border())
                .rounded_sm()
                .bg(theme::background())
                .font_family(theme::MONO_FONT)
                .text_sm()
                .child(self.watch_input.clone())
                .into_any_element(),
        );
        elements.push(message("watch-hint", "Enter adds an expression").into_any_element());
        elements
    }

    fn render_breakpoints(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut elements = Vec::new();
        let filters = self
            .session
            .as_ref()
            .map(|session| {
                session
                    .read(cx)
                    .capabilities()
                    .exception_breakpoint_filters
                    .clone()
            })
            .unwrap_or_default();
        let store = self.store.read(cx);
        for filter in &filters {
            let on = store.exception_filter(filter);
            let id = filter.filter.clone();
            let store_entity = self.store.clone();
            elements.push(
                row(SharedString::from(format!("filter-{id}")), 0)
                    .on_click(move |_, _, cx| {
                        store_entity
                            .update(cx, |store, cx| store.set_exception_filter(&id, !on, cx))
                    })
                    .child(checkbox(on))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(theme::text())
                            .child(selectable("label", filter.label.clone())),
                    )
                    .into_any_element(),
            );
        }
        let status = self.session.as_ref().map(|session| session.read(cx));
        let mut any = false;
        for (path, breakpoints) in store.files() {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            for breakpoint in breakpoints {
                any = true;
                let line = breakpoint.line;
                let verified = status.as_ref().and_then(|session| {
                    session
                        .breakpoint_status(path)
                        .iter()
                        .find(|status| status.line == line)
                        .map(|status| (status.verified, status.message.clone()))
                });
                let detail = [
                    breakpoint.condition.as_ref().map(|c| format!("if {c}")),
                    breakpoint
                        .hit_condition
                        .as_ref()
                        .map(|h| format!("hits {h}")),
                    breakpoint
                        .log_message
                        .as_ref()
                        .map(|m| format!("log \"{m}\"")),
                    verified.as_ref().and_then(|(ok, message)| {
                        (!ok).then(|| message.clone().unwrap_or_else(|| "unverified".into()))
                    }),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
                let enabled = breakpoint.enabled;
                let (toggle_store, remove_store) = (self.store.clone(), self.store.clone());
                let (toggle_path, remove_path, open_path) =
                    (path.clone(), path.clone(), path.clone());
                let key = format!("{}:{line}", path.display());
                elements.push(
                    row(SharedString::from(format!("bp-{key}")), 0)
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(DebugPanelEvent::Open {
                                path: open_path.clone(),
                                line,
                            })
                        }))
                        .child(
                            div()
                                .id(SharedString::from(format!("bp-enabled-{key}")))
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    toggle_store.update(cx, |store, cx| {
                                        store.set_enabled(&toggle_path, line, !enabled, cx)
                                    });
                                })
                                .child(checkbox(enabled)),
                        )
                        .child(dot(enabled, verified.as_ref().is_some_and(|(ok, _)| !ok)))
                        .child(
                            div()
                                .flex_none()
                                .text_color(theme::text())
                                .child(selectable("location", format!("{name}:{}", line + 1))),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(theme::muted())
                                .child(selectable("detail", detail)),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("bp-remove-{key}")))
                                .flex_none()
                                .px_1()
                                .text_color(theme::muted())
                                .hover(|style| style.text_color(theme::error()))
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    remove_store.update(cx, |store, cx| {
                                        store.remove(&remove_path, line, cx)
                                    });
                                })
                                .child("×"),
                        )
                        .into_any_element(),
                );
            }
        }
        if !any {
            elements.push(
                message(
                    "breakpoints-hint",
                    "Click the gutter or press F9 to add a breakpoint",
                )
                .into_any_element(),
            );
        }
        elements
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn message(id: &'static str, text: impl Into<SharedString>) -> impl IntoElement {
    div()
        .px_2()
        .py_1()
        .text_xs()
        .text_color(theme::muted())
        .child(selectable(id, text.into()))
}

fn row(id: SharedString, depth: usize) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(ROW_HEIGHT))
        .w_full()
        .pl(px(6. + INDENT * depth as f32))
        .pr_1()
        .flex()
        .items_center()
        .gap_1()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_sm()
        .cursor_pointer()
        .hover(|style| style.bg(theme::hover_row()))
}

fn checkbox(checked: bool) -> impl IntoElement {
    div()
        .flex_none()
        .size(px(12.))
        .border_1()
        .border_color(if checked {
            theme::accent()
        } else {
            theme::muted()
        })
        .rounded_sm()
        .flex()
        .items_center()
        .justify_center()
        .when(checked, |box_| {
            box_.child(div().size(px(6.)).rounded_sm().bg(theme::accent()))
        })
}

fn dot(enabled: bool, unverified: bool) -> impl IntoElement {
    let color = if enabled {
        theme::breakpoint()
    } else {
        theme::muted()
    };
    div()
        .flex_none()
        .size(px(9.))
        .rounded_full()
        .when(unverified || !enabled, |dot| {
            dot.border_1().border_color(color)
        })
        .when(!unverified && enabled, |dot| dot.bg(color))
}

/// A toolbar button that dispatches `action` (disabled ones are dimmed).
fn tool_button(
    id: &'static str,
    glyph: &'static str,
    title: &'static str,
    enabled: bool,
    action: Box<dyn Action>,
) -> impl IntoElement {
    div()
        .id(id)
        .flex_none()
        .w(px(24.))
        .h(px(22.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .text_sm()
        .text_color(if enabled {
            theme::text()
        } else {
            theme::muted().opacity(0.5)
        })
        .when(enabled, |button| {
            button
                .cursor_pointer()
                .hover(|style| style.bg(theme::hover_row()))
                .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
        })
        .tooltip(move |window, cx| crate::debugger::panel::Tooltip::view(title, window, cx))
        .child(glyph)
}

/// A one-line tooltip.
pub(crate) struct Tooltip(SharedString);

impl Tooltip {
    pub(crate) fn view(text: &'static str, _: &mut Window, cx: &mut App) -> gpui::AnyView {
        cx.new(|_| Self(text.into())).into()
    }
}

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .bg(theme::panel())
            .border_1()
            .border_color(theme::border())
            .rounded_md()
            .shadow_md()
            .text_xs()
            .text_color(theme::text())
            .child(self.0.clone())
    }
}

impl Focusable for DebugPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DebugPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = div()
            .id("debug-panel-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col();
        for section in [
            Section::CallStack,
            Section::Variables,
            Section::Watch,
            Section::Breakpoints,
        ] {
            let extra = (section == Section::Breakpoints).then(|| {
                div()
                    .id("remove-all-breakpoints")
                    .px_1()
                    .text_xs()
                    .text_color(theme::muted())
                    .hover(|style| style.text_color(theme::error()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.store.update(cx, |store, cx| store.remove_all(cx));
                    }))
                    .child("Remove all")
                    .into_any_element()
            });
            body = body.child(self.section_header(section, extra, cx));
            if self.collapsed.contains(&section) {
                continue;
            }
            let rows = match section {
                Section::CallStack => self.render_call_stack(cx),
                Section::Variables => self.render_variables(cx),
                Section::Watch => self.render_watch(cx),
                Section::Breakpoints => self.render_breakpoints(cx),
            };
            body = body.children(rows);
        }
        div()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::confirm_set_variable))
            .on_action(cx.listener(Self::cancel_set_variable))
            .size_full()
            .flex()
            .flex_col()
            .child(self.render_toolbar(cx))
            .child(body)
    }
}
