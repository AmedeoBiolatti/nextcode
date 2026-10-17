//! Headless notebooks: `notebook_open`, `notebook_run`, `notebook_edit`,
//! `notebook_insert`, `notebook_delete`, `notebook_interrupt`,
//! `notebook_restart`, `notebook_save`, `notebook_close` and
//! `notebook_status`. The same [`NotebookDocument`] as a notebook tab, kept
//! open by path until `notebook_close` (or exit, which shuts every kernel
//! down).

use notebook::{CellKind, Output, ansi, outputs};

use super::*;
use crate::notebook::{
    document::{DocCell, KernelStatus, NotebookDocument, RunState},
    kernel::{self, KernelLaunch},
};

fn kind_name(kind: CellKind) -> &'static str {
    match kind {
        CellKind::Code => "code",
        CellKind::Markdown => "markdown",
        CellKind::Raw => "raw",
    }
}

fn output_json(output: &Output) -> Value {
    let text = outputs::plain_text(output);
    match output {
        Output::Stream { name, text } => {
            json!({ "output_type": "stream", "name": name, "text": text })
        }
        Output::DisplayData { data, .. } | Output::ExecuteResult { data, .. } => {
            let output_type = match output {
                Output::DisplayData { .. } => "display_data",
                _ => "execute_result",
            };
            json!({
                "output_type": output_type,
                "mimes": data.keys().collect::<Vec<_>>(),
                "text_plain": data.get("text/plain").and_then(Value::as_str),
                "text": text,
            })
        }
        Output::Error {
            ename,
            evalue,
            traceback,
        } => json!({
            "output_type": "error",
            "ename": ename,
            "evalue": evalue,
            "traceback": traceback.iter().map(|line| ansi::strip(line)).collect::<Vec<_>>(),
            "text": text,
        }),
    }
}

fn cell_json(index: usize, cell: &DocCell) -> Value {
    let status = match cell.run {
        RunState::Idle => "idle",
        RunState::Queued => "queued",
        RunState::Running { .. } => "running",
        RunState::Finished { ok: true, .. } => "ok",
        RunState::Finished { ok: false, .. } => "error",
    };
    json!({
        "index": index,
        "kind": kind_name(cell.cell.kind),
        "source": cell.cell.source,
        "execution_count": cell.cell.execution_count,
        "status": status,
        "stale": cell.stale,
        "error": cell.error,
        "outputs": cell.cell.outputs.iter().map(output_json).collect::<Vec<_>>(),
    })
}

fn cells_json(document: &NotebookDocument) -> Value {
    document
        .cells()
        .iter()
        .enumerate()
        .map(|(index, cell)| cell_json(index, cell))
        .collect()
}

fn kernel_json(document: &NotebookDocument) -> Value {
    let python = document.launch().map(|launch| launch.python.join(" "));
    let mut kernel = json!({ "python": python });
    let (status, message) = match document.kernel_status() {
        KernelStatus::NotStarted => ("not_started", None),
        KernelStatus::Starting => ("starting", None),
        KernelStatus::Idle => ("idle", None),
        KernelStatus::Busy => ("busy", None),
        KernelStatus::Restarting => ("restarting", None),
        KernelStatus::Dead(message) => ("dead", Some(message)),
        KernelStatus::Missing { python, modules } => {
            kernel["python"] = python.as_str().into();
            kernel["missing"] = json!(modules);
            ("missing", None)
        }
        KernelStatus::Failed(message) => ("failed", Some(message)),
    };
    kernel["status"] = status.into();
    if let Some(message) = message {
        kernel["message"] = message.as_str().into();
    }
    kernel
}

/// The cells that finished (ran, failed or were skipped) after `mark`.
fn finished_json(document: &NotebookDocument, mark: u64) -> Value {
    document
        .finished_since(mark)
        .map(|(index, cell)| cell_json(index, cell))
        .collect()
}

/// `value` as a cell index of `document`.
fn cell_index(document: &NotebookDocument, value: &Value) -> Result<usize, String> {
    let count = document.cells().len();
    value
        .as_u64()
        .and_then(|index| usize::try_from(index).ok())
        .filter(|&index| index < count)
        .ok_or_else(|| format!("not a cell index (the notebook has {count} cells): {value}"))
}

impl Session {
    /// `notebook_*` commands.
    pub(super) fn notebooks(
        &mut self,
        id: Value,
        command: &str,
        request: &Value,
        cx: &mut Context<Self>,
    ) {
        let path = match param_str(request, "path") {
            Ok(path) => self.path(request, path),
            Err(err) => return self.out.reply(&id, Err(err)),
        };
        if command == "notebook_open" {
            return self.notebook_open(id, path, request, cx);
        }
        let Some(document) = self.notebooks.get(&path).cloned() else {
            return self.out.reply(
                &id,
                Err(format!(
                    "{} is not open (`notebook_open` it first)",
                    path.display()
                )),
            );
        };
        let out = self.out.clone();
        match command {
            "notebook_run" | "notebook_restart" => {
                let started = document.update(cx, |document, cx| {
                    let mark = document.run_mark();
                    if command == "notebook_restart" {
                        let run_all = request["run_all"].as_bool().unwrap_or(false);
                        document.restart(run_all, cx);
                    } else if let Some(indices) = request["cells"].as_array() {
                        let mut ids = Vec::with_capacity(indices.len());
                        for index in indices {
                            let index = cell_index(document, index)?;
                            ids.push(document.cells()[index].id);
                        }
                        document.run(&ids, cx);
                    } else {
                        document.run_all(cx);
                    }
                    Ok((mark, document.when_idle()))
                });
                let (mark, idle) = match started {
                    Ok(started) => started,
                    Err(err) => return out.reply(&id, Err(err)),
                };
                cx.spawn(async move |_, cx| {
                    idle.await.ok();
                    let result = document
                        .read_with(cx, |document, _| {
                            json!({
                                "kernel": kernel_json(document),
                                "cells": finished_json(document, mark),
                            })
                        })
                        .map_err(|err| err.to_string());
                    out.reply(&id, result);
                })
                .detach();
            }
            "notebook_edit" => {
                let result = document.update(cx, |document, cx| {
                    let index = cell_index(document, &request["cell"])?;
                    let source = param_str(request, "source")?.to_owned();
                    let cell = document.cells()[index].id;
                    document.set_source(cell, source, None, cx);
                    Ok(cell_json(index, &document.cells()[index]))
                });
                out.reply(&id, result);
            }
            "notebook_insert" => {
                let result = document.update(cx, |document, cx| {
                    let kind = match request["kind"].as_str().unwrap_or("code") {
                        "code" => CellKind::Code,
                        "markdown" => CellKind::Markdown,
                        "raw" => CellKind::Raw,
                        other => return Err(format!("unknown cell kind `{other}`")),
                    };
                    let index = request["index"]
                        .as_u64()
                        .map_or(document.cells().len(), |index| index as usize);
                    let source = request["source"].as_str().unwrap_or_default().to_owned();
                    let cell = document.insert(index, kind, source, cx);
                    let index = document.index_of(cell).unwrap_or_default();
                    Ok(cell_json(index, &document.cells()[index]))
                });
                out.reply(&id, result);
            }
            "notebook_delete" => {
                let result = document.update(cx, |document, cx| {
                    let index = cell_index(document, &request["cell"])?;
                    let cell = document.cells()[index].id;
                    document.delete(&[cell], cx);
                    Ok(Value::Null)
                });
                out.reply(&id, result);
            }
            "notebook_interrupt" => {
                document.update(cx, |document, cx| document.interrupt(cx));
                out.reply(&id, Ok(Value::Null));
            }
            "notebook_save" => {
                let result = document.update(cx, |document, cx| document.save(cx));
                out.reply(&id, result.map(|()| Value::Null));
            }
            "notebook_close" => {
                self.notebooks.remove(&path);
                let stopped = document.update(cx, |document, cx| document.shutdown_and_wait(cx));
                cx.spawn(async move |_, _| {
                    stopped.await;
                    out.reply(&id, Ok(Value::Null));
                })
                .detach();
            }
            "notebook_status" => {
                let document = document.read(cx);
                out.reply(
                    &id,
                    Ok(json!({
                        "kernel": kernel_json(document),
                        "interpreter": document.launch().map(KernelLaunch::label),
                        "cells": cells_json(document),
                    })),
                );
            }
            other => out.reply(&id, Err(format!("unknown command `{other}`"))),
        }
    }

    /// `notebook_open`: reads and parses the file off the main thread (a
    /// missing file is a new notebook); `python` and `kernel_name` choose
    /// the kernel.
    fn notebook_open(&mut self, id: Value, path: PathBuf, request: &Value, cx: &mut Context<Self>) {
        let python = request["python"].as_str().map(str::to_owned);
        let kernel_name = request["kernel_name"].as_str().map(str::to_owned);
        let (project, out) = (self.project.clone(), self.out.clone());
        cx.spawn(async move |this, cx| {
            let opened = cx
                .background_spawn(async move {
                    let text = match project.read_text(&path) {
                        Ok(text) => text,
                        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
                        Err(err) => return Err(format!("Cannot read {}: {err}", path.display())),
                    };
                    let notebook = NotebookDocument::parse(&path, &text)?;
                    let launch = (python.is_some() || kernel_name.is_some()).then(|| {
                        let mut launch = match python {
                            Some(python) => KernelLaunch {
                                python: vec![python],
                                kernel_name: None,
                                cwd: path.parent().unwrap_or(project.root()).to_owned(),
                            },
                            None => kernel::default_launch(&project, &path),
                        };
                        launch.kernel_name = kernel_name;
                        launch
                    });
                    Ok((project, path, text, notebook, launch))
                })
                .await;
            let (project, path, text, notebook, launch) = match opened {
                Ok(opened) => opened,
                Err(err) => return out.reply(&id, Err(err)),
            };
            let result = this
                .update(cx, |this, cx| {
                    let document = cx.new(|cx| {
                        let mut document = NotebookDocument::from_notebook(
                            project,
                            path.clone(),
                            notebook,
                            text,
                            cx,
                        );
                        if let Some(launch) = launch {
                            document.set_launch(launch, cx);
                        }
                        document
                    });
                    let result = {
                        let document = document.read(cx);
                        json!({
                            "kind": if document.is_marimo() { "marimo" } else { "ipynb" },
                            "kernel": kernel_json(document),
                            "cells": cells_json(document),
                        })
                    };
                    // Reopening replaces the document (and its kernel).
                    if let Some(previous) = this.notebooks.insert(path, document) {
                        previous.update(cx, |previous, cx| previous.shutdown(cx));
                    }
                    result
                })
                .map_err(|err| err.to_string());
            out.reply(&id, result);
        })
        .detach();
    }

    /// Shuts every notebook's kernel down (headless exit).
    pub(super) fn close_notebooks(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let stopping: Vec<Task<()>> = self
            .notebooks
            .drain()
            .map(|(_, document)| document.update(cx, |document, cx| document.shutdown_and_wait(cx)))
            .collect();
        cx.background_spawn(async move {
            for stopped in stopping {
                stopped.await;
            }
        })
    }
}
