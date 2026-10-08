//! Multi-line code editor view: buffer state, keyboard/mouse handling, IME, and painting.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    GlobalElementId, InspectorElementId, KeyBinding, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollWheelEvent, ShapedLine, SharedString, Style,
    TextRun, UTF16Selection, UnderlineStyle, Window, actions, div, fill, point, prelude::*, px,
    relative, rgb,
};
use text::Buffer;

use crate::theme;

actions!(
    editor,
    [
        Backspace,
        Delete,
        DeleteWordLeft,
        DeleteWordRight,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        WordLeft,
        WordRight,
        SelectWordLeft,
        SelectWordRight,
        Home,
        End,
        SelectHome,
        SelectEnd,
        DocStart,
        DocEnd,
        SelectDocStart,
        SelectDocEnd,
        PageUp,
        PageDown,
        Newline,
        Tab,
        SelectAll,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        Save,
    ]
);

const CONTEXT: &str = "Editor";
const TAB_SIZE: usize = 4;
const GUTTER_PADDING: Pixels = px(12.);

#[cfg(target_os = "macos")]
const WORD: &str = "alt";
#[cfg(not(target_os = "macos"))]
const WORD: &str = "ctrl";

pub fn bind_keys(cx: &mut App) {
    let ctx = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("shift-backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new(&format!("{WORD}-backspace"), DeleteWordLeft, ctx),
        KeyBinding::new(&format!("{WORD}-delete"), DeleteWordRight, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("up", Up, ctx),
        KeyBinding::new("down", Down, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("shift-up", SelectUp, ctx),
        KeyBinding::new("shift-down", SelectDown, ctx),
        KeyBinding::new(&format!("{WORD}-left"), WordLeft, ctx),
        KeyBinding::new(&format!("{WORD}-right"), WordRight, ctx),
        KeyBinding::new(&format!("{WORD}-shift-left"), SelectWordLeft, ctx),
        KeyBinding::new(&format!("{WORD}-shift-right"), SelectWordRight, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("shift-home", SelectHome, ctx),
        KeyBinding::new("shift-end", SelectEnd, ctx),
        KeyBinding::new("secondary-home", DocStart, ctx),
        KeyBinding::new("secondary-end", DocEnd, ctx),
        KeyBinding::new("secondary-shift-home", SelectDocStart, ctx),
        KeyBinding::new("secondary-shift-end", SelectDocEnd, ctx),
        KeyBinding::new("pageup", PageUp, ctx),
        KeyBinding::new("pagedown", PageDown, ctx),
        KeyBinding::new("enter", Newline, ctx),
        KeyBinding::new("shift-enter", Newline, ctx),
        KeyBinding::new("tab", Tab, ctx),
        KeyBinding::new("secondary-a", SelectAll, ctx),
        KeyBinding::new("secondary-c", Copy, ctx),
        KeyBinding::new("secondary-x", Cut, ctx),
        KeyBinding::new("secondary-v", Paste, ctx),
        KeyBinding::new("secondary-z", Undo, ctx),
        KeyBinding::new("secondary-shift-z", Redo, ctx),
        KeyBinding::new("secondary-y", Redo, ctx),
        KeyBinding::new("secondary-s", Save, ctx),
    ]);
}

pub enum EditorEvent {
    /// The buffer gained or lost unsaved changes.
    DirtyChanged,
}

impl EventEmitter<EditorEvent> for Editor {}

pub struct Editor {
    focus_handle: FocusHandle,
    buffer: Buffer,
    /// Selection end that stays put while extending.
    anchor: usize,
    /// Selection end that moves; the cursor.
    head: usize,
    /// Column that vertical motion tries to keep.
    goal_column: Option<usize>,
    /// IME composition range.
    marked: Option<Range<usize>>,
    scroll: Point<Pixels>,
    /// Bring the cursor into view on the next layout.
    autoscroll: bool,
    selecting: bool,
    layout: Option<EditorLayout>,
    error: Option<SharedString>,
}

/// Geometry from the last prepaint, used for painting and hit-testing.
struct EditorLayout {
    text_bounds: Bounds<Pixels>,
    gutter_width: Pixels,
    line_height: Pixels,
    em_width: Pixels,
    first_line: usize,
    lines: Vec<ShapedLine>,
    line_numbers: Vec<ShapedLine>,
}

impl Editor {
    pub fn new(buffer: Buffer, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            buffer,
            anchor: 0,
            head: 0,
            goal_column: None,
            marked: None,
            scroll: Point::default(),
            autoscroll: false,
            selecting: false,
            layout: None,
            error: None,
        }
    }

    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    pub fn save(&mut self, cx: &mut Context<Self>) -> bool {
        let was_dirty = self.buffer.is_dirty();
        let saved = match self.buffer.save() {
            Ok(()) => {
                self.error = None;
                true
            }
            Err(err) => {
                self.error = Some(format!("Save failed: {err}").into());
                false
            }
        };
        self.finish_change(was_dirty, cx);
        saved
    }

    fn selection(&self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    fn move_head(&mut self, offset: usize, select: bool, cx: &mut Context<Self>) {
        self.head = offset;
        if !select {
            self.anchor = offset;
        }
        self.autoscroll = true;
        cx.notify();
    }

    fn horizontal(
        &mut self,
        select: bool,
        target: impl FnOnce(&Self) -> usize,
        cx: &mut Context<Self>,
    ) {
        self.goal_column = None;
        let offset = target(self);
        self.move_head(offset, select, cx);
    }

    fn vertical(&mut self, lines: isize, select: bool, cx: &mut Context<Self>) {
        let goal = *self
            .goal_column
            .get_or_insert_with(|| self.buffer.column(self.head));
        let line = self.buffer.line_of(self.head) as isize + lines;
        let offset = if line < 0 {
            0
        } else if line as usize >= self.buffer.line_count() {
            self.buffer.len()
        } else {
            self.buffer.offset_at(line as usize, goal)
        };
        self.move_head(offset, select, cx);
    }

    fn left_target(&self, select: bool) -> usize {
        let selection = self.selection();
        if !select && !selection.is_empty() {
            selection.start
        } else {
            self.buffer.prev_grapheme(self.head)
        }
    }

    fn right_target(&self, select: bool) -> usize {
        let selection = self.selection();
        if !select && !selection.is_empty() {
            selection.end
        } else {
            self.buffer.next_grapheme(self.head)
        }
    }

    /// First non-blank column of the line, or the line start if already there.
    fn home_target(&self) -> usize {
        let line = self.buffer.line_of(self.head);
        let start = self.buffer.line_start(line);
        let text = self.buffer.line_text(line);
        let indented = start + text.len() - text.trim_start().len();
        if self.head == indented {
            start
        } else {
            indented
        }
    }

    fn end_target(&self) -> usize {
        self.buffer.line_end(self.buffer.line_of(self.head))
    }

    fn page_lines(&self) -> isize {
        self.layout.as_ref().map_or(20, |layout| {
            (layout.text_bounds.size.height / layout.line_height)
                .floor()
                .max(1.) as isize
        })
    }

    /// Replaces `range`, collapses the selection after the new text.
    fn replace(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        let was_dirty = self.buffer.is_dirty();
        self.buffer.edit(range.clone(), text);
        let cursor = range.start + text.len();
        self.anchor = cursor;
        self.head = cursor;
        self.marked = None;
        self.goal_column = None;
        self.autoscroll = true;
        self.finish_change(was_dirty, cx);
    }

    /// Deletes the selection, or `fallback` from the cursor when nothing is selected.
    fn delete_selection_or(
        &mut self,
        fallback: impl FnOnce(&Buffer, usize) -> Range<usize>,
        cx: &mut Context<Self>,
    ) {
        let mut range = self.selection();
        if range.is_empty() {
            range = fallback(&self.buffer, self.head);
        }
        self.replace(range, "", cx);
    }

    fn finish_change(&mut self, was_dirty: bool, cx: &mut Context<Self>) {
        if was_dirty != self.buffer.is_dirty() {
            cx.emit(EditorEvent::DirtyChanged);
        }
        cx.notify();
    }

    /// Converts pasted text to the buffer's line ending.
    fn normalize_line_endings(&self, text: &str) -> String {
        let text = text.replace("\r\n", "\n");
        match self.buffer.line_ending() {
            "\n" => text,
            ending => text.replace('\n', ending),
        }
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_selection_or(|b, head| b.prev_grapheme(head)..head, cx);
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_selection_or(|b, head| head..b.next_grapheme(head), cx);
    }

    fn delete_word_left(&mut self, _: &DeleteWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_selection_or(|b, head| b.prev_word_start(head)..head, cx);
    }

    fn delete_word_right(&mut self, _: &DeleteWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_selection_or(|b, head| head..b.next_word_end(head), cx);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, |e| e.left_target(false), cx);
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, |e| e.right_target(false), cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, |e| e.left_target(true), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, |e| e.right_target(true), cx);
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(-1, false, cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(1, false, cx);
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(-1, true, cx);
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(1, true, cx);
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, |e| e.buffer.prev_word_start(e.head), cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, |e| e.buffer.next_word_end(e.head), cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, |e| e.buffer.prev_word_start(e.head), cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, |e| e.buffer.next_word_end(e.head), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, Self::home_target, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, Self::end_target, cx);
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, Self::home_target, cx);
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, Self::end_target, cx);
    }

    fn doc_start(&mut self, _: &DocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, |_| 0, cx);
    }

    fn doc_end(&mut self, _: &DocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(false, |e| e.buffer.len(), cx);
    }

    fn select_doc_start(&mut self, _: &SelectDocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, |_| 0, cx);
    }

    fn select_doc_end(&mut self, _: &SelectDocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.horizontal(true, |e| e.buffer.len(), cx);
    }

    fn page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(-self.page_lines(), false, cx);
    }

    fn page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(self.page_lines(), false, cx);
    }

    /// Inserts a line break, carrying over the current line's indentation.
    fn newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        let selection = self.selection();
        let line = self.buffer.line_of(selection.start);
        let line_start = self.buffer.line_start(line);
        let before_cursor = self.buffer.slice(line_start..selection.start);
        let indent = &before_cursor[..before_cursor.len() - before_cursor.trim_start().len()];
        let text = format!("{}{indent}", self.buffer.line_ending());
        self.replace(selection, &text, cx);
    }

    /// Inserts spaces up to the next tab stop.
    fn tab(&mut self, _: &Tab, _: &mut Window, cx: &mut Context<Self>) {
        let selection = self.selection();
        let width = TAB_SIZE - self.buffer.column(selection.start) % TAB_SIZE;
        self.replace(selection, &" ".repeat(width), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.anchor = 0;
        self.head = self.buffer.len();
        cx.notify();
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let selection = self.selection();
        if !selection.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.buffer.slice(selection)));
        }
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        let selection = self.selection();
        if !selection.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.buffer.slice(selection.clone()),
            ));
            self.replace(selection, "", cx);
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = self.normalize_line_endings(&text);
            self.replace(self.selection(), &text, cx);
        }
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        let was_dirty = self.buffer.is_dirty();
        if let Some(range) = self.buffer.undo() {
            self.anchor = range.end;
            self.head = range.end;
            self.autoscroll = true;
            self.finish_change(was_dirty, cx);
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        let was_dirty = self.buffer.is_dirty();
        if let Some(range) = self.buffer.redo() {
            self.anchor = range.end;
            self.head = range.end;
            self.autoscroll = true;
            self.finish_change(was_dirty, cx);
        }
    }

    fn save_action(&mut self, _: &Save, _: &mut Window, cx: &mut Context<Self>) {
        self.save(cx);
    }

    fn offset_for_position(&self, position: Point<Pixels>) -> usize {
        let Some(layout) = &self.layout else {
            return self.head;
        };
        let row = (position.y - layout.text_bounds.top() + self.scroll.y) / layout.line_height;
        let line = (row.max(0.) as usize).min(self.buffer.line_count() - 1);
        let x = position.x - layout.text_bounds.left() + self.scroll.x;
        let line_start = self.buffer.line_start(line);
        match line
            .checked_sub(layout.first_line)
            .and_then(|i| layout.lines.get(i))
        {
            Some(shaped) => line_start + shaped.closest_index_for_x(x),
            None => self
                .buffer
                .offset_at(line, (x / layout.em_width).round().max(0.) as usize),
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        let offset = self.offset_for_position(event.position);
        self.goal_column = None;
        if event.click_count >= 2 {
            let end = self.buffer.next_word_end(offset);
            self.anchor = self.buffer.prev_word_start(end);
            self.head = end;
            cx.notify();
        } else {
            self.move_head(offset, event.modifiers.shift, cx);
            self.selecting = true;
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            let offset = self.offset_for_position(event.position);
            self.move_head(offset, true, cx);
        }
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line_height = self
            .layout
            .as_ref()
            .map_or(px(theme::EDITOR_LINE_HEIGHT), |layout| layout.line_height);
        let mut delta = event.delta.pixel_delta(line_height);
        if event.modifiers.shift && delta.x == px(0.) {
            delta = point(delta.y, px(0.));
        }
        // Upper bounds are clamped in `layout`, which knows the viewport.
        self.scroll.x = (self.scroll.x - delta.x).max(px(0.));
        self.scroll.y = (self.scroll.y - delta.y).max(px(0.));
        cx.notify();
    }

    /// Shapes the visible lines and resolves scrolling for `bounds`.
    fn layout(&mut self, bounds: Bounds<Pixels>, window: &mut Window) {
        let style = window.text_style();
        let font = style.font();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let run = |len: usize, color| TextRun {
            len,
            font: font.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let text_system = window.text_system().clone();
        let shape = |text: String, runs: &[TextRun]| {
            text_system.shape_line(text.into(), font_size, runs, None)
        };

        let em_width = shape("m".into(), &[run(1, style.color)]).width;
        let line_count = self.buffer.line_count();
        let digits = line_count.to_string().len().max(3);
        let gutter_width = em_width * digits as f32 + GUTTER_PADDING * 2.;
        let text_bounds = Bounds::from_corners(
            point(bounds.left() + gutter_width, bounds.top()),
            bounds.bottom_right(),
        );
        let viewport = text_bounds.size;

        let cursor_line = self.buffer.line_of(self.head);
        if self.autoscroll {
            let top = line_height * cursor_line as f32;
            if top < self.scroll.y {
                self.scroll.y = top;
            } else if top + line_height > self.scroll.y + viewport.height {
                self.scroll.y = top + line_height - viewport.height;
            }
        }
        let max_scroll_y = line_height * line_count.saturating_sub(1) as f32;
        self.scroll.y = self.scroll.y.min(max_scroll_y).max(px(0.));

        let first_line = (self.scroll.y / line_height).floor() as usize;
        let last_line =
            (first_line + (viewport.height / line_height).ceil() as usize + 1).min(line_count);
        let text_color = rgb(theme::TEXT).into();
        let muted = rgb(theme::MUTED).into();

        let mut lines = Vec::with_capacity(last_line - first_line);
        let mut line_numbers = Vec::with_capacity(last_line - first_line);
        for line in first_line..last_line {
            let start = self.buffer.line_start(line);
            // Tabs render as one space for now; byte offsets stay aligned.
            let text = self.buffer.line_text(line).replace('\t', " ");
            let mut runs = vec![run(text.len(), text_color)];
            if let Some(marked) = &self.marked {
                let lo = marked.start.clamp(start, start + text.len()) - start;
                let hi = marked.end.clamp(start, start + text.len()) - start;
                if lo < hi {
                    let underline = Some(UnderlineStyle {
                        color: Some(text_color),
                        thickness: px(1.),
                        wavy: false,
                    });
                    runs = [
                        run(lo, text_color),
                        TextRun {
                            underline,
                            ..run(hi - lo, text_color)
                        },
                        run(text.len() - hi, text_color),
                    ]
                    .into_iter()
                    .filter(|run| run.len > 0)
                    .collect();
                }
            }
            if text.is_empty() {
                runs.clear();
            }
            lines.push(shape(text, &runs));

            let number = (line + 1).to_string();
            let color = if line == cursor_line {
                text_color
            } else {
                muted
            };
            line_numbers.push(shape(number.clone(), &[run(number.len(), color)]));
        }

        if self.autoscroll
            && let Some(shaped) = cursor_line
                .checked_sub(first_line)
                .and_then(|i| lines.get(i))
        {
            let x = shaped.x_for_index(self.head - self.buffer.line_start(cursor_line));
            let margin = em_width * 2.;
            if x < self.scroll.x {
                self.scroll.x = (x - margin).max(px(0.));
            } else if x + margin > self.scroll.x + viewport.width {
                self.scroll.x = x + margin - viewport.width;
            }
        }
        self.autoscroll = false;

        self.layout = Some(EditorLayout {
            text_bounds,
            gutter_width,
            line_height,
            em_width,
            first_line,
            lines,
            line_numbers,
        });
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let Some(layout) = &self.layout else {
            return;
        };
        let line_height = layout.line_height;
        let line_top = |line: usize| bounds.top() + line_height * line as f32 - self.scroll.y;
        let text_left = layout.text_bounds.left() - self.scroll.x;
        let selection = self.selection();
        let cursor_line = self.buffer.line_of(self.head);
        let focused = self.focus_handle.is_focused(window);

        for (i, number) in layout.line_numbers.iter().enumerate() {
            let x = bounds.left() + layout.gutter_width - GUTTER_PADDING - number.width;
            number
                .paint(
                    point(x, line_top(layout.first_line + i)),
                    line_height,
                    window,
                    cx,
                )
                .ok();
        }

        window.with_content_mask(
            Some(ContentMask {
                bounds: layout.text_bounds,
            }),
            |window| {
                for (i, shaped) in layout.lines.iter().enumerate() {
                    let line = layout.first_line + i;
                    let top = line_top(line);
                    let start = self.buffer.line_start(line);
                    let end = self.buffer.line_end(line);

                    if !selection.is_empty() && selection.start <= end && selection.end >= start {
                        let x0 = shaped.x_for_index(selection.start.max(start) - start);
                        let mut x1 = shaped.x_for_index(selection.end.min(end) - start);
                        if selection.end > end {
                            // Show the selected line break.
                            x1 += layout.em_width / 2.;
                        }
                        window.paint_quad(fill(
                            Bounds::from_corners(
                                point(text_left + x0, top),
                                point(text_left + x1, top + line_height),
                            ),
                            rgb(theme::SELECTION),
                        ));
                    }

                    shaped
                        .paint(point(text_left, top), line_height, window, cx)
                        .ok();

                    if focused && line == cursor_line {
                        let x = text_left + shaped.x_for_index(self.head - start);
                        window.paint_quad(fill(
                            Bounds::from_corners(
                                point(x, top),
                                point(x + px(2.), top + line_height),
                            ),
                            rgb(theme::CURSOR),
                        ));
                    }
                }
            },
        );
    }
}

impl EntityInputHandler for Editor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.buffer.offset_from_utf16(range_utf16.start)
            ..self.buffer.offset_from_utf16(range_utf16.end);
        actual_range.replace(
            self.buffer.offset_to_utf16(range.start)..self.buffer.offset_to_utf16(range.end),
        );
        Some(self.buffer.slice(range))
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let selection = self.selection();
        Some(UTF16Selection {
            range: self.buffer.offset_to_utf16(selection.start)
                ..self.buffer.offset_to_utf16(selection.end),
            reversed: self.head < self.anchor,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|range| {
            self.buffer.offset_to_utf16(range.start)..self.buffer.offset_to_utf16(range.end)
        })
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .map(|r| self.buffer.offset_from_utf16(r.start)..self.buffer.offset_from_utf16(r.end))
            .or(self.marked.clone())
            .unwrap_or_else(|| self.selection());
        self.replace(range, text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .map(|r| self.buffer.offset_from_utf16(r.start)..self.buffer.offset_from_utf16(r.end))
            .or(self.marked.clone())
            .unwrap_or_else(|| self.selection());
        self.replace(range.clone(), text, cx);
        self.marked = (!text.is_empty()).then(|| range.start..range.start + text.len());
        if let Some(selected) = new_selected_range_utf16 {
            // Relative to the inserted text, in UTF-16 units.
            let base = self.buffer.offset_to_utf16(range.start);
            self.anchor = self.buffer.offset_from_utf16(base + selected.start);
            self.head = self.buffer.offset_from_utf16(base + selected.end);
        }
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.layout.as_ref()?;
        let start = self.buffer.offset_from_utf16(range_utf16.start);
        let end = self.buffer.offset_from_utf16(range_utf16.end);
        let line = self.buffer.line_of(start);
        let shaped = layout.lines.get(line.checked_sub(layout.first_line)?)?;
        let line_start = self.buffer.line_start(line);
        let end = end.min(self.buffer.line_end(line));
        let left = layout.text_bounds.left() - self.scroll.x;
        let top = layout.text_bounds.top() + layout.line_height * line as f32 - self.scroll.y;
        Some(Bounds::from_corners(
            point(left + shaped.x_for_index(start - line_start), top),
            point(
                left + shaped.x_for_index(end - line_start),
                top + layout.line_height,
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.buffer.offset_to_utf16(self.offset_for_position(point)))
    }
}

struct EditorElement {
    editor: Entity<Editor>,
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
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
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.editor
            .update(cx, |editor, _| editor.layout(bounds, window));
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.editor.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );
        self.editor
            .update(cx, |editor, cx| editor.paint(bounds, window, cx));
    }
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Editor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let line = self.buffer.line_of(self.head) + 1;
        let column = self.buffer.column(self.head) + 1;
        let line_ending = if self.buffer.line_ending() == "\r\n" {
            "CRLF"
        } else {
            "LF"
        };
        let (message, message_color) = match &self.error {
            Some(error) => (error.clone(), theme::ERROR),
            None => (
                self.buffer
                    .path()
                    .map(|path| path.display().to_string().into())
                    .unwrap_or_default(),
                theme::MUTED,
            ),
        };

        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::BACKGROUND))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::doc_start))
            .on_action(cx.listener(Self::doc_end))
            .on_action(cx.listener(Self::select_doc_start))
            .on_action(cx.listener(Self::select_doc_end))
            .on_action(cx.listener(Self::page_up))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::tab))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::save_action))
            .child(
                div()
                    .flex_1()
                    .relative()
                    .overflow_hidden()
                    .cursor(CursorStyle::IBeam)
                    .font_family(theme::MONO_FONT)
                    .text_size(px(theme::EDITOR_FONT_SIZE))
                    .line_height(px(theme::EDITOR_LINE_HEIGHT))
                    .text_color(rgb(theme::TEXT))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
                    .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
                    .child(EditorElement {
                        editor: cx.entity(),
                    }),
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(rgb(theme::BORDER))
                    .bg(rgb(theme::PANEL))
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(div().text_color(rgb(message_color)).child(message))
                    .child(format!("Ln {line}, Col {column}   {line_ending}   UTF-8")),
            )
    }
}
