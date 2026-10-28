//! Notebook tabs in a workspace (`.ipynb`, marimo `.py`): opening files as
//! notebooks (views in several panes share one `NotebookDocument` and its
//! language-server document), Open as Text / Open as Notebook for marimo
//! files, reloading after external changes, Open in marimo and Install
//! ipykernel in the Terminal panel, going to language-server locations
//! inside notebooks, and what `next_code context` reports of one.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use gpui::{
    AnyEntity, App, AppContext, Context, Div, Entity, InteractiveElement, PromptLevel, Window,
};
use serde_json::{Value, json};
use watcher::{Batch, ChangeKind};

use super::{
    BUILTIN, NotebookDocument, RunState, launch,
    lsp::NotebookLsp,
    view::{NotebookView, NotebookViewEvent, OpenAsNotebook},
};
use crate::{
    builtins::{FileTabKind, FileTabView},
    layout::PanelId,
    pinpoint::{
        context::{Limits, MAX_ITEMS, cap_text, note_json, path_text},
        file_key,
    },
    project::Project,
    workspace::{OfferKind, Workspace},
};

pub(super) const TAB: FileTabKind = FileTabKind {
    kind: "notebook",
    opens,
    open,
};

/// What notebooks keep per workspace.
#[derive(Default)]
pub(super) struct Notebooks {
    /// marimo notebooks the user chose to see as text (Open as Text) this
    /// session: they open in text editors.
    text: HashSet<PathBuf>,
    /// Notebooks with an open "changed on disk" prompt, so it is asked
    /// once.
    reload_prompts: HashSet<PathBuf>,
    /// The file the Open as Notebook offer is about.
    offered: Option<PathBuf>,
}

pub(super) fn model(_: &mut Window, cx: &mut Context<Workspace>) -> AnyEntity {
    cx.new(|_| Notebooks::default()).into()
}

fn notebooks(workspace: &Workspace) -> Option<Entity<Notebooks>> {
    workspace.builtin_model()
}

pub(super) fn actions(element: Div, cx: &mut Context<Workspace>) -> Div {
    element.on_action(cx.listener(open_as_notebook))
}

/// Whether `path` (read as `text`) opens as a notebook: a Jupyter or
/// marimo file the user did not choose to see as text.
fn opens(workspace: &Workspace, path: &Path, text: Option<&str>, cx: &App) -> bool {
    if notebooks(workspace).is_some_and(|notebooks| notebooks.read(cx).text.contains(path)) {
        return false;
    }
    let Some(text) = text else {
        return true;
    };
    path.file_name()
        .is_some_and(|name| notebook::kind_of(&name.to_string_lossy(), Some(text)).is_some())
}

fn open(
    workspace: &mut Workspace,
    path: &Path,
    text: Option<&str>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Result<(), String> {
    open_notebook(workspace, path, text, window, cx).map(|_| ())
}

/// Opens (or activates) `path` as a notebook in the active pane: a file
/// open as a notebook in another pane gets a second view of the same
/// document. `text` is the file's contents when already read.
fn open_notebook(
    workspace: &mut Workspace,
    path: &Path,
    text: Option<&str>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Result<Entity<NotebookView>, String> {
    if let Some(tab) = workspace.activate_file_tab(path, TAB.kind, window, cx) {
        return tab.downcast().ok_or_else(|| "no notebook tab".to_owned());
    }
    let existing = workspace
        .builtin_views::<NotebookView>()
        .into_iter()
        .find(|(shown, _)| shown == path)
        .map(|(_, view)| {
            let view = view.read(cx);
            (view.document().clone(), view.language_server().cloned())
        });
    let (existing, language_server) = existing.unzip();
    let document = match existing {
        Some(document) => document,
        None => {
            let read;
            let text = match text {
                Some(text) => text,
                None => {
                    read = workspace
                        .project()
                        .read_text(path)
                        .map_err(|err| format!("Cannot open {}: {err}", path.display()))?;
                    &read
                }
            };
            NotebookDocument::create(workspace.project().clone(), path.to_path_buf(), text, cx)
                .map_err(|err| format!("Cannot open {} as a notebook: {err}", path.display()))?
        }
    };
    let language_server = language_server.flatten().unwrap_or_else(|| {
        let store = workspace.lsp_store().clone();
        let document = document.clone();
        cx.new(|cx| NotebookLsp::new(document, store, cx))
    });
    let view = cx.new(|cx| {
        let mut view = NotebookView::new(document, window, cx);
        view.set_language_server(language_server, cx);
        view
    });
    let subscription = cx.subscribe_in(&view, window, on_event);
    let tab = BUILTIN.file_tab(view.clone(), TAB.kind);
    workspace.open_file_tab(tab, path, subscription, window, cx);
    Ok(view)
}

fn on_event(
    workspace: &mut Workspace,
    view: &Entity<NotebookView>,
    event: &NotebookViewEvent,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    match event {
        NotebookViewEvent::Changed => cx.notify(),
        NotebookViewEvent::OpenAsText => open_as_text(workspace, view, window, cx),
        NotebookViewEvent::OpenInMarimo => {
            workspace.show_panel(PanelId::Terminal, cx);
            let ports = workspace.ports(cx);
            let terminal = workspace.terminal_panel();
            launch::open_in_marimo(view.clone(), terminal, ports, window, cx);
        }
        NotebookViewEvent::InstallKernel => {
            workspace.show_panel(PanelId::Terminal, cx);
            let terminal = workspace.terminal_panel();
            launch::install_kernel(view.clone(), terminal, window, cx);
        }
        NotebookViewEvent::Close { discard } => {
            workspace.close_view(view.entity_id(), *discard, window, cx)
        }
        NotebookViewEvent::Navigate { locations, title } => {
            workspace.navigate(locations.clone(), title.clone(), window, cx)
        }
        NotebookViewEvent::ApplyEdit {
            edit,
            server,
            command,
            label,
        } => workspace.apply_editor_edit(
            None,
            edit.clone(),
            server.clone(),
            command.clone(),
            label,
            cx,
        ),
    }
}

/// A language-server location in a notebook's virtual document: opens
/// the notebook at that cell. False when `location` is elsewhere.
pub(super) fn navigate(
    workspace: &mut Workspace,
    location: &language::Location,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let Some(document) = workspace
        .lsp_store()
        .read(cx)
        .virtual_document(&location.path)
    else {
        return false;
    };
    let (path, target) = {
        let document = document.borrow();
        (
            document.shown_path().to_path_buf(),
            document.part_at(location.range.start),
        )
    };
    match open_notebook(workspace, &path, None, window, cx) {
        Ok(view) => {
            if let Some((number, position)) = target {
                view.update(cx, |view, cx| view.go_to_cell(number, position, window, cx));
            }
        }
        Err(err) => workspace.report(err, cx),
    }
    true
}

/// Open as Text (marimo): a text editor replaces the file's notebook tabs
/// (its kernel stops); the file opens as text for the rest of the session,
/// or until Open as Notebook.
fn open_as_text(
    workspace: &mut Workspace,
    view: &Entity<NotebookView>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let path = view.read(cx).path(cx);
    if view.read(cx).is_dirty(cx) {
        return workspace.report(
            format!("Save {} before opening it as text", path.display()),
            cx,
        );
    }
    let Some(notebooks) = notebooks(workspace) else {
        return;
    };
    notebooks.update(cx, |notebooks, _| notebooks.text.insert(path));
    workspace.reopen_as_text(view.entity_id(), window, cx);
}

/// Open as Notebook: the file's text editors (without unsaved edits) make
/// way for a notebook tab.
fn reopen_as_notebook(
    workspace: &mut Workspace,
    path: &Path,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if workspace.editors_dirty(path, cx) {
        return workspace.report(
            format!("Save {} before opening it as a notebook", path.display()),
            cx,
        );
    }
    let Some(notebooks) = notebooks(workspace) else {
        return;
    };
    notebooks.update(cx, |notebooks, _| notebooks.text.remove(path));
    if let Err(err) = workspace.reopen_in_file_tab(path, &TAB, window, cx) {
        notebooks.update(cx, |notebooks, _| notebooks.text.insert(path.to_path_buf()));
        workspace.report(err, cx);
    }
}

/// Open as Notebook from the command palette: the active text editor's
/// file, if it is a notebook.
fn open_as_notebook(
    workspace: &mut Workspace,
    _: &OpenAsNotebook,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.sync_active_pane(window, cx);
    let Some((path, editor)) = workspace.active_file_editor() else {
        if workspace.active_builtin_view::<NotebookView>().is_none() {
            workspace.report("Open as Notebook works on a file's editor".into(), cx);
        }
        return;
    };
    let text = editor.read(cx).text();
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if notebook::kind_of(&name, Some(&text)).is_none() {
        return workspace.report(format!("{name} is not a Jupyter or marimo notebook"), cx);
    }
    reopen_as_notebook(workspace, &path, window, cx);
}

const OFFER_KEY: &str = "open-as-notebook";

fn offer() -> OfferKind {
    OfferKind::Builtin {
        extension: BUILTIN.id,
        key: OFFER_KEY.into(),
        action: "Open as Notebook",
    }
}

/// The Open as Notebook offer follows the active tab: shown for a text
/// editor of a notebook the user opened as text.
pub(super) fn tab_activated(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let Some(notebooks) = notebooks(workspace) else {
        return;
    };
    let path = workspace
        .active_file_editor()
        .map(|(path, _)| path)
        .filter(|path| notebooks.read(cx).text.contains(path));
    let before = notebooks.update(cx, |notebooks, _| {
        std::mem::replace(&mut notebooks.offered, path.clone())
    });
    match path {
        _ if before == path => {}
        Some(_) => workspace.offer(offer(), "This marimo notebook is open as text.", cx),
        None => workspace.withdraw_offer(&offer(), cx),
    }
}

pub(super) fn accept_offer(
    workspace: &mut Workspace,
    key: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let offered = notebooks(workspace).and_then(|notebooks| notebooks.read(cx).offered.clone());
    if key == OFFER_KEY
        && let Some(path) = offered
    {
        reopen_as_notebook(workspace, &path, window, cx);
    }
}

/// Reloads notebooks changed on disk: clean ones at once (our own save is
/// no change), dirty ones after asking.
pub(super) fn files_changed(
    workspace: &mut Workspace,
    batch: &Batch,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(notebooks) = notebooks(workspace) else {
        return;
    };
    let mut seen = HashSet::new();
    let affected: Vec<(Entity<NotebookView>, PathBuf, bool)> = workspace
        .builtin_views::<NotebookView>()
        .into_iter()
        .filter(|(path, _)| seen.insert(path.clone()))
        .filter_map(|(path, view)| {
            let removed = match batch.events.iter().find(|event| event.path == path) {
                Some(event) => event.kind == ChangeKind::Removed,
                None if batch.rescan => matches!(workspace.project().kind(&path), Ok(None)),
                None => return None,
            };
            Some((view, path, removed))
        })
        .collect();
    for (view, path, removed) in affected {
        if workspace.is_own_change(&path, cx) {
            continue;
        }
        if removed {
            workspace.report(format!("{} was deleted on disk", path.display()), cx);
            continue;
        }
        let Ok(text) = workspace.project().read_text(&path) else {
            continue;
        };
        let document = view.read(cx).document().clone();
        if !document.read(cx).changed_on_disk(&text) {
            continue;
        }
        if !document.read(cx).is_dirty() {
            if let Err(err) = document.update(cx, |document, cx| document.reload(&text, cx)) {
                workspace.report(format!("Could not reload {}: {err}", path.display()), cx);
            }
        } else if notebooks.update(cx, |notebooks, _| {
            notebooks.reload_prompts.insert(path.clone())
        }) {
            prompt_reload(notebooks.clone(), document, path, window, cx);
        }
    }
}

fn prompt_reload(
    notebooks: Entity<Notebooks>,
    document: Entity<NotebookDocument>,
    path: PathBuf,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let answer = window.prompt(
        PromptLevel::Warning,
        &format!("{name} changed on disk"),
        Some("You have unsaved changes. Reload the notebook from disk and lose them?"),
        &["Keep My Changes", "Reload"],
        cx,
    );
    cx.spawn_in(window, async move |this, cx| {
        let reload = answer.await == Ok(1);
        this.update_in(cx, |workspace, _, cx| {
            notebooks.update(cx, |notebooks, _| notebooks.reload_prompts.remove(&path));
            if !reload {
                return;
            }
            let result = workspace
                .project()
                .read_text(&path)
                .map_err(|err| err.to_string())
                .and_then(|text| document.update(cx, |document, cx| document.reload(&text, cx)));
            if let Err(err) = result {
                workspace.report(format!("Could not reload {}: {err}", path.display()), cx);
            }
        })
        .ok();
    })
    .detach();
}

impl FileTabView for NotebookView {
    fn is_dirty(&self, cx: &App) -> bool {
        self.is_dirty(cx)
    }

    fn save(&mut self, cx: &mut Context<Self>) -> bool {
        self.save_document(cx)
    }

    /// The kernel stops.
    fn release(&mut self, cx: &mut Context<Self>) {
        let document = self.document().clone();
        document.update(cx, |document, cx| document.shutdown(cx));
    }

    fn reveal_part(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        self.reveal_pinned_cell(index, cx)
    }

    /// The notebook and the cell under the cursor: its source, run state,
    /// outputs (as text) and error; and the notes on any cell.
    fn context(&self, project: &Project, limits: Limits, cx: &App) -> Value {
        json!({ "notebook": notebook_json(project, self, limits, cx) })
    }
}

fn notebook_json(project: &Project, view: &NotebookView, limits: Limits, cx: &App) -> Value {
    let document = view.document().read(cx);
    let path = view.path(cx);
    let mut value = json!({
        "file": file_key(project, &path),
        "path": path_text(project, &path),
        "dirty": view.is_dirty(cx),
        "cells": document.cells().len(),
    });
    // Notes on any cell, like an editor's (`cell` added; no `visible`).
    let notes: Vec<Value> = view
        .note_facts(cx)
        .iter()
        .take(MAX_ITEMS)
        .map(|(cell, note)| {
            let mut value = note_json(note, limits);
            value["cell"] = (*cell).into();
            value
        })
        .collect();
    if !notes.is_empty() {
        value["notes"] = notes.into();
    }
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
        "kind": cell.cell.kind.name(),
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
