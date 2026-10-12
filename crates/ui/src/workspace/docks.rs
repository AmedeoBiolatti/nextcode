//! Docks: three panel areas (left, right, bottom) whose panels can be
//! resized, collapsed, reordered and dragged between docks.

use gpui::{
    AnyElement, AnyView, CursorStyle, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, Point, anchored, deferred,
};

use super::*;
use crate::layout::{DockPosition, MIN_DOCK_SIZE, PanelId};

actions!(
    workspace,
    [
        ToggleFiles,
        ToggleSearch,
        ToggleTerminal,
        ToggleGit,
        ToggleProblems,
        ToggleBottomDock,
        SaveLayoutAsDefault,
        ResetLayout,
    ]
);

pub fn bind_keys(cx: &mut App) {
    let ctx = Some("Workspace");
    cx.bind_keys([
        KeyBinding::new("secondary-shift-e", ToggleFiles, ctx),
        KeyBinding::new("alt-1", ToggleFiles, ctx),
        KeyBinding::new("secondary-shift-f", ToggleSearch, ctx),
        // VS Code's Source Control shortcut.
        KeyBinding::new("ctrl-shift-g", ToggleGit, ctx),
        KeyBinding::new("ctrl-`", ToggleTerminal, ctx),
        KeyBinding::new("alt-f12", ToggleTerminal, ctx),
        KeyBinding::new("secondary-shift-m", ToggleProblems, ctx),
        KeyBinding::new("secondary-j", ToggleBottomDock, ctx),
    ]);
}

const HEADER_HEIGHT: f32 = 30.;
const STRIP_SIZE: f32 = 26.;
const HANDLE_SIZE: f32 = 5.;

/// Payload of a panel tab being dragged.
pub(super) struct DraggedPanel(PanelId);

/// What follows the mouse while a tab is dragged.
struct DragPreview(SharedString);

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .text_sm()
            .bg(rgb(theme::ACTIVE_ROW))
            .border_1()
            .border_color(rgb(theme::ACCENT))
            .rounded_md()
            .text_color(rgb(theme::TEXT))
            .child(self.0.clone())
    }
}

/// The open right-click menu of a panel tab.
pub(super) struct PanelMenu {
    panel: PanelId,
    position: Point<Pixels>,
}

impl Workspace {
    fn panel_view(&self, panel: PanelId) -> Option<AnyView> {
        match panel {
            PanelId::Files => Some(self.file_tree.clone().into()),
            PanelId::Problems => Some(self.problems.clone().into()),
            PanelId::Terminal => Some(self.terminal.clone().into()),
            PanelId::Search => Some(self.search.clone().into()),
            PanelId::Git => Some(self.git.clone().into()),
        }
    }

    /// Persists the layout for this project; failures only cost the layout.
    fn layout_changed(&mut self, cx: &mut Context<Self>) {
        if let Err(err) = self.layout.save_for_project(&self.project.state_key()) {
            self.notice = Some(format!("Could not save layout: {err}").into());
        }
        cx.notify();
    }

    pub(super) fn show_panel(&mut self, panel: PanelId, cx: &mut Context<Self>) {
        self.layout.activate(panel);
        self.layout_changed(cx);
    }

    pub(super) fn move_panel(
        &mut self,
        panel: PanelId,
        to: DockPosition,
        index: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.panel_menu = None;
        self.layout.move_panel(panel, to, index);
        self.layout_changed(cx);
    }

    fn toggle_panel(&mut self, panel: PanelId, cx: &mut Context<Self>) {
        self.layout.toggle_panel(panel);
        self.layout_changed(cx);
    }

    pub(super) fn toggle_files(&mut self, _: &ToggleFiles, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_panel(PanelId::Files, cx);
    }

    /// `Ctrl+Shift+F`: shows search and focuses its query (VS Code behavior:
    /// it does not hide an already-visible search panel).
    pub(super) fn toggle_search(
        &mut self,
        _: &ToggleSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_panel(PanelId::Search, cx);
        self.search.read(cx).focus(window, cx);
    }

    /// Shows the Git panel (refreshing its status) and focuses the commit
    /// message.
    pub(super) fn toggle_git(
        &mut self,
        _: &ToggleGit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_panel(PanelId::Git, cx);
        self.git.update(cx, |git, cx| {
            git.refresh(cx);
            git.focus(window, cx);
        });
    }

    /// Shows the terminal (starting a shell if none runs) and focuses it, or
    /// hides it and returns focus to the editor.
    pub(super) fn toggle_terminal(
        &mut self,
        _: &ToggleTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_panel(PanelId::Terminal, cx);
        let visible = self
            .layout
            .position_of(PanelId::Terminal)
            .is_some_and(|position| {
                let dock = self.layout.dock(position);
                dock.open && dock.active == Some(PanelId::Terminal)
            });
        if visible {
            self.terminal
                .update(cx, |terminal, cx| terminal.focus(window, cx));
        } else {
            self.focus_active(window, cx);
        }
    }

    pub(super) fn toggle_problems(
        &mut self,
        _: &ToggleProblems,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_panel(PanelId::Problems, cx);
    }

    pub(super) fn toggle_bottom_dock(
        &mut self,
        _: &ToggleBottomDock,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.layout.toggle_dock(DockPosition::Bottom);
        self.layout_changed(cx);
    }

    pub(super) fn save_layout_as_default(
        &mut self,
        _: &SaveLayoutAsDefault,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.panel_menu = None;
        self.notice = Some(match self.layout.save_as_default() {
            Ok(()) => "Layout saved as the default for new projects".into(),
            Err(err) => format!("Could not save default layout: {err}").into(),
        });
        cx.notify();
    }

    pub(super) fn reset_layout(&mut self, _: &ResetLayout, _: &mut Window, cx: &mut Context<Self>) {
        self.panel_menu = None;
        self.layout = Layout::default();
        self.layout_changed(cx);
    }

    pub(super) fn on_resize_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(position) = self.resizing else {
            return;
        };
        let viewport = window.viewport_size();
        let (size, limit) = match position {
            DockPosition::Left => (event.position.x, viewport.width),
            DockPosition::Right => (viewport.width - event.position.x, viewport.width),
            DockPosition::Bottom => (viewport.height - event.position.y, viewport.height),
        };
        let max = (f32::from(limit) * 0.7).max(MIN_DOCK_SIZE);
        self.layout.dock_mut(position).size = f32::from(size).clamp(MIN_DOCK_SIZE, max);
        cx.notify();
    }

    pub(super) fn on_resize_end(
        &mut self,
        _: &MouseUpEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.resizing.take().is_some() {
            self.layout_changed(cx);
        }
    }

    /// A dock: open with tabs and the active panel, collapsed to a strip of
    /// panel buttons, or (when empty) only a drop zone while a tab is dragged.
    pub(super) fn render_dock(
        &self,
        position: DockPosition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let dock = self.layout.dock(position);
        if dock.panels.is_empty() {
            return cx
                .has_active_drag()
                .then(|| self.render_strip(position, cx));
        }
        if !dock.open {
            return Some(self.render_strip(position, cx));
        }

        let header = div()
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .flex()
            .items_center()
            .border_b_1()
            .border_color(rgb(theme::BORDER))
            .children(dock.panels.iter().enumerate().map(|(index, &panel)| {
                self.render_panel_tab(position, index, panel, dock.active == Some(panel), cx)
            }))
            // Empty header space: drop to append.
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .drag_over::<DraggedPanel>(|style, _, _, _| style.bg(rgb(theme::HOVER_ROW)))
                    .on_drop(cx.listener(move |this, dragged: &DraggedPanel, _, cx| {
                        this.move_panel(dragged.0, position, None, cx);
                    })),
            )
            .child(
                div()
                    .id(SharedString::from(format!("collapse-{}", position.label())))
                    .px_2()
                    .text_color(rgb(theme::MUTED))
                    .hover(|style| style.text_color(rgb(theme::TEXT)))
                    .cursor_pointer()
                    .child("–")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.layout.dock_mut(position).open = false;
                        this.layout_changed(cx);
                    })),
            );
        let body = div().flex_1().min_h_0().min_w_0().children(
            match dock.active.and_then(|panel| self.panel_view(panel)) {
                Some(view) => Some(view.into_any_element()),
                None => Some(
                    div()
                        .p_3()
                        .text_sm()
                        .text_color(rgb(theme::MUTED))
                        .child("This panel is not available yet.")
                        .into_any_element(),
                ),
            },
        );
        let size = px(dock.size);
        let content = div()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(theme::PANEL))
            .drag_over::<DraggedPanel>(|style, _, _, _| {
                style.border_1().border_color(rgb(theme::ACCENT))
            })
            .on_drop(cx.listener(move |this, dragged: &DraggedPanel, _, cx| {
                this.move_panel(dragged.0, position, None, cx);
            }))
            .child(header)
            .child(body);
        let content = match position {
            DockPosition::Left | DockPosition::Right => content.w(size).h_full(),
            DockPosition::Bottom => content.h(size).w_full(),
        };
        let handle = self.render_resize_handle(position, window, cx);
        let dock = match position {
            DockPosition::Left => div().h_full().flex().child(content).child(handle),
            DockPosition::Right => div().h_full().flex().child(handle).child(content),
            DockPosition::Bottom => div()
                .w_full()
                .flex()
                .flex_col()
                .child(handle)
                .child(content),
        };
        Some(dock.flex_none().into_any_element())
    }

    fn render_panel_tab(
        &self,
        position: DockPosition,
        index: usize,
        panel: PanelId,
        active: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        div()
            .id(SharedString::from(format!("panel-tab-{}", panel.title())))
            .h_full()
            .px_3()
            .flex()
            .items_center()
            .text_sm()
            .cursor_pointer()
            .text_color(rgb(if active { theme::TEXT } else { theme::MUTED }))
            .when(active, |tab| {
                tab.border_b_2().border_color(rgb(theme::ACCENT))
            })
            .hover(|style| style.text_color(rgb(theme::TEXT)))
            .child(panel.title())
            .on_click(cx.listener(move |this, _, _, cx| this.show_panel(panel, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.panel_menu = Some(PanelMenu {
                        panel,
                        position: event.position,
                    });
                    cx.notify();
                }),
            )
            .on_drag(DraggedPanel(panel), |dragged, _, _, cx| {
                cx.new(|_| DragPreview(dragged.0.title().into()))
            })
            // Dropping on a tab inserts before it.
            .drag_over::<DraggedPanel>(|style, _, _, _| {
                style.border_l_2().border_color(rgb(theme::ACCENT))
            })
            .on_drop(cx.listener(move |this, dragged: &DraggedPanel, _, cx| {
                this.move_panel(dragged.0, position, Some(index), cx);
            }))
    }

    /// A collapsed dock: one button per panel; also a drop target.
    fn render_strip(&self, position: DockPosition, cx: &mut Context<Self>) -> AnyElement {
        let dock = self.layout.dock(position);
        let vertical = position != DockPosition::Bottom;
        let buttons = dock.panels.iter().map(|&panel| {
            let label = if vertical {
                &panel.title()[..1]
            } else {
                panel.title()
            };
            div()
                .id(SharedString::from(format!("strip-{}", panel.title())))
                .px_1()
                .py(px(2.))
                .rounded_sm()
                .text_xs()
                .text_color(rgb(theme::MUTED))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(theme::HOVER_ROW)).text_color(rgb(theme::TEXT)))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.show_panel(panel, cx)))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        this.panel_menu = Some(PanelMenu {
                            panel,
                            position: event.position,
                        });
                        cx.notify();
                    }),
                )
        });
        let strip = div()
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .bg(rgb(theme::PANEL))
            .border_color(rgb(theme::BORDER))
            .drag_over::<DraggedPanel>(|style, _, _, _| style.bg(rgb(theme::ACTIVE_ROW)))
            .on_drop(cx.listener(move |this, dragged: &DraggedPanel, _, cx| {
                this.move_panel(dragged.0, position, None, cx);
            }))
            .children(buttons);
        match position {
            DockPosition::Left => strip
                .w(px(STRIP_SIZE))
                .h_full()
                .flex_col()
                .pt_2()
                .border_r_1(),
            DockPosition::Right => strip
                .w(px(STRIP_SIZE))
                .h_full()
                .flex_col()
                .pt_2()
                .border_l_1(),
            DockPosition::Bottom => strip.h(px(STRIP_SIZE)).w_full().px_2().border_t_1(),
        }
        .into_any_element()
    }

    fn render_resize_handle(
        &self,
        position: DockPosition,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let active = self.resizing == Some(position);
        let handle = div()
            .flex_none()
            .bg(rgb(if active { theme::ACCENT } else { theme::BORDER }))
            .hover(|style| style.bg(rgb(theme::ACCENT)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.resizing = Some(position);
                    cx.stop_propagation();
                    cx.notify();
                }),
            );
        match position {
            DockPosition::Left | DockPosition::Right => handle
                .w(px(HANDLE_SIZE))
                .h_full()
                .cursor(CursorStyle::ResizeLeftRight),
            DockPosition::Bottom => handle
                .h(px(HANDLE_SIZE))
                .w_full()
                .cursor(CursorStyle::ResizeUpDown),
        }
    }

    /// The right-click menu of a panel tab or strip button.
    pub(super) fn render_panel_menu(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let menu = self.panel_menu.as_ref()?;
        let panel = menu.panel;
        let current = self.layout.position_of(panel);
        let item = |id: SharedString, label: SharedString| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(|style| style.bg(rgb(theme::ACTIVE_ROW)))
                .child(label)
        };
        let moves = DockPosition::ALL
            .into_iter()
            .filter(|&position| Some(position) != current)
            .map(|position| {
                item(
                    format!("move-{}", position.label()).into(),
                    format!("Move to {}", position.label()).into(),
                )
                .on_click(
                    cx.listener(move |this, _, _, cx| this.move_panel(panel, position, None, cx)),
                )
            });
        Some(deferred(
            anchored().position(menu.position).child(
                div()
                    .id("panel-menu")
                    .occlude()
                    .min_w(px(200.))
                    .p_1()
                    .flex()
                    .flex_col()
                    .text_sm()
                    .text_color(rgb(theme::TEXT))
                    .bg(rgb(theme::PANEL))
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .rounded_md()
                    .shadow_lg()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.panel_menu = None;
                        cx.notify();
                    }))
                    .children(moves)
                    .child(div().my_1().h(px(1.)).bg(rgb(theme::BORDER)))
                    .child(
                        item("save-default".into(), "Save layout as default".into()).on_click(
                            cx.listener(|this, _, window, cx| {
                                this.save_layout_as_default(&SaveLayoutAsDefault, window, cx)
                            }),
                        ),
                    )
                    .child(item("reset-layout".into(), "Reset layout".into()).on_click(
                        cx.listener(|this, _, window, cx| {
                            this.reset_layout(&ResetLayout, window, cx)
                        }),
                    )),
            ),
        ))
    }
}
