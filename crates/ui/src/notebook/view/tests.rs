//! Notebook tabs driven like a user: opening files, command- and
//! edit-mode keys, saving, closing (the kernel stops), and lazily created
//! cell editors.

use std::{
    io::{self, BufRead, BufReader, PipeReader, PipeWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use gpui::{Entity, Focusable, TestAppContext, WindowHandle};
use notebook::CellKind;
use serde_json::{Value, json};

use super::NotebookView;
use crate::{
    notebook::{
        KernelStatus,
        kernel::{Kernel, KernelLaunch},
    },
    project::Project,
    recent,
    workspace::Workspace,
};

const NOTEBOOK: &str = r##"{
 "cells": [
  {
   "cell_type": "markdown",
   "id": "intro",
   "metadata": {},
   "source": [
    "# Title\n",
    "Some *text*"
   ]
  },
  {
   "cell_type": "code",
   "execution_count": null,
   "id": "first",
   "metadata": {},
   "outputs": [],
   "source": [
    "x = 1\n",
    "print(x)"
   ]
  },
  {
   "cell_type": "code",
   "execution_count": null,
   "id": "second",
   "metadata": {},
   "outputs": [],
   "source": [
    "y = 2"
   ]
  }
 ],
 "metadata": {
  "kernelspec": {
   "display_name": "Python 3",
   "language": "python",
   "name": "python3"
  }
 },
 "nbformat": 4,
 "nbformat_minor": 5
}
"##;

const MARIMO: &str = r#"import marimo

__generated_with = "0.25.1"
app = marimo.App()


@app.cell
def _():
    x = 1
    return (x,)


if __name__ == "__main__":
    app.run()
"#;

fn project_dir(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = std::env::temp_dir().join(format!("nc-nbview-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    for (file, text) in files {
        std::fs::write(root.join(file), text).unwrap();
    }
    root
}

/// Removes the project and this test's state (session, layout, recents).
fn cleanup(root: &Path) {
    std::fs::remove_dir_all(root).ok();
    if let Some(state) = crate::layout::state_root() {
        std::fs::remove_dir_all(state).ok();
    }
    if let Some(recent) = recent::path() {
        std::fs::remove_file(recent).ok();
    }
}

fn open_workspace(root: &Path, cx: &mut TestAppContext) -> WindowHandle<Workspace> {
    cx.update(|cx| {
        crate::editor::bind_keys(cx);
        crate::workspace::bind_keys(cx);
        super::bind_keys(cx);
    });
    let project = Project::local(root.to_path_buf());
    let window = cx.add_window(|window, cx| Workspace::new(project, None, window, cx));
    cx.run_until_parked();
    window
}

fn open(window: WindowHandle<Workspace>, path: &Path, cx: &mut TestAppContext) {
    window
        .update(cx, |workspace, window, cx| workspace.open(path, window, cx))
        .unwrap();
    cx.run_until_parked();
}

fn keys(window: WindowHandle<Workspace>, keystrokes: &str, cx: &mut TestAppContext) {
    cx.simulate_keystrokes(window.into(), keystrokes);
    cx.run_until_parked();
}

fn notebook(
    window: WindowHandle<Workspace>,
    cx: &mut TestAppContext,
) -> Option<Entity<NotebookView>> {
    window
        .read_with(cx, |workspace, _| workspace.active_notebook())
        .unwrap()
}

fn kinds(view: &Entity<NotebookView>, cx: &mut TestAppContext) -> Vec<CellKind> {
    view.read_with(cx, |view, cx| {
        view.document()
            .read(cx)
            .cells()
            .iter()
            .map(|cell| cell.cell.kind)
            .collect()
    })
}

fn sources(view: &Entity<NotebookView>, cx: &mut TestAppContext) -> Vec<String> {
    view.read_with(cx, |view, cx| {
        view.document()
            .read(cx)
            .cells()
            .iter()
            .map(|cell| cell.cell.source.clone())
            .collect()
    })
}

/// The selected cell's index and the selected range.
fn selection(
    view: &Entity<NotebookView>,
    cx: &mut TestAppContext,
) -> (Option<usize>, std::ops::Range<usize>) {
    view.read_with(cx, |view, cx| view.selection(cx))
}

/// Whether focus is in the selected cell's editor (edit mode), else on
/// the view (command mode).
fn editing(
    window: WindowHandle<Workspace>,
    view: &Entity<NotebookView>,
    cx: &mut TestAppContext,
) -> Option<bool> {
    let view = view.clone();
    window
        .update(cx, |_, window, cx| {
            let view = view.read(cx);
            let in_editor = view
                .cursor
                .and_then(|id| view.cell_editor(id))
                .is_some_and(|editor| editor.focus_handle(cx).is_focused(window));
            if in_editor {
                Some(true)
            } else if view.focus_handle.is_focused(window) {
                Some(false)
            } else {
                None
            }
        })
        .unwrap()
}

fn dirty(window: WindowHandle<Workspace>, cx: &mut TestAppContext) -> usize {
    window
        .read_with(cx, |workspace, cx| workspace.dirty_count(cx))
        .unwrap()
}

#[gpui::test]
fn notebooks_open_in_notebook_tabs_and_plain_python_in_editors(cx: &mut TestAppContext) {
    let root = project_dir(
        "route",
        &[
            ("nb.ipynb", NOTEBOOK),
            ("app.py", MARIMO),
            ("plain.py", "print('hello')\n"),
        ],
    );
    let window = open_workspace(&root, cx);

    open(window, &root.join("nb.ipynb"), cx);
    let view = notebook(window, cx).expect("a notebook tab for .ipynb");
    assert_eq!(
        kinds(&view, cx),
        [CellKind::Markdown, CellKind::Code, CellKind::Code]
    );

    open(window, &root.join("app.py"), cx);
    let marimo = notebook(window, cx).expect("a notebook tab for a marimo file");
    assert!(marimo.read_with(cx, |view, cx| view.document().read(cx).is_marimo()));

    open(window, &root.join("plain.py"), cx);
    assert!(notebook(window, cx).is_none(), "a plain .py opens as text");
    let editor = window
        .read_with(cx, |workspace, _| workspace.active_editor())
        .unwrap();
    assert!(editor.is_some());

    // Opening the notebook again activates its tab.
    open(window, &root.join("nb.ipynb"), cx);
    assert_eq!(notebook(window, cx), Some(view));
    let tabs = window
        .read_with(cx, |workspace, _| workspace.tab_paths())
        .unwrap();
    assert_eq!(tabs.len(), 3);

    // Open as Text: the marimo file becomes a text tab, and back.
    open(window, &root.join("app.py"), cx);
    cx.dispatch_action(window.into(), super::OpenAsText);
    cx.run_until_parked();
    assert!(notebook(window, cx).is_none());
    let text = window
        .read_with(cx, |workspace, cx| {
            workspace
                .active_editor()
                .map(|editor| editor.read(cx).text())
        })
        .unwrap();
    assert_eq!(text.as_deref(), Some(MARIMO));
    cx.dispatch_action(window.into(), super::OpenAsNotebook);
    cx.run_until_parked();
    assert!(notebook(window, cx).is_some(), "Open as Notebook");
    cleanup(&root);
}

#[gpui::test]
fn command_mode_keys_edit_the_notebook(cx: &mut TestAppContext) {
    let root = project_dir("keys", &[("nb.ipynb", NOTEBOOK)]);
    let window = open_workspace(&root, cx);
    open(window, &root.join("nb.ipynb"), cx);
    let view = notebook(window, cx).unwrap();
    use CellKind::{Code, Markdown};
    assert_eq!(editing(window, &view, cx), Some(false), "command mode");
    assert_eq!(selection(&view, cx), (Some(0), 0..1));

    // B inserts a code cell below and selects it; DD deletes it; Z undoes.
    keys(window, "b", cx);
    assert_eq!(kinds(&view, cx), [Markdown, Code, Code, Code]);
    assert_eq!(sources(&view, cx)[1], "");
    assert_eq!(selection(&view, cx).0, Some(1));
    keys(window, "a", cx);
    assert_eq!(kinds(&view, cx).len(), 5);
    assert_eq!(selection(&view, cx).0, Some(1));
    keys(window, "d d", cx);
    keys(window, "d d", cx);
    assert_eq!(
        sources(&view, cx),
        ["# Title\nSome *text*", "x = 1\nprint(x)", "y = 2"]
    );
    assert_eq!(
        selection(&view, cx).0,
        Some(1),
        "the next cell takes the selection"
    );
    keys(window, "z", cx);
    assert_eq!(kinds(&view, cx).len(), 4);
    keys(window, "shift-z", cx);
    assert_eq!(kinds(&view, cx).len(), 3);

    // M / Y change the kind; J / K and Shift+J move and extend.
    keys(window, "home", cx);
    keys(window, "j", cx);
    keys(window, "m", cx);
    assert_eq!(kinds(&view, cx), [Markdown, Markdown, Code]);
    keys(window, "y", cx);
    assert_eq!(kinds(&view, cx), [Markdown, Code, Code]);
    keys(window, "k", cx);
    assert_eq!(selection(&view, cx), (Some(0), 0..1));
    keys(window, "j shift-j", cx);
    assert_eq!(selection(&view, cx), (Some(2), 1..3));

    // C copies, V pastes below, X cuts.
    keys(window, "c", cx);
    let clipboard = cx.read_from_clipboard().and_then(|item| item.text());
    assert_eq!(clipboard.as_deref(), Some("x = 1\nprint(x)\n\ny = 2"));
    keys(window, "v", cx);
    assert_eq!(
        sources(&view, cx),
        [
            "# Title\nSome *text*",
            "x = 1\nprint(x)",
            "y = 2",
            "x = 1\nprint(x)",
            "y = 2"
        ]
    );
    assert_eq!(selection(&view, cx), (Some(4), 3..5), "the pasted cells");
    keys(window, "x", cx);
    assert_eq!(sources(&view, cx).len(), 3);
    assert_eq!(
        selection(&view, cx),
        (Some(2), 2..3),
        "the cell where the cut ones were"
    );
    keys(window, "shift-v", cx);
    assert_eq!(
        sources(&view, cx),
        [
            "# Title\nSome *text*",
            "x = 1\nprint(x)",
            "x = 1\nprint(x)",
            "y = 2",
            "y = 2"
        ],
        "pasted above"
    );
    keys(window, "z", cx);
    assert_eq!(sources(&view, cx).len(), 3);

    // Enter edits the selected cell; Escape is command mode again.
    keys(window, "home j", cx);
    keys(window, "enter", cx);
    assert_eq!(editing(window, &view, cx), Some(true));
    keys(window, "escape", cx);
    assert_eq!(editing(window, &view, cx), Some(false));

    // Enter on Markdown shows its editor; Escape renders it again.
    keys(window, "home enter", cx);
    let markdown = view.read_with(cx, |view, cx| view.document().read(cx).cells()[0].id);
    assert!(view.read_with(cx, |view, _| view.is_editing_markdown(markdown)));
    keys(window, "escape", cx);
    assert!(!view.read_with(cx, |view, _| view.is_editing_markdown(markdown)));

    // Ctrl+Shift+- splits the edited cell at the cursor.
    keys(window, "j enter", cx);
    let first = view.read_with(cx, |view, cx| view.document().read(cx).cells()[1].id);
    let editor = view
        .read_with(cx, |view, _| view.cell_editor(first))
        .unwrap();
    editor.update(cx, |editor, cx| editor.set_cursor("x = 1\n".len(), cx));
    keys(window, "ctrl-shift--", cx);
    assert_eq!(
        sources(&view, cx),
        ["# Title\nSome *text*", "x = 1", "print(x)", "y = 2"]
    );
    assert_eq!(
        editing(window, &view, cx),
        Some(true),
        "editing the new cell"
    );
    assert_eq!(selection(&view, cx).0, Some(2));

    // Shift+Enter on the last cell (Markdown: nothing runs) inserts a code
    // cell at the end and edits it.
    keys(window, "escape end m", cx);
    keys(window, "shift-enter", cx);
    assert_eq!(kinds(&view, cx), [Markdown, Code, Code, Markdown, Code]);
    assert_eq!(selection(&view, cx).0, Some(4));
    assert_eq!(editing(window, &view, cx), Some(true));
    // ... and on another cell moves the selection down in command mode.
    keys(window, "escape home", cx);
    keys(window, "shift-enter", cx);
    assert_eq!(selection(&view, cx).0, Some(1));
    assert_eq!(editing(window, &view, cx), Some(false));

    // Typing in a cell edits the document: the tab is dirty; Ctrl+S saves.
    keys(window, "enter", cx);
    let edited = view.read_with(cx, |view, cx| view.document().read(cx).cells()[1].id);
    let editor = view
        .read_with(cx, |view, _| view.cell_editor(edited))
        .unwrap();
    editor.update(cx, |editor, cx| editor.set_cursor(0, cx));
    cx.simulate_input(window.into(), "z = 0\n");
    cx.run_until_parked();
    assert_eq!(sources(&view, cx)[1], "z = 0\nx = 1");
    assert_eq!(dirty(window, cx), 1);
    keys(window, "escape ctrl-s", cx);
    assert_eq!(dirty(window, cx), 0);
    let saved = std::fs::read_to_string(root.join("nb.ipynb")).unwrap();
    let saved: Value = serde_json::from_str(&saved).unwrap();
    let cells = saved["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 5);
    assert_eq!(cells[1]["source"], json!(["z = 0\n", "x = 1"]));
    assert_eq!(cells[2]["source"], json!(["print(x)"]));
    cleanup(&root);
}

#[gpui::test]
fn a_large_notebook_creates_editors_only_for_the_cells_drawn(cx: &mut TestAppContext) {
    let cells: Vec<Value> = (0..500)
        .map(|index| {
            json!({
                "cell_type": "code",
                "execution_count": index + 1,
                "id": format!("cell-{index}"),
                "metadata": {},
                "outputs": [{
                    "name": "stdout",
                    "output_type": "stream",
                    "text": (0..20).map(|line| format!("line {line} of cell {index}\n")).collect::<Vec<_>>(),
                }],
                "source": [format!("print({index})")],
            })
        })
        .collect();
    let text = serde_json::to_string_pretty(&json!({
        "cells": cells,
        "metadata": {},
        "nbformat": 4,
        "nbformat_minor": 5,
    }))
    .unwrap();
    let root = project_dir("large", &[("big.ipynb", &text)]);
    let window = open_workspace(&root, cx);
    open(window, &root.join("big.ipynb"), cx);
    let view = notebook(window, cx).unwrap();
    let drawn = view.read_with(cx, |view, _| view.editor_count());
    assert!(drawn > 0, "the visible cells have editors");
    assert!(drawn < 50, "{drawn} editors for the first screen");

    // Jumping to the end draws the last cells only.
    keys(window, "end", cx);
    cx.run_until_parked();
    let drawn = view.read_with(cx, |view, _| view.editor_count());
    let last = view.read_with(cx, |view, cx| view.document().read(cx).cells()[499].id);
    assert!(
        view.read_with(cx, |view, _| view.cell_editor(last).is_some()),
        "the last cell was drawn"
    );
    assert!(drawn < 100, "{drawn} editors after jumping to the end");
    cleanup(&root);
}

/// Drop guard of the fake bridge: set once the kernel is gone.
struct Stopped(Arc<AtomicBool>);

impl Drop for Stopped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A bridge answering every request: `hello`, then for `execute` a busy
/// status, the code echoed on stdout, the reply and idle.
fn fake_bridge(stdin: PipeReader, mut stdout: PipeWriter) {
    let mut send = |line: Value| {
        writeln!(stdout, "{line}").ok();
        stdout.flush().ok();
    };
    let mut lines = BufReader::new(stdin).lines();
    lines.next();
    send(json!({ "event": "hello", "python": "/fake/python", "version": "3.13", "missing": [] }));
    let mut count = 0;
    for line in lines.map_while(Result::ok) {
        let request: Value = serde_json::from_str(&line).unwrap();
        let id = request["id"].clone();
        match request["op"].as_str().unwrap_or_default() {
            "execute" => {
                count += 1;
                send(json!({ "event": "status", "state": "busy", "request": id }));
                send(json!({ "event": "execute_input", "request": id, "execution_count": count }));
                send(json!({
                    "event": "output", "request": id, "msg_type": "stream",
                    "content": { "name": "stdout", "text": "ran\n" },
                }));
                send(
                    json!({ "reply": id, "ok": true, "result": { "status": "ok", "execution_count": count } }),
                );
                send(json!({ "event": "status", "state": "idle", "request": id }));
            }
            "shutdown" => {
                send(json!({ "reply": id, "ok": true, "result": null }));
                return;
            }
            _ => send(json!({ "reply": id, "ok": true, "result": {} })),
        }
    }
}

#[gpui::test]
fn closing_the_last_tab_of_a_notebook_stops_its_kernel(cx: &mut TestAppContext) {
    let root = project_dir("close", &[("nb.ipynb", NOTEBOOK)]);
    let window = open_workspace(&root, cx);
    open(window, &root.join("nb.ipynb"), cx);
    let view = notebook(window, cx).unwrap();
    let document = view.read_with(cx, |view, _| view.document().clone());
    let stopped = Arc::new(AtomicBool::new(false));
    let guard = stopped.clone();
    let cwd = root.clone();
    document.update(cx, |document, cx| {
        document.set_spawner(Arc::new(move |_, _| {
            let (bridge_in, kernel_out) = io::pipe()?;
            let (kernel_in, bridge_out) = io::pipe()?;
            thread::spawn(move || fake_bridge(bridge_in, bridge_out));
            Kernel::from_pipes(
                Box::new(kernel_out),
                Box::new(kernel_in),
                Box::new(Stopped(guard.clone())),
            )
        }));
        document.set_launch(
            KernelLaunch {
                python: vec!["python".into()],
                kernel_name: None,
                cwd,
            },
            cx,
        );
    });

    // Run the first code cell (Ctrl+Enter in command mode).
    keys(window, "j ctrl-enter", cx);
    for _ in 0..1000 {
        cx.run_until_parked();
        let done = document.read_with(cx, |document, _| {
            !document.is_busy() && document.cells()[1].cell.execution_count.is_some()
        });
        if done {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let (count, status) = document.read_with(cx, |document, _| {
        (
            document.cells()[1].cell.execution_count,
            document.kernel_status().clone(),
        )
    });
    assert_eq!(count, Some(1));
    assert_eq!(status, KernelStatus::Idle);

    // A second view (split) keeps the kernel; closing the last one stops it.
    keys(window, "ctrl-\\", cx);
    assert_ne!(notebook(window, cx), Some(view.clone()), "another view");
    keys(window, "ctrl-w", cx);
    assert!(!stopped.load(Ordering::SeqCst));
    keys(window, "ctrl-w", cx);
    // Outputs are part of an .ipynb: running made it dirty.
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Don't Save");
    cx.run_until_parked();
    for _ in 0..200 {
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        cx.run_until_parked();
        thread::sleep(Duration::from_millis(5));
    }
    assert!(stopped.load(Ordering::SeqCst), "the kernel stopped");
    assert!(notebook(window, cx).is_none());
    cleanup(&root);
}
