//! Notebook commands that run in the project's Terminal panel, where their
//! output stays visible: Open in marimo (`marimo edit`, whose URL is
//! opened in the browser, through a forwarded port for WSL and container
//! projects) and Install ipykernel. The terminal's output is followed
//! through an [`OutputTap`] (lines, then the exit code).

use std::{
    net::TcpListener,
    path::{Path, PathBuf},
    sync::Arc,
};

use devcontainer::ports::ForwardPort;
use futures::{
    StreamExt,
    channel::mpsc::{self, UnboundedReceiver},
};
use gpui::{App, Entity, Window};
use parking_lot::Mutex;
use terminal::{Output, OutputTap};

use super::{
    kernel::{KernelLaunch, default_launch, install_command},
    view::NotebookView,
};
use crate::{
    ports::{Origin, Ports},
    project::Project,
    terminal_panel::TerminalPanel,
};

/// Longest output line kept while waiting for its end.
const MAX_LINE: usize = 64 * 1024;

enum TapEvent {
    Line(String),
    Exited(Option<i32>),
}

/// A tap sending the program's output lines (ANSI stripped) and its end.
fn line_tap() -> (OutputTap, UnboundedReceiver<TapEvent>) {
    let (tx, rx) = mpsc::unbounded();
    let pending = Mutex::new(Vec::<u8>::new());
    let tap: OutputTap = Arc::new(move |output| match output {
        Output::Data(data) => {
            let mut pending = pending.lock();
            pending.extend_from_slice(data);
            while let Some(end) = pending
                .iter()
                .position(|byte| *byte == b'\n' || *byte == b'\r')
                .or_else(|| (pending.len() > MAX_LINE).then_some(MAX_LINE - 1))
            {
                let line: Vec<u8> = pending.drain(..=end).collect();
                let text = notebook::ansi::strip(&String::from_utf8_lossy(&line));
                let text = text.trim();
                if !text.is_empty() {
                    tx.unbounded_send(TapEvent::Line(text.to_owned())).ok();
                }
            }
        }
        Output::Exited(code) => {
            tx.unbounded_send(TapEvent::Exited(code)).ok();
        }
    });
    (tap, rx)
}

/// The URL marimo prints when it is ready (`➜  URL: http://localhost:2718?access_token=…`).
pub fn marimo_url(line: &str) -> Option<&str> {
    let start = line.find("http://").or_else(|| line.find("https://"))?;
    let url = line[start..].split_whitespace().next()?;
    Some(url)
}

/// The port of an `http://host:port/…` URL.
pub fn url_port(url: &str) -> Option<u16> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    authority.rsplit_once(':')?.1.parse().ok()
}

/// `url` with its host and port replaced by `localhost:local`.
pub fn with_local_port(url: &str, local: u16) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    format!("{scheme}://localhost:{local}{}", &rest[end..])
}

/// How `marimo edit` runs for the notebook (blocks): the `marimo` of the
/// notebook's virtualenv (or the project's), else `uv run --with marimo`
/// in a uv project, else `python -m marimo` with the notebook's
/// interpreter. A local project gets a free port; elsewhere marimo picks
/// one (its default, else the next free).
fn marimo_command(
    project: &Project,
    notebook: &Path,
    launch: Option<KernelLaunch>,
) -> (String, Vec<String>) {
    let dir = notebook.parent().unwrap_or(project.root());
    let mut tail = vec![
        "edit".to_owned(),
        notebook.to_string_lossy().into_owned(),
        "--headless".to_owned(),
    ];
    if !project.is_remote()
        && let Ok(listener) = TcpListener::bind(("127.0.0.1", 0))
        && let Ok(address) = listener.local_addr()
    {
        tail.extend(["--port".to_owned(), address.port().to_string()]);
    }
    if let Ok(Some(marimo)) = project.find_executable_in("marimo", dir) {
        return (marimo.to_string_lossy().into_owned(), tail);
    }
    let uv_project = matches!(
        project.kind(&project.resolve("pyproject.toml")),
        Ok(Some(worktree::FileKind::File))
    );
    if uv_project && let Ok(Some(uv)) = project.find_executable("uv") {
        let mut args: Vec<String> = ["run", "--with", "marimo", "marimo"]
            .map(str::to_owned)
            .to_vec();
        args.extend(tail);
        return (uv.to_string_lossy().into_owned(), args);
    }
    let launch = launch.unwrap_or_else(|| default_launch(project, notebook));
    let mut python = launch.python.into_iter();
    let program = python.next().unwrap_or_else(|| "python3".to_owned());
    let mut args: Vec<String> = python.collect();
    args.extend(["-m".to_owned(), "marimo".to_owned()]);
    args.extend(tail);
    (program, args)
}

fn document_parts(
    view: &Entity<NotebookView>,
    cx: &App,
) -> (Project, PathBuf, Option<KernelLaunch>) {
    let document = view.read(cx).document().read(cx);
    (
        document.project().clone(),
        document.path().to_path_buf(),
        document.launch().cloned(),
    )
}

/// Open in marimo: starts `marimo edit` in a terminal tab, waits for its
/// URL, forwards its port for a remote project, and opens it in the
/// browser. Progress and errors show in `view`.
pub fn open_in_marimo(
    view: Entity<NotebookView>,
    terminal: Entity<TerminalPanel>,
    ports: Entity<Ports>,
    window: &mut Window,
    cx: &mut App,
) {
    let (project, path, launch) = document_parts(&view, cx);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let status = |text: String, error: bool, cx: &mut App| {
        view.update(cx, |view, cx| view.set_task_status(Some((text, error)), cx));
    };
    status("Starting marimo… (see the Terminal)".into(), false, cx);
    let view = view.downgrade();
    window
        .spawn(cx, async move |cx| {
            let remote = project.is_remote();
            let (program, args) = cx
                .background_executor()
                .spawn({
                    let project = project.clone();
                    async move { marimo_command(&project, &path, launch) }
                })
                .await;
            let (tap, mut events) = line_tap();
            let title = format!("marimo: {name}");
            let started = cx.update(|window, cx| {
                terminal.update(cx, |terminal, cx| {
                    terminal.run_program(&title, &program, &args, Some(tap), window, cx)
                })
            });
            let report = |text: String, error: bool, cx: &mut gpui::AsyncWindowContext| {
                view.update(cx, |view, cx| view.set_task_status(Some((text, error)), cx))
                    .ok();
            };
            match started {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => return report(err, true, cx),
                Err(_) => return,
            }
            let mut opened = false;
            let mut last = String::new();
            while let Some(event) = events.next().await {
                match event {
                    TapEvent::Line(line) if !opened => {
                        let Some(url) = marimo_url(&line).map(str::to_owned) else {
                            last = line;
                            continue;
                        };
                        opened = true;
                        let url = match (remote, url_port(&url)) {
                            (true, Some(port)) => {
                                let target = ForwardPort { host: None, port };
                                let forwarded = cx.update(|_, cx| {
                                    ports.update(cx, |ports, cx| {
                                        ports.forward(target, Origin::Auto, None, cx)
                                    })
                                });
                                match forwarded {
                                    Ok(Ok(local)) => with_local_port(&url, local),
                                    Ok(Err(err)) => {
                                        report(
                                            format!("marimo runs at {url}, but {err}"),
                                            true,
                                            cx,
                                        );
                                        continue;
                                    }
                                    Err(_) => return,
                                }
                            }
                            _ => url,
                        };
                        cx.update(|_, cx| cx.open_url(&url)).ok();
                        report(format!("marimo is open in the browser: {url}"), false, cx);
                    }
                    TapEvent::Line(_) => {}
                    TapEvent::Exited(code) => {
                        let how = match code {
                            Some(code) => format!("exit code {code}"),
                            None => "ended".to_owned(),
                        };
                        if opened {
                            report(format!("marimo stopped ({how})"), false, cx);
                        } else {
                            let detail = if last.is_empty() {
                                String::new()
                            } else {
                                format!(": {last}")
                            };
                            report(
                                format!("marimo exited before it was ready ({how}){detail}"),
                                true,
                                cx,
                            );
                        }
                        break;
                    }
                }
            }
        })
        .detach();
}

/// Install ipykernel: runs `install_command` in a terminal tab and reports
/// its outcome in `view` (the user runs a cell again afterwards).
pub fn install_kernel(
    view: Entity<NotebookView>,
    terminal: Entity<TerminalPanel>,
    window: &mut Window,
    cx: &mut App,
) {
    let (project, path, launch) = document_parts(&view, cx);
    view.update(cx, |view, cx| {
        view.set_task_status(
            Some(("Installing ipykernel… (see the Terminal)".into(), false)),
            cx,
        )
    });
    let view = view.downgrade();
    window
        .spawn(cx, async move |cx| {
            let (program, args) = cx
                .background_executor()
                .spawn(async move {
                    let launch = launch.unwrap_or_else(|| default_launch(&project, &path));
                    install_command(&project, &path, &launch)
                })
                .await;
            let (tap, mut events) = line_tap();
            let started = cx.update(|window, cx| {
                terminal.update(cx, |terminal, cx| {
                    terminal.run_program(
                        "Install ipykernel",
                        &program,
                        &args,
                        Some(tap),
                        window,
                        cx,
                    )
                })
            });
            let report = |text: String, error: bool, cx: &mut gpui::AsyncWindowContext| {
                view.update(cx, |view, cx| view.set_task_status(Some((text, error)), cx))
                    .ok();
            };
            match started {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => return report(err, true, cx),
                Err(_) => return,
            }
            while let Some(event) = events.next().await {
                if let TapEvent::Exited(code) = event {
                    match code {
                        Some(0) => report(
                            "ipykernel is installed: restart the kernel or run a cell".into(),
                            false,
                            cx,
                        ),
                        Some(code) => report(
                            format!(
                                "Installing ipykernel failed (exit code {code}): see the Terminal"
                            ),
                            true,
                            cx,
                        ),
                        None => report("Installing ipykernel was stopped".into(), true, cx),
                    }
                    break;
                }
            }
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_and_rewrites_marimo_urls() {
        let line = "➜  URL: http://localhost:2718?access_token=abc";
        let url = marimo_url(line).unwrap();
        assert_eq!(url, "http://localhost:2718?access_token=abc");
        assert_eq!(url_port(url), Some(2718));
        assert_eq!(
            with_local_port(url, 40001),
            "http://localhost:40001?access_token=abc"
        );
        assert_eq!(
            with_local_port("http://127.0.0.1:2718/edit", 2719),
            "http://localhost:2719/edit"
        );
        assert_eq!(marimo_url("Edit nb.py in your browser"), None);
    }
}
