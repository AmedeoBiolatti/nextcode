//! Buffer lines ↔ screen rows: soft wrapping and folded line ranges.
//!
//! A buffer line shows as one row, several rows when word wrap splits it,
//! or none when a fold hides it. Wrapping is measured in columns (chars,
//! two for wide East Asian ones; the editor font is monospaced), so it
//! needs no text shaping: rows per line are counted once per wrap width
//! and then only for the lines an edit touches. Row starts are a prefix
//! sum over lines, rebuilt after edits and fold changes (integer adds, no
//! text access). Without wrapping or folds, rows are lines and nothing is
//! stored.

use std::ops::Range;

use text::{Bias, Buffer, Change};
use unicode_width::UnicodeWidthChar;

/// A folded region. The header line stays visible; the lines after it,
/// through `end`'s line, are hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Fold {
    /// Start of the header line.
    pub header: usize,
    /// End of the header line: hidden text starts after it.
    pub start: usize,
    /// End of the last hidden line.
    pub end: usize,
}

impl Fold {
    /// The fold hiding lines `header + 1 ..= last` (both must exist).
    pub fn lines(buffer: &Buffer, header: usize, last: usize) -> Self {
        Self {
            header: buffer.line_start(header),
            start: buffer.line_end(header),
            end: buffer.line_end(last),
        }
    }

    /// Whether `offset` is in the hidden text.
    pub fn hides(&self, offset: usize) -> bool {
        self.start < offset && offset <= self.end
    }

    pub fn header_line(&self, buffer: &Buffer) -> usize {
        buffer.line_of(self.start)
    }

    pub fn last_line(&self, buffer: &Buffer) -> usize {
        buffer.line_of(self.end)
    }

    /// Whether the offsets still delimit whole lines after edits.
    fn is_valid(&self, buffer: &Buffer) -> bool {
        let line = buffer.line_of(self.start);
        let last = buffer.line_of(self.end);
        self.header <= self.start
            && self.end <= buffer.len()
            && buffer.line_start(line) == self.header
            && buffer.line_end(line) == self.start
            && buffer.line_end(last) == self.end
            && last > line
    }

    /// Where the fold goes through `change`; `None` when the change edits
    /// the hidden text, replaces the header line, or splits it at its end.
    fn map(self, change: &Change) -> Option<Self> {
        let inserted_lines = change.new_end_point.row > change.start_point.row;
        let replaces_header = change.start <= self.header
            && change.old_end >= self.start
            && change.old_end > change.start;
        if (change.start <= self.end && change.old_end > self.start)
            || replaces_header
            || (change.start == self.start && inserted_lines)
        {
            return None;
        }
        Some(Self {
            header: change.map_offset(self.header, Bias::Left),
            start: change.map_offset(self.start, Bias::Right),
            end: change.map_offset(self.end, Bias::Left),
        })
    }
}

/// One screen row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DisplayRow {
    pub line: usize,
    /// Buffer offsets shown on the row (the next row starts at `end`).
    pub range: Range<usize>,
    /// Columns before the text: the line's indentation on continuation
    /// rows, 0 on its first row (whose indentation is part of the text).
    pub indent: usize,
    pub first: bool,
    pub last: bool,
}

impl DisplayRow {
    /// Whether a cursor at `offset` is drawn on this row: a wrap position
    /// belongs to the row it starts, or with `at_row_end` to the row it
    /// ends.
    pub fn shows(&self, offset: usize, at_row_end: bool) -> bool {
        if at_row_end && self.wraps_at(offset) {
            return true;
        }
        self.range.start <= offset
            && (offset < self.range.end || self.last && offset == self.range.end)
            && !(at_row_end && !self.first && offset == self.range.start)
    }

    /// Whether `offset` is the end of this row and the start of the next
    /// one (of the same line).
    pub fn wraps_at(&self, offset: usize) -> bool {
        !self.last && offset == self.range.end
    }
}

/// Text a language server shows inside a line (an inlay hint): drawn
/// before the char at `offset`, never part of the buffer. Rows count its
/// columns when wrapping, and screen columns include it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Inlay {
    pub offset: usize,
    pub label: String,
    /// A space before/after the label (outside its background).
    pub padding_left: bool,
    pub padding_right: bool,
}

impl Inlay {
    /// Screen columns taken, padding included.
    pub fn columns(&self) -> usize {
        self.label.chars().map(char_width).sum::<usize>()
            + usize::from(self.padding_left)
            + usize::from(self.padding_right)
    }
}

/// Columns of the inlays in `inlays` (sorted) at exactly `offset`, from
/// index `*next` on; advances `*next` past them.
fn inlay_columns_at(inlays: &[Inlay], next: &mut usize, offset: usize) -> usize {
    let mut columns = 0;
    while let Some(inlay) = inlays.get(*next) {
        if inlay.offset > offset {
            break;
        }
        if inlay.offset == offset {
            columns += inlay.columns();
        }
        *next += 1;
    }
    columns
}

/// Columns a char takes on screen. Tabs render as one space for now.
pub(super) fn char_width(c: char) -> usize {
    if c == '\t' { 1 } else { c.width().unwrap_or(1) }
}

/// Wraps one line of `chars` at `width` columns: calls `on_break` with
/// the byte offset (relative to the line) of each continuation row's
/// start and returns the continuation indent in columns. Rows break
/// after whitespace and around wide chars; a word longer than a row
/// breaks anywhere. Whitespace never starts a row (it hangs past the
/// edge). Continuation rows are indented like the line (VS Code's
/// `wrappingIndent: same`) unless that leaves less than half the width.
/// `inlays` (sorted; buffer offsets, the line starting at `start`) take
/// columns before the char at their offset and move to the next row
/// with it.
pub(super) fn wrap_line(
    chars: impl Iterator<Item = char>,
    width: usize,
    (inlays, start): (&[Inlay], usize),
    mut on_break: impl FnMut(usize),
) -> usize {
    let width = width.max(1);
    let mut indent = 0;
    let mut leading = true;
    let continuation = |indent: usize| if indent * 2 <= width { indent } else { 0 };
    let mut column = 0;
    let mut row_start = 0;
    // Latest place this row may break, with the column there.
    let mut opportunity: Option<(usize, usize)> = None;
    let mut previous_space = false;
    let mut previous_wide = false;
    let mut offset = 0;
    let mut next_inlay = 0;
    for c in chars {
        let w = char_width(c);
        // An inlay before the char travels with it.
        let span = inlay_columns_at(inlays, &mut next_inlay, start + offset) + w;
        let space = c.is_whitespace();
        if leading {
            if space {
                indent += w;
            } else {
                leading = false;
            }
        }
        if !space {
            if offset > row_start && (previous_space || previous_wide || w > 1) {
                opportunity = Some((offset, column));
            }
            if column + span > width && offset > row_start {
                let (at, at_column) = opportunity
                    .filter(|&(at, _)| at > row_start)
                    .unwrap_or((offset, column));
                on_break(at);
                row_start = at;
                column = continuation(indent) + column - at_column;
                opportunity = None;
                if column + span > width && offset > row_start {
                    on_break(offset);
                    row_start = offset;
                    column = continuation(indent);
                }
            }
        }
        column += span;
        previous_space = space;
        previous_wide = w > 1;
        offset += c.len_utf8();
    }
    continuation(indent)
}

pub(super) struct DisplayMap {
    /// Columns rows wrap at; `None` without word wrap.
    wrap: Option<usize>,
    /// Rows of each line when wrapping, ignoring folds (empty otherwise).
    line_rows: Vec<u32>,
    /// Sorted by start, then outer (longer) first; nested folds allowed.
    folds: Vec<Fold>,
    /// Merged, sorted hidden line ranges.
    hidden: Vec<Range<usize>>,
    /// First row of each line (a hidden line has the next shown line's),
    /// then the row count; empty while every line is one row.
    row_starts: Vec<usize>,
    line_count: usize,
    /// Inlay hints, sorted by offset; moved through edits.
    inlays: Vec<Inlay>,
}

impl DisplayMap {
    pub fn new(buffer: &Buffer) -> Self {
        Self {
            wrap: None,
            line_rows: Vec::new(),
            folds: Vec::new(),
            hidden: Vec::new(),
            row_starts: Vec::new(),
            line_count: buffer.line_count(),
            inlays: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn inlays(&self) -> &[Inlay] {
        &self.inlays
    }

    /// The inlays from `range.start` through `range.end` (inclusive).
    pub fn inlays_in(&self, range: Range<usize>) -> &[Inlay] {
        Self::inlays_between(&self.inlays, range)
    }

    fn inlays_between(inlays: &[Inlay], range: Range<usize>) -> &[Inlay] {
        let first = inlays.partition_point(|inlay| inlay.offset < range.start);
        let rest = &inlays[first..];
        &rest[..rest.partition_point(|inlay| inlay.offset <= range.end)]
    }

    fn line_inlays<'a>(inlays: &'a [Inlay], buffer: &Buffer, line: usize) -> &'a [Inlay] {
        Self::inlays_between(inlays, buffer.line_start(line)..buffer.line_end(line))
    }

    /// Replaces the inlays (any order; offsets past the end are dropped)
    /// and rewraps the lines they were or are on. Returns whether
    /// anything changed.
    pub fn set_inlays(&mut self, buffer: &Buffer, mut inlays: Vec<Inlay>) -> bool {
        inlays.retain(|inlay| inlay.offset <= buffer.len());
        inlays.sort_by_key(|inlay| inlay.offset);
        if inlays == self.inlays {
            return false;
        }
        let old = std::mem::replace(&mut self.inlays, inlays);
        if let Some(width) = self.wrap {
            let mut lines: Vec<usize> = old
                .iter()
                .chain(&self.inlays)
                .map(|inlay| buffer.line_of(inlay.offset.min(buffer.len())))
                .collect();
            lines.sort_unstable();
            lines.dedup();
            for line in lines {
                if let Some(rows) = self.line_rows.get_mut(line) {
                    *rows = Self::count_rows(buffer, line, width, &self.inlays);
                }
            }
            self.rebuild(buffer);
        }
        true
    }

    /// Wraps at `columns` (`None`: no wrapping). Counts every line's rows
    /// when the width changes.
    pub fn set_wrap(&mut self, buffer: &Buffer, columns: Option<usize>) {
        if self.wrap == columns {
            return;
        }
        self.wrap = columns;
        self.line_rows.clear();
        if let Some(width) = columns {
            self.line_rows = (0..buffer.line_count())
                .map(|line| Self::count_rows(buffer, line, width, &self.inlays))
                .collect();
        }
        self.rebuild(buffer);
    }

    fn count_rows(buffer: &Buffer, line: usize, width: usize, inlays: &[Inlay]) -> u32 {
        let mut rows = 1;
        let inlays = Self::line_inlays(inlays, buffer, line);
        let start = buffer.line_start(line);
        wrap_line(
            Self::line_chars(buffer, line),
            width,
            (inlays, start),
            |_| rows += 1,
        );
        rows
    }

    fn line_chars(buffer: &Buffer, line: usize) -> impl Iterator<Item = char> + '_ {
        buffer
            .chunks(buffer.line_start(line)..buffer.line_end(line))
            .flat_map(str::chars)
    }

    /// Follows `changes` (in order, as drained from the buffer): folds move
    /// with the text or open when edited inside, and edited lines are
    /// wrapped again.
    pub fn edit(&mut self, buffer: &Buffer, changes: &[Change]) {
        if changes.is_empty() {
            return;
        }
        for change in changes {
            self.folds = self
                .folds
                .iter()
                .filter_map(|fold| fold.map(change))
                .collect();
            if self.wrap.is_some() {
                let start = change.start_point.row;
                let old_end = (change.old_end_point.row + 1).min(self.line_rows.len());
                let new_lines = change.new_end_point.row - start + 1;
                // 0 marks a line to count below, once the text is final.
                self.line_rows.splice(
                    start.min(old_end)..old_end,
                    std::iter::repeat_n(0, new_lines),
                );
            }
            // Inlays move with the text; ones inside replaced text go.
            self.inlays.retain_mut(|inlay| {
                let inside = change.start < inlay.offset && inlay.offset < change.old_end;
                inlay.offset = change.map_offset(inlay.offset, Bias::Right);
                !inside
            });
        }
        self.folds.retain(|fold| fold.is_valid(buffer));
        self.sort_folds();
        if let Some(width) = self.wrap {
            self.line_rows.resize(buffer.line_count(), 0);
            let inlays = &self.inlays;
            for (line, rows) in self.line_rows.iter_mut().enumerate() {
                if *rows == 0 {
                    *rows = Self::count_rows(buffer, line, width, inlays);
                }
            }
        }
        self.rebuild(buffer);
    }

    fn sort_folds(&mut self) {
        self.folds
            .sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
        self.folds.dedup();
    }

    fn rebuild(&mut self, buffer: &Buffer) {
        self.line_count = buffer.line_count();
        self.hidden.clear();
        for fold in &self.folds {
            let lines = fold.header_line(buffer) + 1..fold.last_line(buffer) + 1;
            match self.hidden.last_mut() {
                Some(last) if lines.start <= last.end => last.end = last.end.max(lines.end),
                _ => self.hidden.push(lines),
            }
        }
        self.row_starts.clear();
        if self.wrap.is_none() && self.hidden.is_empty() {
            return;
        }
        self.row_starts.reserve(self.line_count + 1);
        let mut row = 0;
        let mut hidden = self.hidden.iter().peekable();
        for line in 0..self.line_count {
            self.row_starts.push(row);
            while hidden.peek().is_some_and(|range| range.end <= line) {
                hidden.next();
            }
            if !hidden.peek().is_some_and(|range| range.contains(&line)) {
                row += self.line_rows.get(line).map_or(1, |&rows| rows as usize);
            }
        }
        self.row_starts.push(row);
    }

    pub fn folds(&self) -> &[Fold] {
        &self.folds
    }

    /// Adds folds (ignoring ones already there).
    pub fn fold(&mut self, buffer: &Buffer, folds: impl IntoIterator<Item = Fold>) {
        self.folds
            .extend(folds.into_iter().filter(|fold| fold.is_valid(buffer)));
        self.sort_folds();
        self.rebuild(buffer);
    }

    /// Removes the folds `remove` picks; returns whether there were any.
    pub fn unfold(&mut self, buffer: &Buffer, mut remove: impl FnMut(&Fold) -> bool) -> bool {
        let before = self.folds.len();
        self.folds.retain(|fold| !remove(fold));
        let removed = self.folds.len() != before;
        if removed {
            self.rebuild(buffer);
        }
        removed
    }

    /// The outermost fold hiding `offset`.
    pub fn fold_hiding(&self, offset: usize) -> Option<&Fold> {
        self.folds.iter().find(|fold| fold.hides(offset))
    }

    /// Whether a fold with its header on `line` is folded.
    pub fn is_folded_header(&self, buffer: &Buffer, line: usize) -> bool {
        let end = buffer.line_end(line);
        self.folds.iter().any(|fold| fold.start == end)
    }

    pub fn is_hidden(&self, line: usize) -> bool {
        let index = self.hidden.partition_point(|range| range.end <= line);
        self.hidden
            .get(index)
            .is_some_and(|range| range.contains(&line))
    }

    /// The nearest shown offset: hidden text moves to its fold's header end
    /// (`Left`) or the line after the fold (`Right`, if there is one).
    pub fn clip(&self, buffer: &Buffer, offset: usize, bias: Bias) -> usize {
        let Some(fold) = self.fold_hiding(offset) else {
            return offset;
        };
        let after = fold.last_line(buffer) + 1;
        match bias {
            Bias::Right if after < buffer.line_count() => {
                // Folds nested in a hidden region are covered by this one.
                buffer.line_start(after)
            }
            _ => fold.start,
        }
    }

    pub fn row_count(&self) -> usize {
        self.row_starts.last().copied().unwrap_or(self.line_count)
    }

    /// The first row of `line` (of the next shown line if it is hidden).
    pub fn line_row(&self, line: usize) -> usize {
        match self.row_starts.get(line) {
            Some(&row) => row,
            None if self.row_starts.is_empty() => line,
            None => self.row_count(),
        }
    }

    /// The shown line `row` belongs to (rows past the end: the last one).
    pub fn row_line(&self, row: usize) -> usize {
        if self.row_starts.is_empty() {
            return row.min(self.line_count - 1);
        }
        let row = row.min(self.row_count().saturating_sub(1));
        let starts = &self.row_starts[..self.line_count];
        let line = starts
            .partition_point(|&start| start <= row)
            .saturating_sub(1);
        // Lines hidden at the end share the row count as their start.
        let mut line = line;
        while line > 0 && self.is_hidden(line) {
            line -= 1;
        }
        line
    }

    /// The next (`forward`) or previous shown line from `line`, if any.
    pub fn shown_line(&self, line: usize, forward: bool) -> Option<usize> {
        if forward {
            let mut next = line + 1;
            while next < self.line_count && self.is_hidden(next) {
                next += 1;
            }
            (next < self.line_count).then_some(next)
        } else {
            let mut previous = line.checked_sub(1)?;
            while self.is_hidden(previous) {
                previous = previous.checked_sub(1)?;
            }
            Some(previous)
        }
    }

    /// Start offsets of `line`'s rows (the first is the line start) and the
    /// continuation indent.
    pub fn line_breaks(&self, buffer: &Buffer, line: usize) -> (Vec<usize>, usize) {
        let start = buffer.line_start(line);
        let mut starts = vec![start];
        let indent = match self.wrap {
            Some(width) => wrap_line(
                Self::line_chars(buffer, line),
                width,
                (Self::line_inlays(&self.inlays, buffer, line), start),
                |at| starts.push(start + at),
            ),
            None => 0,
        };
        (starts, indent)
    }

    /// The rows of `line`.
    pub fn line_display_rows(&self, buffer: &Buffer, line: usize) -> Vec<DisplayRow> {
        let (starts, indent) = self.line_breaks(buffer, line);
        let end = buffer.line_end(line);
        let count = starts.len();
        (0..count)
            .map(|i| DisplayRow {
                line,
                range: starts[i]..starts.get(i + 1).copied().unwrap_or(end),
                indent: if i == 0 { 0 } else { indent },
                first: i == 0,
                last: i + 1 == count,
            })
            .collect()
    }

    /// Up to `count` rows from `first`.
    pub fn rows(&self, buffer: &Buffer, first: usize, count: usize) -> Vec<DisplayRow> {
        let mut rows = Vec::with_capacity(count);
        if first >= self.row_count() {
            return rows;
        }
        let mut line = self.row_line(first);
        let mut skip = first - self.line_row(line);
        loop {
            let line_rows = self.line_display_rows(buffer, line);
            rows.extend(line_rows.into_iter().skip(skip).take(count - rows.len()));
            skip = 0;
            match self.shown_line(line, true) {
                Some(next) if rows.len() < count => line = next,
                _ => break,
            }
        }
        rows
    }

    /// The row showing a cursor at `offset` (with `at_row_end`, a wrap
    /// position is on the upper row) and its index (hidden text: the end
    /// of its fold's header).
    pub fn row_of(&self, buffer: &Buffer, offset: usize, at_row_end: bool) -> (usize, DisplayRow) {
        let offset = self.clip(buffer, offset, Bias::Left);
        let line = buffer.line_of(offset);
        let rows = self.line_display_rows(buffer, line);
        let index = rows
            .iter()
            .position(|row| row.shows(offset, at_row_end))
            .unwrap_or(rows.len() - 1);
        let row = self.line_row(line) + index;
        (row, rows.into_iter().nth(index).expect("a line has a row"))
    }

    /// Row and screen column of a cursor at `offset` (inlays before it on
    /// the row count; one at `offset` is drawn after the cursor).
    pub fn point(&self, buffer: &Buffer, offset: usize, at_row_end: bool) -> (usize, usize) {
        let offset = self.clip(buffer, offset, Bias::Left);
        let (row, display_row) = self.row_of(buffer, offset, at_row_end);
        let inlays: usize = self
            .inlays_in(display_row.range.start..offset)
            .iter()
            .filter(|inlay| inlay.offset < offset)
            .map(Inlay::columns)
            .sum();
        let column = display_row.indent
            + inlays
            + buffer
                .chunks(display_row.range.start..offset)
                .flat_map(str::chars)
                .map(char_width)
                .sum::<usize>();
        (row, column)
    }

    /// The cursor nearest screen `column` of `row`: its offset and whether
    /// it sits at the end of a row that continues below (`at_row_end`).
    pub fn offset_at(&self, buffer: &Buffer, row: usize, column: usize) -> (usize, bool) {
        let line = self.row_line(row);
        let rows = self.line_display_rows(buffer, line);
        let index = (row.saturating_sub(self.line_row(line))).min(rows.len() - 1);
        self.offset_in_row(buffer, &rows[index], column)
    }

    pub fn offset_in_row(&self, buffer: &Buffer, row: &DisplayRow, column: usize) -> (usize, bool) {
        let mut at = row.range.start;
        let mut current = row.indent;
        let inlays = self.inlays_in(row.range.clone());
        let mut next_inlay = 0;
        for c in buffer.chunks(row.range.clone()).flat_map(str::chars) {
            if current >= column {
                break;
            }
            // The inlay before the char is skipped together with it.
            let w = inlay_columns_at(inlays, &mut next_inlay, at) + char_width(c);
            // A wide char across the column: stop before it when the
            // column is nearer its left edge.
            if current + w > column && column - current < current + w - column {
                break;
            }
            current += w;
            at += c.len_utf8();
        }
        (at, row.wraps_at(at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `text` wrapped at `width`, as rows (continuation indent as spaces).
    fn wrapped(text: &str, width: usize) -> Vec<String> {
        let mut breaks = Vec::new();
        let indent = wrap_line(text.chars(), width, (&[], 0), |at| breaks.push(at));
        let mut rows = Vec::new();
        let mut start = 0;
        for (i, end) in breaks.iter().copied().chain([text.len()]).enumerate() {
            let pad = if i == 0 { 0 } else { indent };
            rows.push(format!("{}{}", " ".repeat(pad), &text[start..end]));
            start = end;
        }
        rows
    }

    #[test]
    fn wraps_at_word_boundaries() {
        assert_eq!(
            wrapped("hello world foo bar", 10),
            ["hello ", "world foo ", "bar"]
        );
        assert_eq!(
            wrapped("hello world foo bar", 11),
            ["hello world ", "foo bar"]
        );
        assert_eq!(wrapped("short", 11), ["short"]);
        // Spaces past the edge hang instead of starting a row.
        assert_eq!(wrapped("abcde     fgh", 5), ["abcde     ", "fgh"]);
    }

    #[test]
    fn breaks_words_longer_than_a_row() {
        assert_eq!(
            wrapped("abcdefghijklmnop", 5),
            ["abcde", "fghij", "klmno", "p"]
        );
        assert_eq!(wrapped("ab abcdefghij", 5), ["ab ", "abcde", "fghij"]);
    }

    #[test]
    fn continuation_rows_keep_the_indentation() {
        assert_eq!(
            wrapped("    call(alpha, beta, gamma)", 16),
            ["    call(alpha, ", "    beta, gamma)"]
        );
        // Too deep to keep: continuation starts at the edge.
        assert_eq!(
            wrapped("          aaaa bbbb cccc", 16),
            ["          aaaa ", "bbbb cccc"]
        );
    }

    #[test]
    fn wide_chars_take_two_columns_and_break_anywhere() {
        // Five CJK chars are ten columns.
        assert_eq!(wrapped("漢字漢字漢", 6), ["漢字漢", "字漢"]);
        assert_eq!(wrapped("ab漢字", 4), ["ab漢", "字"]);
    }

    fn buffer(text: &str) -> Buffer {
        Buffer::new(text)
    }

    #[test]
    fn rows_and_offsets_round_trip_with_wrap_and_folds() {
        let text = "fn a() {\n    one two three four five six\n}\nfn b() {\n    x\n}\nend";
        let buffer = buffer(text);
        let mut map = DisplayMap::new(&buffer);
        assert_eq!(map.row_count(), 7);
        map.set_wrap(&buffer, Some(14));
        // Line 1 wraps into "    one two ", "    three four ", "    five six".
        let rows = map.line_display_rows(&buffer, 1);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().skip(1).all(|row| row.indent == 4));
        assert_eq!(map.row_count(), 9);
        assert_eq!(map.line_row(2), 4);
        assert_eq!(map.row_line(3), 1);
        for offset in 0..=buffer.len() {
            if !text.is_char_boundary(offset) {
                continue;
            }
            // At a wrap position the affinity picks the row; elsewhere it
            // changes nothing.
            let wraps = map.row_of(&buffer, offset, true).0 != map.row_of(&buffer, offset, false).0;
            for at_row_end in [false, true] {
                let (row, column) = map.point(&buffer, offset, at_row_end);
                assert_eq!(
                    map.offset_at(&buffer, row, column),
                    (offset, at_row_end && wraps),
                    "offset {offset} at {row}:{column}"
                );
            }
        }
        let wrap = buffer.line_start(1) + "    one two ".len();
        assert_eq!(map.point(&buffer, wrap, false), (2, 4));
        assert_eq!(map.point(&buffer, wrap, true), (1, 12));
        // Fold `fn b`'s body: line 4 vanishes, `}` stays.
        map.fold(&buffer, [Fold::lines(&buffer, 3, 4)]);
        assert!(map.is_hidden(4) && !map.is_hidden(5));
        assert_eq!(map.row_count(), 8);
        assert_eq!(map.row_line(map.line_row(3) + 1), 5);
        assert_eq!(map.shown_line(3, true), Some(5));
        assert_eq!(map.shown_line(5, false), Some(3));
        let inside = buffer.line_start(4) + 2;
        assert_eq!(map.clip(&buffer, inside, Bias::Left), buffer.line_end(3));
        assert_eq!(map.clip(&buffer, inside, Bias::Right), buffer.line_start(5));
        assert_eq!(map.point(&buffer, inside, false).0, map.line_row(3));
        let rows = map.rows(&buffer, 4, 10);
        let lines: Vec<usize> = rows.iter().map(|row| row.line).collect();
        assert_eq!(lines, [2, 3, 5, 6]);
    }

    #[test]
    fn folds_follow_edits_and_open_when_edited_inside() {
        let mut buffer = buffer("a {\n  b\n}\nc {\n  d\n}\n");
        let mut map = DisplayMap::new(&buffer);
        map.fold(&buffer, [Fold::lines(&buffer, 3, 4)]);
        // Insert a line above: the fold moves down a line.
        buffer.edit(0..0, "new\n");
        let changes: Vec<Change> = buffer.drain_changes().collect();
        map.edit(&buffer, &changes);
        assert_eq!(map.folds().len(), 1);
        assert_eq!(map.folds()[0].header_line(&buffer), 4);
        assert!(map.is_hidden(5));
        // Typing at the end of the header keeps it folded.
        let end = buffer.line_end(4);
        buffer.edit(end..end, " // x");
        let changes: Vec<Change> = buffer.drain_changes().collect();
        map.edit(&buffer, &changes);
        assert_eq!(map.folds().len(), 1);
        // Editing the hidden line opens it.
        let inside = buffer.line_start(5) + 2;
        buffer.edit(inside..inside, "z");
        let changes: Vec<Change> = buffer.drain_changes().collect();
        map.edit(&buffer, &changes);
        assert!(map.folds().is_empty());
        assert!(!map.is_hidden(5));
    }

    #[test]
    fn wrapped_rows_are_recounted_for_edited_lines_only() {
        let mut buffer = buffer("aaaa bbbb\ncc\n");
        let mut map = DisplayMap::new(&buffer);
        map.set_wrap(&buffer, Some(5));
        assert_eq!(map.row_count(), 4);
        buffer.edit(10..12, "cccc dddd eeee");
        let changes: Vec<Change> = buffer.drain_changes().collect();
        map.edit(&buffer, &changes);
        assert_eq!(map.row_count(), 6);
        buffer.edit(0..10, "");
        let changes: Vec<Change> = buffer.drain_changes().collect();
        map.edit(&buffer, &changes);
        assert_eq!(map.row_count(), 4);
        map.set_wrap(&buffer, None);
        assert_eq!(map.row_count(), 2);
    }

    fn inlay(offset: usize, label: &str) -> Inlay {
        Inlay {
            offset,
            label: label.into(),
            padding_left: false,
            padding_right: false,
        }
    }

    #[test]
    fn inlays_take_columns_when_wrapping_and_move_with_their_char() {
        let rows = |text: &str, inlays: &[Inlay], width: usize| {
            let mut breaks = Vec::new();
            wrap_line(text.chars(), width, (inlays, 0), |at| breaks.push(at));
            breaks
        };
        assert!(rows("let x = 1;", &[], 12).is_empty());
        // `let x: i32 = 1;` is 15 columns: the line now wraps.
        assert_eq!(rows("let x = 1;", &[inlay(5, ": i32")], 12), [8]);
        // A hint before a word goes to the next row with the word.
        assert_eq!(rows("aaa bbb", &[inlay(4, "xy")], 7), [4]);
        // Padding counts too.
        let padded = Inlay {
            padding_left: true,
            ..inlay(4, "x")
        };
        assert_eq!(padded.columns(), 2);
        assert!(rows("aaa bbb", &[inlay(4, "x")], 8).is_empty());
        assert_eq!(rows("aaa bbbb", &[padded], 8), [4]);
    }

    #[test]
    fn screen_columns_count_inlays_and_columns_inside_one_snap_to_a_side() {
        let buffer = buffer("let x = 1;\nfoo(a, b)\n");
        let mut map = DisplayMap::new(&buffer);
        // `let x: i32 = 1;` and `foo(a, second: b)`.
        let b = buffer.line_start(1) + "foo(a, ".len();
        assert!(map.set_inlays(&buffer, vec![inlay(b, "second: "), inlay(5, ": i32")]));
        assert!(!map.set_inlays(&buffer, vec![inlay(5, ": i32"), inlay(b, "second: ")]));
        // The cursor at a hint is drawn before it, one char later after it.
        assert_eq!(map.point(&buffer, 5, false), (0, 5));
        assert_eq!(map.point(&buffer, 6, false), (0, 11));
        assert_eq!(map.point(&buffer, b, false), (1, 7));
        assert_eq!(map.point(&buffer, b + 1, false), (1, 16));
        // Vertical motion into a hint lands on its nearer side.
        assert_eq!(map.offset_at(&buffer, 0, 6).0, 5);
        assert_eq!(map.offset_at(&buffer, 0, 10).0, 6);
        assert_eq!(map.offset_at(&buffer, 1, 9).0, b);
        assert_eq!(map.offset_at(&buffer, 1, 13).0, b + 1);
        // Every offset round-trips through its screen column.
        for offset in 0..=buffer.len() {
            let (row, column) = map.point(&buffer, offset, false);
            assert_eq!(map.offset_at(&buffer, row, column).0, offset);
        }
    }

    #[test]
    fn inlays_follow_edits_and_rewrap_their_lines() {
        let mut buffer = buffer("let x = 1;\nlet y = 2;\n");
        let mut map = DisplayMap::new(&buffer);
        map.set_wrap(&buffer, Some(12));
        assert_eq!(map.row_count(), 3);
        map.set_inlays(&buffer, vec![inlay(5, ": i32"), inlay(16, ": i32")]);
        assert_eq!(map.row_count(), 5, "both lines wrap with their hints");
        // Typing at the end of `x` pushes its hint along; a line above
        // moves both.
        buffer.edit(5..5, "x");
        buffer.edit(0..0, "\n");
        let changes: Vec<Change> = buffer.drain_changes().collect();
        map.edit(&buffer, &changes);
        let offsets: Vec<usize> = map.inlays().iter().map(|inlay| inlay.offset).collect();
        assert_eq!(offsets, [7, 18]);
        // Deleting the text around a hint drops it.
        buffer.edit(15..20, "");
        let changes: Vec<Change> = buffer.drain_changes().collect();
        map.edit(&buffer, &changes);
        assert_eq!(map.inlays().len(), 1);
        assert_eq!(buffer.slice(0..buffer.len()), "\nlet xx = 1;\nle 2;\n");
        assert_eq!(map.row_count(), 5, "the edited line lost its wrap");
        map.set_inlays(&buffer, Vec::new());
        assert_eq!(map.row_count(), 4);
    }
}
