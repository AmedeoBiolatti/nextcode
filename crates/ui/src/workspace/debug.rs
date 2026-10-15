//! The workspace side of debugging: starting and stopping sessions (one
//! at a time: starting another stops the running one first), the debug
//! commands and keys, breakpoints between the store and the editors'
//! gutters, revealing the paused location, the Debug panel and console,
//! and the debug bar (state + toolbar) above the editors.

use std::rc::Rc;

use gpui::{KeyContext, WeakEntity};

use super::*;
use crate::{
    debugger::{
        Continue, DebugAdapterKind, DebugCurrentFile, DebugRequest, DebugRequestKind,
        DownloadCodeLldb, EditBreakpoint, Pause, RemoveAllBreakpoints, Restart, StartDebugging,
        StepInto, StepOut, StepOver, Stop, ToggleAllBreakpoints, ToggleBreakpoint,
        ToggleDebugConsole, ToggleDebugPanel, adapters,
        console::{self, DebugConsole},
        panel::{self, DebugPanel, DebugPanelEvent},
        session::{DebugSession, DebugSessionEvent, SessionState, Step},
        store::{Breakpoint, DebugStore, DebugStoreEvent},
    },
    editor::{DebugEvaluator, DebugLine, GutterBreakpoint},
};

pub fn bind_keys(cx: &mut App) {
    let ctx = Some("Workspace");
    cx.bind_keys([
        // VS Code's keys (JetBrains' Ctrl+F8 too); F8 stays Next Problem.
        KeyBinding::new("f5", StartDebugging, ctx),
        KeyBinding::new("f5", Continue, Some("Workspace && debug_paused")),
        KeyBinding::new("f6", Pause, ctx),
        KeyBinding::new("shift-f5", Stop, ctx),
        KeyBinding::new("secondary-shift-f5", Restart, ctx),
        KeyBinding::new("ctrl-f8", ToggleBreakpoint, ctx),
        KeyBinding::new("f9", ToggleBreakpoint, ctx),
        KeyBinding::new("f10", StepOver, ctx),
        KeyBinding::new("f11", StepInto, ctx),
        KeyBinding::new("shift-f11", StepOut, ctx),
    ]);
    panel::bind_keys(cx);
    console::bind_keys(cx);
}

/// The workspace's debugging state.
pub(super) struct DebugState {
    pub(super) panel: Entity<DebugPanel>,
    pub(super) console: Entity<DebugConsole>,
    pub(super) store: Entity<DebugStore>,
    session: Option<Entity<DebugSession>>,
    /// The last request, for Restart and the retry after a download.
    last_request: Option<DebugRequest>,
    /// Starts once the running session has ended.
    queued: Option<DebugRequest>,
    /// The file whose editors show the paused line.
    shown_line: Option<PathBuf>,
    download: Option<Task<()>>,
    /// The ending session's error is the missing adapter the download
    /// offer already explains.
    offered_download: bool,
    _session_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl DebugState {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let store = cx.new(|_| DebugStore::default());
        let panel = cx.new(|cx| DebugPanel::new(store.clone(), cx));
        let console = cx.new(DebugConsole::new);
        let subscriptions = vec![
            cx.subscribe_in(&store, window, |this, _, event, _, cx| {
                this.on_debug_store_event(event, cx)
            }),
            cx.subscribe_in(&panel, window, |this, _, event, window, cx| match event {
                DebugPanelEvent::Open { path, line } => {
                    this.record_position(cx);
                    this.forward.clear();
                    if let Some(editor) = this.open_editor(path, window, cx) {
                        editor.update(cx, |editor, cx| editor.go_to_line(*line, 0, cx));
                    }
                }
            }),
        ];
        Self {
            panel,
            console,
            store,
            session: None,
            last_request: None,
            queued: None,
            shown_line: None,
            download: None,
            offered_download: false,
            _session_subscription: None,
            _subscriptions: subscriptions,
        }
    }

    fn active_session(&self, cx: &App) -> Option<&Entity<DebugSession>> {
        self.session
            .as_ref()
            .filter(|session| session.read(cx).is_active())
    }

    fn state(&self, cx: &App) -> Option<SessionState> {
        self.active_session(cx)
            .map(|session| session.read(cx).state())
    }
}

impl Workspace {
    /// Starts debugging `request`, or explains in a notice why it cannot.
    /// A running session is stopped first.
    pub fn start_debugging(
        &mut self,
        request: DebugRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.debug.last_request = Some(request.clone());
        if let Some(session) = self.debug.active_session(cx).cloned() {
            self.debug.queued = Some(request);
            session.update(cx, |session, cx| session.stop(cx));
            return;
        }
        let settings = self.lsp_store.read(cx).settings().debug.clone();
        let project = self.project.clone();
        let store = self.debug.store.clone();
        let session = cx.new(|cx| DebugSession::start(project, request, store, settings, cx));
        self.set_debug_session(Some(session), window, cx);
        self.show_panel(PanelId::DebugConsole, cx);
        self.show_panel(PanelId::Debug, cx);
    }

    fn set_debug_session(
        &mut self,
        session: Option<Entity<DebugSession>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.debug._session_subscription = session.as_ref().map(|session| {
            cx.subscribe_in(session, window, |this, session, event, window, cx| {
                this.on_debug_session_event(session, event, window, cx)
            })
        });
        self.debug.session = session.clone();
        self.debug
            .panel
            .update(cx, |panel, cx| panel.set_session(session.clone(), cx));
        self.debug
            .console
            .update(cx, |console, cx| console.set_session(session, cx));
        self.refresh_all_breakpoint_gutters(cx);
        cx.notify();
    }

    fn on_debug_session_event(
        &mut self,
        session: &Entity<DebugSession>,
        event: &DebugSessionEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            DebugSessionEvent::Stopped => self.on_debug_stopped(session, window, cx),
            DebugSessionEvent::FrameSelected => self.reveal_debug_location(window, cx),
            DebugSessionEvent::Changed => {
                if session.read(cx).state() != SessionState::Stopped {
                    self.clear_debug_location(cx);
                }
                cx.notify();
            }
            DebugSessionEvent::BreakpointsVerified(path) => {
                self.refresh_breakpoint_gutters(path, cx)
            }
            DebugSessionEvent::Output { .. } => {}
            DebugSessionEvent::AdapterMissing { message, download } => {
                if *download {
                    // The offer says it all: no second copy in the notice.
                    self.debug.offered_download = true;
                    self.offer(OfferKind::DownloadCodeLldb, message, cx);
                }
            }
            DebugSessionEvent::Ended { error } => {
                self.clear_debug_location(cx);
                self.refresh_all_breakpoint_gutters(cx);
                if let Some(error) = error
                    && !std::mem::take(&mut self.debug.offered_download)
                {
                    self.notice = Some(format!("Debugging stopped: {error}").into());
                }
                if let Some(request) = self.debug.queued.take() {
                    self.start_debugging(request, window, cx);
                }
                cx.notify();
            }
        }
    }

    /// Paused: shows the first frame with a file (stepping into a library
    /// without sources selects the nearest frame that has one).
    fn on_debug_stopped(
        &mut self,
        session: &Entity<DebugSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let frame_count = session.read(cx).frames().len();
        let first_with_source = (0..frame_count.min(10)).find(|&index| {
            session
                .read(cx)
                .frame_location(index)
                .is_some_and(|(path, _)| {
                    matches!(self.project.kind(&path), Ok(Some(worktree::FileKind::File)))
                })
        });
        match first_with_source {
            Some(index) if index != session.read(cx).selected_frame() => {
                session.update(cx, |session, cx| session.select_frame(index, cx));
            }
            _ => self.reveal_debug_location(window, cx),
        }
        let evaluator = debug_evaluator(session.downgrade());
        for editor in self
            .all_tabs()
            .filter_map(Tab::editor)
            .cloned()
            .collect::<Vec<_>>()
        {
            let evaluator = evaluator.clone();
            editor.update(cx, |editor, _| editor.set_debug_evaluator(Some(evaluator)));
        }
        cx.notify();
    }

    /// Opens the selected frame's line and highlights it.
    fn reveal_debug_location(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.debug.active_session(cx).cloned() else {
            return;
        };
        let session = session.read(cx);
        if session.state() != SessionState::Stopped {
            return;
        }
        let index = session.selected_frame();
        let kind = if index == 0 {
            DebugLine::Paused
        } else {
            DebugLine::Frame
        };
        let Some((path, line)) = session.frame_location(index) else {
            self.clear_debug_location(cx);
            return;
        };
        if self.debug.shown_line.as_ref() != Some(&path) {
            self.clear_debug_location(cx);
        }
        if let Some(editor) = self.open_editor(&path, window, cx) {
            editor.update(cx, |editor, cx| {
                editor.set_debug_line(Some((line, kind)), cx)
            });
        }
        for editor in self.editors_of(&path) {
            editor.update(cx, |editor, cx| {
                editor.set_debug_line(Some((line, kind)), cx)
            });
        }
        self.debug.shown_line = Some(path);
    }

    fn clear_debug_location(&mut self, cx: &mut Context<Self>) {
        let editors: Vec<_> = self.all_tabs().filter_map(Tab::editor).cloned().collect();
        for editor in editors {
            editor.update(cx, |editor, cx| {
                editor.set_debug_line(None, cx);
                editor.set_debug_evaluator(None);
            });
        }
        self.debug.shown_line = None;
    }

    fn editors_of(&self, path: &Path) -> Vec<Entity<Editor>> {
        self.all_tabs()
            .filter(|tab| tab.path == path)
            .filter_map(Tab::editor)
            .cloned()
            .collect()
    }

    /// What the gutter shows for `path`: the store's breakpoints with the
    /// session's verification.
    fn gutter_breakpoints(&self, path: &Path, cx: &App) -> Vec<GutterBreakpoint> {
        let session = self.debug.active_session(cx).map(|s| s.read(cx));
        self.debug
            .store
            .read(cx)
            .breakpoints(path)
            .iter()
            .map(|bp| GutterBreakpoint {
                line: bp.line,
                enabled: bp.enabled,
                verified: session.and_then(|session| {
                    session
                        .breakpoint_status(path)
                        .iter()
                        .find(|status| status.line == bp.line)
                        .map(|status| status.verified)
                }),
                condition: bp.condition.clone(),
                hit_condition: bp.hit_condition.clone(),
                log_message: bp.log_message.clone(),
            })
            .collect()
    }

    fn refresh_breakpoint_gutters(&mut self, path: &Path, cx: &mut Context<Self>) {
        let breakpoints = self.gutter_breakpoints(path, cx);
        for editor in self.editors_of(path) {
            let breakpoints = breakpoints.clone();
            editor.update(cx, |editor, cx| editor.set_breakpoints(breakpoints, cx));
        }
    }

    fn refresh_all_breakpoint_gutters(&mut self, cx: &mut Context<Self>) {
        let mut paths: Vec<PathBuf> = self.all_tabs().map(|tab| tab.path.clone()).collect();
        paths.sort();
        paths.dedup();
        for path in paths {
            self.refresh_breakpoint_gutters(&path, cx);
        }
    }

    fn on_debug_store_event(&mut self, event: &DebugStoreEvent, cx: &mut Context<Self>) {
        if let DebugStoreEvent::Breakpoints(path) = event {
            self.refresh_breakpoint_gutters(path, cx);
        }
        self.schedule_session_save(cx);
    }

    /// A new editor shows its file's breakpoints (and the paused line).
    pub(super) fn debug_editor_opened(
        &mut self,
        editor: &Entity<Editor>,
        path: &Path,
        cx: &mut Context<Self>,
    ) {
        let breakpoints = self.gutter_breakpoints(path, cx);
        let paused = self
            .debug
            .active_session(cx)
            .filter(|session| session.read(cx).state() == SessionState::Stopped)
            .cloned();
        editor.update(cx, |editor, cx| {
            editor.set_breakpoints(breakpoints, cx);
            if let Some(session) = &paused {
                editor.set_debug_evaluator(Some(debug_evaluator(session.downgrade())));
            }
        });
    }

    /// Breakpoint edits from an editor's gutter.
    pub(super) fn on_editor_debug_event(
        &mut self,
        editor: &Entity<Editor>,
        event: &EditorEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self
            .all_tabs()
            .find(|tab| tab.editor() == Some(editor))
            .map(|tab| tab.path.clone())
        else {
            return;
        };
        self.debug.store.update(cx, |store, cx| match event {
            EditorEvent::ToggleBreakpoint { line } => store.toggle(&path, *line, cx),
            EditorEvent::SetBreakpoint {
                line,
                enabled,
                condition,
                hit_condition,
                log_message,
            } => store.set(
                &path,
                Breakpoint {
                    line: *line,
                    enabled: *enabled,
                    condition: condition.clone(),
                    hit_condition: hit_condition.clone(),
                    log_message: log_message.clone(),
                },
                cx,
            ),
            EditorEvent::RemoveBreakpoint { line } => store.remove(&path, *line, cx),
            EditorEvent::BreakpointsMoved(lines) => store.moved(&path, lines, cx),
            _ => {}
        });
    }

    /// The `Workspace` key context with the debug state (F5 continues
    /// while paused).
    pub(super) fn workspace_key_context(&self, cx: &App) -> KeyContext {
        let mut context = KeyContext::default();
        context.add("Workspace");
        match self.debug.state(cx) {
            Some(SessionState::Stopped) => {
                context.add("debug_active");
                context.add("debug_paused");
            }
            Some(_) => context.add("debug_active"),
            None => {}
        }
        context
    }

    /// The debug commands' handlers.
    pub(super) fn on_debug_actions(&self, element: gpui::Div, cx: &mut Context<Self>) -> gpui::Div {
        element
            .on_action(cx.listener(Self::start_debugging_action))
            .on_action(cx.listener(Self::debug_current_file))
            .on_action(cx.listener(|this, _: &Continue, _, cx| this.debug_step(Step::Continue, cx)))
            .on_action(cx.listener(|this, _: &StepOver, _, cx| this.debug_step(Step::Over, cx)))
            .on_action(cx.listener(|this, _: &StepInto, _, cx| this.debug_step(Step::Into, cx)))
            .on_action(cx.listener(|this, _: &StepOut, _, cx| this.debug_step(Step::Out, cx)))
            .on_action(cx.listener(|this, _: &Pause, _, cx| {
                if let Some(session) = this.debug.active_session(cx).cloned() {
                    session.update(cx, |session, cx| session.pause(cx));
                }
            }))
            .on_action(cx.listener(|this, _: &Stop, _, cx| {
                this.debug.queued = None;
                if let Some(session) = this.debug.active_session(cx).cloned() {
                    session.update(cx, |session, cx| session.stop(cx));
                }
            }))
            .on_action(cx.listener(|this, _: &Restart, window, cx| {
                match this.debug.last_request.clone() {
                    Some(request) => this.start_debugging(request, window, cx),
                    None => {
                        this.notice = Some("Nothing to restart: start debugging first (F5)".into());
                        cx.notify();
                    }
                }
            }))
            .on_action(cx.listener(Self::toggle_breakpoint_action))
            .on_action(cx.listener(Self::edit_breakpoint_action))
            .on_action(cx.listener(|this, _: &RemoveAllBreakpoints, _, cx| {
                this.debug
                    .store
                    .update(cx, |store, cx| store.remove_all(cx));
            }))
            .on_action(cx.listener(|this, _: &ToggleAllBreakpoints, _, cx| {
                this.debug
                    .store
                    .update(cx, |store, cx| store.toggle_all_enabled(cx));
            }))
            .on_action(cx.listener(|this, _: &ToggleDebugPanel, _, cx| {
                this.toggle_panel(PanelId::Debug, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleDebugConsole, window, cx| {
                this.toggle_panel(PanelId::DebugConsole, cx);
                let visible =
                    this.layout
                        .position_of(PanelId::DebugConsole)
                        .is_some_and(|position| {
                            let dock = this.layout.dock(position);
                            dock.open && dock.active == Some(PanelId::DebugConsole)
                        });
                if visible {
                    this.debug.console.read(cx).focus_input(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &DownloadCodeLldb, window, cx| {
                this.download_codelldb(window, cx)
            }))
    }

    /// F5: continue when paused; otherwise debug the selected run
    /// configuration (`run::DebugConfiguration`), else the current file.
    fn start_debugging_action(
        &mut self,
        _: &StartDebugging,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.debug.state(cx) {
            Some(SessionState::Stopped) => return self.debug_step(Step::Continue, cx),
            Some(_) => {
                self.notice = Some(
                    "A debug session is running: Shift+F5 stops it, Ctrl+Shift+F5 restarts it"
                        .into(),
                );
                return cx.notify();
            }
            None => {}
        }
        match cx.build_action("run::DebugConfiguration", None) {
            Ok(action) => window.dispatch_action(action, cx),
            Err(_) => self.debug_current_file(&DebugCurrentFile, window, cx),
        }
    }

    /// Debugs the active Python file (Rust and others need a run
    /// configuration, which builds first).
    fn debug_current_file(
        &mut self,
        _: &DebugCurrentFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.active_tab() else {
            self.notice = Some("Open a Python file to debug it".into());
            return cx.notify();
        };
        let path = tab.path.clone();
        let python =
            tab.editor().and_then(|editor| editor.read(cx).language()) == Some(Language::Python);
        if !python {
            self.notice = Some(
                "Debug Current File runs Python files; debug other programs with a run \
                 configuration (Shift+F9)"
                    .into(),
            );
            return cx.notify();
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let request = DebugRequest {
            name,
            adapter: DebugAdapterKind::Python,
            request: DebugRequestKind::Launch,
            configuration: serde_json::json!({ "program": path.to_string_lossy() }),
        };
        self.start_debugging(request, window, cx);
    }

    fn debug_step(&mut self, step: Step, cx: &mut Context<Self>) {
        if let Some(session) = self.debug.active_session(cx).cloned() {
            session.update(cx, |session, cx| session.step(step, cx));
        }
    }

    fn toggle_breakpoint_action(
        &mut self,
        _: &ToggleBreakpoint,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let Some(editor) = tab.editor() else {
            return;
        };
        let (path, line) = (tab.path.clone(), editor.read(cx).cursor_line());
        self.debug
            .store
            .update(cx, |store, cx| store.toggle(&path, line, cx));
    }

    fn edit_breakpoint_action(
        &mut self,
        _: &EditBreakpoint,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = self.active_tab().and_then(Tab::editor).cloned() {
            editor.update(cx, |editor, cx| {
                let line = editor.cursor_line();
                editor.open_breakpoint_editor(line, window, cx)
            });
        }
    }

    /// Downloads CodeLLDB where the project lives, then retries the last
    /// request (the one that found no adapter).
    pub(super) fn download_codelldb(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.debug.download.is_some() {
            return;
        }
        self.withdraw_offer(&OfferKind::DownloadCodeLldb, cx);
        self.notice = Some(
            format!(
                "Downloading CodeLLDB {} (about 55 MB)…",
                adapters::CODELLDB_VERSION
            )
            .into(),
        );
        cx.notify();
        let project = self.project.clone();
        let download = cx.background_spawn(async move { adapters::download_codelldb(&project) });
        self.debug.download = Some(cx.spawn_in(window, async move |this, cx| {
            let result = download.await;
            this.update_in(cx, |this, window, cx| {
                this.debug.download = None;
                match result {
                    Ok(_) => {
                        this.notice = None;
                        if let Some(request) = this.debug.last_request.clone() {
                            this.start_debugging(request, window, cx);
                        }
                    }
                    Err(err) => {
                        this.notice = Some(format!("CodeLLDB download failed: {err}").into())
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The debug bar above the editors while a session runs: its state and
    /// the toolbar.
    pub(super) fn render_debug_bar(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let session = self.debug.active_session(cx)?.read(cx);
        let state = session.state();
        let paused = state == SessionState::Stopped;
        let status = match (state, session.stop_reason()) {
            (SessionState::Stopped, Some(reason)) => format!("paused ({reason})"),
            (state, _) => state.label().to_owned(),
        };
        let button = |id: &'static str,
                      label: &'static str,
                      enabled: bool,
                      action: Box<dyn gpui::Action>| {
            div()
                .id(id)
                .px_2()
                .rounded_sm()
                .text_xs()
                .text_color(if enabled {
                    theme::text()
                } else {
                    theme::muted()
                })
                .when(enabled, |button| {
                    button
                        .cursor_pointer()
                        .hover(|style| style.bg(theme::hover_row()))
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(action.boxed_clone(), cx)
                        })
                })
                .child(label)
        };
        let running = state == SessionState::Running;
        Some(
            div()
                .flex_none()
                .h(px(26.))
                .px_2()
                .flex()
                .items_center()
                .gap_1()
                .bg(theme::panel())
                .border_b_1()
                .border_color(theme::border())
                .child(div().size(px(8.)).rounded_full().bg(if paused {
                    theme::warning()
                } else {
                    theme::run_marker()
                }))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme::text())
                        .child(format!("Debugging {}: {status}", session.name())),
                )
                .child(div().flex_1())
                .child(if paused {
                    button("bar-continue", "▶ Continue F5", true, Box::new(Continue))
                } else {
                    button("bar-pause", "‖ Pause F6", running, Box::new(Pause))
                })
                .child(button("bar-over", "↷ Over F10", paused, Box::new(StepOver)))
                .child(button("bar-into", "↓ Into F11", paused, Box::new(StepInto)))
                .child(button("bar-out", "↑ Out ⇧F11", paused, Box::new(StepOut)))
                .child(button("bar-restart", "↻ Restart", true, Box::new(Restart)))
                .child(button("bar-stop", "■ Stop ⇧F5", true, Box::new(Stop))),
        )
    }

    /// Debugging state saved with the session.
    pub(super) fn saved_debug_state(&self, cx: &App) -> crate::debugger::store::SavedDebugState {
        self.debug.store.read(cx).saved()
    }

    pub(super) fn restore_debug_state(
        &mut self,
        saved: crate::debugger::store::SavedDebugState,
        cx: &mut Context<Self>,
    ) {
        self.debug
            .store
            .update(cx, |store, cx| store.restore(saved, cx));
    }
}

/// Hover values from `session` while it is paused.
fn debug_evaluator(session: WeakEntity<DebugSession>) -> DebugEvaluator {
    Rc::new(move |expression, cx: &mut App| match session.upgrade() {
        Some(session) => session.update(cx, |session, cx| session.evaluate_hover(expression, cx)),
        None => Task::ready(None),
    })
}

#[cfg(test)]
mod tests {
    use dap::Client;
    use gpui::{TestAppContext, WindowHandle};
    use serde_json::{Value, json};

    use super::*;

    fn open_workspace(root: &Path, cx: &mut TestAppContext) -> WindowHandle<Workspace> {
        cx.update(|cx| {
            crate::editor::bind_keys(cx);
            super::super::bind_keys(cx);
        });
        let project = Project::local(root.to_path_buf());
        let window = cx.add_window(|window, cx| Workspace::new(project, None, window, cx));
        cx.run_until_parked();
        window
    }

    #[gpui::test]
    fn breakpoints_from_keys_reach_the_adapter_and_the_paused_line_shows(cx: &mut TestAppContext) {
        let root = std::env::temp_dir().join(format!("nc-debug-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let main = root.join("main.py");
        std::fs::write(
            &main,
            "total = 0\nfor i in range(3):\n    total += i\nprint(total)\n",
        )
        .unwrap();
        let window = open_workspace(&root, cx);
        window
            .update(cx, |workspace, window, cx| {
                workspace.open(&main, window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        let editor = window
            .read_with(cx, |workspace, _| workspace.active_editor().unwrap())
            .unwrap();

        // F9 on line 1, Ctrl+F8 on line 3, F9 again on line 1 removes it.
        cx.simulate_keystrokes(window.into(), "f9");
        editor.update(cx, |editor, cx| editor.go_to_line(2, 0, cx));
        cx.simulate_keystrokes(window.into(), "ctrl-f8");
        cx.run_until_parked();
        let gutter = |cx: &mut TestAppContext| {
            editor.read_with(cx, |editor, _| {
                editor
                    .breakpoints()
                    .map(|bp| (bp.line, bp.verified))
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(gutter(cx), [(0, None), (2, None)]);
        editor.update(cx, |editor, cx| editor.go_to_line(0, 0, cx));
        cx.simulate_keystrokes(window.into(), "f9");
        cx.run_until_parked();
        assert_eq!(gutter(cx), [(2, None)]);
        // Kept in the session.
        let saved = window
            .read_with(cx, |workspace, cx| workspace.session(cx).debug)
            .unwrap();
        assert_eq!(saved.breakpoints[0].breakpoints[0].line, 2);

        // A session (scripted adapter) verifies it and stops there.
        let (client, incoming, mut peer) = Client::pair();
        window
            .update(cx, |workspace, window, cx| {
                let request = DebugRequest {
                    name: "main.py".into(),
                    adapter: DebugAdapterKind::Python,
                    request: DebugRequestKind::Launch,
                    configuration: json!({ "program": main.to_string_lossy() }),
                };
                let project = workspace.project.clone();
                let store = workspace.debug.store.clone();
                let session = cx.new(|cx| {
                    DebugSession::connected(project, request, store, client, incoming, cx)
                });
                workspace.set_debug_session(Some(session), window, cx);
            })
            .unwrap();
        let expect = |peer: &mut dap::Peer, command: &str, cx: &mut TestAppContext| -> Value {
            cx.run_until_parked();
            let request = peer.received.try_recv().unwrap();
            assert_eq!(request["command"], command, "{request}");
            request
        };
        let initialize = expect(&mut peer, "initialize", cx);
        peer.respond(
            &initialize,
            json!({ "supportsConfigurationDoneRequest": true }),
        );
        let launch = expect(&mut peer, "launch", cx);
        peer.event("initialized", Value::Null);
        let set = expect(&mut peer, "setBreakpoints", cx);
        assert_eq!(set["arguments"]["breakpoints"], json!([{ "line": 3 }]));
        peer.respond(
            &set,
            json!({ "breakpoints": [{ "id": 1, "verified": true }] }),
        );
        let done = expect(&mut peer, "configurationDone", cx);
        peer.respond(&done, Value::Null);
        let threads = expect(&mut peer, "threads", cx);
        peer.respond(
            &threads,
            json!({ "threads": [{ "id": 1, "name": "MainThread" }] }),
        );
        peer.respond(&launch, Value::Null);
        cx.run_until_parked();
        assert_eq!(gutter(cx), [(2, Some(true))]);

        peer.event("stopped", json!({ "reason": "breakpoint", "threadId": 1 }));
        let threads = expect(&mut peer, "threads", cx);
        peer.respond(
            &threads,
            json!({ "threads": [{ "id": 1, "name": "MainThread" }] }),
        );
        let stack = expect(&mut peer, "stackTrace", cx);
        peer.respond(
            &stack,
            json!({ "stackFrames": [{ "id": 5, "name": "<module>", "line": 3, "column": 5,
                     "source": { "path": main.to_string_lossy() } }] }),
        );
        let scopes = expect(&mut peer, "scopes", cx);
        peer.respond(&scopes, json!({ "scopes": [] }));
        cx.run_until_parked();
        assert_eq!(
            editor.read_with(cx, |editor, _| editor.debug_line()),
            Some((2, DebugLine::Paused))
        );
        let paused = window
            .read_with(cx, |workspace, cx| {
                workspace.workspace_key_context(cx).contains("debug_paused")
            })
            .unwrap();
        assert!(paused);

        // F10 steps over; the highlight goes while running.
        cx.simulate_keystrokes(window.into(), "f10");
        let next = expect(&mut peer, "next", cx);
        assert_eq!(next["arguments"]["threadId"], 1);
        assert_eq!(editor.read_with(cx, |editor, _| editor.debug_line()), None);
        peer.respond(&next, Value::Null);

        // The debuggee ends: the session ends, verification clears.
        peer.event("terminated", Value::Null);
        let disconnect = expect(&mut peer, "disconnect", cx);
        peer.respond(&disconnect, Value::Null);
        cx.run_until_parked();
        assert_eq!(gutter(cx), [(2, None)]);
        std::fs::remove_dir_all(&root).ok();
        if let Some(state) = crate::layout::state_root() {
            std::fs::remove_dir_all(state).ok();
        }
    }
}
