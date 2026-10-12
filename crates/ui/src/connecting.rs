//! Window content while a WSL project connects: progress, then the
//! workspace, or the failure with a Retry button.

use gpui::{Context, SharedString, Task, Window, div, prelude::*, px, rgb};

use crate::{
    project::{Location, Project},
    theme,
    workspace::Workspace,
};

pub struct Connecting {
    distro: String,
    path: String,
    error: Option<SharedString>,
    _task: Task<()>,
}

impl Connecting {
    pub fn new(distro: String, path: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            distro,
            path,
            error: None,
            _task: Task::ready(()),
        };
        this.connect(window, cx);
        this
    }

    /// The project this window is opening.
    pub fn location(&self) -> Location {
        Location::Wsl {
            distro: self.distro.clone(),
            path: self.path.clone(),
        }
    }

    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        let (distro, path) = (self.distro.clone(), self.path.clone());
        self._task = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { Project::connect_wsl(&distro, &path) })
                .await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(project) => {
                    window.set_window_title(&format!("{} — next_code", project.location()));
                    window.replace_root(cx, |window, cx| Workspace::new(project, window, cx));
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
}

impl Render for Connecting {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let heading = match &self.error {
            None => format!("Connecting to WSL distro '{}'…", self.distro),
            Some(_) => format!(
                "Could not open {} in WSL distro '{}'",
                self.path, self.distro
            ),
        };
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
            .child(div().text_lg().child(heading))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(theme::MUTED))
                    .child(self.path.clone()),
            )
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
                    .on_click(cx.listener(|this, _, window, cx| this.connect(window, cx)))
            }))
    }
}
