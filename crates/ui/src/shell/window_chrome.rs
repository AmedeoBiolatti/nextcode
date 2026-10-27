//! Linux client decorations, including resize targets along the window edges.

use gpui::{
    AnyElement, CursorStyle, Decorations, MouseButton, ResizeEdge, Window, div, prelude::*, px,
};

pub(super) fn custom_title_bar(window: &Window) -> bool {
    cfg!(windows)
        || (cfg!(target_os = "linux")
            && matches!(window.window_decorations(), Decorations::Client { .. }))
}

pub(super) fn resize_handles(window: &Window) -> Vec<AnyElement> {
    if !cfg!(target_os = "linux") || window.is_maximized() || window.is_fullscreen() {
        return Vec::new();
    }
    let Decorations::Client { tiling } = window.window_decorations() else {
        return Vec::new();
    };
    use ResizeEdge::*;
    [
        (Top, !tiling.top),
        (Bottom, !tiling.bottom),
        (Left, !tiling.left),
        (Right, !tiling.right),
        (TopLeft, !(tiling.top || tiling.left)),
        (TopRight, !(tiling.top || tiling.right)),
        (BottomLeft, !(tiling.bottom || tiling.left)),
        (BottomRight, !(tiling.bottom || tiling.right)),
    ]
    .into_iter()
    .filter(|(_, enabled)| *enabled)
    .enumerate()
    .map(|(index, (edge, _))| {
        let handle = div().id(("window-resize", index)).absolute().on_mouse_down(
            MouseButton::Left,
            move |_, window, cx| {
                cx.stop_propagation();
                window.start_window_resize(edge);
            },
        );
        match edge {
            Top => handle
                .top_0()
                .left_0()
                .w_full()
                .h(px(4.))
                .cursor(CursorStyle::ResizeUpDown),
            Bottom => handle
                .bottom_0()
                .left_0()
                .w_full()
                .h(px(4.))
                .cursor(CursorStyle::ResizeUpDown),
            Left => handle
                .top_0()
                .left_0()
                .h_full()
                .w(px(4.))
                .cursor(CursorStyle::ResizeLeftRight),
            Right => handle
                .top_0()
                .right_0()
                .h_full()
                .w(px(4.))
                .cursor(CursorStyle::ResizeLeftRight),
            TopLeft => handle
                .top_0()
                .left_0()
                .size(px(8.))
                .cursor(CursorStyle::ResizeUpLeftDownRight),
            TopRight => handle
                .top_0()
                .right_0()
                .size(px(8.))
                .cursor(CursorStyle::ResizeUpRightDownLeft),
            BottomLeft => handle
                .bottom_0()
                .left_0()
                .size(px(8.))
                .cursor(CursorStyle::ResizeUpRightDownLeft),
            BottomRight => handle
                .bottom_0()
                .right_0()
                .size(px(8.))
                .cursor(CursorStyle::ResizeUpLeftDownRight),
        }
        .into_any_element()
    })
    .collect()
}
