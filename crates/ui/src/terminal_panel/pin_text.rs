//! Pinpoint for the terminal grid: the screen's text on the terminal's
//! node (`…/terminal:<n>`), the cells of terminal text agents highlight
//! (by match or by line/column of the visible screen), and the word under
//! the picker.

use std::ops::Range;

use gpui::{Bounds, Pixels, Point, SharedString, Size, Window, point, size};
use terminal::Snapshot;

use crate::{
    pinpoint::{self, PickText, TerminalRef, TerminalWant},
    pinpoint_text,
};

/// The visible screen as text: rows joined with `\n`, trailing blanks
/// trimmed. Built once per shaped screen and kept with it (see
/// `TerminalLayout`): recording it for Pinpoint each frame copies nothing.
pub(super) struct Screen {
    text: SharedString,
    rows: Vec<ScreenRow>,
}

struct ScreenRow {
    /// Byte range of the row in the screen text.
    range: Range<usize>,
    /// Per character: byte offset in the screen text, first cell, cells.
    chars: Vec<(usize, usize, usize)>,
}

impl Screen {
    pub(super) fn new(snapshot: &Snapshot) -> Self {
        let mut text = String::with_capacity(snapshot.rows * (snapshot.columns + 1));
        let mut rows = Vec::with_capacity(snapshot.rows);
        for row in 0..snapshot.rows {
            if row > 0 {
                text.push('\n');
            }
            let start = text.len();
            let mut chars = Vec::new();
            for (column, cell) in snapshot.row(row).iter().enumerate() {
                if cell.spacer {
                    continue;
                }
                chars.push((text.len(), column, if cell.wide { 2 } else { 1 }));
                text.push(if cell.c == '\0' { ' ' } else { cell.c });
            }
            let trimmed = text[start..].trim_end_matches(' ').len();
            text.truncate(start + trimmed);
            chars.retain(|&(at, ..)| at < start + trimmed);
            rows.push(ScreenRow {
                range: start..text.len(),
                chars,
            });
        }
        Self {
            text: text.into(),
            rows,
        }
    }

    /// Cell rects of `range` (bytes of the screen text), one per row,
    /// for a grid at `origin` with cells of `cell` size.
    fn rects(
        &self,
        range: Range<usize>,
        origin: Point<Pixels>,
        cell: Size<Pixels>,
    ) -> Vec<Bounds<Pixels>> {
        let mut rects = Vec::new();
        for (row, screen_row) in self.rows.iter().enumerate() {
            let covered = screen_row
                .chars
                .iter()
                .filter(|(at, ..)| range.start <= *at && *at < range.end);
            let (mut first, mut last) = (None, None);
            for &(_, column, width) in covered {
                first.get_or_insert(column);
                last = Some(column + width);
            }
            if let (Some(first), Some(last)) = (first, last) {
                rects.push(Bounds::new(
                    point(
                        origin.x + cell.width * first as f32,
                        origin.y + cell.height * row as f32,
                    ),
                    size(cell.width * (last - first) as f32, cell.height),
                ));
            }
        }
        rects
    }

    /// Bytes of 1-based `line`/char `column` through `end_line`/
    /// `end_column` (inclusive); no column: whole rows. `None` when `line`
    /// is not on screen.
    fn line_range(
        &self,
        line: u32,
        column: Option<u32>,
        end_line: Option<u32>,
        end_column: Option<u32>,
    ) -> Option<Range<usize>> {
        let row = (line as usize).checked_sub(1)?;
        let first = self.rows.get(row)?;
        let end_row = end_line
            .map_or(row, |l| (l as usize).saturating_sub(1))
            .clamp(row, self.rows.len() - 1);
        let last = &self.rows[end_row];
        // The byte of 0-based char `index` of `row`, else its end.
        let at = |row: &ScreenRow, index: usize| {
            row.chars.get(index).map_or(row.range.end, |&(at, ..)| at)
        };
        let start = column.map_or(first.range.start, |c| {
            at(first, (c as usize).saturating_sub(1))
        });
        let end = end_column.map_or(last.range.end, |c| at(last, c as usize));
        Some(start..end.max(start))
    }

    /// Byte of the character drawn at cell (`row`, `column`).
    fn index_at(&self, row: usize, column: usize) -> Option<usize> {
        self.rows
            .get(row)?
            .chars
            .iter()
            .find(|&&(_, first, width)| first <= column && column < first + width)
            .map(|&(at, ..)| at)
    }

    /// Columns `start..end` of the lowest occurrence of `text`.
    fn columns_of(&self, text: &str) -> Option<(usize, usize)> {
        let at = self.text.rfind(text)?;
        let end = at + text.len();
        let row = self.rows.iter().find(|row| row.range.contains(&at))?;
        let (_, start, _) = *row.chars.iter().find(|(byte, ..)| *byte >= at)?;
        let (_, last, width) = *row.chars.iter().rev().find(|(byte, ..)| *byte < end)?;
        Some((start, last + width))
    }
}

/// The 1-based terminal number in `path` (`…/terminal:<n>`).
fn terminal_number(path: &str) -> Option<usize> {
    path.rsplit('/')
        .next()?
        .strip_prefix("terminal:")?
        .parse()
        .ok()
}

fn wants(want: &TerminalWant, number: Option<usize>, path: &str) -> bool {
    match want.terminal {
        TerminalRef::Index(n) => number == Some(n),
        // The panel shows its active terminal.
        TerminalRef::Active => number.is_some() && path.contains("panel:terminal/"),
    }
}

/// In prepaint: the screen's text on the terminal's node `path`, reports
/// for wanted terminal text, and the picker's word.
pub(super) fn record(
    screen: &Screen,
    path: &str,
    origin: Point<Pixels>,
    cell: Size<Pixels>,
    window: &Window,
) {
    if path.is_empty() {
        return;
    }
    pinpoint::set_text(window, path, screen.text.clone());
    let mask = window.content_mask().bounds;
    let report = |id, range: Option<Range<usize>>| {
        let rects = range.map(|range| screen.rects(range, origin, cell));
        let off_screen = rects.is_none();
        let (rects, clipped) = pinpoint_text::clip_rects(rects.unwrap_or_default(), mask);
        pinpoint::report(window, id, rects, clipped || off_screen);
    };
    for (id, range) in pinpoint::wanted_text(window, path) {
        report(id, Some(range));
    }
    let number = terminal_number(path);
    for (id, want) in pinpoint::wanted_terminal(window) {
        if !wants(&want, number, path) {
            continue;
        }
        match (&want.text, want.line) {
            (Some(text), _) => {
                if let Some(range) = pinpoint_text::nth_match(&screen.text, text, want.nth) {
                    report(id, Some(range));
                }
            }
            (None, Some(line)) => report(
                id,
                screen.line_range(line, want.column, want.end_line, want.end_column),
            ),
            (None, None) => {}
        }
    }
    if let Some(pointer) = pinpoint::picking(window)
        && mask.contains(&pointer)
        && pointer.x >= origin.x
        && pointer.y >= origin.y
    {
        let row = ((pointer.y - origin.y) / cell.height) as usize;
        let column = ((pointer.x - origin.x) / cell.width) as usize;
        if let Some(index) = screen.index_at(row, column) {
            let screen_row = &screen.rows[row];
            let word = pinpoint_text::word_at(
                &screen.text[screen_row.range.clone()],
                index - screen_row.range.start,
            );
            let range = screen_row.range.start + word.start..screen_row.range.start + word.end;
            if !screen.text[range.clone()].trim().is_empty() {
                pinpoint::report_pick(
                    window,
                    PickText {
                        reference: pinpoint_text::range_reference(
                            path,
                            &screen.text,
                            range.clone(),
                        ),
                        path: path.to_owned().into(),
                        rects: screen.rects(range, origin, cell),
                    },
                );
            }
        }
    }
}

impl super::TerminalPanel {
    /// Shows terminal tab `index` (1-based; `None`: the active one),
    /// scrolled to the lowest line holding `text` when given.
    pub(crate) fn pinpoint_reveal(
        &mut self,
        index: Option<usize>,
        text: Option<&str>,
        cx: &mut gpui::Context<Self>,
    ) -> Result<(), String> {
        if let Some(index) = index {
            if index == 0 || index > self.tabs.len() {
                return Err(format!("there is no terminal {index}"));
            }
            self.active = index - 1;
        }
        let (view, _) = self.tabs.get(self.active).ok_or("no terminal is open")?;
        cx.notify();
        if let Some(text) = text {
            if !view.read(cx).terminal.scroll_to_text(text) {
                return Err(format!("no terminal line holds {text:?}"));
            }
            // Sideways too, when its line is wider than the view.
            view.update(cx, |view, cx| {
                view.reveal_columns = Screen::new(&view.terminal.snapshot()).columns_of(text);
                view.screen_changed(cx);
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(rows: &[&str]) -> Screen {
        let columns = 12;
        let mut cells = Vec::new();
        for row in rows {
            let mut count = 0;
            for c in row.chars() {
                let wide = unicode_width::UnicodeWidthChar::width(c) == Some(2);
                cells.push(cell(c, wide, false));
                count += 1;
                if wide {
                    cells.push(cell(' ', false, true));
                    count += 1;
                }
            }
            for _ in count..columns {
                cells.push(cell('\0', false, false));
            }
        }
        Screen::new(&Snapshot {
            columns,
            rows: rows.len(),
            cells,
            cursor: None,
            display_offset: 0,
            history: 0,
            app_cursor: false,
            bracketed_paste: false,
            alt_screen: false,
            wrapped: vec![false; rows.len()],
            hyperlinks: Vec::new(),
        })
    }

    fn cell(c: char, wide: bool, spacer: bool) -> terminal::SnapshotCell {
        let color = terminal::Rgb { r: 0, g: 0, b: 0 };
        terminal::SnapshotCell {
            c,
            fg: color,
            bg: color,
            bold: false,
            italic: false,
            underline: false,
            strikeout: false,
            wide,
            spacer,
        }
    }

    const CELL: Size<Pixels> = Size {
        width: gpui::px(10.),
        height: gpui::px(20.),
    };

    fn rects(screen: &Screen, range: Range<usize>) -> Vec<(f32, f32, f32)> {
        screen
            .rects(range, point(gpui::px(0.), gpui::px(0.)), CELL)
            .iter()
            .map(|r| {
                (
                    f32::from(r.left()),
                    f32::from(r.top()),
                    f32::from(r.size.width),
                )
            })
            .collect()
    }

    #[test]
    fn screen_text_maps_to_cells_wide_chars_included() {
        let screen = screen(&["$ ls", "日本 ok", "", "done  "]);
        assert_eq!(screen.text.as_str(), "$ ls\n日本 ok\n\ndone");
        let ok = pinpoint_text::nth_match(&screen.text, "ok", 1).unwrap();
        // `日本` takes 4 cells: `ok` starts at cell 5.
        assert_eq!(rects(&screen, ok), [(50., 20., 20.)]);
        let wide = pinpoint_text::nth_match(&screen.text, "本", 1).unwrap();
        assert_eq!(rects(&screen, wide), [(20., 20., 20.)]);
        // Across rows: one rect per row with text.
        let across = 2..screen.text.len();
        assert_eq!(
            rects(&screen, across),
            [(20., 0., 20.), (0., 20., 70.), (0., 60., 40.)]
        );
        assert_eq!(screen.index_at(1, 3), Some(8), "second cell of 本");
        assert_eq!(screen.index_at(1, 11), None, "blank");
    }

    #[test]
    fn lines_and_columns_count_chars_of_the_visible_screen() {
        let screen = screen(&["$ ls", "日本 ok", "done"]);
        let text = |line, column, end_line, end_column| {
            screen
                .line_range(line, column, end_line, end_column)
                .map(|r| &screen.text[r])
        };
        assert_eq!(text(2, None, None, None), Some("日本 ok"));
        assert_eq!(text(2, Some(2), None, Some(2)), Some("本"));
        assert_eq!(text(2, Some(4), None, None), Some("ok"));
        assert_eq!(text(1, Some(3), Some(3), Some(2)), Some("ls\n日本 ok\ndo"));
        assert_eq!(text(4, None, None, None), None, "below the screen");
        assert_eq!(
            terminal_number("workspace/bottom-dock/panel:terminal/terminal:2"),
            Some(2)
        );
    }
}
