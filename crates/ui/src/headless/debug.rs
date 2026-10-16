//! Headless debugging (`debug_*` commands and `debug_event` events) over
//! the same [`DebugSession`] and [`DebugStore`] as the GUI. One session
//! at a time; lines are 0-based.

use gpui::{AppContext, Context, Entity, Subscription, Task};
use serde_json::{Value, json};

use super::{Session, param_str, path_json};
use crate::debugger::{
    DebugRequest,
    session::{DebugSession, DebugSessionEvent, SessionState, Step},
    store::{Breakpoint, DebugStore},
};

/// How long `debug_stop` and shutdown wait for the session to end.
const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(8);

pub(super) struct HeadlessDebug {
    store: Entity<DebugStore>,
    session: Option<Entity<DebugSession>>,
    /// Replies waiting for the session to end (`debug_stop`).
    waiting_for_end: Vec<Value>,
    _subscription: Option<Subscription>,
}

impl HeadlessDebug {
    pub(super) fn new(cx: &mut Context<Session>) -> Self {
        Self {
            store: cx.new(|_| DebugStore::default()),
            session: None,
            waiting_for_end: Vec::new(),
            _subscription: None,
        }
    }

    fn active(&self, cx: &gpui::App) -> Option<Entity<DebugSession>> {
        self.session
            .clone()
            .filter(|session| session.read(cx).is_active())
    }
}

fn frame_json(session: &DebugSession, index: usize) -> Value {
    let frame = &session.frames()[index];
    let location = session.frame_location(index);
    json!({
        "index": index,
        "id": frame.id,
        "name": frame.name,
        "path": location.as_ref().map(|(path, _)| path_json(path)),
        "line": location.map(|(_, line)| line),
        "column": frame.column.saturating_sub(1),
    })
}

fn variables_json(variables: &[dap::types::Variable]) -> Value {
    variables
        .iter()
        .map(|variable| {
            json!({
                "name": variable.name,
                "value": variable.value,
                "type": variable.type_name,
                "reference": variable.variables_reference,
            })
        })
        .collect()
}

impl Session {
    /// `debug_*` commands.
    pub(super) fn debug(
        &mut self,
        id: Value,
        command: &str,
        request: &Value,
        cx: &mut Context<Self>,
    ) {
        let result = match command {
            "debug_start" => self.debug_start(request, cx),
            "debug_set_breakpoints" => self.debug_set_breakpoints(request, cx),
            "debug_continue" => self.debug_step(Step::Continue, cx),
            "debug_step" => match request["kind"].as_str().unwrap_or("over") {
                "over" => self.debug_step(Step::Over, cx),
                "into" => self.debug_step(Step::Into, cx),
                "out" => self.debug_step(Step::Out, cx),
                other => Err(format!("unknown step kind `{other}` (over, into, out)")),
            },
            "debug_pause" => self.with_debug_session(cx, |session, cx| session.pause(cx)),
            "debug_stack" => self.debug_stack(cx),
            "debug_select_frame" => match request["frame"].as_u64() {
                Some(frame) => self
                    .with_debug_session(cx, |session, cx| session.select_frame(frame as usize, cx)),
                None => Err("missing `frame`".into()),
            },
            "debug_variables" => return self.debug_variables(id, request, cx),
            "debug_evaluate" => return self.debug_evaluate(id, request, cx),
            "debug_output" => Ok(self.debug_output(cx)),
            "debug_stop" => return self.debug_stop(id, cx),
            "debug_download_codelldb" => {
                return self.blocking(id, cx, |project| {
                    crate::debugger::adapters::download_codelldb(project)
                        .map(|path| json!({ "path": path }))
                        .map_err(std::io::Error::other)
                });
            }
            other => Err(format!("unknown command `{other}`")),
        };
        self.out.reply(&id, result);
    }

    fn with_debug_session(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut DebugSession, &mut Context<DebugSession>),
    ) -> Result<Value, String> {
        let session = self.debug.active(cx).ok_or("no debug session is running")?;
        session.update(cx, f);
        Ok(Value::Null)
    }

    /// `request`: a `DebugRequest` (`name`, `adapter`, `request`,
    /// `configuration`). Progress comes as `debug_event`s.
    fn debug_start(&mut self, request: &Value, cx: &mut Context<Self>) -> Result<Value, String> {
        let debug_request: DebugRequest = serde_json::from_value(request["request"].clone())
            .map_err(|err| format!("invalid `request`: {err}"))?;
        if let Some(session) = self.debug.active(cx) {
            session.update(cx, |session, cx| session.stop(cx));
        }
        let settings = self.store.read(cx).settings().debug.clone();
        let project = self.project.clone();
        let store = self.debug.store.clone();
        let session = cx.new(|cx| DebugSession::start(project, debug_request, store, settings, cx));
        let out = self.out.clone();
        self.debug._subscription = Some(cx.subscribe(&session, move |this, session, event, cx| {
            this.on_debug_event(&session, event, &out, cx)
        }));
        self.debug.session = Some(session);
        Ok(json!({ "state": "starting" }))
    }

    fn on_debug_event(
        &mut self,
        session: &Entity<DebugSession>,
        event: &DebugSessionEvent,
        out: &super::Out,
        cx: &mut Context<Self>,
    ) {
        let session_ref = session.read(cx);
        let fields = match event {
            DebugSessionEvent::Stopped => {
                let frame = (!session_ref.frames().is_empty())
                    .then(|| frame_json(session_ref, session_ref.selected_frame()));
                json!({
                    "type": "stopped",
                    "reason": session_ref.stop_reason(),
                    "thread_id": session_ref.thread_id(),
                    "frame": frame,
                })
            }
            DebugSessionEvent::Output { category, text } => {
                json!({ "type": "output", "category": category.name(), "text": text })
            }
            DebugSessionEvent::BreakpointsVerified(path) => {
                let breakpoints: Vec<Value> = session_ref
                    .breakpoint_status(path)
                    .iter()
                    .map(|status| {
                        json!({ "line": status.line, "verified": status.verified, "message": status.message })
                    })
                    .collect();
                json!({ "type": "breakpoints", "path": path_json(path), "breakpoints": breakpoints })
            }
            DebugSessionEvent::Ended { error } => {
                for id in std::mem::take(&mut self.debug.waiting_for_end) {
                    out.reply(&id, Ok(Value::Null));
                }
                json!({
                    "type": "terminated",
                    "error": error,
                    "exit_code": session_ref.exit_code(),
                })
            }
            DebugSessionEvent::AdapterMissing { message, download } => {
                json!({ "type": "adapter_missing", "message": message, "download": download })
            }
            DebugSessionEvent::FrameSelected => {
                json!({ "type": "frame_selected", "frame": frame_json(session_ref, session_ref.selected_frame()) })
            }
            DebugSessionEvent::Changed => {
                // Only state changes, not every refresh.
                let state = session_ref.state();
                if state != SessionState::Running {
                    return;
                }
                json!({ "type": "running" })
            }
        };
        out.event("debug_event", fields);
    }

    /// `path`, `breakpoints`: `[{line, condition?, hit_condition?,
    /// log_message?, enabled?}]` replacing the file's.
    fn debug_set_breakpoints(
        &mut self,
        request: &Value,
        cx: &mut Context<Self>,
    ) -> Result<Value, String> {
        let path = self.path(request, param_str(request, "path")?);
        let breakpoints: Vec<Breakpoint> =
            serde_json::from_value(request["breakpoints"].clone())
                .map_err(|err| format!("invalid `breakpoints`: {err}"))?;
        self.debug.store.update(cx, |store, cx| {
            let old: Vec<usize> = store.breakpoints(&path).iter().map(|bp| bp.line).collect();
            for line in old {
                store.remove(&path, line, cx);
            }
            for breakpoint in breakpoints {
                store.set(&path, breakpoint, cx);
            }
        });
        Ok(Value::Null)
    }

    fn debug_step(&mut self, step: Step, cx: &mut Context<Self>) -> Result<Value, String> {
        let session = self.debug.active(cx).ok_or("no debug session is running")?;
        if session.read(cx).state() != SessionState::Stopped {
            return Err("the debuggee is not paused".into());
        }
        session.update(cx, |session, cx| session.step(step, cx));
        Ok(Value::Null)
    }

    fn debug_stack(&self, cx: &mut Context<Self>) -> Result<Value, String> {
        let session = self
            .debug
            .session
            .as_ref()
            .ok_or("no debug session")?
            .read(cx);
        let frames: Vec<Value> = (0..session.frames().len())
            .map(|index| frame_json(session, index))
            .collect();
        let threads: Vec<Value> = session
            .threads()
            .iter()
            .map(|thread| json!({ "id": thread.id, "name": thread.name }))
            .collect();
        Ok(json!({
            "state": session.state().label(),
            "reason": session.stop_reason(),
            "thread_id": session.thread_id(),
            "threads": threads,
            "frames": frames,
            "selected_frame": session.selected_frame(),
        }))
    }

    /// `reference`: a variable's children; else `frame` (index, default
    /// the selected one) and `scope` (name, default the first): that
    /// scope's variables.
    fn debug_variables(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let Some(session) = self.debug.active(cx) else {
            return self
                .out
                .reply(&id, Err("no debug session is running".into()));
        };
        let out = self.out.clone();
        if let Some(reference) = request["reference"].as_i64() {
            let task = session.update(cx, |session, cx| session.fetch_variables(reference, cx));
            return cx
                .spawn(async move |_, _| {
                    out.reply(&id, task.await.map(|vars| variables_json(&vars)))
                })
                .detach();
        }
        let frame = request["frame"]
            .as_u64()
            .map(|frame| frame as usize)
            .unwrap_or_else(|| session.read(cx).selected_frame());
        let scope_name = request["scope"].as_str().map(str::to_owned);
        let scopes = session.update(cx, |session, cx| session.fetch_scopes(frame, cx));
        cx.spawn(async move |_, cx| {
            let result = async {
                let scopes = scopes.await?;
                let scope = match &scope_name {
                    Some(name) => scopes.iter().find(|scope| &scope.name == name),
                    None => scopes.first(),
                }
                .ok_or_else(|| format!("no scope {}", scope_name.as_deref().unwrap_or("")))?;
                let reference = scope.variables_reference;
                let task = session
                    .update(cx, |session, cx| session.fetch_variables(reference, cx))
                    .map_err(|err| err.to_string())?;
                let variables = task.await?;
                Ok::<_, String>(json!({
                    "scope": scope.name,
                    "scopes": scopes.iter().map(|scope| scope.name.clone()).collect::<Vec<_>>(),
                    "variables": variables_json(&variables),
                }))
            }
            .await;
            out.reply(&id, result);
        })
        .detach();
    }

    /// `expression`, `context`? (`repl` | `watch` | `hover`; default
    /// `repl`), evaluated in the selected frame.
    fn debug_evaluate(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let Some(session) = self.debug.active(cx) else {
            return self
                .out
                .reply(&id, Err("no debug session is running".into()));
        };
        let expression = match param_str(request, "expression") {
            Ok(expression) => expression.to_owned(),
            Err(err) => return self.out.reply(&id, Err(err)),
        };
        let context = request["context"].as_str().unwrap_or("repl").to_owned();
        let task = session.update(cx, |session, cx| session.evaluate(expression, &context, cx));
        let out = self.out.clone();
        cx.spawn(async move |_, _| {
            let result = task.await.map(|response| {
                json!({
                    "result": response.result,
                    "type": response.type_name,
                    "reference": response.variables_reference,
                })
            });
            out.reply(&id, result);
        })
        .detach();
    }

    fn debug_output(&self, cx: &mut Context<Self>) -> Value {
        let Some(session) = &self.debug.session else {
            return json!([]);
        };
        session
            .read(cx)
            .output()
            .iter()
            .map(|line| json!({ "category": line.category.name(), "text": line.text }))
            .collect()
    }

    /// Replies once the session has ended.
    fn debug_stop(&mut self, id: Value, cx: &mut Context<Self>) {
        let Some(session) = self.debug.active(cx) else {
            return self.out.reply(&id, Ok(Value::Null));
        };
        self.debug.waiting_for_end.push(id);
        session.update(cx, |session, cx| session.stop(cx));
    }

    /// Stops a running session (on shutdown); resolves once it ended.
    pub(super) fn debug_shutdown(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let Some(session) = self.debug.active(cx) else {
            return Task::ready(());
        };
        session.update(cx, |session, cx| session.stop(cx));
        let executor = cx.background_executor().clone();
        cx.spawn(async move |_, cx| {
            let deadline = std::time::Instant::now() + STOP_WAIT;
            while std::time::Instant::now() < deadline
                && session
                    .read_with(cx, |session, _| session.is_active())
                    .unwrap_or(false)
            {
                executor.timer(std::time::Duration::from_millis(50)).await;
            }
        })
    }
}
