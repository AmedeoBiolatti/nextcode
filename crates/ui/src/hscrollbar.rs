//! The horizontal scrollbar ("left-right bar") of terminal-like views whose
//! lines are not wrapped: along the bottom edge, over the content, only
//! while something is wider than the view. The thumb drags; a press on the
//! track centres the thumb there and keeps dragging (as the editor's
//! vertical bar does). Shift+wheel, sideways trackpad scrolling and the
//! wheel over the bar move it too. Pinned as `…/hscrollbar` (label
//! "Horizontal scrollbar") with `…/hscrollbar/thumb`.
//!
//! [`HScrollbar`] is the geometry, for views that paint themselves (the
//! terminal); [`for_handle`] is the element for a div scrolled by a
//! [`ScrollHandle`].

use std::{cell::Cell, rc::Rc};

use gpui::{
    App, Bounds, DispatchPhase, Element, ElementId, GlobalElementId, Hitbox, HitboxBehavior,
    InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Position, ScrollHandle, ScrollWheelEvent, SharedString, Style,
    Window, fill, point, px, size,
};

use crate::{pinpoint, theme};

/// Height of the track.
pub const HEIGHT: Pixels = px(10.);
/// Shortest thumb.
const MIN_THUMB: Pixels = px(24.);

/// Where the bar is drawn, for painting and hit-testing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HScrollbar {
    pub track: Bounds<Pixels>,
    pub thumb: Bounds<Pixels>,
    /// The scroll with the thumb at the right end of the track.
    max_scroll: Pixels,
}

impl HScrollbar {
    /// The bar along the bottom of `area` for content `content` wide seen
    /// through `viewport`, scrolled `scroll` right; `None` when it fits.
    pub fn new(
        area: Bounds<Pixels>,
        viewport: Pixels,
        content: Pixels,
        scroll: Pixels,
    ) -> Option<Self> {
        let max_scroll = content - viewport;
        // Sub-pixel overflow is layout noise, not something to scroll.
        if max_scroll < px(1.) || area.size.width < MIN_THUMB * 2. {
            return None;
        }
        let width = area.size.width;
        let track = Bounds::new(
            point(area.left(), area.bottom() - HEIGHT),
            size(width, HEIGHT),
        );
        let thumb_width = (width * (viewport / content)).max(MIN_THUMB).min(width);
        let travel = width - thumb_width;
        let left = travel * (scroll / max_scroll).clamp(0., 1.);
        let thumb = Bounds::new(
            point(track.left() + left, track.top()),
            size(thumb_width, HEIGHT),
        );
        Some(Self {
            track,
            thumb,
            max_scroll,
        })
    }

    /// The scroll that puts the thumb's left `grab` before window `x`.
    pub fn scroll_for(&self, x: Pixels, grab: Pixels) -> Pixels {
        let travel = self.track.size.width - self.thumb.size.width;
        if travel <= px(0.) {
            return px(0.);
        }
        let left = (x - grab - self.track.left()).max(px(0.)).min(travel);
        self.max_scroll * (left / travel)
    }

    /// A press at `position`: `None` off the track; else where the thumb
    /// is held (from its left) and, for a press beside the thumb, the
    /// scroll that centres the thumb there.
    pub fn press(&self, position: Point<Pixels>) -> Option<(Pixels, Option<Pixels>)> {
        if !self.track.contains(&position) {
            return None;
        }
        if self.thumb.contains(&position) {
            return Some((position.x - self.thumb.left(), None));
        }
        let grab = self.thumb.size.width / 2.;
        Some((grab, Some(self.scroll_for(position.x, grab))))
    }

    /// Paints the thumb (brighter while `active`: hovered or dragged).
    pub fn paint(&self, active: bool, window: &mut Window) {
        window.paint_quad(fill(self.track, theme::background().opacity(0.5)));
        window.paint_quad(
            fill(
                Bounds::new(
                    point(self.thumb.left(), self.thumb.top() + px(2.)),
                    size(self.thumb.size.width, HEIGHT - px(4.)),
                ),
                if active {
                    theme::scrollbar_thumb_hover()
                } else {
                    theme::scrollbar_thumb()
                },
            )
            .corner_radii(px(3.)),
        );
    }

    /// Pins the bar under `prefix` (the enclosing pin's path; call in
    /// prepaint).
    pub fn record_pins(&self, prefix: &str, window: &Window) {
        let join = |rest: &str| -> SharedString {
            if prefix.is_empty() {
                rest.to_owned().into()
            } else {
                format!("{prefix}/{rest}").into()
            }
        };
        pinpoint::record(
            window,
            join("hscrollbar"),
            Some("Horizontal scrollbar".into()),
            self.track,
        );
        pinpoint::record(window, join("hscrollbar/thumb"), None, self.thumb);
    }
}

/// A wheel event's sideways part, in pixels with gpui's sign (positive
/// moves the content right, i.e. scrolls toward the start): sideways
/// trackpad motion, or the wheel with Shift held (X11 and Windows already
/// turn it sideways, Wayland does not). Zero when mostly vertical.
pub fn wheel_dx(event: &ScrollWheelEvent, line_height: Pixels) -> Pixels {
    let mut delta = event.delta.pixel_delta(line_height);
    if event.modifiers.shift && delta.x == px(0.) {
        delta = point(delta.y, px(0.));
    }
    if delta.x.abs() > delta.y.abs() {
        delta.x
    } else {
        px(0.)
    }
}

/// The wheel over the bar itself: its dominant direction, sideways.
pub fn wheel_over_bar(event: &ScrollWheelEvent, line_height: Pixels) -> Pixels {
    let delta = event.delta.pixel_delta(line_height);
    if delta.x.abs() > delta.y.abs() {
        delta.x
    } else {
        delta.y
    }
}

/// The bar for a div scrolled by `handle` (`overflow_x_scroll` and
/// `track_scroll`): add it after that div in a `relative()` parent; it
/// draws over the div's bottom edge, from the div's last layout.
pub fn for_handle(id: impl Into<ElementId>, handle: &ScrollHandle) -> HandleScrollbar {
    HandleScrollbar {
        id: id.into(),
        handle: handle.clone(),
        pin_path: SharedString::default(),
    }
}

pub struct HandleScrollbar {
    id: ElementId,
    handle: ScrollHandle,
    pin_path: SharedString,
}

impl IntoElement for HandleScrollbar {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

/// Scrolls `handle` to `scroll` pixels from the left (clamped).
fn scroll_handle_to(handle: &ScrollHandle, scroll: Pixels) {
    let most = handle.max_offset().width;
    let mut offset = handle.offset();
    offset.x = -scroll.clamp(px(0.), most.max(px(0.)));
    handle.set_offset(offset);
}

pub struct HandleScrollbarState {
    bar: HScrollbar,
    bar_hitbox: Hitbox,
    area_hitbox: Hitbox,
}

impl Element for HandleScrollbar {
    type RequestLayoutState = ();
    type PrepaintState = Option<HandleScrollbarState>;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        self.pin_path = pinpoint::current_path().unwrap_or_default();
        let style = Style {
            position: Position::Absolute,
            ..Style::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        _: &mut App,
    ) -> Option<HandleScrollbarState> {
        let area = self.handle.bounds();
        let viewport = area.size.width;
        let bar = HScrollbar::new(
            area,
            viewport,
            viewport + self.handle.max_offset().width,
            -self.handle.offset().x,
        )?;
        bar.record_pins(&self.pin_path, window);
        Some(HandleScrollbarState {
            bar,
            bar_hitbox: window.insert_hitbox(bar.track, HitboxBehavior::BlockMouse),
            area_hitbox: window.insert_hitbox(area, HitboxBehavior::Normal),
        })
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        state: &mut Option<HandleScrollbarState>,
        window: &mut Window,
        _: &mut App,
    ) {
        let (Some(state), Some(global_id)) = (state.take(), global_id) else {
            return;
        };
        // Where the thumb is held while it drags, kept across frames.
        let drag =
            window.with_element_state(global_id, |held: Option<Rc<Cell<Option<Pixels>>>>, _| {
                let held = held.unwrap_or_default();
                (held.clone(), held)
            });
        let HandleScrollbarState {
            bar,
            bar_hitbox,
            area_hitbox,
        } = state;
        let hovered = bar_hitbox.is_hovered(window);
        bar.paint(hovered || drag.get().is_some(), window);
        let view = window.current_view();
        let line_height = window.line_height();

        let (handle, held, hitbox) = (self.handle.clone(), drag.clone(), bar_hitbox.clone());
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble
                || event.button != MouseButton::Left
                || !hitbox.is_hovered(window)
            {
                return;
            }
            if let Some((grab, jump)) = bar.press(event.position) {
                if let Some(scroll) = jump {
                    scroll_handle_to(&handle, scroll);
                }
                held.set(Some(grab));
                cx.stop_propagation();
                cx.notify(view);
            }
        });
        let (handle, held, hitbox) = (self.handle.clone(), drag.clone(), bar_hitbox.clone());
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            if let Some(grab) = held.get() {
                scroll_handle_to(&handle, bar.scroll_for(event.position.x, grab));
                cx.notify(view);
            } else if hitbox.is_hovered(window) != hovered {
                cx.notify(view);
            }
        });
        let held = drag;
        window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble && held.take().is_some() {
                cx.notify(view);
            }
        });
        let handle = self.handle.clone();
        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            // The div scrolls sideways itself for sideways deltas; Shift
            // with a vertical delta (Wayland) and the wheel over the bar
            // are handled here.
            let dx = if bar_hitbox.is_hovered(window) {
                wheel_over_bar(event, line_height)
            } else if area_hitbox.should_handle_scroll(window)
                && event.modifiers.shift
                && event.delta.pixel_delta(line_height).x == px(0.)
            {
                wheel_dx(event, line_height)
            } else {
                return;
            };
            scroll_handle_to(&handle, -handle.offset().x - dx);
            cx.stop_propagation();
            cx.notify(view);
        });
    }
}

#[cfg(test)]
mod tests {
    use gpui::{
        Context, Modifiers, Render, ScrollDelta, TestAppContext, TouchPhase, div, prelude::*,
    };

    use super::*;

    fn area() -> Bounds<Pixels> {
        Bounds::new(point(px(100.), px(50.)), size(px(400.), px(300.)))
    }

    #[test]
    fn content_that_fits_has_no_bar() {
        assert!(HScrollbar::new(area(), px(400.), px(400.), px(0.)).is_none());
        assert!(HScrollbar::new(area(), px(400.), px(400.5), px(0.)).is_none());
        assert!(HScrollbar::new(area(), px(400.), px(800.), px(0.)).is_some());
    }

    #[test]
    fn the_thumb_shows_the_visible_part_and_drags_over_the_rest() {
        // Twice as wide as the view: a half-track thumb.
        let at = |scroll| HScrollbar::new(area(), px(400.), px(800.), scroll).unwrap();
        let start = at(px(0.));
        assert_eq!(
            start.track,
            Bounds::new(point(px(100.), px(340.)), size(px(400.), HEIGHT))
        );
        assert_eq!(start.thumb.size.width, px(200.));
        assert_eq!(start.thumb.left(), px(100.));
        assert_eq!(start.max_scroll, px(400.));
        let end = at(px(400.));
        assert_eq!(end.thumb.right(), px(500.));
        // Scrolled past the end (content shrank): the thumb stays inside.
        assert_eq!(at(px(900.)).thumb.right(), px(500.));
        // Holding the thumb 10 px in, moved to x = 160: its left 50 px along
        // 200 px of travel, a quarter of the way.
        assert_eq!(start.scroll_for(px(160.), px(10.)), px(100.));
        assert_eq!(start.scroll_for(px(0.), px(10.)), px(0.));
        assert_eq!(start.scroll_for(px(9000.), px(10.)), px(400.));
    }

    #[test]
    fn pressing_the_track_centres_the_thumb_there() {
        let bar = HScrollbar::new(area(), px(400.), px(800.), px(0.)).unwrap();
        let y = bar.track.center().y;
        assert_eq!(bar.press(point(px(150.), y)), Some((px(50.), None)));
        // Right of the thumb at 400: its centre goes there.
        assert_eq!(
            bar.press(point(px(400.), y)),
            Some((px(100.), Some(px(400.))))
        );
        assert_eq!(bar.press(point(px(150.), px(60.))), None, "above the track");
    }

    #[test]
    fn huge_content_keeps_a_grabbable_thumb() {
        let bar = HScrollbar::new(area(), px(400.), px(4_000_000.), px(0.)).unwrap();
        assert_eq!(bar.thumb.size.width, MIN_THUMB);
    }

    fn wheel(x: f32, y: f32, shift: bool) -> ScrollWheelEvent {
        ScrollWheelEvent {
            position: point(px(0.), px(0.)),
            delta: ScrollDelta::Pixels(point(px(x), px(y))),
            modifiers: Modifiers {
                shift,
                ..Modifiers::default()
            },
            touch_phase: TouchPhase::Moved,
        }
    }

    #[test]
    fn shift_wheel_and_sideways_motion_scroll_sideways() {
        let line = px(20.);
        assert_eq!(wheel_dx(&wheel(0., -30., true), line), px(-30.));
        assert_eq!(wheel_dx(&wheel(0., -30., false), line), px(0.));
        // Already sideways (X11, Windows): kept.
        assert_eq!(wheel_dx(&wheel(-30., 0., true), line), px(-30.));
        // Trackpads: sideways when mostly sideways.
        assert_eq!(wheel_dx(&wheel(12., 2., false), line), px(12.));
        assert_eq!(wheel_dx(&wheel(2., 12., false), line), px(0.));
        // Over the bar any wheel scrolls it.
        assert_eq!(wheel_over_bar(&wheel(0., -30., false), line), px(-30.));
    }

    /// A 200x100 output box with one line far wider than it.
    struct Wide {
        handle: ScrollHandle,
    }

    impl Render for Wide {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(
                div()
                    .relative()
                    .w(px(200.))
                    .h(px(100.))
                    .child(
                        div()
                            .id("wide")
                            .size_full()
                            .overflow_scroll()
                            .track_scroll(&self.handle)
                            .whitespace_nowrap()
                            .flex()
                            .flex_col()
                            .items_start()
                            .child(div().flex_none().child("x".repeat(300))),
                    )
                    .child(for_handle("bar", &self.handle)),
            )
        }
    }

    #[gpui::test]
    fn a_scrolled_div_gets_a_bar_that_drags_and_takes_shift_wheel(cx: &mut TestAppContext) {
        let handle = ScrollHandle::new();
        let (_, cx) = cx.add_window_view(|_, _| Wide {
            handle: handle.clone(),
        });
        cx.run_until_parked();
        let most = handle.max_offset().width;
        assert!(most > px(100.), "the line overflows: {most:?}");
        let area = handle.bounds();
        let wheel = |shift| ScrollWheelEvent {
            position: area.center(),
            delta: ScrollDelta::Lines(point(0., -2.)),
            modifiers: if shift {
                Modifiers::shift()
            } else {
                Modifiers::none()
            },
            touch_phase: TouchPhase::Moved,
        };
        // Shift+wheel (as Wayland sends it) scrolls right, not down.
        cx.simulate_event(wheel(true));
        assert!(handle.offset().x < px(0.), "{:?}", handle.offset());
        assert_eq!(handle.offset().y, px(0.));

        // Pressing the track's right end jumps there; dragging back to the
        // left end returns.
        let y = area.bottom() - HEIGHT / 2.;
        cx.simulate_mouse_down(
            point(area.right() - px(2.), y),
            MouseButton::Left,
            Modifiers::none(),
        );
        assert_eq!(handle.offset().x, -most);
        cx.simulate_mouse_move(
            point(area.left() - px(300.), y),
            MouseButton::Left,
            Modifiers::none(),
        );
        cx.simulate_mouse_up(point(area.left(), y), MouseButton::Left, Modifiers::none());
        assert_eq!(handle.offset().x, px(0.));
        // Dragging is over: moving no longer scrolls.
        cx.simulate_mouse_move(point(area.right(), y), None, Modifiers::none());
        assert_eq!(handle.offset().x, px(0.));
    }
}
