//! A project tab's content while a remote project (WSL, dev container)
//! opens: progress, then [`ConnectingEvent::Connected`], or the failure
//! with a Retry button.

use std::io;

use futures::{
    StreamExt,
    channel::{mpsc, oneshot},
};
use gpui::{Context, EventEmitter, SharedString, Task, Window, div, prelude::*, px};

use crate::{
    pinpoint::Pin,
    project::{Location, Project},
    selectable::selectable,
    theme,
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

pub enum ConnectingEvent {
    Connected(Project),
}

impl EventEmitter<ConnectingEvent> for Connecting {}

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

    /// Why the last attempt failed (until Retry).
    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
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
            this.update(cx, |this, cx| match result {
                Ok(project) => cx.emit(ConnectingEvent::Connected(project)),
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
            .bg(theme::background())
            .text_color(theme::text())
            .child(div().text_lg().child(selectable("heading", self.heading())))
            .child(
                div()
                    .text_sm()
                    .text_color(theme::muted())
                    .child(selectable("location", self.location.to_string())),
            )
            .when(!self.log.is_empty(), |this| {
                // One block, so a selection can span the lines.
                this.child(
                    div()
                        .w(px(720.))
                        .p_2()
                        .rounded_md()
                        .bg(theme::panel())
                        .font_family(theme::MONO_FONT)
                        .text_xs()
                        .text_color(theme::muted())
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(selectable("log", self.log.join("\n"))),
                )
            })
            .children(self.error.clone().map(|error| {
                div()
                    .max_w(px(720.))
                    .text_sm()
                    .text_color(theme::error())
                    .child(selectable("error", error))
            }))
            .children(self.error.is_some().then(|| {
                div()
                    .id("retry")
                    .px_4()
                    .py_1()
                    .rounded_md()
                    .bg(theme::accent())
                    .text_color(theme::on_accent())
                    .cursor_pointer()
                    .child("Retry")
                    .on_click(cx.listener(|this, _, window, cx| this.connect(None, window, cx)))
                    .pin("retry")
                    .label("Retry")
            }))
            .pin("connecting")
            .label(self.heading())
    }
}
