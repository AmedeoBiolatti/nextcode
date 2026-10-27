//! Pinpoint nodes of the parts EditorElement paints itself (gutter,
//! blame, inlay hints, sticky lines, minimap, scrollbar): only the rows on
//! screen, recorded each prepaint after layout.

use git::LineChangeKind;

use super::*;
use crate::pinpoint;

impl Editor {
    /// Records the painted parts under `prefix` (the enclosing pins' path).
    pub(super) fn record_pins(&self, prefix: &str, window: &Window) {
        let Some(layout) = &self.layout else {
            return;
        };
        if self.single_line || self.text_area {
            return;
        }
        let join = |rest: &str| -> SharedString {
            if prefix.is_empty() {
                rest.to_owned().into()
            } else {
                format!("{prefix}/{rest}").into()
            }
        };
        let record = |path: SharedString, label: Option<SharedString>, bounds| {
            pinpoint::record(window, path, label, bounds)
        };
        let height = layout.line_height;
        let (bounds, text) = (layout.bounds, layout.text_bounds);
        record(
            join("gutter"),
            None,
            Bounds::from_corners(bounds.origin, point(text.left(), bounds.bottom())),
        );
        let column = |left: Pixels, width: Pixels, top: Pixels| {
            Bounds::new(point(left, top), size(width, height))
        };
        if let Some(left) = layout.blame_left {
            record(
                join("blame"),
                Some("Blame annotations".into()),
                Bounds::from_corners(
                    point(left, bounds.top()),
                    point(
                        left + layout.em_width * blame::BLAME_COLUMNS,
                        bounds.bottom(),
                    ),
                ),
            );
        }
        let breakpoint_width = layout.em_width * debug_gutter::BREAKPOINT_COLUMNS;
        let bulb = self.lightbulb();
        let count = self.buffer.line_count();
        for (i, row) in layout.rows.iter().enumerate() {
            let top = layout.row_top(i, self.scroll.y);
            let line = row.row.line;
            let n = line + 1;
            if let Some(number) = &row.number {
                record(
                    join(&format!("gutter/line:{n}")),
                    None,
                    column(layout.numbers_right - number.width, number.width, top),
                );
                if let Some(severity) = row.mark {
                    record(
                        join(&format!("gutter/diagnostic:{n}")),
                        Some(format!("{severity:?}").into()),
                        Bounds::new(
                            point(bounds.left() + px(3.), top + px(3.)),
                            size(px(3.), height - px(6.)),
                        ),
                    );
                }
                if let Some((_, breakpoint)) = self
                    .debug
                    .breakpoints
                    .iter()
                    .find(|(_, bp)| bp.line == line)
                {
                    let label = match (&breakpoint.log_message, &breakpoint.condition) {
                        (Some(message), _) => format!("Logpoint: {message}"),
                        (None, Some(condition)) => format!("Breakpoint if {condition}"),
                        (None, None) if !breakpoint.enabled => "Disabled breakpoint".into(),
                        (None, None) => "Breakpoint".into(),
                    };
                    record(
                        join(&format!("gutter/breakpoint:{n}")),
                        Some(label.into()),
                        column(layout.breakpoint_left, breakpoint_width, top),
                    );
                }
                if let Some((offset, _)) = self.debug.debug_line
                    && self.buffer.line_of(offset) == line
                {
                    record(
                        join(&format!("gutter/execution-point:{n}")),
                        Some("Execution point".into()),
                        column(layout.breakpoint_left, breakpoint_width, top),
                    );
                }
                if let Some(left) = layout.marker_left
                    && let Some((_, marker)) =
                        self.debug.markers.iter().find(|(_, m)| m.line == line)
                {
                    record(
                        join(&format!("gutter/run:{n}")),
                        Some(marker.tooltip.clone()),
                        column(left, layout.em_width * debug_gutter::MARKER_COLUMNS, top),
                    );
                }
                if bulb.is_some_and(|(bulb_line, _)| bulb_line == line) {
                    let (left, width) = Self::lightbulb_column(layout);
                    record(
                        join("gutter/lightbulb"),
                        Some("Show code actions".into()),
                        column(left, width, top),
                    );
                }
            }
            if let Some(folded) = row.chevron {
                record(
                    join(&format!("gutter/fold:{n}")),
                    Some(if folded { "Unfold" } else { "Fold" }.into()),
                    column(layout.fold_left, text.left() - layout.fold_left, top),
                );
            }
            if let (Some(left), true) = (layout.change_left, row.row.first)
                && let Some(change) = self.change_at(line)
            {
                let label = match change.kind {
                    LineChangeKind::Added => "Added",
                    LineChangeKind::Modified => "Modified",
                    LineChangeKind::Deleted => "Deleted",
                };
                // A deletion's marker sits on the line after it (or under
                // the last line).
                let deleted_here =
                    change.new.start == line || (change.new.start >= count && line + 1 == count);
                if change.kind != LineChangeKind::Deleted || deleted_here {
                    record(
                        join(&format!("gutter/git-change:{n}")),
                        Some(label.into()),
                        column(left, git_gutter::CHANGE_COLUMN, top),
                    );
                }
            }
            if let (Some(_), Some(left)) = (&row.blame, layout.blame_left) {
                record(
                    join(&format!("blame/line:{n}")),
                    None,
                    column(left, layout.em_width * blame::BLAME_COLUMNS, top),
                );
            }
            if row.row.first {
                let text_left = layout.text_left(row, self.scroll.x);
                for (k, span) in row.inlays.spans().enumerate() {
                    let x0 = row.shaped.x_for_index(span.start);
                    let x1 = row.shaped.x_for_index(span.end);
                    record(
                        join(&format!("inlay:{n}:{}", k + 1)),
                        None,
                        column(text_left + x0, x1 - x0, top),
                    );
                }
            }
        }
        if let Some(sticky) = &layout.sticky {
            let area = sticky.bounds(text.top());
            record(join("sticky-scroll"), Some("Sticky scroll".into()), area);
            for row in &sticky.rows {
                record(
                    join(&format!("sticky-scroll/line:{}", row.line + 1)),
                    None,
                    Bounds::new(point(area.left(), row.top), size(area.size.width, height)),
                );
            }
        }
        if let Some(minimap) = &layout.minimap {
            record(join("minimap"), Some("Minimap".into()), minimap.bounds);
        }
        if let Some(scrollbar) = &layout.scrollbar {
            record(join("scrollbar"), Some("Scrollbar".into()), scrollbar.track);
            record(join("scrollbar/thumb"), None, scrollbar.thumb);
        }
    }
}
