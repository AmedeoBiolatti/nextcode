//! `context` on a real workspace: the active editor's cursor, selection,
//! lines around it and diagnostics, a terminal's last lines, and the caps.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use gpui::{TestAppContext, WindowHandle};
use serde_json::{Value, json};

use super::*;

fn project_dir(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("nc-context-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let source: String = (1..=60).map(|n| format!("x{n} = {n}\n")).collect();
    std::fs::write(root.join("main.py"), source).unwrap();
    std::fs::write(root.join("other.txt"), "other\n").unwrap();
    root
}

fn cleanup(root: &Path) {
    std::fs::remove_dir_all(root).ok();
    if let Some(state) = crate::layout::state_root() {
        std::fs::remove_dir_all(state).ok();
    }
    if let Some(recent) = crate::recent::path() {
        std::fs::remove_file(recent).ok();
    }
}

fn open_workspace(root: &Path, cx: &mut TestAppContext) -> WindowHandle<Workspace> {
    cx.update(|cx| {
        crate::editor::bind_keys(cx);
        crate::workspace::bind_keys(cx);
    });
    let project = Project::local(root.to_path_buf());
    let window = cx.add_window(|window, cx| Workspace::new(project, None, window, cx));
    cx.run_until_parked();
    window
}

fn context_of(window: WindowHandle<Workspace>, request: Value, cx: &mut TestAppContext) -> Value {
    // Through the untyped handle: the typed one leases the workspace.
    let window: gpui::AnyWindowHandle = window.into();
    cx.update(|cx| window.update(cx, |_, window, cx| context(&request, window, cx)))
        .unwrap()
}
fn diagnostic(line: u32, start: u32, end: u32, message: &str) -> lsp::types::Diagnostic {
    lsp::types::Diagnostic {
        range: lsp::types::Range {
            start: lsp::types::Position {
                line,
                character: start,
            },
            end: lsp::types::Position {
                line,
                character: end,
            },
        },
        severity: Some(lsp::types::DiagnosticSeverity::ERROR),
        source: Some("pyright".into()),
        message: message.into(),
        ..Default::default()
    }
}

#[gpui::test]
fn the_active_editor_reports_cursor_selection_lines_around_and_diagnostics(
    cx: &mut TestAppContext,
) {
    let root = project_dir("editor");
    let window = open_workspace(&root, cx);
    window
        .update(cx, |workspace, window, cx| {
            workspace.open(&root.join("other.txt"), window, cx);
            workspace.open(&root.join("main.py"), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
        .update(cx, |workspace, _, cx| {
            let editor = workspace.active_editor().unwrap();
            editor.update(cx, |editor, cx| {
                let text = editor.text();
                // Lines 30–32 (1-based), the cursor ending on line 32.
                let start = text.find("x30 =").unwrap();
                let end = text.find("x32 =").unwrap() + 3;
                editor.select_text_range(start, end, cx);
                editor.set_diagnostics(
                    &[
                        diagnostic(31, 0, 3, "\"x32\" is not defined"),
                        diagnostic(2, 0, 2, "far away"),
                    ],
                    cx,
                );
            });
        })
        .unwrap();
    cx.run_until_parked();

    let reply = context_of(window, json!({ "around": 2, "max_lines": 2 }), cx);
    assert_eq!(reply["ok"], true);
    let editor = &reply["editor"];
    assert_eq!(editor["file"], "main.py");
    assert_eq!(editor["language"], "Python");
    assert_eq!(editor["dirty"], false);
    assert_eq!(editor["cursors"], json!([{ "line": 32, "column": 4 }]));
    let selection = &editor["selections"][0];
    assert_eq!(selection["start"], json!({ "line": 30, "column": 1 }));
    assert_eq!(selection["end"], json!({ "line": 32, "column": 4 }));
    // Three lines selected, two allowed.
    assert_eq!(selection["text"], "x30 = 30\nx31 = 31");
    assert_eq!(selection["truncated"], true);
    assert_eq!(
        editor["around"],
        json!({ "start_line": 30, "text": "x30 = 30\nx31 = 31\nx32 = 32\nx33 = 33\nx34 = 34", "truncated": false })
    );
    let diagnostics = editor["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 2);
    let at_cursor: Vec<(&Value, &Value)> = diagnostics
        .iter()
        .map(|d| (&d["line"], &d["at_cursor"]))
        .collect();
    assert!(at_cursor.contains(&(&json!(32), &json!(true))));
    assert!(at_cursor.contains(&(&json!(3), &json!(false))));
    let near = diagnostics.iter().find(|d| d["line"] == 32).unwrap();
    assert_eq!(near["message"], "\"x32\" is not defined");
    assert_eq!(near["source"], "pyright");
    assert_eq!(
        (&near["column"], &near["end_column"]),
        (&json!(1), &json!(4))
    );

    let tabs: Vec<(&Value, &Value)> = reply["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tab| (&tab["file"], &tab["active"]))
        .collect();
    assert_eq!(
        tabs,
        [
            (&json!("other.txt"), &json!(false)),
            (&json!("main.py"), &json!(true))
        ]
    );
    assert_eq!(reply["project"]["remote"], false);

    // No lines around the cursor on request; the Markdown names it all.
    let reply = context_of(window, json!({ "around": 0 }), cx);
    assert!(reply["editor"].get("around").is_none());
    let markdown = pinpoint_cli::context_markdown(&reply);
    // A trailing line break starts an empty 61st line.
    assert!(
        markdown.contains("## Editor: main.py (Python, 61 lines)"),
        "{markdown}"
    );
    assert!(markdown.contains("### Selection 30:1–32:4"), "{markdown}");
    assert!(
        markdown.contains("- 32:1 error [pyright]: \"x32\" is not defined ← at cursor"),
        "{markdown}"
    );
    cleanup(&root);
}

#[gpui::test]
fn a_terminal_reports_its_last_lines_and_exit_code(cx: &mut TestAppContext) {
    let root = project_dir("terminal");
    let window = open_workspace(&root, cx);
    cx.executor().allow_parking();
    window
        .update(cx, |workspace, window, cx| {
            let terminal = workspace.context_parts(window, cx).terminal;
            // Prints 1..=100, one per line, then exits with 3.
            let (program, args): (&str, &[&str]) = if cfg!(windows) {
                ("powershell", &["-NoProfile", "-Command", "1..100; exit 3"])
            } else {
                ("sh", &["-c", "seq 1 100; exit 3"])
            };
            let args: Vec<_> = args.iter().map(|&arg| arg.into()).collect();
            terminal.update(cx, |terminal, cx| {
                terminal
                    .run_program("count", program, &args, None, window, cx)
                    .unwrap();
            });
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let terminal = loop {
        std::thread::sleep(Duration::from_millis(50));
        cx.run_until_parked();
        let reply = context_of(window, json!({ "max_lines": 5 }), cx);
        let terminal = reply["terminal"].clone();
        if terminal["exit_code"] == 3 || Instant::now() > deadline {
            break terminal;
        }
    };
    assert_eq!(terminal["exit_code"], 3, "{terminal}");
    assert_eq!(terminal["running"], false);
    assert_eq!(terminal["run_name"], "count");
    assert_eq!(terminal["lines"], "96\n97\n98\n99\n100");
    assert_eq!(terminal["truncated"], true);
    cleanup(&root);
}

#[test]
fn texts_are_capped_by_lines_and_characters() {
    assert_eq!(cap_text("a\nb\nc", 2, false), ("a\nb".into(), true));
    assert_eq!(cap_text("a\nb\nc", 2, true), ("b\nc".into(), true));
    assert_eq!(cap_text("a\nb", 2, false), ("a\nb".into(), false));
    let long = "é".repeat(MAX_CHARS);
    let (head, cut) = cap_text(&long, 10, false);
    assert!(cut && head.len() <= MAX_CHARS && head.chars().all(|c| c == 'é'));
    let (tail, cut) = cap_text(&long, 10, true);
    assert!(cut && tail.len() <= MAX_CHARS && tail.chars().all(|c| c == 'é'));
}

#[gpui::test]
fn a_notebook_reports_the_notes_on_its_cells(cx: &mut TestAppContext) {
    let root = project_dir("notebook");
    let notebook = json!({
        "cells": [
            {"cell_type": "code", "metadata": {}, "outputs": [], "execution_count": null,
             "source": ["import os\n"]},
            {"cell_type": "code", "metadata": {}, "outputs": [], "execution_count": null,
             "source": ["x = 1\n", "y = x / 0\n"]}
        ],
        "metadata": {}, "nbformat": 4, "nbformat_minor": 5
    });
    std::fs::write(root.join("a.ipynb"), notebook.to_string()).unwrap();
    let anchor = ::notes::anchor_from("x = 1\ny = x / 0\n", 2, 2, None);
    let author = ::notes::Author::user("me");
    let to = Some("agent".into());
    let note = ::notes::Store::open(root.clone())
        .add("a.ipynb", Some(2), anchor, "divides by zero", author, to)
        .unwrap();
    let window = open_workspace(&root, cx);
    window
        .update(cx, |workspace, window, cx| {
            workspace.open(&root.join("a.ipynb"), window, cx)
        })
        .unwrap();
    cx.run_until_parked();
    let reply = context_of(window, json!({}), cx);
    let notes = &reply["notebook"]["notes"];
    assert_eq!(notes.as_array().map(Vec::len), Some(1), "{reply:#}");
    let fact = &notes[0];
    assert_eq!(fact["id"], note.id.as_str());
    assert_eq!(fact["cell"], 2);
    assert_eq!(
        (fact["line"].clone(), fact["end_line"].clone()),
        (json!(2), json!(2))
    );
    assert_eq!(fact["anchor"], "exact");
    assert_eq!(fact["status"], "open");
    assert_eq!(fact["to"], "agent");
    assert_eq!(fact["author"], "me");
    assert_eq!(fact["text"], "divides by zero");
    cleanup(&root);
}

#[test]
fn a_terminal_screen_keeps_its_tail() {
    let facts = TerminalFacts {
        title: "bash".into(),
        folder: None,
        run_name: None,
        screen: "$ pytest\nFAILED test_a.py::test_x\n$ \n\n\n".into(),
        exit: None,
        cwd: Some("/work/sub".into()),
        last_command_exit: Some(2),
    };
    let value = terminal_json(
        &facts,
        Limits {
            max_lines: 2,
            around: 0,
        },
    );
    assert_eq!(value["lines"], "FAILED test_a.py::test_x\n$");
    assert_eq!(value["truncated"], true);
    assert_eq!(value["running"], true);
    assert!(value.get("exit_code").is_none());
    assert_eq!(value["cwd"], "/work/sub");
    assert_eq!(value["last_command_exit_code"], 2);
}
