//! Debugging in the editor: the breakpoint column at the gutter's left
//! edge (click toggles, right-click edits condition / hit count / log
//! message), run markers set by others (e.g. ▶ at tests), the paused line
//! (background + arrow), and values of identifiers hovered while paused.
//!
//! Breakpoints and markers are anchored to their line's start offset and
//! move with edits; moved breakpoint lines are reported with
//! [`EditorEvent::BreakpointsMoved`] so the owner's store follows.

use std::rc::Rc;

use gpui::{anchored, deferred};

use super::*;

actions!(
    editor,
    [
        ConfirmBreakpointEdit,
        CancelBreakpointEdit,
        NextBreakpointField
    ]
);

pub(super) const BREAKPOINT_CONTEXT: &str = "BreakpointEditor";
/// Width of the breakpoint column, in em widths.
pub(super) const BREAKPOINT_COLUMNS: f32 = 1.6;
/// Width of the run-marker column (only while there are markers).
pub(super) const MARKER_COLUMNS: f32 = 1.5;

/// A breakpoint as the gutter shows it (0-based line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GutterBreakpoint {
    pub line: usize,
    pub enabled: bool,
    /// What the running debug session said; `None` without a session.
    pub verified: Option<bool>,
    pub condition: Option<String>,
    pub hit_condition: Option<String>,
    pub log_message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GutterMarkerKind {
    /// Run (▶) a test defined on the line.
    RunTest,
}

/// A clickable glyph in the gutter (0-based line).
#[derive(Debug, Clone, PartialEq)]
pub struct GutterMarker {
    pub line: usize,
    pub kind: GutterMarkerKind,
    pub tooltip: SharedString,
}

/// How the line where the debugger is paused shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugLine {
    /// The paused thread's top frame.
    Paused,
    /// Another selected stack frame.
    Frame,
}

/// Evaluates an expression for a hover while paused.
pub type DebugEvaluator = Rc<dyn Fn(String, &mut App) -> Task<Option<String>>>;

#[derive(Default)]
pub(super) struct DebugGutterState {
    /// In the order given; each with its line start offset.
    pub(super) breakpoints: Vec<(usize, GutterBreakpoint)>,
    pub(super) markers: Vec<(usize, GutterMarker)>,
    /// Line start offset and kind.
    pub(super) debug_line: Option<(usize, DebugLine)>,
    pub evaluator: Option<DebugEvaluator>,
    editing: Option<BreakpointEditor>,
}

/// The condition / hit count / log message input of a breakpoint.
pub(super) struct BreakpointEditor {
    line: usize,
    enabled: bool,
    /// Condition, hit count, log message.
    fields: [Entity<Editor>; 3],
    focused: usize,
}

const FIELD_LABELS: [&str; 3] = ["Condition", "Hit count", "Log message"];

impl Editor {
    /// Shows these breakpoints (replacing the previous ones).
    pub fn set_breakpoints(&mut self, breakpoints: Vec<GutterBreakpoint>, cx: &mut Context<Self>) {
        let anchored = breakpoints
            .into_iter()
            .map(|bp| (self.line_anchor(bp.line), bp))
            .collect();
        if self.debug.breakpoints != anchored {
            self.debug.breakpoints = anchored;
            cx.notify();
        }
    }

    #[cfg(test)]
    pub fn breakpoints(&self) -> impl Iterator<Item = &GutterBreakpoint> {
        self.debug.breakpoints.iter().map(|(_, bp)| bp)
    }

    /// Shows these clickable markers (replacing the previous ones).
    pub fn set_gutter_markers(&mut self, markers: Vec<GutterMarker>, cx: &mut Context<Self>) {
        let anchored = markers
            .into_iter()
            .map(|marker| (self.line_anchor(marker.line), marker))
            .collect();
        if self.debug.markers != anchored {
            self.debug.markers = anchored;
            cx.notify();
        }
    }

    #[cfg(test)]
    pub fn gutter_markers(&self) -> impl Iterator<Item = &GutterMarker> {
        self.debug.markers.iter().map(|(_, marker)| marker)
    }

    /// Highlights the line where the debugger is paused (or a selected
    /// frame's), scrolling it into view; `None` clears it.
    pub fn set_debug_line(&mut self, line: Option<(usize, DebugLine)>, cx: &mut Context<Self>) {
        let anchored = line.map(|(line, kind)| (self.line_anchor(line), kind));
        if let Some((offset, _)) = anchored {
            let line = self.buffer.line_of(offset);
            if !self.selections_on_line(line) {
                self.set_cursor(offset, cx);
            } else {
                self.center_cursor = !self.line_visible(line);
            }
        }
        if self.debug.debug_line != anchored {
            self.debug.debug_line = anchored;
            cx.notify();
        }
    }

    /// The primary cursor's line (where F9 toggles a breakpoint).
    pub fn cursor_line(&self) -> usize {
        self.buffer.line_of(self.primary().head)
    }

    #[cfg(test)]
    pub fn debug_line(&self) -> Option<(usize, DebugLine)> {
        self.debug
            .debug_line
            .map(|(offset, kind)| (self.buffer.line_of(offset), kind))
    }

    /// Hover values while paused (`None` when not paused).
    pub fn set_debug_evaluator(&mut self, evaluator: Option<DebugEvaluator>) {
        self.debug.evaluator = evaluator;
    }

    fn line_anchor(&self, line: usize) -> usize {
        let line = line.min(self.buffer.line_count().saturating_sub(1));
        self.buffer.line_start(line)
    }

    fn selections_on_line(&self, line: usize) -> bool {
        let selection = self.primary();
        self.buffer.line_of(selection.head) == line
    }

    fn line_visible(&self, line: usize) -> bool {
        self.layout.as_ref().is_some_and(|layout| {
            layout
                .rows
                .iter()
                .any(|row| row.row.line == line && row.row.first)
        })
    }

    /// Moves breakpoints, markers and the paused line with `self.changes`;
    /// reports moved breakpoints.
    pub(super) fn debug_gutter_edited(&mut self, cx: &mut Context<Self>) {
        if self.changes.is_empty() {
            return;
        }
        let map = |offset: &mut usize, changes: &[Change]| {
            for change in changes {
                *offset = change.map_offset(*offset, Bias::Right);
            }
        };
        let mut moved = false;
        for (offset, breakpoint) in &mut self.debug.breakpoints {
            map(offset, &self.changes);
            let line = self.buffer.line_of(*offset);
            if line != breakpoint.line {
                breakpoint.line = line;
                moved = true;
            }
        }
        for (offset, marker) in &mut self.debug.markers {
            map(offset, &self.changes);
            marker.line = self.buffer.line_of(*offset);
        }
        if let Some((offset, _)) = &mut self.debug.debug_line {
            map(offset, &self.changes);
        }
        if moved {
            let lines = self
                .debug
                .breakpoints
                .iter()
                .map(|(_, bp)| bp.line)
                .collect();
            cx.emit(EditorEvent::BreakpointsMoved(lines));
        }
    }

    /// Left edges of the breakpoint and (when there are markers) marker
    /// columns, and the gutter width they take.
    pub(super) fn debug_columns(
        &self,
        left: Pixels,
        em_width: Pixels,
    ) -> (Pixels, Option<Pixels>, Pixels) {
        let breakpoints = left + GUTTER_PADDING / 4.;
        let markers_left = breakpoints + em_width * BREAKPOINT_COLUMNS;
        if self.debug.markers.is_empty() {
            (breakpoints, None, markers_left - left)
        } else {
            let end = markers_left + em_width * MARKER_COLUMNS;
            (breakpoints, Some(markers_left), end - left)
        }
    }

    /// The line whose gutter row is at `position`, if it is in `column`
    /// (left edge and width).
    fn gutter_line_at(
        &self,
        position: Point<Pixels>,
        left: Pixels,
        width: Pixels,
    ) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        if !(left..left + width).contains(&position.x) {
            return None;
        }
        let (_, row) = layout.row_layout_at(position.y, self.scroll.y)?;
        Some(row.row.line)
    }

    /// A click in the breakpoint column toggles a breakpoint; on a marker
    /// it reports the marker.
    pub(super) fn debug_gutter_click(
        &mut self,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(layout) = &self.layout else {
            return false;
        };
        let (em, breakpoints_left, markers_left) =
            (layout.em_width, layout.breakpoint_left, layout.marker_left);
        if let Some(line) = self.gutter_line_at(position, breakpoints_left, em * BREAKPOINT_COLUMNS)
        {
            cx.emit(EditorEvent::ToggleBreakpoint { line });
            return true;
        }
        if let Some(left) = markers_left
            && let Some(line) = self.gutter_line_at(position, left, em * MARKER_COLUMNS)
            && let Some((_, marker)) = self.debug.markers.iter().find(|(_, m)| m.line == line)
        {
            cx.emit(EditorEvent::GutterMarkerClicked {
                line,
                kind: marker.kind,
            });
            return true;
        }
        false
    }

    /// Right-click in the breakpoint column: edit (or create) the line's
    /// breakpoint's condition, hit count and log message.
    pub(super) fn on_right_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.single_line {
            return;
        }
        let Some(layout) = &self.layout else {
            return;
        };
        let (em, left) = (layout.em_width, layout.breakpoint_left);
        let Some(line) = self.gutter_line_at(event.position, left, em * BREAKPOINT_COLUMNS) else {
            self.open_gutter_menu(event.position, cx);
            return;
        };
        cx.stop_propagation();
        self.open_breakpoint_editor(line, window, cx);
    }

    pub fn open_breakpoint_editor(
        &mut self,
        line: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = self
            .debug
            .breakpoints
            .iter()
            .find(|(_, bp)| bp.line == line)
            .map(|(_, bp)| bp.clone());
        let values = [
            existing.as_ref().and_then(|bp| bp.condition.clone()),
            existing.as_ref().and_then(|bp| bp.hit_condition.clone()),
            existing.as_ref().and_then(|bp| bp.log_message.clone()),
        ];
        // Start where there is something to edit (the log message of a
        // logpoint), else in the condition.
        let focused = values.iter().position(Option::is_some).unwrap_or(0);
        let fields = values.map(|value| {
            cx.new(|cx| {
                let mut input = Editor::single_line(cx);
                input.set_text(value.as_deref().unwrap_or_default(), cx);
                input.select_all_text(cx);
                input
            })
        });
        window.focus(&fields[focused].focus_handle(cx));
        self.hover = None;
        self.completion = None;
        self.debug.editing = Some(BreakpointEditor {
            line,
            enabled: existing.is_none_or(|bp| bp.enabled),
            fields,
            focused,
        });
        cx.notify();
    }

    pub(super) fn confirm_breakpoint_edit(
        &mut self,
        _: &ConfirmBreakpointEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editing) = self.debug.editing.take() else {
            return;
        };
        let [condition, hit_condition, log_message] = editing.fields.map(|field| {
            let text = field.read(cx).text().trim().to_owned();
            (!text.is_empty()).then_some(text)
        });
        window.focus(&self.focus_handle);
        cx.emit(EditorEvent::SetBreakpoint {
            line: editing.line,
            enabled: editing.enabled,
            condition,
            hit_condition,
            log_message,
        });
        cx.notify();
    }

    pub(super) fn cancel_breakpoint_edit(
        &mut self,
        _: &CancelBreakpointEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.debug.editing.take().is_some() {
            window.focus(&self.focus_handle);
            cx.notify();
        }
    }

    pub(super) fn next_breakpoint_field(
        &mut self,
        _: &NextBreakpointField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editing) = &mut self.debug.editing {
            editing.focused = (editing.focused + 1) % editing.fields.len();
            window.focus(&editing.fields[editing.focused].focus_handle(cx));
        }
    }

    fn remove_edited_breakpoint(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editing) = self.debug.editing.take() {
            window.focus(&self.focus_handle);
            cx.emit(EditorEvent::RemoveBreakpoint { line: editing.line });
            cx.notify();
        }
    }

    /// The breakpoint editor, under its line's gutter.
    pub(super) fn render_breakpoint_editor(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let editing = self.debug.editing.as_ref()?;
        let layout = self.layout.as_ref()?;
        let offset = self
            .buffer
            .line_start(editing.line.min(self.buffer.line_count() - 1));
        let (_, top) = self.screen_position(offset).unwrap_or((px(0.), px(0.)));
        let origin = point(
            layout.breakpoint_left,
            layout.bounds.top() + top + layout.line_height,
        );
        let enabled = editing.enabled;
        let field_row = |index: usize, field: &Entity<Editor>| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .w(px(84.))
                        .flex_none()
                        .font_family(theme::UI_FONT)
                        .text_xs()
                        .text_color(theme::muted())
                        .child(selectable(("label", index), FIELD_LABELS[index])),
                )
                .child(
                    div()
                        .flex_1()
                        .h(layout.line_height)
                        .px_1()
                        .border_1()
                        .border_color(theme::border())
                        .rounded_sm()
                        .bg(theme::background())
                        .font_family(theme::MONO_FONT)
                        .text_size(px(theme::EDITOR_FONT_SIZE))
                        .line_height(layout.line_height)
                        .child(field.clone()),
                )
                .pin(format!(
                    "field:{}",
                    FIELD_LABELS[index].to_lowercase().replace(' ', "-")
                ))
                .label(FIELD_LABELS[index])
        };
        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px_2()
                .rounded_sm()
                .cursor_pointer()
                .text_xs()
                .font_family(theme::UI_FONT)
                .text_color(theme::accent_text())
                .hover(|style| style.bg(theme::hover_row()))
                .child(label)
        };
        Some(deferred(
            anchored().position(origin).snap_to_window().child(
                div()
                    .key_context(BREAKPOINT_CONTEXT)
                    .w(px(420.))
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .bg(theme::panel())
                    .border_1()
                    .border_color(theme::accent())
                    .rounded_md()
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .font_family(theme::UI_FONT)
                            .text_xs()
                            .text_color(theme::text())
                            .child(selectable(
                                "title",
                                format!("Breakpoint on line {}", editing.line + 1),
                            )),
                    )
                    .children(
                        editing
                            .fields
                            .iter()
                            .enumerate()
                            .map(|(index, field)| field_row(index, field)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .font_family(theme::UI_FONT)
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(selectable(
                                        "hint",
                                        "Enter to apply · Tab next field · Escape to cancel",
                                    )),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .child(
                                        button(
                                            "breakpoint-enabled",
                                            if enabled { "Disable" } else { "Enable" },
                                        )
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if let Some(editing) = &mut this.debug.editing {
                                                editing.enabled = !editing.enabled;
                                                cx.notify();
                                            }
                                        }))
                                        .pin("enabled")
                                        .label(if enabled { "Disable" } else { "Enable" }),
                                    )
                                    .child(
                                        button("breakpoint-remove", "Remove")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.remove_edited_breakpoint(window, cx)
                                            }))
                                            .pin("remove")
                                            .label("Remove"),
                                    ),
                            ),
                    )
                    .pin("breakpoint-editor")
                    .label("Edit Breakpoint"),
            ),
        ))
    }

    /// Paints the gutter's debug glyphs for the row at `top` showing
    /// `line` (its first row), and the run markers.
    pub(super) fn paint_debug_gutter(
        &self,
        layout: &EditorLayout,
        line: usize,
        top: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) {
        let height = layout.line_height;
        let column = layout.em_width * BREAKPOINT_COLUMNS;
        let diameter = (height * 0.62).min(column - px(2.));
        let center_x = layout.breakpoint_left + column / 2.;
        let center_y = top + height / 2.;
        let dot = Bounds::new(
            point(center_x - diameter / 2., center_y - diameter / 2.),
            size(diameter, diameter),
        );
        if let Some((_, breakpoint)) = self
            .debug
            .breakpoints
            .iter()
            .find(|(_, bp)| bp.line == line)
        {
            let color = if breakpoint.enabled {
                theme::breakpoint()
            } else {
                theme::muted()
            };
            let hollow = !breakpoint.enabled || breakpoint.verified == Some(false);
            let logpoint = breakpoint.log_message.is_some();
            let radius = if logpoint { px(2.) } else { diameter / 2. };
            let quad = if hollow {
                gpui::quad(
                    dot,
                    radius,
                    gpui::transparent_black(),
                    px(1.5),
                    color,
                    Default::default(),
                )
            } else {
                fill(dot, color).corner_radii(radius)
            };
            window.paint_quad(quad);
            // Conditions show as a bar across the dot.
            if breakpoint.condition.is_some() || breakpoint.hit_condition.is_some() {
                let bar = Bounds::new(
                    point(dot.left() + diameter * 0.25, center_y - px(1.)),
                    size(diameter * 0.5, px(2.)),
                );
                let bar_color = if hollow { color } else { theme::background() };
                window.paint_quad(fill(bar, bar_color));
            }
        }
        if let Some((offset, kind)) = self.debug.debug_line
            && self.buffer.line_of(offset) == line
        {
            // An arrow: a shaft and a head built from shrinking bars.
            let color = match kind {
                DebugLine::Paused => theme::debug_arrow(),
                DebugLine::Frame => theme::accent_text(),
            };
            let left = layout.breakpoint_left + px(1.);
            let width = column - px(2.);
            let shaft = height * 0.18;
            window.paint_quad(fill(
                Bounds::new(
                    point(left, center_y - shaft / 2.),
                    size(width * 0.55, shaft),
                ),
                color,
            ));
            let head_left = left + width * 0.45;
            let head_width = width * 0.55;
            let steps = 6;
            for step in 0..steps {
                let fraction = step as f32 / steps as f32;
                let half = height * 0.32 * (1. - fraction);
                window.paint_quad(fill(
                    Bounds::new(
                        point(head_left + head_width * fraction, center_y - half),
                        size(head_width / steps as f32 + px(0.5), half * 2.),
                    ),
                    color,
                ));
            }
        }
        if let Some(markers_left) = layout.marker_left
            && self.debug.markers.iter().any(|(_, m)| m.line == line)
        {
            let glyph = &layout.run_marker;
            let column = layout.em_width * MARKER_COLUMNS;
            let x = markers_left + (column - glyph.width) / 2.;
            glyph.paint(point(x, top), height, window, cx).ok();
        }
    }

    /// Background of the paused (or selected frame's) line, before text.
    pub(super) fn paint_debug_line(
        &self,
        layout: &EditorLayout,
        row: &RowLayout,
        top: Pixels,
        window: &mut Window,
    ) {
        let Some((offset, kind)) = self.debug.debug_line else {
            return;
        };
        if self.buffer.line_of(offset) != row.row.line {
            return;
        }
        let color = match kind {
            DebugLine::Paused => theme::debug_current_line(),
            DebugLine::Frame => theme::debug_frame_line(),
        };
        window.paint_quad(fill(
            Bounds::from_corners(
                point(layout.text_bounds.left(), top),
                point(layout.text_bounds.right(), top + layout.line_height),
            ),
            color,
        ));
    }

    /// The expression to evaluate for a hover at `word`: the word with a
    /// leading `a.b.` member chain (`self.items` for `items`).
    pub(super) fn hover_expression(&self, word: &Range<usize>) -> Option<String> {
        let text = self.buffer.slice(word.clone());
        if text.is_empty() || !text.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return None;
        }
        if text.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            return None;
        }
        let line_start = self.buffer.line_start(self.buffer.line_of(word.start));
        let before = self.buffer.slice(line_start..word.start);
        let chain_start = before
            .char_indices()
            .rev()
            .take_while(|&(_, c)| c.is_alphanumeric() || c == '_' || c == '.')
            .last()
            .map_or(before.len(), |(index, _)| index);
        let chain = before[chain_start..].trim_start_matches('.');
        Some(format!("{chain}{text}"))
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::TestAppContext;

    use super::{super::tests::*, *};

    fn breakpoint(line: usize) -> GutterBreakpoint {
        GutterBreakpoint {
            line,
            enabled: true,
            verified: None,
            condition: None,
            hit_condition: None,
            log_message: None,
        }
    }

    #[gpui::test]
    fn breakpoints_and_markers_move_with_edits(cx: &mut TestAppContext) {
        let window = editor("a.py", "|a\nb\nc\nd\n", cx);
        let moved = Rc::new(RefCell::new(Vec::new()));
        let entity = window.root(cx).unwrap();
        let recorded = moved.clone();
        cx.update(|cx| {
            cx.subscribe(&entity, move |_, event: &EditorEvent, _| {
                if let EditorEvent::BreakpointsMoved(lines) = event {
                    recorded.borrow_mut().push(lines.clone());
                }
            })
            .detach()
        });
        update(window, cx, |e, _, cx| {
            e.set_breakpoints(vec![breakpoint(1), breakpoint(3)], cx);
            e.set_gutter_markers(
                vec![GutterMarker {
                    line: 2,
                    kind: GutterMarkerKind::RunTest,
                    tooltip: "Run c".into(),
                }],
                cx,
            );
        });
        let lines = |cx: &mut TestAppContext| {
            window
                .read_with(cx, |e, _| {
                    (
                        e.breakpoints().map(|bp| bp.line).collect::<Vec<_>>(),
                        e.gutter_markers().map(|m| m.line).collect::<Vec<_>>(),
                    )
                })
                .unwrap()
        };

        // A line inserted above moves everything down.
        type_text(window, "x\n", cx);
        assert_eq!(lines(cx), (vec![2, 4], vec![3]));
        // Typing inside a breakpoint's line keeps it there.
        update(window, cx, |e, _, cx| e.go_to_line(2, 1, cx));
        type_text(window, "zz", cx);
        assert_eq!(lines(cx), (vec![2, 4], vec![3]));
        // Enter at the start of a breakpoint's line moves it with its text.
        update(window, cx, |e, _, cx| e.go_to_line(4, 0, cx));
        type_text(window, "\n", cx);
        assert_eq!(lines(cx), (vec![2, 5], vec![3]));
        // Deleting the line above joins it: the breakpoint moves up.
        update(window, cx, |e, w, cx| {
            e.go_to_line(5, 0, cx);
            e.backspace(&Backspace, w, cx);
        });
        assert_eq!(lines(cx), (vec![2, 4], vec![3]));
        // Undo moves it back.
        update(window, cx, |e, w, cx| e.undo(&Undo, w, cx));
        assert_eq!(lines(cx), (vec![2, 5], vec![3]));
        cx.run_until_parked();
        assert_eq!(
            moved.borrow().as_slice(),
            [vec![2, 4], vec![2, 5], vec![2, 4], vec![2, 5]],
            "one report per edit that moved a breakpoint"
        );
    }

    #[gpui::test]
    fn hover_expressions_include_member_chains(cx: &mut TestAppContext) {
        let window = editor("a.py", "|total = self.items.count + 1\n", cx);
        window
            .read_with(cx, |e, _| {
                let text = e.text();
                let at = |word: &str| {
                    let start = text.find(word).unwrap();
                    e.hover_expression(&(start..start + word.len()))
                };
                assert_eq!(at("total").as_deref(), Some("total"));
                assert_eq!(at("items").as_deref(), Some("self.items"));
                assert_eq!(at("count").as_deref(), Some("self.items.count"));
                assert_eq!(at("1"), None);
                assert_eq!(at("="), None);
            })
            .unwrap();
    }
}
