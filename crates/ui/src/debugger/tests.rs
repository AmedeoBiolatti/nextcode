//! The session state machine against a scripted adapter (an in-process
//! [`dap::Peer`]): startup and configuration, a stop with its stack,
//! scopes and variables, paging, the REPL, stepping, breakpoint updates,
//! the end of the debuggee, and failures (crash, error answer, timeout).

use std::{path::PathBuf, time::Duration};

use dap::{Client, Peer};
use gpui::{AppContext, Entity, TestAppContext};
use serde_json::{Value, json};

use super::{
    DebugAdapterKind, DebugRequest, DebugRequestKind,
    session::{Children, DebugSession, DebugSessionEvent, OutputCategory, SessionState, Step},
    store::{Breakpoint, DebugStore},
};
use crate::project::Project;

struct Harness {
    session: Entity<DebugSession>,
    store: Entity<DebugStore>,
    peer: Peer,
    root: PathBuf,
    events: std::rc::Rc<std::cell::RefCell<Vec<DebugSessionEvent>>>,
}

impl Harness {
    fn new(cx: &mut TestAppContext) -> Self {
        let root = std::env::temp_dir().join("next_code_debug_tests");
        let project = Project::local(root.clone());
        let store = cx.new(|_| DebugStore::default());
        let (client, incoming, peer) = Client::pair();
        let request = DebugRequest {
            name: "main.py".into(),
            adapter: DebugAdapterKind::Python,
            request: DebugRequestKind::Launch,
            configuration: json!({ "program": root.join("main.py").to_string_lossy() }),
        };
        let store_for_session = store.clone();
        let session = cx.new(|cx| {
            DebugSession::connected(project, request, store_for_session, client, incoming, cx)
        });
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let recorded = events.clone();
        cx.update(|cx| {
            cx.subscribe(&session, move |_, event: &DebugSessionEvent, _| {
                recorded.borrow_mut().push(event.clone())
            })
            .detach()
        });
        Self {
            session,
            store,
            peer,
            root,
            events,
        }
    }

    /// The next request the session sent; it must be `command`.
    fn expect(&mut self, command: &str, cx: &mut TestAppContext) -> Value {
        cx.run_until_parked();
        let request = self
            .peer
            .received
            .try_recv()
            .ok()
            .unwrap_or_else(|| panic!("no request; expected `{command}`"));
        assert_eq!(request["command"], command, "{request}");
        request
    }

    fn nothing_sent(&mut self, cx: &mut TestAppContext) {
        cx.run_until_parked();
        if let Ok(request) = self.peer.received.try_recv() {
            panic!("unexpected request {request}");
        }
    }

    fn state(&self, cx: &mut TestAppContext) -> SessionState {
        self.session.read_with(cx, |session, _| session.state())
    }

    /// Initialize, launch, `initialized`, configuration; the launch
    /// answer comes after `configurationDone`, as debugpy does.
    fn start(&mut self, cx: &mut TestAppContext) {
        let initialize = self.expect("initialize", cx);
        assert_eq!(initialize["arguments"]["linesStartAt1"], true);
        assert_eq!(initialize["arguments"]["pathFormat"], "path");
        self.peer.respond(
            &initialize,
            json!({
                "supportsConfigurationDoneRequest": true,
                "supportsEvaluateForHovers": true,
                "exceptionBreakpointFilters": [
                    { "filter": "raised", "label": "Raised Exceptions" },
                    { "filter": "uncaught", "label": "Uncaught Exceptions", "default": true }
                ]
            }),
        );
        let launch = self.expect("launch", cx);
        assert_eq!(
            launch["arguments"]["request"],
            Value::Null,
            "the test configuration is sent as given"
        );
        assert!(
            launch["arguments"]["program"]
                .as_str()
                .unwrap()
                .ends_with("main.py")
        );
        self.peer.event("initialized", Value::Null);
        let set = self.expect("setBreakpoints", cx);
        assert_eq!(set["arguments"]["breakpoints"], json!([{ "line": 3 }]));
        assert!(
            set["arguments"]["source"]["path"]
                .as_str()
                .unwrap()
                .ends_with("main.py")
        );
        self.peer.respond(
            &set,
            json!({ "breakpoints": [{ "id": 1, "verified": true, "line": 3 }] }),
        );
        let exceptions = self.expect("setExceptionBreakpoints", cx);
        assert_eq!(exceptions["arguments"]["filters"], json!(["uncaught"]));
        self.peer.respond(&exceptions, Value::Null);
        let done = self.expect("configurationDone", cx);
        self.peer.respond(&done, Value::Null);
        let threads = self.expect("threads", cx);
        self.peer.respond(
            &threads,
            json!({ "threads": [{ "id": 1, "name": "MainThread" }] }),
        );
        self.peer.respond(&launch, Value::Null);
        cx.run_until_parked();
        assert_eq!(self.state(cx), SessionState::Running);
    }

    /// A breakpoint stop: threads, stack, scopes and the first scope's
    /// variables load.
    fn stop(&mut self, cx: &mut TestAppContext) {
        self.peer.event(
            "stopped",
            json!({ "reason": "breakpoint", "threadId": 1, "allThreadsStopped": true }),
        );
        let threads = self.expect("threads", cx);
        let stack = self.expect("stackTrace", cx);
        assert_eq!(stack["arguments"]["threadId"], 1);
        self.peer.respond(
            &threads,
            json!({ "threads": [{ "id": 1, "name": "MainThread" }] }),
        );
        let path = self.root.join("main.py");
        self.peer.respond(
            &stack,
            json!({ "stackFrames": [
                { "id": 7, "name": "loop", "line": 3, "column": 1,
                  "source": { "path": path.to_string_lossy() } },
                { "id": 8, "name": "<module>", "line": 9, "column": 1,
                  "source": { "path": path.to_string_lossy() } }
            ] }),
        );
        let scopes = self.expect("scopes", cx);
        assert_eq!(scopes["arguments"]["frameId"], 7);
        self.peer.respond(
            &scopes,
            json!({ "scopes": [
                { "name": "Locals", "variablesReference": 10 },
                { "name": "Globals", "variablesReference": 20 }
            ] }),
        );
        let variables = self.expect("variables", cx);
        assert_eq!(variables["arguments"]["variablesReference"], 10);
        self.peer.respond(
            &variables,
            json!({ "variables": [
                { "name": "i", "value": "2", "type": "int", "variablesReference": 0 },
                { "name": "items", "value": "[0, 1, …]", "type": "list",
                  "variablesReference": 11, "indexedVariables": 250 }
            ] }),
        );
        cx.run_until_parked();
    }
}

#[gpui::test]
fn a_session_stops_shows_variables_evaluates_steps_and_ends(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    let main = h.root.join("main.py");
    h.store.update(cx, |store, cx| {
        store.toggle(&main, 2, cx);
        // Disabled breakpoints are not sent.
        store.set(
            &main,
            Breakpoint {
                enabled: false,
                ..Breakpoint::at(5)
            },
            cx,
        );
    });
    h.start(cx);
    h.session.read_with(cx, |session, _| {
        let status = session.breakpoint_status(&main);
        assert_eq!(status.len(), 1);
        assert!(status[0].verified && status[0].line == 2);
    });

    h.stop(cx);
    h.session.read_with(cx, |session, _| {
        assert_eq!(session.state(), SessionState::Stopped);
        assert_eq!(session.stop_reason(), Some("breakpoint"));
        assert_eq!(session.frames().len(), 2);
        assert_eq!(session.frame_location(0), Some((main.clone(), 2)));
        assert!(session.is_expanded("Locals"), "the first scope opens");
        let Some(Children::Loaded { variables, .. }) = session.children(10) else {
            panic!("locals not loaded");
        };
        assert_eq!(
            (variables[0].name.as_str(), variables[0].value.as_str()),
            ("i", "2")
        );
    });
    assert!(h.events.borrow().contains(&DebugSessionEvent::Stopped));

    // A long list loads a page of indexed children (plus the named ones),
    // then the next page on request.
    let path = DebugSession::child_path("Locals", "items");
    h.session.update(cx, |session, cx| {
        session.toggle_expanded(path.clone(), 11, Some(250), cx)
    });
    let named = h.expect("variables", cx);
    let indexed = h.expect("variables", cx);
    assert_eq!(named["arguments"]["filter"], "named");
    assert_eq!(
        (
            &indexed["arguments"]["filter"],
            &indexed["arguments"]["start"],
            &indexed["arguments"]["count"]
        ),
        (&json!("indexed"), &json!(0), &json!(100))
    );
    h.peer.respond(
        &named,
        json!({ "variables": [{ "name": "len()", "value": "250" }] }),
    );
    h.peer.respond(
        &indexed,
        json!({ "variables": (0..100).map(|i| json!({ "name": i.to_string(), "value": i.to_string() })).collect::<Vec<_>>() }),
    );
    cx.run_until_parked();
    h.session
        .update(cx, |session, cx| session.load_more(11, cx));
    let more = h.expect("variables", cx);
    assert_eq!(
        (&more["arguments"]["start"], &more["arguments"]["count"]),
        (&json!(100), &json!(100))
    );
    h.peer.respond(
        &more,
        json!({ "variables": [{ "name": "100", "value": "100" }] }),
    );
    cx.run_until_parked();
    h.session.read_with(cx, |session, _| {
        let Some(Children::Loaded {
            variables,
            next_indexed,
            ..
        }) = session.children(11)
        else {
            panic!("items not loaded");
        };
        assert_eq!(variables.len(), 102);
        assert_eq!(*next_indexed, Some(200));
    });

    // Selecting another frame loads its scopes.
    h.session
        .update(cx, |session, cx| session.select_frame(1, cx));
    let scopes = h.expect("scopes", cx);
    assert_eq!(scopes["arguments"]["frameId"], 8);
    h.peer.respond(
        &scopes,
        json!({ "scopes": [{ "name": "Locals", "variablesReference": 30 }] }),
    );
    let variables = h.expect("variables", cx);
    assert_eq!(variables["arguments"]["variablesReference"], 30);
    h.peer.respond(&variables, json!({ "variables": [] }));
    assert!(
        h.events
            .borrow()
            .contains(&DebugSessionEvent::FrameSelected)
    );
    h.session
        .update(cx, |session, cx| session.select_frame(0, cx));
    let scopes = h.expect("scopes", cx);
    h.peer.respond(
        &scopes,
        json!({ "scopes": [{ "name": "Locals", "variablesReference": 40 }] }),
    );
    let variables = h.expect("variables", cx);
    h.peer.respond(&variables, json!({ "variables": [] }));

    // The REPL evaluates in the selected frame; errors show too.
    h.session
        .update(cx, |session, cx| session.repl("i * 10".into(), cx));
    let evaluate = h.expect("evaluate", cx);
    assert_eq!(
        evaluate["arguments"],
        json!({ "expression": "i * 10", "frameId": 7, "context": "repl" })
    );
    h.peer.respond(
        &evaluate,
        json!({ "result": "20", "variablesReference": 0 }),
    );
    h.session
        .update(cx, |session, cx| session.repl("nope".into(), cx));
    let evaluate = h.expect("evaluate", cx);
    h.peer.fail(&evaluate, "name 'nope' is not defined");
    // Hover values use the hover context.
    let hover = h
        .session
        .update(cx, |session, cx| session.evaluate_hover("i".into(), cx));
    let evaluate = h.expect("evaluate", cx);
    assert_eq!(evaluate["arguments"]["context"], "hover");
    h.peer.respond(
        &evaluate,
        json!({ "result": "2", "type": "int", "variablesReference": 0 }),
    );
    cx.run_until_parked();
    assert_eq!(
        futures::FutureExt::now_or_never(hover),
        Some(Some("i: int = 2".into()))
    );
    h.session.read_with(cx, |session, _| {
        let lines: Vec<(OutputCategory, &str)> = session
            .output()
            .iter()
            .map(|line| (line.category, line.text.as_str()))
            .collect();
        assert!(
            lines.contains(&(OutputCategory::Input, "> i * 10")),
            "{lines:?}"
        );
        assert!(lines.contains(&(OutputCategory::Result, "20")), "{lines:?}");
        assert!(
            lines.contains(&(
                OutputCategory::Stderr,
                "evaluate failed: name 'nope' is not defined"
            )),
            "{lines:?}"
        );
    });

    // Step over: running until the next stop.
    h.session
        .update(cx, |session, cx| session.step(Step::Over, cx));
    let next = h.expect("next", cx);
    assert_eq!(next["arguments"], json!({ "threadId": 1 }));
    assert_eq!(h.state(cx), SessionState::Running);
    h.session
        .read_with(cx, |session, _| assert!(session.frames().is_empty()));
    h.peer.respond(&next, Value::Null);

    // Program output, partial lines joined; telemetry dropped.
    h.peer
        .event("output", json!({ "category": "stdout", "output": "hel" }));
    h.peer.event(
        "output",
        json!({ "category": "stdout", "output": "lo\nworld\n" }),
    );
    h.peer
        .event("output", json!({ "category": "telemetry", "output": "{}" }));
    cx.run_until_parked();
    h.session.read_with(cx, |session, _| {
        let stdout: Vec<&str> = session
            .output()
            .iter()
            .filter(|line| line.category == OutputCategory::Stdout)
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(stdout, ["hello", "world"]);
    });

    // Changing breakpoints while running resends the file's.
    h.store.update(cx, |store, cx| store.toggle(&main, 7, cx));
    let set = h.expect("setBreakpoints", cx);
    assert_eq!(
        set["arguments"]["breakpoints"],
        json!([{ "line": 3 }, { "line": 8 }])
    );
    h.peer.respond(
        &set,
        json!({ "breakpoints": [{ "id": 1, "verified": true }, { "id": 2, "verified": false, "message": "no code" }] }),
    );
    cx.run_until_parked();
    // A breakpoint event updates verification by id.
    h.peer.event(
        "breakpoint",
        json!({ "reason": "changed", "breakpoint": { "id": 2, "verified": true } }),
    );
    cx.run_until_parked();
    h.session.read_with(cx, |session, _| {
        assert!(
            session
                .breakpoint_status(&main)
                .iter()
                .all(|status| status.verified)
        );
    });

    // The program ends: exited, terminated → disconnect → ended.
    h.peer.event("exited", json!({ "exitCode": 0 }));
    h.peer.event("terminated", Value::Null);
    let disconnect = h.expect("disconnect", cx);
    h.peer.respond(&disconnect, Value::Null);
    cx.run_until_parked();
    assert_eq!(h.state(cx), SessionState::Ended);
    h.session.read_with(cx, |session, _| {
        assert_eq!(session.exit_code(), Some(0));
        assert_eq!(session.error(), None);
    });
    assert!(
        h.events
            .borrow()
            .contains(&DebugSessionEvent::Ended { error: None })
    );
    h.nothing_sent(cx);
}

#[gpui::test]
fn stopping_terminates_then_disconnects(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    h.store
        .update(cx, |store, cx| store.toggle(&h.root.join("main.py"), 2, cx));
    h.start(cx);
    h.session.update(cx, |session, cx| session.pause(cx));
    let pause = h.expect("pause", cx);
    assert_eq!(pause["arguments"]["threadId"], 1);
    h.peer.respond(&pause, Value::Null);
    h.session.update(cx, |session, cx| session.stop(cx));
    assert_eq!(h.state(cx), SessionState::Stopping);
    // No terminate support was announced: straight to disconnect.
    let disconnect = h.expect("disconnect", cx);
    assert_eq!(disconnect["arguments"]["terminateDebuggee"], true);
    h.peer.respond(&disconnect, Value::Null);
    cx.run_until_parked();
    assert_eq!(h.state(cx), SessionState::Ended);
}

#[gpui::test]
fn an_adapter_crash_ends_the_session_with_a_reason(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    h.store
        .update(cx, |store, cx| store.toggle(&h.root.join("main.py"), 2, cx));
    h.start(cx);
    h.stop(cx);
    // A request in flight when the adapter dies fails instead of hanging.
    h.session
        .update(cx, |session, cx| session.repl("i".into(), cx));
    h.expect("evaluate", cx);
    h.peer.exit();
    cx.run_until_parked();
    assert_eq!(h.state(cx), SessionState::Ended);
    let error = h
        .session
        .read_with(cx, |session, _| session.error().map(str::to_owned));
    assert!(error.unwrap().contains("exited unexpectedly"));
    h.session.read_with(cx, |session, _| {
        assert!(session.frames().is_empty());
        assert!(
            session
                .output()
                .iter()
                .any(|line| line.text.contains("the debug adapter exited"))
        );
    });
}

#[gpui::test]
fn a_failed_launch_and_a_silent_adapter_end_the_session(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    let initialize = h.expect("initialize", cx);
    h.peer.respond(&initialize, json!({}));
    let launch = h.expect("launch", cx);
    h.peer.fail(&launch, "program not found");
    cx.run_until_parked();
    assert_eq!(h.state(cx), SessionState::Ended);
    assert_eq!(
        h.session
            .read_with(cx, |session, _| session.error().map(str::to_owned)),
        Some("Could not launch: launch failed: program not found".into())
    );

    // An adapter that never answers `initialize` times out.
    let mut h = Harness::new(cx);
    h.expect("initialize", cx);
    cx.executor().advance_clock(Duration::from_secs(119));
    cx.run_until_parked();
    assert_eq!(h.state(cx), SessionState::Starting);
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert_eq!(h.state(cx), SessionState::Ended);
    let error = h
        .session
        .read_with(cx, |session, _| session.error().map(str::to_owned));
    assert!(error.unwrap().contains("did not answer `initialize`"));
}

#[gpui::test]
fn reverse_requests_are_declined_with_a_reason(cx: &mut TestAppContext) {
    let mut h = Harness::new(cx);
    h.store
        .update(cx, |store, cx| store.toggle(&h.root.join("main.py"), 2, cx));
    h.start(cx);
    h.peer.reverse_request(
        41,
        "runInTerminal",
        json!({ "cwd": "/p", "args": ["python", "main.py"] }),
    );
    h.peer.reverse_request(
        42,
        "startDebugging",
        json!({ "request": "attach", "configuration": {} }),
    );
    cx.run_until_parked();
    for seq in [41, 42] {
        let answer = h.peer.received.try_recv().unwrap();
        assert_eq!(
            (answer["type"].as_str(), answer["request_seq"].as_i64()),
            (Some("response"), Some(seq))
        );
        assert_eq!(answer["success"], false);
    }
    h.session.read_with(cx, |session, _| {
        assert!(
            session
                .output()
                .iter()
                .any(|line| line.text.contains("not supported"))
        );
    });
}

#[test]
fn output_merging_and_requests_through_json() {
    // DebugRequest keeps the documented JSON shape for configurations.
    let request: DebugRequest = serde_json::from_value(json!({
        "name": "app",
        "adapter": { "type": "custom", "command": "my-dap", "args": ["--stdio"] },
        "request": "attach",
        "configuration": { "port": 5678 }
    }))
    .unwrap();
    assert_eq!(
        request.adapter,
        DebugAdapterKind::Custom {
            command: "my-dap".into(),
            args: vec!["--stdio".into()]
        }
    );
    assert_eq!(request.request, DebugRequestKind::Attach);
    let python: DebugAdapterKind = serde_json::from_value(json!({ "type": "python" })).unwrap();
    assert_eq!(python, DebugAdapterKind::Python);
}
