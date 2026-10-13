//! Window content while a remote project (WSL, dev container) opens:
//! progress, then the workspace, or the failure with a Retry button.

use std::io;

use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
};
use gpui::{Context, SharedString, Task, Window, div, prelude::*, px, rgb};

use crate::{
    project::{Location, Project},
    theme,
    workspace::Workspace,
};

/// Progress lines kept on screen.
const LOG_LINES: usize = 14;

/// [`Project::open`] running on a thread (see `workspace::Preload`): its
/// result, and the steps it reports.
pub struct PendingConnection {
    pub result: oneshot::Receiver<io::Result<Project>>,
    pub progress: mpsc::UnboundedReceiver<String>,
}

impl PendingConnection {
    /// Opens `location` on a new thread.
    pub fn start(location: Location) -> Self {
        let (result_tx, result) = oneshot::channel();
        let (progress_tx, progress) = mpsc::unbounded();
        std::thread::spawn(move || {
            let opened = Project::open(&location, &mut |line| {
                progress_tx.unbounded_send(line.to_owned()).ok();
            });
            result_tx.send(opened).ok();
        });
        Self { result, progress }
    }
}

pub struct Connecting {
    location: Location,
    log: Vec<SharedString>,
    error: Option<SharedString>,
    _task: Task<()>,
}

impl Connecting {
    pub fn new(
        location: Location,
        pending: Option<PendingConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            location,
            log: Vec::new(),
            error: None,
            _task: Task::ready(()),
        };
        this.connect(pending, window, cx);
        this
    }

    /// The project this window is opening.
    pub fn location(&self) -> Location {
        self.location.clone()
    }

    /// Connects, or takes over a connection already under way.
    fn connect(
        &mut self,
        pending: Option<PendingConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.error = None;
        self.log.clear();
        let PendingConnection {
            result,
            mut progress,
        } = pending.unwrap_or_else(|| PendingConnection::start(self.location.clone()));
        self._task = cx.spawn_in(window, async move |this, cx| {
            while let Some(line) = progress.next().await {
                let updated = this.update(cx, |this, cx| {
                    this.log.push(line.into());
                    if this.log.len() > LOG_LINES {
                        this.log.remove(0);
                    }
                    cx.notify();
                });
                if updated.is_err() {
                    return;
                }
            }
            let result = result
                .await
                .unwrap_or_else(|_| Err(io::Error::other("the connection was abandoned")));
            this.update_in(cx, |this, window, cx| match result {
                Ok(project) => {
                    window.set_window_title(&format!("{} — next_code", project.location()));
                    window.replace_root(cx, |window, cx| Workspace::new(project, None, window, cx));
                }
                Err(err) => {
                    this.error = Some(err.to_string().into());
                    cx.notify();
                }
            })
            .ok();
        });
        cx.notify();
    }

    fn heading(&self) -> String {
        match (&self.location, &self.error) {
            (Location::Wsl { distro, .. }, None) => format!("Connecting to WSL distro '{distro}'…"),
            (Location::DevContainer { .. }, None) => "Opening the dev container…".into(),
            (_, None) => "Opening…".into(),
            (location, Some(_)) => format!("Could not open {location}"),
        }
    }
}

impl Render for Connecting {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .p_8()
            .bg(rgb(theme::BACKGROUND))
            .text_color(rgb(theme::TEXT))
            .child(div().text_lg().child(self.heading()))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(theme::MUTED))
                    .child(self.location.to_string()),
            )
            .when(!self.log.is_empty(), |this| {
                this.child(
                    div()
                        .w(px(720.))
                        .p_2()
                        .rounded_md()
                        .bg(rgb(theme::PANEL))
                        .font_family(theme::MONO_FONT)
                        .text_xs()
                        .text_color(rgb(theme::MUTED))
                        .children(
                            self.log
                                .iter()
                                .map(|line| div().truncate().child(line.clone())),
                        ),
                )
            })
            .children(self.error.clone().map(|error| {
                div()
                    .max_w(px(720.))
                    .text_sm()
                    .text_color(rgb(theme::ERROR))
                    .child(error)
            }))
            .children(self.error.is_some().then(|| {
                div()
                    .id("retry")
                    .px_4()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(theme::ACCENT))
                    .text_color(rgb(0xffffff))
                    .cursor_pointer()
                    .child("Retry")
                    .on_click(cx.listener(|this, _, window, cx| this.connect(None, window, cx)))
            }))
    }
}
