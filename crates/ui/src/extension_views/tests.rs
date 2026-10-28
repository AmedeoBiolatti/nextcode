use super::*;
use gpui::{AppContext, TestAppContext, VisualTestContext};
use serde_json::json;
use std::{cell::RefCell, rc::Rc};

fn panel(value: &str, include_input: bool) -> PanelUpdate {
    let mut children = vec![Widget::Button {
        id: "run".into(),
        text: "Run".into(),
        disabled: false,
    }];
    if include_input {
        children.insert(
            0,
            Widget::TextInput {
                id: "query".into(),
                label: "Query".into(),
                value: value.into(),
                placeholder: String::new(),
                disabled: false,
            },
        );
    }
    children.push(Widget::Checkbox {
        id: "enabled".into(),
        label: "Enabled".into(),
        value: true,
        disabled: false,
    });
    PanelUpdate {
        refresh_ms: None,
        id: "panel".into(),
        root: Widget::Column {
            id: "root".into(),
            children,
            gap: 8,
        },
        state: json!({"value": value}),
    }
}
fn view(cx: &mut TestAppContext) -> Entity<ExtensionView> {
    cx.update(|cx| {
        cx.new(|cx| ExtensionView::new("demo".into(), "panel".into(), "Demo".into(), cx))
    })
}

#[gpui::test]
fn newer_edits_survive_replies_and_unrelated_updates(cx: &mut TestAppContext) {
    let view = view(cx);
    view.update(cx, |view, cx| {
        view.replace(panel("initial", true), None, cx)
    });
    let editor = view.read_with(cx, |view, _| view.editors["query"].clone());
    editor.update(cx, |editor, cx| editor.set_text("submitted", cx));
    let submitted = view.read_with(cx, |view, cx| view.snapshot(cx).unwrap().2);
    view.update(cx, |view, cx| view.set_busy(true, cx));
    editor.update(cx, |editor, cx| editor.set_text("newer", cx));
    view.update(cx, |view, cx| {
        view.replace(panel("guest reply", true), Some(&submitted), cx)
    });
    assert_eq!(editor.read_with(cx, |editor, _| editor.text()), "newer");
    // The preserved edit is still dirty against the guest value on a later command rerender.
    view.update(cx, |view, cx| {
        view.replace(panel("command reply", true), None, cx)
    });
    assert_eq!(editor.read_with(cx, |editor, _| editor.text()), "newer");
    let submitted = view.read_with(cx, |view, cx| view.snapshot(cx).unwrap().2);
    view.update(cx, |view, cx| {
        view.replace(panel("accepted", true), Some(&submitted), cx)
    });
    assert_eq!(editor.read_with(cx, |editor, _| editor.text()), "accepted");
    assert_eq!(view.read_with(cx, |view, _| view.revision()), 4);
    view.update(cx, |view, cx| {
        view.replace(panel("removed", false), None, cx)
    });
    view.read_with(cx, |view, cx| {
        assert!(!view.editors.contains_key("query"));
        assert!(!view.snapshot(cx).unwrap().2.contains_key("query"));
    });
}

#[gpui::test]
fn dispatch_rejects_invalid_disabled_and_busy_actions_and_emits_complete_form(
    cx: &mut TestAppContext,
) {
    let view = view(cx);
    view.update(cx, |view, cx| view.replace(panel("local", true), None, cx));
    let events = Rc::new(RefCell::new(Vec::new()));
    let recorded = events.clone();
    let _subscription = cx.update(|cx| {
        cx.subscribe(&view, move |_, event: &ExtensionViewEvent, _| {
            recorded.borrow_mut().push(event.clone())
        })
    });
    view.update(cx, |view, cx| {
        view.emit(
            UiEvent {
                widget: "missing".into(),
                kind: UiEventKind::Click,
            },
            cx,
        );
        view.emit(
            UiEvent {
                widget: "query".into(),
                kind: UiEventKind::Click,
            },
            cx,
        );
        assert!(view.error.is_some());
    });
    assert!(events.borrow().is_empty());
    let editor = view.read_with(cx, |view, _| view.editors["query"].clone());
    editor.update(cx, |editor, cx| editor.set_text("typed locally", cx));
    view.update(cx, |view, cx| {
        view.emit(
            UiEvent {
                widget: "run".into(),
                kind: UiEventKind::Click,
            },
            cx,
        )
    });
    cx.run_until_parked();
    let events_ref = events.borrow();
    assert_eq!(events_ref.len(), 1);
    let ExtensionViewEvent::Invoke {
        revision, inputs, ..
    } = &events_ref[0]
    else {
        panic!("expected UI event")
    };
    assert_eq!(*revision, 1);
    assert_eq!(
        inputs.get("query"),
        Some(&InputValue::Text("typed locally".into()))
    );
    assert_eq!(inputs.get("enabled"), Some(&InputValue::Bool(true)));
    drop(events_ref);
    view.update(cx, |view, cx| {
        view.emit(
            UiEvent {
                widget: "run".into(),
                kind: UiEventKind::Click,
            },
            cx,
        )
    });
    assert_eq!(events.borrow().len(), 1);
    let mut update = panel("disabled", true);
    if let Widget::Column { children, .. } = &mut update.root
        && let Widget::Button { disabled, .. } = &mut children[1]
    {
        *disabled = true;
    }
    view.update(cx, |view, cx| {
        view.replace(update, None, cx);
        view.emit(
            UiEvent {
                widget: "run".into(),
                kind: UiEventKind::Click,
            },
            cx,
        );
    });
    assert_eq!(events.borrow().len(), 1);
}

#[gpui::test]
fn keyed_editors_keep_focus_across_revisions_and_tab_moves_in_tree_order(cx: &mut TestAppContext) {
    cx.update(|cx| {
        crate::editor::bind_keys(cx);
        bind_keys(cx);
    });
    let window =
        cx.add_window(|_, cx| ExtensionView::new("demo".into(), "panel".into(), "Demo".into(), cx));
    let view = window.root(cx).unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    view.update(&mut cx, |view, cx| {
        view.replace(panel("first", true), None, cx)
    });
    let editor = view.read_with(&cx, |view, _| view.editors["query"].clone());
    cx.update(|window, cx| window.focus(&editor.focus_handle(cx)));
    view.update(&mut cx, |view, cx| {
        view.replace(panel("second", true), None, cx)
    });
    cx.update(|window, cx| {
        assert!(editor.focus_handle(cx).is_focused(window));
    });
    cx.simulate_keystrokes("tab");
    cx.update(|window, cx| {
        assert!(view.read(cx).controls["run"].is_focused(window));
    });
    cx.simulate_keystrokes("shift-tab");
    cx.update(|window, cx| {
        assert!(editor.focus_handle(cx).is_focused(window));
    });
}

fn exited(id: &str) -> ProcessExit {
    ProcessExit {
        id: id.into(),
        exit_code: Some(0),
        stdout: "{}".into(),
        stderr: String::new(),
        truncated: false,
        error: None,
    }
}
#[gpui::test]
fn sibling_processes_complete_serially_against_the_updated_panel_revision(cx: &mut TestAppContext) {
    let view = view(cx);
    let events = Rc::new(RefCell::new(Vec::new()));
    let recorded = events.clone();
    let _subscription = cx.update(|cx| {
        cx.subscribe(&view, move |_, event: &ExtensionViewEvent, _| {
            recorded.borrow_mut().push(event.clone())
        })
    });
    let a = CancelToken::new();
    let b = CancelToken::new();
    view.update(cx, |v, cx| {
        v.replace(panel("initial", true), None, cx);
        v.start_process("a".into(), a.clone());
        v.start_process("b".into(), b.clone());
        v.process_finished(exited("a"), &a, false, cx);
        v.process_finished(exited("b"), &b, false, cx);
        // Registry observations must not release a locally pending invocation.
        v.set_busy(false, cx);
    });
    cx.run_until_parked();
    assert_eq!(events.borrow().len(), 1);
    assert_eq!(events.borrow()[0].revision(), 1);
    view.update(cx, |v, cx| {
        v.replace_from(panel("first reply", true), None, true, cx);
        v.finish_invocation(cx);
    });
    cx.run_until_parked();
    assert_eq!(events.borrow().len(), 2);
    assert_eq!(events.borrow()[1].revision(), 2);
}
#[gpui::test]
fn replacing_a_panel_cancels_jobs_and_old_ids_cannot_consume_new_results(cx: &mut TestAppContext) {
    let view = view(cx);
    let old = CancelToken::new();
    let new = CancelToken::new();
    view.update(cx, |v, cx| {
        v.replace(panel("one", false), None, cx);
        v.start_process("status".into(), old.clone());
        v.replace(panel("two", false), None, cx);
        assert!(old.is_cancelled());
        v.start_process("status".into(), new.clone());
        v.process_finished(exited("status"), &old, false, cx);
        assert!(v.processes.contains_key("status"));
        assert!(!new.is_cancelled());
    });
}
#[gpui::test]
fn panel_timer_is_one_shot_and_waits_for_processes(cx: &mut TestAppContext) {
    let view = view(cx);
    let token = CancelToken::new();
    let events = Rc::new(RefCell::new(Vec::new()));
    let recorded = events.clone();
    let _subscription = cx.update(|cx| {
        cx.subscribe(&view, move |_, event: &ExtensionViewEvent, _| {
            recorded.borrow_mut().push(event.clone())
        })
    });
    view.update(cx, |v, cx| {
        let mut update = panel("one", false);
        update.refresh_ms = Some(1000);
        v.replace(update, None, cx);
        v.start_process("status".into(), token.clone());
    });
    cx.run_until_parked();
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(3));
    cx.run_until_parked();
    assert!(events.borrow().is_empty());
    view.update(cx, |v, cx| {
        v.process_finished(exited("status"), &token, false, cx)
    });
    cx.run_until_parked();
    assert_eq!(events.borrow().len(), 1);
    view.update(cx, |v, cx| v.finish_invocation(cx));
    cx.run_until_parked();
    assert_eq!(events.borrow().len(), 2);
    assert!(matches!(
        events.borrow()[1],
        ExtensionViewEvent::Background { process: None, .. }
    ));
    view.update(cx, |v, cx| v.finish_invocation(cx));
    cx.executor()
        .advance_clock(std::time::Duration::from_secs(3));
    cx.run_until_parked();
    assert_eq!(events.borrow().len(), 2);
}
