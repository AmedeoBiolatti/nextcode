//! The vertical scrollbar over the text's right edge: a thumb for the
//! visible part and marks for find matches, diagnostics and cursors.
//! Clicking the track jumps there; the thumb drags.

use gpui::{Bounds, Context, Hsla, Pixels, Point, Window, fill, point, px, size};

use super::{Drag, Editor, EditorLayout, Severity};
use crate::theme;

/// Width of the track.
const WIDTH: Pixels = px(12.);
/// Shortest thumb.
const MIN_THUMB: Pixels = px(24.);
/// Shortest mark.
const MIN_MARK: Pixels = px(2.);

/// Where the scrollbar was drawn, for hit-testing.
pub(super) struct ScrollbarLayout {
    pub track: Bounds<Pixels>,
    pub thumb: Bounds<Pixels>,
    /// Scroll offset at the bottom of the track.
    max_scroll: Pixels,
    /// Height of all rows plus the scrolling past the last one.
    content_height: Pixels,
}

impl ScrollbarLayout {
    /// The scrollbar along the right of `text`, when the rows don't fit.
    pub fn new(
        text: Bounds<Pixels>,
        line_height: Pixels,
        row_count: usize,
        scroll_y: Pixels,
    ) -> Option<Self> {
        let viewport = text.size.height;
        if line_height * row_count as f32 <= viewport || text.size.width < WIDTH * 4. {
            return None;
        }
        let max_scroll = line_height * row_count.saturating_sub(1) as f32;
        let content_height = max_scroll + viewport;
        let track = Bounds::new(
            point(text.right() - WIDTH, text.top()),
            size(WIDTH, viewport),
        );
        let thumb_height = (viewport * (viewport / content_height)).max(MIN_THUMB);
        let travel = (viewport - thumb_height).max(px(0.));
        let top = travel * (scroll_y / max_scroll).clamp(0., 1.);
        let thumb = Bounds::new(
            point(track.left(), track.top() + top),
            size(WIDTH, thumb_height),
        );
        Some(Self {
            track,
            thumb,
            max_scroll,
            content_height,
        })
    }

    /// The scroll offset that puts the thumb's top `grab` above `y`.
    fn scroll_for(&self, y: Pixels, grab: Pixels) -> Pixels {
        let travel = self.track.size.height - self.thumb.size.height;
        if travel <= px(0.) {
            return px(0.);
        }
        let top = (y - grab - self.track.top()).max(px(0.)).min(travel);
        self.max_scroll * (top / travel)
    }

    /// The track span of rows `start..end` (at least [`MIN_MARK`] tall).
    fn mark(&self, line_height: Pixels, start: usize, end: usize) -> (Pixels, Pixels) {
        let scale = self.track.size.height / self.content_height;
        let top = self.track.top() + line_height * start as f32 * scale;
        let height = (line_height * end.saturating_sub(start).max(1) as f32 * scale).max(MIN_MARK);
        (top, height)
    }
}

impl Editor {
    /// A press on the scrollbar: grabs the thumb, or jumps to center it
    /// there first.
    pub(super) fn scrollbar_mouse_down(
        &mut self,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(scrollbar) = self.layout.as_ref().and_then(|l| l.scrollbar.as_ref()) else {
            return false;
        };
        if !scrollbar.track.contains(&position) {
            return false;
        }
        let grab = if scrollbar.thumb.contains(&position) {
            position.y - scrollbar.thumb.top()
        } else {
            let grab = scrollbar.thumb.size.height / 2.;
            self.scroll.y = scrollbar.scroll_for(position.y, grab);
            grab
        };
        self.drag = Some(Drag::Scrollbar { grab });
        cx.notify();
        true
    }

    /// Dragging the thumb.
    pub(super) fn scrollbar_drag(&mut self, y: Pixels, grab: Pixels, cx: &mut Context<Self>) {
        if let Some(scrollbar) = self.layout.as_ref().and_then(|l| l.scrollbar.as_ref()) {
            self.scroll.y = scrollbar.scroll_for(y, grab);
            cx.notify();
        }
    }

    /// Whether `position` is over the scrollbar.
    pub(super) fn over_scrollbar(&self, position: Point<Pixels>) -> bool {
        self.layout
            .as_ref()
            .and_then(|layout| layout.scrollbar.as_ref())
            .is_some_and(|scrollbar| scrollbar.track.contains(&position))
    }

    pub(super) fn paint_scrollbar(&self, layout: &EditorLayout, window: &mut Window) {
        let Some(scrollbar) = &layout.scrollbar else {
            return;
        };
        let line_height = layout.line_height;
        let track = scrollbar.track;
        let row = |offset: usize| self.display.line_row(self.buffer.line_of(offset));
        // Rows already marked in a lane, so thousands of matches cost one
        // quad per pixel row at most.
        let mut last_mark = None;
        let mut mark =
            |window: &mut Window, lane: (Pixels, Pixels), rows: (usize, usize), color: Hsla| {
                let (top, height) = scrollbar.mark(line_height, rows.0, rows.1);
                let key = (lane.0, f32::from(top) as i32, color);
                if last_mark == Some(key) {
                    return;
                }
                last_mark = Some(key);
                window.paint_quad(fill(
                    Bounds::new(point(track.left() + lane.0, top), size(lane.1, height)),
                    color,
                ));
            };

        let hovered = self.scrollbar_hovered || matches!(self.drag, Some(Drag::Scrollbar { .. }));
        window.paint_quad(
            fill(
                Bounds::new(
                    point(scrollbar.thumb.left() + px(2.), scrollbar.thumb.top()),
                    size(WIDTH - px(4.), scrollbar.thumb.size.height),
                ),
                if hovered {
                    theme::scrollbar_thumb_hover()
                } else {
                    theme::scrollbar_thumb()
                },
            )
            .corner_radii(px(3.)),
        );

        // Find matches on the left, diagnostics on the right.
        let left = (px(2.), px(4.));
        let right = (WIDTH - px(5.), px(3.));
        if let Some(find) = self.find.as_ref().filter(|find| find.open) {
            let color = theme::find_match_active().opacity(1.);
            for m in &find.matches {
                let start = row(m.start);
                mark(window, left, (start, start + 1), color);
            }
        }
        let mut diagnostics: Vec<_> = self
            .diagnostics
            .iter()
            .filter(|d| d.severity < Severity::Hint)
            .collect();
        // Errors over warnings.
        diagnostics.sort_by_key(|d| std::cmp::Reverse(d.severity));
        for diagnostic in diagnostics {
            let start = row(diagnostic.range.start);
            mark(
                window,
                right,
                (start, start + 1),
                diagnostic.severity.color(),
            );
        }
        for selection in self.selections.all() {
            let start = row(selection.head);
            mark(
                window,
                (px(0.), WIDTH),
                (start, start + 1),
                theme::cursor().opacity(0.6),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text() -> Bounds<Pixels> {
        Bounds::new(point(px(100.), px(50.)), size(px(800.), px(400.)))
    }

    #[test]
    fn rows_that_fit_have_no_scrollbar() {
        assert!(ScrollbarLayout::new(text(), px(20.), 20, px(0.)).is_none());
        assert!(ScrollbarLayout::new(text(), px(20.), 21, px(0.)).is_some());
    }

    #[test]
    fn the_thumb_follows_the_scroll_and_dragging_maps_back() {
        // 101 rows: 2000 px of scrolling over a 400 px view.
        let at = |scroll| ScrollbarLayout::new(text(), px(20.), 101, scroll).unwrap();
        let top = at(px(0.));
        assert_eq!(top.track.left(), px(888.));
        assert_eq!(top.thumb.top(), px(50.));
        assert!((f32::from(top.thumb.size.height) - 400. * 400. / 2400.).abs() < 0.01);
        let bottom = at(px(2000.));
        assert_eq!(bottom.thumb.bottom(), px(450.));
        // Grabbing the thumb's top and moving it to the end scrolls fully.
        assert_eq!(top.scroll_for(px(450.), px(0.)), px(2000.));
        assert_eq!(top.scroll_for(px(0.), px(0.)), px(0.));
        let middle = top.scroll_for(px(50.) + (px(400.) - top.thumb.size.height) / 2., px(0.));
        assert!((f32::from(middle) - 1000.).abs() < 0.01, "{middle:?}");
    }

    #[test]
    fn long_files_keep_a_grabbable_thumb() {
        let scrollbar = ScrollbarLayout::new(text(), px(20.), 100_000, px(0.)).unwrap();
        assert_eq!(scrollbar.thumb.size.height, MIN_THUMB);
        let (_, height) = scrollbar.mark(px(20.), 5, 6);
        assert_eq!(height, MIN_MARK);
    }
}
