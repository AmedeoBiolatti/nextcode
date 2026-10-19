//! The minimap (VS Code): the whole document scaled down beside the text,
//! one [`ROW_HEIGHT`] row per screen row (wrapped rows and folds follow
//! the text), each run of same-coloured characters drawn as a block in its
//! syntax colour. A translucent slider shows the visible part; a click
//! centres the text there and dragging the slider scrolls. When the rows
//! don't fit, the minimap scrolls with the text so its top and bottom
//! meet the document's. Change markers, errors and warnings, selections
//! and find matches show over it.
//!
//! Blocks are built per chunk of [`CHUNK_LINES`] lines and cached for the
//! buffer version, so a frame only looks up the rows it shows.

use std::{ops::Range, sync::Arc};

use git::LineChangeKind;
use gpui::{Bounds, ContentMask, Context, Hsla, Pixels, Point, Window, fill, point, px, size};
use syntax::{Highlight, Language, Syntax};
use text::Buffer;

use super::{Drag, Editor, EditorLayout, Severity, display_map::DisplayRow, scrollbar};
use crate::{settings::MinimapSide, theme};

/// Height of one row.
pub(super) const ROW_HEIGHT: Pixels = px(2.);
/// Width of one character.
const CHAR_WIDTH: Pixels = px(1.);
/// Height of a character block (the rest of the row is a gap).
const BLOCK_HEIGHT: Pixels = px(1.5);
/// Space before the first column (change markers sit in it).
const PADDING: Pixels = px(6.);
/// Narrower editors have no minimap.
const MIN_EDITOR_WIDTH: Pixels = px(500.);
/// The minimap takes at most this share of the editor's width.
const MAX_WIDTH_SHARE: f32 = 0.2;
/// Lines per cached chunk of blocks.
const CHUNK_LINES: usize = 128;
/// Chunks kept (a tall minimap shows about six).
const MAX_CHUNKS: usize = 16;
/// Chunks with more text are highlighted line by line, only as far as the
/// drawn columns go (minified files).
const WHOLE_CHUNK_BYTES: usize = 256 * 1024;

/// A run of same-coloured characters: columns `start..end` of its line.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Block {
    start: u32,
    end: u32,
    color: Hsla,
}

/// Blocks of lines `first_line..first_line + line_starts.len() - 1`.
struct Chunk {
    first_line: usize,
    /// Index in `blocks` of each line's first block, and the end.
    line_starts: Vec<u32>,
    blocks: Vec<Block>,
    /// Use stamp, for evicting the least recently used chunk.
    used: u64,
}

impl Chunk {
    /// The line after the last one held.
    fn end_line(&self) -> usize {
        self.first_line + self.line_starts.len().saturating_sub(1)
    }

    fn line_blocks(&self, line: usize) -> &[Block] {
        let index = line - self.first_line;
        let start = self.line_starts[index] as usize;
        let end = self.line_starts[index + 1] as usize;
        &self.blocks[start..end]
    }
}

/// What the cached blocks were built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CacheKey {
    version: u64,
    theme: usize,
    language: Option<Language>,
    max_columns: usize,
}

#[derive(Default)]
pub(super) struct MinimapState {
    key: Option<CacheKey>,
    chunks: Vec<Chunk>,
    tick: u64,
    /// Lines edited since the chunks were last checked, as (first line,
    /// last line before, last line after) in order, so only the chunks
    /// they touch are rebuilt; `None` once too many to track.
    edits: Option<Vec<(usize, usize, usize)>>,
    /// The mouse is over the minimap: the slider brightens.
    hovered: bool,
}

/// Edits tracked between two frames at most, before everything is rebuilt.
const MAX_TRACKED_EDITS: usize = 256;

impl MinimapState {
    /// Notes the lines `changes` edited, for `evict_edited`.
    fn record(&mut self, changes: &[text::Change]) {
        let Some(edits) = &mut self.edits else {
            return;
        };
        if edits.len() + changes.len() > MAX_TRACKED_EDITS {
            self.edits = None;
            return;
        }
        edits.extend(changes.iter().map(|change| {
            (
                change.start_point.row,
                change.old_end_point.row,
                change.new_end_point.row,
            )
        }));
    }

    /// Drops the chunks the recorded edits touched: the ones holding an
    /// edited line, or every chunk from the first edited line on when
    /// lines were added or removed (later lines moved). Everything when
    /// the edits were not tracked.
    fn evict_edited(&mut self) {
        let Some(edits) = self.edits.replace(Vec::new()) else {
            self.chunks.clear();
            return;
        };
        for (start, old_end, new_end) in edits {
            if old_end == new_end {
                self.chunks
                    .retain(|chunk| chunk.first_line > old_end || start >= chunk.end_line());
            } else {
                self.chunks.retain(|chunk| chunk.end_line() <= start);
            }
        }
    }
}

/// Scroll math of a minimap `height` tall for `row_count` rows of
/// `line_height`, `viewport` of them visible.
#[derive(Debug, Clone, Copy)]
pub(super) struct Geometry {
    pub height: Pixels,
    pub row_count: usize,
    pub line_height: Pixels,
    pub viewport: Pixels,
}

impl Geometry {
    /// The text's scroll range: the last row may reach the top.
    fn max_scroll(&self) -> Pixels {
        self.line_height * self.row_count.saturating_sub(1) as f32
    }

    pub fn slider_height(&self) -> Pixels {
        ROW_HEIGHT * (self.viewport / self.line_height)
    }

    /// How much taller than the minimap its rows are (with the slider at
    /// the last row).
    fn overflow(&self) -> Pixels {
        let content = ROW_HEIGHT * self.row_count.saturating_sub(1) as f32 + self.slider_height();
        (content - self.height).max(px(0.))
    }

    /// How far the slider moves over the whole scroll range.
    fn travel(&self) -> Pixels {
        ROW_HEIGHT * self.row_count.saturating_sub(1) as f32 - self.overflow()
    }

    fn ratio(&self, scroll: Pixels) -> f32 {
        let max = self.max_scroll();
        if max <= px(0.) {
            0.
        } else {
            (scroll / max).clamp(0., 1.)
        }
    }

    /// How far the minimap's rows are scrolled for the text's `scroll`:
    /// in step with it, so both ends meet.
    pub fn offset(&self, scroll: Pixels) -> Pixels {
        self.overflow() * self.ratio(scroll)
    }

    /// The slider's top, from the minimap's top.
    pub fn slider_top(&self, scroll: Pixels) -> Pixels {
        self.travel().max(px(0.)) * self.ratio(scroll)
    }

    /// The row drawn at `y` from the minimap's top.
    pub fn row_at(&self, y: Pixels, scroll: Pixels) -> usize {
        let row = ((y + self.offset(scroll)) / ROW_HEIGHT).floor().max(0.) as usize;
        row.min(self.row_count.saturating_sub(1))
    }

    /// The scroll that centres `row` in the text.
    pub fn centered_scroll(&self, row: usize) -> Pixels {
        let top = self.line_height * row as f32 + self.line_height / 2. - self.viewport / 2.;
        top.clamp(px(0.), self.max_scroll())
    }

    /// The scroll after the slider, grabbed at `start`, moved `delta`.
    pub fn dragged_scroll(&self, start: Pixels, delta: Pixels) -> Pixels {
        let travel = self.travel();
        if travel <= px(0.) {
            return start;
        }
        (start + delta * (self.max_scroll() / travel)).clamp(px(0.), self.max_scroll())
    }
}

/// Where the minimap was laid out, for painting and hit-testing.
pub(super) struct MinimapLayout {
    pub bounds: Bounds<Pixels>,
    geometry: Geometry,
    /// The text's scroll this was laid out for.
    scroll: Pixels,
    /// Rows from `first_row` that fill the minimap.
    first_row: usize,
    rows: Vec<DisplayRow>,
    pub slider: Bounds<Pixels>,
}

impl MinimapLayout {
    /// Window y of the top of `rows[index]`.
    fn row_top(&self, index: usize) -> Pixels {
        self.bounds.top() + ROW_HEIGHT * (self.first_row + index) as f32
            - self.geometry.offset(self.scroll)
    }

    /// Window y of the top of screen row `row`.
    fn screen_row_top(&self, row: usize) -> Pixels {
        self.bounds.top() + ROW_HEIGHT * row as f32 - self.geometry.offset(self.scroll)
    }
}

/// The scrollbar's area: the text, and with the minimap on the right also
/// the minimap and the strip after it (text | minimap | scrollbar).
pub(super) fn scrollbar_area(
    text: Bounds<Pixels>,
    full: Bounds<Pixels>,
    minimap: Option<Bounds<Pixels>>,
) -> Bounds<Pixels> {
    match minimap {
        Some(column) if column.left() >= text.right() => {
            Bounds::from_corners(text.origin, point(full.right(), text.bottom()))
        }
        _ => text,
    }
}

/// Blocks of `lines` (at most [`CHUNK_LINES`]) up to `max_columns`, in the
/// colours `color` gives highlights (`None`: plain text).
fn build_chunk(
    buffer: &Buffer,
    syntax: Option<&Syntax>,
    color: &dyn Fn(Option<Highlight>) -> Hsla,
    lines: Range<usize>,
    max_columns: usize,
) -> Chunk {
    let start = buffer.line_start(lines.start);
    let end = buffer.line_end(lines.end - 1);
    let whole = end - start <= WHOLE_CHUNK_BYTES;
    let mut spans = match syntax {
        Some(syntax) if whole => syntax.highlights(buffer, start..end),
        _ => Vec::new(),
    };
    let mut chunk = Chunk {
        first_line: lines.start,
        line_starts: Vec::with_capacity(lines.len() + 1),
        blocks: Vec::new(),
        used: 0,
    };
    for line in lines {
        let line_first_block = chunk.blocks.len();
        chunk.line_starts.push(line_first_block as u32);
        let line_start = buffer.line_start(line);
        let line_end = buffer.line_end(line);
        if !whole && let Some(syntax) = syntax {
            // Chars take up to 4 bytes: enough for the drawn columns.
            let end = line_end.min(line_start + max_columns * 4);
            spans = syntax.highlights(buffer, line_start..end);
        }
        let mut span = spans.partition_point(|span| span.range.end <= line_start);
        let mut column = 0u32;
        let mut offset = line_start;
        'line: for piece in buffer.chunks(line_start..line_end) {
            for c in piece.chars() {
                if column as usize >= max_columns {
                    break 'line;
                }
                if !c.is_whitespace() {
                    while span < spans.len() && spans[span].range.end <= offset {
                        span += 1;
                    }
                    let highlight = spans
                        .get(span)
                        .filter(|span| span.range.start <= offset)
                        .map(|span| span.highlight);
                    let color = color(highlight);
                    let in_line = chunk.blocks.len() > line_first_block;
                    match chunk.blocks.last_mut() {
                        Some(block) if in_line && block.end == column && block.color == color => {
                            block.end += 1;
                        }
                        _ => chunk.blocks.push(Block {
                            start: column,
                            end: column + 1,
                            color,
                        }),
                    }
                }
                column += 1;
                offset += c.len_utf8();
            }
        }
    }
    chunk.line_starts.push(chunk.blocks.len() as u32);
    chunk
}

impl Editor {
    /// Splits `bounds` into the gutter and text's part and the minimap's
    /// column (`None`: no minimap here). On the right the column leaves
    /// the scrollbar's strip after it.
    pub(super) fn minimap_split(
        &self,
        bounds: Bounds<Pixels>,
    ) -> (Bounds<Pixels>, Option<Bounds<Pixels>>) {
        let settings = self.file_settings();
        let minimap = &settings.editor.minimap;
        if self.single_line
            || self.text_area
            || !minimap.enabled
            || bounds.size.width < MIN_EDITOR_WIDTH
        {
            return (bounds, None);
        }
        let width = (PADDING * 2. + CHAR_WIDTH * minimap.max_columns.max(1) as f32)
            .min(bounds.size.width * MAX_WIDTH_SHARE)
            .round();
        let height = bounds.size.height;
        match minimap.side {
            MinimapSide::Right => {
                let right = bounds.right() - scrollbar::WIDTH;
                let column = Bounds::new(point(right - width, bounds.top()), size(width, height));
                let rest =
                    Bounds::from_corners(bounds.origin, point(column.left(), bounds.bottom()));
                (rest, Some(column))
            }
            MinimapSide::Left => {
                let column = Bounds::new(bounds.origin, size(width, height));
                let rest = Bounds::from_corners(
                    point(column.right(), bounds.top()),
                    bounds.bottom_right(),
                );
                (rest, Some(column))
            }
        }
    }

    /// Lays out the minimap in `column` for the resolved scroll, building
    /// the blocks of the rows it shows that are not cached yet.
    pub(super) fn layout_minimap(
        &mut self,
        column: Bounds<Pixels>,
        line_height: Pixels,
        viewport: Pixels,
    ) -> MinimapLayout {
        let geometry = Geometry {
            height: column.size.height,
            row_count: self.display.row_count(),
            line_height,
            viewport,
        };
        let scroll = self.scroll.y;
        let first_row = (geometry.offset(scroll) / ROW_HEIGHT).floor().max(0.) as usize;
        let count = (column.size.height / ROW_HEIGHT).ceil() as usize + 1;
        let rows = self.display.rows(&self.buffer, first_row, count);
        if let (Some(first), Some(last)) = (rows.first(), rows.last()) {
            self.cache_minimap_blocks(first.line..last.line + 1);
        }
        let slider = Bounds::new(
            point(column.left(), column.top() + geometry.slider_top(scroll)),
            size(column.size.width, geometry.slider_height()),
        );
        MinimapLayout {
            bounds: column,
            geometry,
            scroll,
            first_row,
            rows,
            slider,
        }
    }

    /// After an edit: notes the lines `changes` touched, so the next frame
    /// rebuilds only their chunks.
    pub(super) fn minimap_edited(&mut self) {
        self.minimap.record(&self.changes);
    }

    /// Builds the chunks covering `lines` that are missing, dropping the
    /// ones edited since (all of them when the theme or settings changed).
    fn cache_minimap_blocks(&mut self, lines: Range<usize>) {
        let max_columns = self.file_settings().editor.minimap.max_columns.max(1);
        let theme = theme::current();
        let key = CacheKey {
            version: self.version,
            theme: Arc::as_ptr(&theme) as usize,
            language: self.language(),
            max_columns,
        };
        let language = self.language();
        let state = &mut self.minimap;
        if state.key != Some(key) {
            let edited_only = state.key.is_some_and(|old| {
                CacheKey {
                    version: key.version,
                    ..old
                } == key
            });
            if edited_only {
                state.evict_edited();
            } else {
                state.chunks.clear();
                state.edits = Some(Vec::new());
            }
            state.key = Some(key);
        }
        state.tick += 1;
        let text = theme::text();
        let color = |highlight: Option<Highlight>| match highlight {
            Some(highlight) => theme.syntax_color(language, highlight),
            None => text,
        };
        let line_count = self.buffer.line_count();
        for index in lines.start / CHUNK_LINES..=(lines.end - 1) / CHUNK_LINES {
            let first_line = index * CHUNK_LINES;
            let tick = state.tick;
            if let Some(chunk) = state.chunks.iter_mut().find(|c| c.first_line == first_line) {
                chunk.used = tick;
                continue;
            }
            let chunk_lines = first_line..(first_line + CHUNK_LINES).min(line_count);
            let mut chunk = build_chunk(
                &self.buffer,
                self.syntax.as_ref(),
                &color,
                chunk_lines,
                max_columns,
            );
            chunk.used = tick;
            if state.chunks.len() >= MAX_CHUNKS
                && let Some(oldest) = state
                    .chunks
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, chunk)| chunk.used)
                    .map(|(i, _)| i)
            {
                state.chunks.swap_remove(oldest);
            }
            state.chunks.push(chunk);
        }
    }

    fn minimap_line_blocks(&self, line: usize) -> &[Block] {
        let first_line = line / CHUNK_LINES * CHUNK_LINES;
        self.minimap
            .chunks
            .iter()
            .find(|chunk| chunk.first_line == first_line)
            .map_or(&[], |chunk| chunk.line_blocks(line))
    }

    /// Columns of `row`: of its start and end in its line.
    fn row_columns(&self, row: &DisplayRow) -> (u32, u32) {
        let start = if row.first {
            0
        } else {
            self.buffer.column(row.range.start) as u32
        };
        let end = if row.last {
            u32::MAX
        } else {
            self.buffer.column(row.range.end) as u32
        };
        (start, end)
    }

    pub(super) fn paint_minimap(&self, layout: &EditorLayout, window: &mut Window) {
        let Some(minimap) = &layout.minimap else {
            return;
        };
        let column = minimap.bounds;
        window.paint_quad(fill(column, theme::background()));
        let text_left = column.left() + PADDING;
        let x_of = |columns: u32| text_left + CHAR_WIDTH * columns as f32;
        let rows = &minimap.rows;
        let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
            return;
        };
        let shown = first.range.start..last.range.end;

        window.with_content_mask(Some(ContentMask { bounds: column }), |window| {
            // Backgrounds over `range`'s part of each row.
            let highlight = |window: &mut Window, range: &Range<usize>, color: Hsla| {
                let index = rows.partition_point(|row| row.range.end < range.start);
                for (i, row) in rows.iter().enumerate().skip(index) {
                    if row.range.start > range.end {
                        break;
                    }
                    let start = range.start.max(row.range.start);
                    let end = range.end.min(row.range.end);
                    if start == end && !range.is_empty() && !(row.last && start == row.range.end) {
                        continue;
                    }
                    let (row_start, _) = self.row_columns(row);
                    let column_of = |offset: usize| {
                        (self.buffer.column(offset) as u32).saturating_sub(row_start)
                            + row.indent as u32
                    };
                    let (x0, x1) = (column_of(start), column_of(end));
                    // A line break inside the range shows as one column.
                    let x1 = x1.max(x0 + 1) + u32::from(range.end > row.range.end && row.last);
                    window.paint_quad(fill(
                        Bounds::from_corners(
                            point(x_of(x0), minimap.row_top(i)),
                            point(x_of(x1), minimap.row_top(i) + ROW_HEIGHT),
                        ),
                        color,
                    ));
                }
            };
            // Errors over warnings.
            for severity in [Severity::Warning, Severity::Error] {
                for diagnostic in &self.diagnostics {
                    if diagnostic.severity == severity
                        && diagnostic.range.start <= shown.end
                        && diagnostic.range.end >= shown.start
                    {
                        highlight(window, &diagnostic.range, severity.color().opacity(0.6));
                    }
                }
            }
            if let Some(find) = self.find.as_ref().filter(|find| find.open) {
                let index = find.matches.partition_point(|m| m.end < shown.start);
                let color = theme::find_match_active();
                for m in find.matches[index..]
                    .iter()
                    .take_while(|m| m.start <= shown.end)
                {
                    highlight(window, m, color);
                }
            }
            let selections = self.selections.all();
            let index = selections.partition_point(|s| s.end() < shown.start);
            for s in selections[index..]
                .iter()
                .take_while(|s| s.start() <= shown.end)
            {
                if !s.is_empty() {
                    highlight(window, &s.range(), theme::selection());
                }
            }

            for (i, row) in rows.iter().enumerate() {
                let (row_start, row_end) = self.row_columns(row);
                let top = minimap.row_top(i);
                for block in self.minimap_line_blocks(row.line) {
                    let start = block.start.max(row_start);
                    let end = block.end.min(row_end);
                    if start >= end {
                        continue;
                    }
                    let left = x_of(start - row_start + row.indent as u32);
                    window.paint_quad(fill(
                        Bounds::new(
                            point(left, top),
                            size(CHAR_WIDTH * (end - start) as f32, BLOCK_HEIGHT),
                        ),
                        block.color,
                    ));
                }
            }

            // Change markers in the padding, as in the gutter.
            let changes = &self.git.changes;
            let index = changes.partition_point(|c| c.new.end < first.line);
            for change in changes[index..]
                .iter()
                .take_while(|c| c.new.start <= last.line)
            {
                let left = column.left() + px(1.);
                let top = minimap.screen_row_top(self.display.line_row(change.new.start));
                let (bottom, color) = match change.kind {
                    LineChangeKind::Deleted => (top + ROW_HEIGHT, theme::git_deleted()),
                    kind => {
                        let end = self
                            .display
                            .line_row(change.new.end)
                            .max(self.display.line_row(change.new.start) + 1);
                        let color = if kind == LineChangeKind::Added {
                            theme::git_added()
                        } else {
                            theme::git_modified()
                        };
                        (minimap.screen_row_top(end), color)
                    }
                };
                window.paint_quad(fill(
                    Bounds::from_corners(point(left, top), point(left + px(2.), bottom)),
                    color,
                ));
            }

            let active = self.minimap.hovered || matches!(self.drag, Some(Drag::Minimap { .. }));
            window.paint_quad(fill(
                minimap.slider,
                if active {
                    theme::scrollbar_thumb_hover()
                } else {
                    theme::scrollbar_thumb()
                },
            ));
        });
    }

    /// A press on the minimap: grabs the slider, or first centres the text
    /// on the row clicked.
    pub(super) fn minimap_mouse_down(
        &mut self,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(minimap) = self.layout.as_ref().and_then(|l| l.minimap.as_ref()) else {
            return false;
        };
        if !minimap.bounds.contains(&position) {
            return false;
        }
        if !minimap.slider.contains(&position) {
            let row = minimap
                .geometry
                .row_at(position.y - minimap.bounds.top(), minimap.scroll);
            self.scroll.y = minimap.geometry.centered_scroll(row);
        }
        self.drag = Some(Drag::Minimap {
            y: position.y,
            scroll: self.scroll.y,
        });
        cx.notify();
        true
    }

    /// Dragging the slider from `start_y`, where the scroll was
    /// `start_scroll`.
    pub(super) fn minimap_drag(
        &mut self,
        y: Pixels,
        start_y: Pixels,
        start_scroll: Pixels,
        cx: &mut Context<Self>,
    ) {
        if let Some(minimap) = self.layout.as_ref().and_then(|l| l.minimap.as_ref()) {
            self.scroll.y = minimap.geometry.dragged_scroll(start_scroll, y - start_y);
            cx.notify();
        }
    }

    pub(super) fn over_minimap(&self, position: Point<Pixels>) -> bool {
        self.layout
            .as_ref()
            .and_then(|layout| layout.minimap.as_ref())
            .is_some_and(|minimap| minimap.bounds.contains(&position))
    }

    pub(super) fn minimap_mouse_moved(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let hovered = self.over_minimap(position);
        if hovered != self.minimap.hovered {
            self.minimap.hovered = hovered;
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use gpui::TestAppContext;

    use super::*;
    use crate::editor::tests::{editor, update};

    fn geometry(row_count: usize) -> Geometry {
        Geometry {
            height: px(400.),
            row_count,
            line_height: px(20.),
            viewport: px(400.),
        }
    }

    fn close(a: Pixels, b: f32) -> bool {
        (f32::from(a) - b).abs() < 0.01
    }

    #[test]
    fn edits_drop_only_the_chunks_they_touch() {
        let chunk = |first_line: usize, lines: usize| Chunk {
            first_line,
            line_starts: vec![0; lines + 1],
            blocks: Vec::new(),
            used: 0,
        };
        let change = |start: usize, old_end: usize, new_end: usize| {
            let point = |row| text::Point { row, column: 0 };
            text::Change {
                start: 0,
                old_end: 0,
                new_end: 0,
                start_point: point(start),
                old_end_point: point(old_end),
                new_end_point: point(new_end),
                start_utf16: point(start),
                old_end_utf16: point(old_end),
            }
        };
        let first_lines = |state: &MinimapState| -> Vec<usize> {
            state.chunks.iter().map(|chunk| chunk.first_line).collect()
        };
        let mut state = MinimapState {
            chunks: vec![
                chunk(0, 128),
                chunk(128, 128),
                chunk(256, 128),
                chunk(384, 10),
            ],
            edits: Some(Vec::new()),
            ..Default::default()
        };
        // Typing on line 130: only its chunk goes.
        state.record(&[change(130, 130, 130)]);
        state.evict_edited();
        assert_eq!(first_lines(&state), [0, 256, 384]);
        // A change across lines 255..256 without new lines: both chunks.
        state.chunks.push(chunk(128, 128));
        state.record(&[change(255, 256, 256)]);
        state.evict_edited();
        assert_eq!(first_lines(&state), [0, 384]);
        // A new line at 300: its chunk and everything after move.
        state.chunks.extend([chunk(128, 128), chunk(256, 128)]);
        state.record(&[change(300, 300, 301)]);
        state.evict_edited();
        assert_eq!(first_lines(&state), [0, 128]);
        // Edits recorded in order: the first untracked batch drops all.
        state.edits = None;
        state.record(&[change(0, 0, 0)]);
        state.evict_edited();
        assert!(state.chunks.is_empty());
        assert_eq!(state.edits, Some(Vec::new()), "tracking again");
        // Too many edits to track: everything goes at the next frame.
        let many: Vec<_> = (0..MAX_TRACKED_EDITS + 1)
            .map(|_| change(1, 1, 1))
            .collect();
        state.record(&many);
        assert_eq!(state.edits, None);
    }

    #[test]
    fn short_documents_map_rows_one_to_one() {
        // 101 rows of 2 px: 200 px of rows plus a 40 px slider fit in 400.
        let g = geometry(101);
        assert_eq!(g.slider_height(), px(40.));
        assert_eq!(g.offset(px(2000.)), px(0.), "never scrolls");
        // Scrolled 10 rows: the slider is 10 minimap rows down.
        assert!(close(g.slider_top(px(200.)), 20.));
        assert!(close(g.slider_top(px(2000.)), 200.), "at the last row");
        assert_eq!(g.row_at(px(21.), px(0.)), 10);
        assert_eq!(g.row_at(px(399.), px(0.)), 100, "clamped to the last row");
        // Dragging the slider one minimap row scrolls one text row.
        assert!(close(g.dragged_scroll(px(200.), px(2.)), 220.));
        assert_eq!(g.dragged_scroll(px(200.), px(-500.)), px(0.));
        assert_eq!(g.dragged_scroll(px(200.), px(500.)), px(2000.));
    }

    #[test]
    fn long_documents_scroll_the_minimap_in_step() {
        // 1001 rows: 2000 px of rows plus the slider, in 400 px.
        let g = geometry(1001);
        let max_scroll = 20_000.;
        assert_eq!(g.offset(px(0.)), px(0.));
        assert!(close(g.offset(px(max_scroll)), 2040. - 400.));
        // Half way: the slider is half way down its travel (400 - 40).
        assert!(close(g.slider_top(px(max_scroll / 2.)), 180.));
        assert!(close(g.slider_top(px(max_scroll)), 360.));
        // The slider covers the rows the text shows.
        for scroll in [0., 3_000., 10_000., max_scroll] {
            let top = g.slider_top(px(scroll));
            let first_visible = (scroll / 20.) as usize;
            assert_eq!(
                g.row_at(top + px(0.5), px(scroll)),
                first_visible,
                "at {scroll}"
            );
        }
        // Dragging the slider over its travel scrolls the whole document.
        assert!(close(g.dragged_scroll(px(0.), px(360.)), max_scroll));
        assert!(close(g.dragged_scroll(px(0.), px(180.)), max_scroll / 2.));
        // A click centres the clicked row.
        let row = g.row_at(px(100.), px(max_scroll / 2.));
        assert!(close(g.centered_scroll(row), row as f32 * 20. + 10. - 200.));
        assert_eq!(g.centered_scroll(0), px(0.), "clamped at the top");
        assert!(close(g.centered_scroll(1000), 20_000. + 10. - 200.));
    }

    #[test]
    fn huge_documents_keep_precision() {
        let g = geometry(100_000);
        let max_scroll = 20. * 99_999.;
        assert!(close(g.slider_top(px(max_scroll)), 360.));
        for row in [0usize, 1, 31_337, 50_000, 99_999] {
            let scroll = px(20. * row as f32).min(px(max_scroll));
            let top = g.slider_top(scroll);
            assert_eq!(g.row_at(top + px(0.5), scroll), row, "row {row}");
        }
        // One pixel of slider is ~278 rows of text.
        let step = g.dragged_scroll(px(0.), px(1.));
        assert!(close(step, max_scroll / 360.));
    }

    #[test]
    fn tiny_views_never_divide_by_zero() {
        let one_row = geometry(1);
        assert_eq!(one_row.slider_top(px(0.)), px(0.));
        assert_eq!(one_row.dragged_scroll(px(0.), px(50.)), px(0.));
        assert_eq!(one_row.row_at(px(300.), px(0.)), 0);
        let empty = geometry(0);
        assert_eq!(empty.row_at(px(10.), px(0.)), 0);
    }

    fn blocks(text: &str, name: &str, max_columns: usize) -> Vec<Vec<(u32, u32, bool)>> {
        let buffer = Buffer::with_path(text, PathBuf::from(name));
        let syntax = Language::from_path(name.as_ref()).and_then(|l| Syntax::new(l, &buffer));
        let plain = gpui::red();
        let color = |highlight: Option<Highlight>| match highlight {
            // One colour per highlight, so blocks split where tokens do.
            Some(highlight) => gpui::hsla(highlight.index() as f32 / 1000., 0.5, 0.5, 1.),
            None => plain,
        };
        let chunk = build_chunk(
            &buffer,
            syntax.as_ref(),
            &color,
            0..buffer.line_count(),
            max_columns,
        );
        (0..buffer.line_count())
            .map(|line| {
                chunk
                    .line_blocks(line)
                    .iter()
                    .map(|b| (b.start, b.end, b.color == plain))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn blocks_follow_tokens_and_skip_whitespace() {
        let lines = blocks("def f(x):\n    return 'é' + x\n\n", "a.py", 120);
        // `def` keyword, `f` function, `(x):` plain/variable runs.
        assert_eq!(lines[0][0], (0, 3, false));
        assert_eq!(lines[0][1], (4, 5, false));
        assert!(
            lines[0].iter().all(|b| b.0 >= 4 || b.1 <= 3),
            "no block over the space"
        );
        // Indentation is skipped; columns count chars, not bytes.
        assert_eq!(lines[1][0], (4, 10, false), "return");
        assert_eq!(lines[1][1], (11, 14, false), "the string");
        assert_eq!(lines[1].last().unwrap().1, 18);
        assert!(lines[2].is_empty());
    }

    #[test]
    fn blocks_stop_at_max_columns() {
        let long = "x".repeat(10_000);
        let lines = blocks(&format!("{long}\nab cd\n"), "a.txt", 120);
        assert_eq!(lines[0], [(0, 120, true)]);
        assert_eq!(lines[1], [(0, 2, true), (3, 5, true)]);
    }

    #[gpui::test]
    fn chunks_are_cached_per_version(cx: &mut TestAppContext) {
        let text: String = (0..3000).map(|i| format!("value_{i} = {i}\n")).collect();
        let window = editor("big.py", &format!("|{text}"), cx);
        update(window, cx, |e, _, _| {
            e.minimap = MinimapState::default();
            e.cache_minimap_blocks(0..300);
            assert_eq!(e.minimap.chunks.len(), 3);
            let key = e.minimap.key;
            e.cache_minimap_blocks(100..200);
            assert_eq!((e.minimap.chunks.len(), e.minimap.key), (3, key));
            assert_eq!(
                e.minimap_line_blocks(130)
                    .iter()
                    .map(|b| (b.start, b.end))
                    .collect::<Vec<_>>(),
                [(0, 9), (10, 11), (12, 15)],
                "value_130 = 130"
            );
            // Far lines evict the least recently used chunks.
            for index in 3..20 {
                e.cache_minimap_blocks(index * CHUNK_LINES..index * CHUNK_LINES + 1);
            }
            assert_eq!(e.minimap.chunks.len(), MAX_CHUNKS);
            assert!(
                e.minimap
                    .chunks
                    .iter()
                    .any(|c| c.first_line == 19 * CHUNK_LINES)
            );
            assert!(!e.minimap.chunks.iter().any(|c| c.first_line == 0));
            assert!(
                e.minimap_line_blocks(0).is_empty(),
                "not cached: nothing drawn"
            );
        });
        let key = window.read_with(cx, |e, _| e.minimap.key).unwrap();
        crate::editor::tests::type_text(window, "x", cx);
        update(window, cx, |e, _, _| {
            e.cache_minimap_blocks(0..1);
            assert_ne!(e.minimap.key, key, "an edit drops the old blocks");
            let first = e.minimap_line_blocks(0)[0];
            assert_eq!((first.start, first.end), (0, 8), "xvalue_0");
        });
    }

    #[gpui::test]
    fn the_column_sits_before_the_scrollbar_and_hides_when_narrow(cx: &mut TestAppContext) {
        let window = editor("a.py", "|x = 1\n", cx);
        update(window, cx, |e, _, _| {
            let bounds = Bounds::new(point(px(100.), px(50.)), size(px(1000.), px(600.)));
            let (rest, column) = e.minimap_split(bounds);
            let column = column.expect("a minimap");
            // 120 columns and padding, then the scrollbar's strip.
            assert_eq!(column.size.width, px(132.));
            assert_eq!(column.right(), px(1100.) - scrollbar::WIDTH);
            assert_eq!((rest.left(), rest.right()), (px(100.), column.left()));
            assert_eq!(column.size.height, px(600.));
            // The scrollbar keeps the far right, after the minimap.
            let text = Bounds::from_corners(point(px(150.), px(50.)), rest.bottom_right());
            assert_eq!(
                scrollbar_area(text, bounds, Some(column)).right(),
                px(1100.)
            );
            assert_eq!(scrollbar_area(text, bounds, None), text);
            // Narrow editors and input fields have none.
            let narrow = Bounds::new(bounds.origin, size(px(499.), px(600.)));
            assert_eq!(e.minimap_split(narrow), (narrow, None));
            e.text_area = true;
            assert_eq!(e.minimap_split(bounds), (bounds, None));
        });
    }

    #[gpui::test]
    fn a_huge_document_stays_bounded_and_ends_at_the_bottom(cx: &mut TestAppContext) {
        let text: String = (0..20_000)
            .map(|i| format!("class C{i}:\n    def f(self):\n        return {i}\n\n\n"))
            .collect();
        let window = editor("huge.py", &format!("|{text}"), cx);
        update(window, cx, |e, _, _| {
            let line_height = px(20.);
            let column = Bounds::new(point(px(900.), px(0.)), size(px(132.), px(700.)));
            let rows = e.display.row_count();
            assert!(rows > 100_000);
            let max_scroll = line_height * (rows - 1) as f32;
            for step in 0..=40 {
                e.scroll.y = max_scroll * (step as f32 / 40.);
                let layout = e.layout_minimap(column, line_height, px(700.));
                // Only the rows that fit are read, and few chunks kept.
                assert!(layout.rows.len() <= 351, "{}", layout.rows.len());
                assert!(e.minimap.chunks.len() <= MAX_CHUNKS);
                // The slider is over the first visible row's minimap row.
                let first_visible = (e.scroll.y / line_height).floor() as usize;
                let under_slider = layout
                    .geometry
                    .row_at(layout.slider.top() - column.top() + px(0.5), e.scroll.y);
                assert_eq!(under_slider, first_visible, "step {step}");
                assert!(layout.slider.bottom() <= column.bottom() + px(0.01));
            }
            // At the end the last line is drawn at the slider's top and the
            // slider ends at the bottom.
            let layout = e.layout_minimap(column, line_height, px(700.));
            assert!((f32::from(layout.slider.bottom() - column.bottom())).abs() < 0.01);
            let last = layout.rows.iter().position(|r| r.line == rows - 1).unwrap();
            assert!((f32::from(layout.row_top(last) - layout.slider.top())).abs() < 0.01);
        });
    }
}
