//! Markdown file tabs: source, rendered preview, or a resizable live split.
//! The ordinary Editor remains the sole document and undo/save owner.

use std::{cell::Cell, rc::Rc, time::Duration};

use gpui::{
    App, Bounds, Context, CursorStyle, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding,
    MouseButton, Pixels, ScrollHandle, Subscription, Task, Window, actions, canvas, div,
    prelude::*, px, relative,
};
use serde::{Deserialize, Serialize};

use crate::{
    editor::{self, Editor, EditorEvent},
    markdown::{MarkdownDocument, MarkdownImages},
    pinpoint::Pin,
    project::Project,
    theme,
};

actions!(markdown, [ShowSource, ShowPreview, ShowSplit]);
const CONTEXT: &str = "MarkdownDocument";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-shift-v", ShowPreview, Some(CONTEXT)),
        KeyBinding::new("secondary-k v", ShowSplit, Some(CONTEXT)),
        KeyBinding::new("secondary-k e", ShowSource, Some(CONTEXT)),
        KeyBinding::new("secondary-s", editor::Save, Some("MarkdownPreview")),
    ]);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Source,
    Preview,
    #[default]
    Split,
}

pub struct ModeChanged;
impl EventEmitter<ModeChanged> for MarkdownView {}

pub struct MarkdownView {
    editor: Entity<Editor>,
    project: Project,
    mode: Mode,
    root_focus: FocusHandle,
    preview_focus: FocusHandle,
    document: Option<MarkdownDocument>,
    dirty: bool,
    generation: u64,
    ratio: f32,
    resizing: bool,
    bounds: Rc<Cell<Bounds<Pixels>>>,
    scroll: ScrollHandle,
    _parse: Option<Task<()>>,
    _subscription: Subscription,
}

impl MarkdownView {
    pub fn new(editor: Entity<Editor>, project: Project, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&editor, |this: &mut Self, _, event, cx| {
            if matches!(event, EditorEvent::Edited) {
                this.refresh(true, cx);
            }
        });
        let mut view = Self {
            editor,
            project,
            mode: Mode::default(),
            root_focus: cx.focus_handle(),
            preview_focus: cx.focus_handle(),
            document: None,
            dirty: true,
            generation: 0,
            ratio: 0.5,
            resizing: false,
            bounds: Rc::default(),
            scroll: ScrollHandle::new(),
            _parse: None,
            _subscription: subscription,
        };
        view.refresh(false, cx);
        view
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.root_focus.contains_focused(window, cx)
    }

    pub fn set_mode(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode != mode {
            self.mode = mode;
            if self.dirty {
                self.refresh(false, cx);
            }
            cx.emit(ModeChanged);
            cx.notify();
        }
        window.focus(&self.focus_handle(cx));
    }

    // Session restore sets the mode without moving focus between project tabs.
    pub fn restore_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.mode = mode;
        if self.dirty {
            self.refresh(false, cx);
        }
        cx.notify();
    }

    fn refresh(&mut self, debounce: bool, cx: &mut Context<Self>) {
        self.dirty = true;
        self.generation += 1;
        self._parse = None;
        if self.mode == Mode::Source {
            return;
        }
        let generation = self.generation;
        self._parse = Some(cx.spawn(async move |this, cx| {
            if debounce {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
            let Ok(text) = this.read_with(cx, |this, cx| this.editor.read(cx).text()) else {
                return;
            };
            let document = cx
                .background_spawn(async move { MarkdownDocument::parse(&text) })
                .await;
            this.update(cx, |this, cx| {
                if this.generation == generation {
                    this.document = Some(document);
                    this.dirty = false;
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn show_source(&mut self, _: &ShowSource, window: &mut Window, cx: &mut Context<Self>) {
        self.set_mode(Mode::Source, window, cx);
    }
    fn show_preview(&mut self, _: &ShowPreview, window: &mut Window, cx: &mut Context<Self>) {
        self.set_mode(Mode::Preview, window, cx);
    }
    fn show_split(&mut self, _: &ShowSplit, window: &mut Window, cx: &mut Context<Self>) {
        self.set_mode(Mode::Split, window, cx);
    }
    fn save(&mut self, _: &editor::Save, _: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.save(cx))
            .detach();
    }

    fn render_preview(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let path = self
            .editor
            .read(cx)
            .buffer()
            .path()
            .map(|path| path.to_path_buf());
        let base_dir = path
            .as_deref()
            .and_then(|path| path.parent())
            .filter(|_| !self.project.is_remote());
        div()
            .id("markdown-scroll")
            .key_context("MarkdownPreview")
            .track_focus(&self.preview_focus)
            .track_scroll(&self.scroll)
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_y_scroll()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if !this.preview_focus.contains_focused(window, cx) {
                        window.focus(&this.preview_focus);
                    }
                }),
            )
            .child(
                div()
                    .w_full()
                    .max_w(px(1000.))
                    .mx_auto()
                    .p_6()
                    .text_base()
                    .line_height(px(26.))
                    .when_some(self.document.as_ref(), |preview, document| {
                        preview.child(document.render(
                            "document",
                            MarkdownImages {
                                attachments: None,
                                base_dir,
                            },
                        ))
                    })
                    .when(self.document.is_none(), |preview| {
                        preview.child("Rendering Markdown…")
                    }),
            )
            .pin("preview")
            .label("Markdown preview")
            .into_any_element()
    }

    fn stop_resize(&mut self, _: &gpui::MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.resizing {
            self.resizing = false;
            cx.notify();
        }
    }
}

impl Focusable for MarkdownView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        if self.mode == Mode::Preview {
            self.preview_focus.clone()
        } else {
            self.editor.focus_handle(cx)
        }
    }
}

impl Render for MarkdownView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bounds = self.bounds.clone();
        let source = div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .child(self.editor.clone())
            .pin("source")
            .label("Markdown source");
        let content = match self.mode {
            Mode::Source => source.into_any_element(),
            Mode::Preview => self.render_preview(cx),
            Mode::Split => div()
                .relative()
                .size_full()
                .min_w_0()
                .min_h_0()
                .flex()
                .child(
                    canvas(move |laid_out, _, _| bounds.set(laid_out), |_, _, _, _| {})
                        .absolute()
                        .size_full(),
                )
                .child(
                    div()
                        .w(relative(self.ratio))
                        .h_full()
                        .min_w_0()
                        .flex_none()
                        .flex()
                        .child(source),
                )
                .child(
                    div()
                        .id("markdown-divider")
                        .w(px(5.))
                        .h_full()
                        .flex_none()
                        .bg(theme::border())
                        .cursor(CursorStyle::ResizeLeftRight)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.resizing = true;
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        )
                        .pin("divider")
                        .label("Resize Markdown source and preview"),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .flex()
                        .child(self.render_preview(cx)),
                )
                .into_any_element(),
        };
        div()
            .key_context(CONTEXT)
            .track_focus(&self.root_focus)
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(theme::background())
            .text_color(theme::text())
            .on_action(cx.listener(Self::show_source))
            .on_action(cx.listener(Self::show_preview))
            .on_action(cx.listener(Self::show_split))
            .on_action(cx.listener(Self::save))
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                if this.resizing {
                    let bounds = this.bounds.get();
                    this.ratio =
                        ((event.position.x - bounds.left()) / bounds.size.width).clamp(0.15, 0.85);
                    cx.notify();
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::stop_resize))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::stop_resize))
            .child(
                div()
                    .h(px(34.))
                    .flex_none()
                    .px_2()
                    .flex()
                    .justify_end()
                    .items_center()
                    .gap_1()
                    .border_b_1()
                    .border_color(theme::border())
                    .bg(theme::panel())
                    .children(
                        [
                            (Mode::Source, "Source"),
                            (Mode::Preview, "Preview"),
                            (Mode::Split, "Split"),
                        ]
                        .into_iter()
                        .map(|(mode, label)| {
                            div()
                                .id(label)
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .text_xs()
                                .cursor_pointer()
                                .when(self.mode == mode, |button| {
                                    button.bg(theme::accent()).text_color(theme::on_accent())
                                })
                                .when(self.mode != mode, |button| {
                                    button.hover(|style| style.bg(theme::hover_row()))
                                })
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.set_mode(mode, window, cx)
                                }))
                                .child(label)
                                .pin(label.to_ascii_lowercase())
                                .label(format!("Markdown: {label}"))
                        }),
                    )
                    .pin("modes")
                    .label("Markdown view mode"),
            )
            .child(div().flex_1().min_w_0().min_h_0().flex().child(content))
            .pin("markdown")
            .label("Markdown document")
    }
}
