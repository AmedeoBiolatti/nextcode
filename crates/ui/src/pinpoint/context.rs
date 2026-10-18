//! `context`: what the person is looking at, for an agent that would
//! otherwise ask them to paste it: the focused area, the active editor
//! (cursor, selection, lines around it, diagnostics), open tabs, the
//! terminal's last lines, Problems, the Tests panel, the debugger, the
//! notebook cell and the workflow run. Read on request only; every list
//! and text is capped, and no file content is sent beyond the selections
//! and `around` lines around the cursor. Documented in
//! `docs/pinpoint-for-agents.md`; `pinpoint_cli::context_markdown`
//! renders the reply.

use std::{ops::Range, path::PathBuf};

use gpui::{App, ClipboardItem, Entity, Window, actions};
use serde_json::{Map, Value, json};
use text::Buffer;

use crate::{
    debugger::session::{Children, DebugSession},
    editor::Editor,
    layout::PanelId,
    notebook::{RunState, view::NotebookView},
    pinpoint::file_key,
    problems::ProblemsPanel,
    project::Project,
    shell::Shell,
    terminal_panel::TerminalPanel,
    test_explorer::TestsPanel,
    workspace::Workspace,
};

actions!(
    pinpoint,
    [
        /// Copies what the window shows (`context`, as Markdown) for pasting
        /// into an agent's prompt.
        CopyContextForAgent
    ]
);

/// Copy Context for Agent: the Markdown `next_code context` prints.
pub fn copy_to_clipboard(window: &Window, cx: &mut App) {
    let reply = context(&Value::Null, window, cx);
    let text = pinpoint_cli::context_markdown(&reply);
    cx.write_to_clipboard(ClipboardItem::new_string(text));
}

/// Lines of a selection, terminal tail, output or traceback by default.
pub const DEFAULT_MAX_LINES: usize = 40;
/// Lines shown above and below the cursor by default.
pub const DEFAULT_AROUND: usize = 20;
/// Characters of any one text (a long minified line is still one line).
const MAX_CHARS: usize = 8000;
/// Entries of any one list (diagnostics, problems, failures, tabs…).
const MAX_ITEMS: usize = 50;

/// Request options.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_lines: usize,
    pub around: usize,
}

impl Limits {
    pub fn of(request: &Value) -> Self {
        let number = |key: &str, default: usize| {
            request
                .get(key)
                .and_then(Value::as_u64)
                .map_or(default, |n| n as usize)
        };
        Self {
            max_lines: number("max_lines", DEFAULT_MAX_LINES),
            around: number("around", DEFAULT_AROUND),
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_lines: DEFAULT_MAX_LINES,
            around: DEFAULT_AROUND,
        }
    }
}

/// An editor's selections, view and diagnostics (`Editor::context_facts`).
pub struct EditorFacts {
    /// (anchor, head) byte offsets, primary first.
    pub selections: Vec<(usize, usize)>,
    /// 0-based buffer lines on screen.
    pub visible: Range<usize>,
    pub diagnostics: Vec<DiagnosticFact>,
}

pub struct DiagnosticFact {
    pub range: Range<usize>,
    pub severity: &'static str,
    pub message: String,
    pub source: Option<String>,
    pub code: Option<String>,
}

/// A terminal tab (`TerminalView::context_facts`).
pub struct TerminalFacts {
    pub title: String,
    pub folder: Option<String>,
    pub run_name: Option<String>,
    /// The visible screen, trailing blanks trimmed per line.
    pub screen: String,
    /// Exited, with its code when known.
    pub exit: Option<Option<i32>>,
}

pub struct TabPart {
    pub path: PathBuf,
    pub title: String,
    pub kind: &'static str,
    pub dirty: bool,
    pub active: bool,
}

pub enum ActiveView {
    Editor(Entity<Editor>),
    Notebook(Entity<NotebookView>),
    /// A diff or comparison.
    Other,
}

/// A workspace's views (`Workspace::context_parts`).
pub struct Parts {
    pub project: Project,
    pub tabs: Vec<TabPart>,
    pub active: Option<ActiveView>,
    /// `editor` or `panel:<key>`.
    pub focused: Option<String>,
    pub open_panels: Vec<PanelId>,
    pub terminal: Entity<TerminalPanel>,
    pub problems: Entity<ProblemsPanel>,
    pub tests: Entity<TestsPanel>,
    pub debug_session: Option<Entity<DebugSession>>,
}

/// The `context` reply for `window` (its shown project tab or workflow).
pub fn context(request: &Value, window: &Window, cx: &App) -> Value {
    let limits = Limits::of(request);
    let mut reply = json!({
        "ok": true,
        "window": window.window_handle().window_id().as_u64(),
        "title": window.window_title(),
    });
    let shell = window.root::<Shell>().flatten();
    if let Some(view) = shell.as_ref().and_then(|s| s.read(cx).active_workflow()) {
        reply["focus"] = "workflow".into();
        let (path, project, run) = view.read(cx).context_run(cx);
        reply["project"] = project_json(project);
        reply["workflow"] = workflow_json(view.read(cx).name(), path, project, run, limits);
        return reply;
    }
    let workspace = match shell {
        Some(shell) => shell.read(cx).active_workspace(),
        // Tests open a bare workspace window.
        None => window.root::<Workspace>().flatten(),
    };
    match workspace {
        Some(workspace) => {
            let parts = workspace.read(cx).context_parts(window, cx);
            merge(&mut reply, workspace_json(&parts, limits, cx));
        }
        None => reply["focus"] = Value::Null,
    }
    reply
}

fn merge(into: &mut Value, from: Value) {
    if let (Value::Object(into), Value::Object(from)) = (into, from) {
        into.extend(from);
    }
}

fn project_json(project: &Project) -> Value {
    let roots: Vec<Value> = project
        .roots()
        .iter()
        .map(|root| json!({ "name": root.name, "path": path_text(project, &root.path) }))
        .collect();
    json!({ "roots": roots, "remote": project.is_remote() })
}

/// An absolute path as the project's machine writes it.
fn path_text(project: &Project, path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    if project.is_remote() {
        text.replace('\\', "/")
    } else {
        text.into_owned()
    }
}

/// Everything a workspace shows: the JSON members of the reply.
pub fn workspace_json(parts: &Parts, limits: Limits, cx: &App) -> Value {
    let project = &parts.project;
    let mut reply = Map::new();
    reply.insert("project".into(), project_json(project));
    let focus = parts
        .focused
        .clone()
        .map(|focused| match (&parts.active, focused.as_str()) {
            (Some(ActiveView::Notebook(_)), "editor") => "notebook".to_owned(),
            _ => focused,
        });
    reply.insert("focus".into(), focus.into());
    match &parts.active {
        Some(ActiveView::Editor(editor)) => {
            let editor = editor.read(cx);
            reply.insert(
                "editor".into(),
                editor_json(project, editor, &editor.context_facts(), limits),
            );
        }
        Some(ActiveView::Notebook(view)) => {
            reply.insert("notebook".into(), notebook_json(project, view, limits, cx));
        }
        _ => {}
    }
    let tabs: Vec<Value> = parts
        .tabs
        .iter()
        .take(MAX_ITEMS)
        .map(|tab| {
            json!({
                "file": file_key(project, &tab.path),
                "title": tab.title,
                "kind": tab.kind,
                "dirty": tab.dirty,
                "active": tab.active,
            })
        })
        .collect();
    reply.insert("tabs".into(), tabs.into());
    reply.insert(
        "open_panels".into(),
        parts
            .open_panels
            .iter()
            .map(|panel| Value::from(panel.key()))
            .collect(),
    );
    let (view, count) = parts.terminal.read(cx).active_view();
    if let Some(view) = view {
        let mut terminal = terminal_json(&view.read(cx).context_facts(), limits);
        terminal["tabs"] = count.into();
        reply.insert("terminal".into(), terminal);
    }
    reply.insert(
        "problems".into(),
        problems_json(project, parts.problems.read(cx)),
    );
    if let Some(tests) = tests_json(project, parts.tests.read(cx), limits) {
        reply.insert("tests".into(), tests);
    }
    if let Some(session) = &parts.debug_session {
        reply.insert("debugger".into(), debugger_json(project, session.read(cx)));
    }
    Value::Object(reply)
}

/// `text` cut to `max_lines` lines (the first ones, or the last with
/// `tail`) and [`MAX_CHARS`]; whether anything was cut.
pub fn cap_text(text: &str, max_lines: usize, tail: bool) -> (String, bool) {
    let lines: Vec<&str> = text.lines().collect();
    let mut truncated = lines.len() > max_lines;
    let kept = if !truncated {
        &lines[..]
    } else if tail {
        &lines[lines.len() - max_lines..]
    } else {
        &lines[..max_lines]
    };
    let mut out = kept.join("\n");
    if out.len() > MAX_CHARS {
        truncated = true;
        out = if tail {
            let mut start = out.len() - MAX_CHARS;
            while !out.is_char_boundary(start) {
                start += 1;
            }
            out[start..].to_owned()
        } else {
            let mut end = MAX_CHARS;
            while !out.is_char_boundary(end) {
                end -= 1;
            }
            out[..end].to_owned()
        };
    }
    (out, truncated)
}

/// 1-based line and character column of `offset`.
fn line_col(buffer: &Buffer, offset: usize) -> (usize, usize) {
    let offset = offset.min(buffer.len());
    let line = buffer.line_of(offset);
    let column = buffer
        .slice(buffer.line_start(line)..offset)
        .chars()
        .count();
    (line + 1, column + 1)
}

fn position(buffer: &Buffer, offset: usize) -> Value {
    let (line, column) = line_col(buffer, offset);
    json!({ "line": line, "column": column })
}

pub fn editor_json(
    project: &Project,
    editor: &Editor,
    facts: &EditorFacts,
    limits: Limits,
) -> Value {
    let buffer = editor.buffer();
    let path = buffer.path().map(|path| path.to_path_buf());
    let mut value = json!({
        "file": path.as_deref().map(|path| file_key(project, path)),
        "path": path.as_deref().map(|path| path_text(project, path)),
        "language": editor.language().map(|language| language.name()),
        "dirty": buffer.is_dirty(),
        "line_count": buffer.line_count(),
    });
    let cursors: Vec<Value> = facts
        .selections
        .iter()
        .take(MAX_ITEMS)
        .map(|(_, head)| position(buffer, *head))
        .collect();
    value["cursors"] = cursors.into();
    let selections: Vec<Value> = facts
        .selections
        .iter()
        .filter(|(anchor, head)| anchor != head)
        .take(MAX_ITEMS)
        .map(|&(anchor, head)| {
            let range = anchor.min(head)..anchor.max(head);
            let (text, truncated) = cap_text(&buffer.slice(range.clone()), limits.max_lines, false);
            json!({
                "start": position(buffer, range.start),
                "end": position(buffer, range.end),
                "text": text,
                "truncated": truncated,
            })
        })
        .collect();
    value["selections"] = selections.into();
    if !facts.visible.is_empty() {
        value["visible_lines"] = json!([facts.visible.start + 1, facts.visible.end]);
    }
    let cursor = facts.selections.first().map_or(0, |(_, head)| *head);
    let cursor_line = buffer.line_of(cursor.min(buffer.len()));
    if limits.around > 0 && buffer.line_count() > 0 {
        let first = cursor_line.saturating_sub(limits.around);
        let last = (cursor_line + limits.around).min(buffer.line_count() - 1);
        let text = buffer.slice(buffer.line_start(first)..buffer.line_end(last));
        let (text, truncated) = cap_text(&text, 2 * limits.around + 1, false);
        value["around"] = json!({
            "start_line": first + 1,
            "text": text,
            "truncated": truncated,
        });
    }
    let primary = facts
        .selections
        .first()
        .map_or(0..0, |&(a, h)| a.min(h)..a.max(h));
    let (primary_first, primary_last) = (
        buffer.line_of(primary.start.min(buffer.len())),
        buffer.line_of(primary.end.min(buffer.len())),
    );
    let diagnostics: Vec<Value> = facts
        .diagnostics
        .iter()
        .take(MAX_ITEMS)
        .map(|diagnostic| {
            let (line, column) = line_col(buffer, diagnostic.range.start);
            let (end_line, end_column) = line_col(buffer, diagnostic.range.end);
            // At the cursor: on a line the primary selection covers.
            let at_cursor = line - 1 <= primary_last && end_line > primary_first;
            json!({
                "line": line,
                "column": column,
                "end_line": end_line,
                "end_column": end_column,
                "severity": diagnostic.severity,
                "message": cap_text(&diagnostic.message, 10, false).0,
                "source": diagnostic.source,
                "code": diagnostic.code,
                "at_cursor": at_cursor,
            })
        })
        .collect();
    value["diagnostics"] = diagnostics.into();
    value["diagnostics_total"] = facts.diagnostics.len().into();
    // Sticky notes where they are now (followed through unsaved edits).
    let notes: Vec<Value> = editor
        .note_facts()
        .iter()
        .take(MAX_ITEMS)
        .map(|note| {
            json!({
                "id": note.id,
                "line": note.start_line,
                "end_line": note.end_line,
                "anchor": note.state,
                "status": if note.open { "open" } else { "resolved" },
                "to": note.to,
                "author": note.author,
                "text": cap_text(&note.text, limits.max_lines, false).0,
                "replies": note.replies,
                "visible": note.start_line <= facts.visible.end
                    && note.end_line > facts.visible.start,
            })
        })
        .collect();
    if !notes.is_empty() {
        value["notes"] = notes.into();
    }
    value
}

pub fn terminal_json(facts: &TerminalFacts, limits: Limits) -> Value {
    let screen = facts.screen.trim_end_matches(['\n', ' ']);
    let (text, truncated) = cap_text(screen, limits.max_lines, true);
    let mut value = json!({
        "title": facts.title,
        "lines": text,
        "truncated": truncated,
        "running": facts.exit.is_none(),
    });
    if let Some(folder) = &facts.folder {
        value["folder"] = folder.as_str().into();
    }
    if let Some(name) = &facts.run_name {
        value["run_name"] = name.as_str().into();
    }
    if let Some(Some(code)) = facts.exit {
        value["exit_code"] = code.into();
    }
    value
}

fn problems_json(project: &Project, panel: &ProblemsPanel) -> Value {
    let (errors, warnings, problems) = panel.context_problems();
    let items: Vec<Value> = problems
        .iter()
        .take(MAX_ITEMS)
        .map(|(location, severity, message, code)| {
            json!({
                "file": file_key(project, &location.path),
                "line": location.range.start.line + 1,
                "column": location.range.start.character + 1,
                "severity": severity,
                "message": cap_text(message, 10, false).0,
                "code": code,
            })
        })
        .collect();
    json!({
        "errors": errors,
        "warnings": warnings,
        "total": problems.len(),
        "items": items,
    })
}

/// `None` before tests were found or run.
fn tests_json(project: &Project, panel: &TestsPanel, limits: Limits) -> Option<Value> {
    let results = panel.results();
    if results.is_empty() && panel.status_text().is_none() && !panel.is_busy() {
        return None;
    }
    let tree = panel.tree();
    let (mut passed, mut failed, mut skipped) = (0, 0, 0);
    let mut failures: Vec<(&String, &run::TestResult)> = Vec::new();
    for (id, result) in results {
        let is_test = tree
            .and_then(|tree| tree.get(id).map(|index| &tree.nodes[index]))
            .is_none_or(|node| node.kind == run::NodeKind::Test);
        if !is_test {
            continue;
        }
        match result.status {
            run::Status::Passed => passed += 1,
            run::Status::Failed | run::Status::Error => {
                failed += 1;
                failures.push((id, result));
            }
            run::Status::Skipped => skipped += 1,
            run::Status::NotRun | run::Status::Running => {}
        }
    }
    failures.sort_by(|a, b| a.0.cmp(b.0));
    let tail = (limits.max_lines / 2).max(5);
    let failures: Vec<Value> = failures
        .iter()
        .take(MAX_ITEMS)
        .map(|(id, result)| {
            let node = tree.and_then(|tree| tree.get(id).map(|index| &tree.nodes[index]));
            let file = node.and_then(|node| node.path.as_deref()).map(|path| {
                let path = std::path::Path::new(path);
                if path.is_absolute() {
                    file_key(project, path)
                } else {
                    path.to_string_lossy().replace('\\', "/")
                }
            });
            let mut value = json!({
                "id": id,
                "label": node.map(|node| node.label.as_str()),
                "file": file,
                "line": node.and_then(|node| node.line).map(|line| line + 1),
                "status": if result.status == run::Status::Error { "error" } else { "failed" },
                "message": result.message,
            });
            if let Some(output) = &result.output {
                let (text, truncated) = cap_text(output.trim_end(), tail, true);
                value["output_tail"] = text.into();
                value["output_truncated"] = truncated.into();
            }
            value
        })
        .collect();
    Some(json!({
        "status": panel.status_text(),
        "running": panel.is_busy(),
        "passed": passed,
        "failed": failed,
        "skipped": skipped,
        "failures": failures,
    }))
}

fn debugger_json(project: &Project, session: &DebugSession) -> Value {
    let mut value = json!({
        "name": session.name().to_string(),
        "state": session.state().label(),
    });
    if let Some(reason) = session.stop_reason() {
        value["stop_reason"] = reason.into();
    }
    if let Some(code) = session.exit_code() {
        value["exit_code"] = code.into();
    }
    if let Some(error) = session.error() {
        value["error"] = error.into();
    }
    let frames: Vec<Value> = session
        .frames()
        .iter()
        .enumerate()
        .take(10)
        .map(|(index, frame)| {
            let file = session
                .source_path(frame.source.as_ref())
                .map(|path| file_key(project, &path));
            json!({
                "name": frame.name,
                "file": file,
                "line": frame.line,
                "selected": index == session.selected_frame(),
            })
        })
        .collect();
    if let Some((path, line)) = session.frame_location(session.selected_frame()) {
        value["location"] = json!({ "file": file_key(project, &path), "line": line + 1 });
    }
    value["frames"] = frames.into();
    // Locals: the first cheap scope's variables already fetched for the
    // Debug panel (no new adapter requests).
    if let Some(scope) = session.scopes().iter().find(|scope| !scope.expensive)
        && let Some(Children::Loaded { variables, .. }) =
            session.children(scope.variables_reference)
    {
        let locals: Vec<Value> = variables
            .iter()
            .take(MAX_ITEMS)
            .map(|variable| {
                json!({
                    "name": variable.name,
                    "value": cap_text(&variable.value, 1, false).0,
                    "type": variable.type_name,
                })
            })
            .collect();
        value["scope"] = scope.name.as_str().into();
        value["locals"] = locals.into();
    }
    value
}

fn notebook_json(
    project: &Project,
    view: &Entity<NotebookView>,
    limits: Limits,
    cx: &App,
) -> Value {
    let view = view.read(cx);
    let document = view.document().read(cx);
    let path = view.path(cx);
    let mut value = json!({
        "file": file_key(project, &path),
        "path": path_text(project, &path),
        "dirty": view.is_dirty(cx),
        "cells": document.cells().len(),
    });
    let Some(id) = view.cursor_cell() else {
        return value;
    };
    let (Some(index), Some(cell)) = (document.index_of(id), document.cell(id)) else {
        return value;
    };
    let (source, truncated) = cap_text(&cell.cell.source, limits.max_lines, false);
    let run = match cell.run {
        RunState::Idle => "idle",
        RunState::Queued => "queued",
        RunState::Running { .. } => "running",
        RunState::Finished { ok: true, .. } => "ok",
        RunState::Finished { ok: false, .. } => "failed",
    };
    let mut active = json!({
        "index": index + 1,
        "kind": match cell.cell.kind {
            notebook::CellKind::Code => "code",
            notebook::CellKind::Markdown => "markdown",
            notebook::CellKind::Raw => "raw",
        },
        "source": source,
        "source_truncated": truncated,
        "execution_count": cell.cell.execution_count,
        "run": run,
    });
    let outputs: Vec<Value> = cell
        .cell
        .outputs
        .iter()
        .take(10)
        .map(|output| {
            let kind = match output {
                notebook::Output::Stream { name, .. } => name.as_str(),
                notebook::Output::DisplayData { .. } => "display_data",
                notebook::Output::ExecuteResult { .. } => "execute_result",
                notebook::Output::Error { .. } => "error",
            };
            let (text, truncated) = cap_text(
                &notebook::outputs::plain_text(output),
                limits.max_lines,
                true,
            );
            json!({ "type": kind, "text": text, "truncated": truncated })
        })
        .collect();
    active["outputs"] = outputs.into();
    if let Some(error) = cell.cell.outputs.iter().find_map(|output| match output {
        notebook::Output::Error { ename, evalue, .. } => Some(format!("{ename}: {evalue}")),
        _ => None,
    }) {
        active["error"] = error.into();
    }
    if let Some(error) = &cell.error {
        active["marimo_error"] = error.as_str().into();
    }
    value["cell"] = active;
    value
}

fn workflow_json(
    name: String,
    path: &std::path::Path,
    project: &Project,
    run: Option<&crate::workflows::ActiveRun>,
    limits: Limits,
) -> Value {
    let mut value = json!({
        "name": name,
        "file": file_key(project, path),
        "path": path_text(project, path),
    });
    let Some(active) = run else {
        value["outcome"] = "idle".into();
        return value;
    };
    let run = &active.run;
    value["outcome"] = run.outcome().name().into();
    let tail = (limits.max_lines / 4).max(3);
    let steps: Vec<Value> = run
        .nodes()
        .iter()
        .zip(run.steps())
        .take(MAX_ITEMS)
        .map(|(node, state)| {
            let mut step = json!({
                "id": node.path,
                "status": state.status.name(),
            });
            if !state.detail.is_empty() {
                step["detail"] = state.detail.as_str().into();
            }
            match &state.status {
                workflow::run::Status::Failed(error) => {
                    step["error"] = error.as_str().into();
                    let lines = state.last_lines(tail);
                    if !lines.is_empty() {
                        step["last_lines"] = lines.join("\n").into();
                    }
                }
                workflow::run::Status::Skipped(why) => step["reason"] = why.as_str().into(),
                _ => {}
            }
            step
        })
        .collect();
    value["steps"] = steps.into();
    value
}

#[cfg(test)]
mod tests;
