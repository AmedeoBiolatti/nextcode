//! UI-agnostic text buffer.
//!
//! All public offsets are UTF-8 byte offsets into the whole buffer. Callers must
//! pass offsets on char boundaries; grapheme helpers and UTF-16 conversions
//! always return valid boundaries. Lines are separated by `\n` (a preceding `\r`
//! belongs to the line break), matching tree-sitter and LSP row numbering.

use std::{
    fs,
    io::{self, BufWriter, Write},
    ops::Range,
    path::{Path, PathBuf},
};

use ropey::Rope;
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};

#[derive(Debug, Clone)]
struct Edit {
    offset: usize,
    old: String,
    new: String,
}

/// Row and column of an offset; the column unit depends on the [`Encoding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub row: usize,
    pub column: usize,
}

/// Unit of [`Point::column`]: UTF-8 bytes (tree-sitter, most LSP servers)
/// or UTF-16 code units (the LSP default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16,
}

/// One text replacement, in the coordinates incremental parsers expect.
/// `*_point` columns are UTF-8 bytes; `*_utf16` are the same positions in
/// UTF-16 units, all measured before the edit except `new_end*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    pub start: usize,
    pub old_end: usize,
    pub new_end: usize,
    pub start_point: Point,
    pub old_end_point: Point,
    pub new_end_point: Point,
    pub start_utf16: Point,
    pub old_end_utf16: Point,
}

/// [`Buffer::edit_batch`] was given overlapping ranges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlappingEdits;

impl std::fmt::Display for OverlappingEdits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("edits overlap")
    }
}

impl std::error::Error for OverlappingEdits {}

/// Which side of an edit a mapped position sticks to when the edit touches it.
/// Map range starts with `Right` and ends with `Left` so text typed at either
/// edge stays outside the range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bias {
    /// Stay before text inserted at the position.
    Left,
    /// Move after text inserted at the position.
    Right,
}

impl Change {
    /// Where a pre-edit `offset` ends up after this change. Offsets inside the
    /// replaced text move to its start (`Left`) or end (`Right`).
    pub fn map_offset(&self, offset: usize, bias: Bias) -> usize {
        if offset < self.start || (offset == self.start && bias == Bias::Left) {
            offset
        } else if offset >= self.old_end && !(offset == self.start && bias == Bias::Left) {
            offset + self.new_end - self.old_end
        } else {
            match bias {
                Bias::Left => self.start,
                Bias::Right => self.new_end,
            }
        }
    }
}

pub struct Buffer {
    rope: Rope,
    path: Option<PathBuf>,
    /// Undo steps; each is one or more edits in the order they were applied.
    undo: Vec<Vec<Edit>>,
    redo: Vec<Vec<Edit>>,
    /// Undo-stack depth matching the file on disk; `None` if that state is unreachable.
    saved_at: Option<usize>,
    line_ending: &'static str,
    /// Changes not yet consumed by `drain_changes`.
    changes: Vec<Change>,
}

impl Buffer {
    pub fn new(text: &str) -> Self {
        let line_ending = match text.find('\n') {
            Some(i) if text[..i].ends_with('\r') => "\r\n",
            _ => "\n",
        };
        Self {
            rope: Rope::from_str(text),
            path: None,
            undo: Vec::new(),
            redo: Vec::new(),
            saved_at: Some(0),
            line_ending,
            changes: Vec::new(),
        }
    }

    /// Line ending used by the file's first line break (`"\n"` if none).
    pub fn line_ending(&self) -> &'static str {
        self.line_ending
    }

    /// Opens a UTF-8 text file. Non-UTF-8 content fails with `InvalidData`.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let text = fs::read_to_string(&path)?;
        if text.contains('\0') {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "binary file"));
        }
        let mut buffer = Self::new(&text);
        buffer.path = Some(path);
        Ok(buffer)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn save(&mut self) -> io::Result<()> {
        let path = self
            .path
            .as_deref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "buffer has no path"))?;
        let mut writer = BufWriter::new(fs::File::create(path)?);
        self.rope.write_to(&mut writer)?;
        writer.flush()?;
        self.saved_at = Some(self.undo.len());
        Ok(())
    }

    pub fn is_dirty(&self) -> bool {
        self.saved_at != Some(self.undo.len())
    }

    pub fn len(&self) -> usize {
        self.rope.len_bytes()
    }

    pub fn is_empty(&self) -> bool {
        self.rope.len_bytes() == 0
    }

    /// Number of lines; a trailing line break starts an extra empty line.
    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    pub fn line_of(&self, offset: usize) -> usize {
        self.rope.byte_to_line(offset)
    }

    pub fn line_start(&self, line: usize) -> usize {
        self.rope.line_to_byte(line)
    }

    /// Offset of the end of `line`, before its line break.
    pub fn line_end(&self, line: usize) -> usize {
        let slice = self.rope.line(line);
        let mut end = self.rope.line_to_byte(line) + slice.len_bytes();
        let mut chars = slice.chars_at(slice.len_chars());
        if chars.prev() == Some('\n') {
            end -= 1;
            if chars.prev() == Some('\r') {
                end -= 1;
            }
        }
        end
    }

    /// Point of `offset` with UTF-8 byte columns.
    pub fn point(&self, offset: usize) -> Point {
        let row = self.rope.byte_to_line(offset);
        Point {
            row,
            column: offset - self.rope.line_to_byte(row),
        }
    }

    pub fn point_in(&self, offset: usize, encoding: Encoding) -> Point {
        match encoding {
            Encoding::Utf8 => self.point(offset),
            Encoding::Utf16 => {
                let row = self.rope.byte_to_line(offset);
                let char = self.rope.byte_to_char(offset);
                let line_char = self.rope.line_to_char(row);
                Point {
                    row,
                    column: self.rope.char_to_utf16_cu(char)
                        - self.rope.char_to_utf16_cu(line_char),
                }
            }
        }
    }

    /// Offset of `point`, clamped to the buffer and to the end of its line
    /// (servers may send positions past either). A column inside a
    /// multi-unit character snaps to that character's start.
    pub fn offset_of(&self, point: Point, encoding: Encoding) -> usize {
        if point.row >= self.line_count() {
            return self.len();
        }
        let line_start = self.line_start(point.row);
        let line_end = self.line_end(point.row);
        match encoding {
            Encoding::Utf8 => {
                let offset = (line_start + point.column).min(line_end);
                self.rope.char_to_byte(self.rope.byte_to_char(offset))
            }
            Encoding::Utf16 => {
                let start_char = self.rope.byte_to_char(line_start);
                let end_char = self.rope.byte_to_char(line_end);
                let start_unit = self.rope.char_to_utf16_cu(start_char);
                let end_unit = self.rope.char_to_utf16_cu(end_char);
                let unit = (start_unit + point.column).min(end_unit);
                self.rope.char_to_byte(self.rope.utf16_cu_to_char(unit))
            }
        }
    }

    /// Text from `offset` to the end of its rope chunk; empty at the end.
    pub fn chunk_at(&self, offset: usize) -> &str {
        if offset >= self.len() {
            return "";
        }
        let (chunk, start, _, _) = self.rope.chunk_at_byte(offset);
        &chunk[offset - start..]
    }

    /// Chunks covering `range`, in order.
    pub fn chunks(&self, range: Range<usize>) -> impl Iterator<Item = &str> {
        self.rope.byte_slice(range).chunks()
    }

    /// Changes since the last call, oldest first.
    pub fn drain_changes(&mut self) -> std::vec::Drain<'_, Change> {
        self.changes.drain(..)
    }

    /// Text of `line` without its line break.
    pub fn line_text(&self, line: usize) -> String {
        self.slice(self.line_start(line)..self.line_end(line))
    }

    pub fn slice(&self, range: Range<usize>) -> String {
        self.rope.byte_slice(range).to_string()
    }

    /// Column of `offset` in chars from the start of its line.
    pub fn column(&self, offset: usize) -> usize {
        let line = self.line_of(offset);
        self.rope.byte_to_char(offset) - self.rope.line_to_char(line)
    }

    /// Offset at char `column` of `line`, clamped to the line end.
    pub fn offset_at(&self, line: usize, column: usize) -> usize {
        let line = line.min(self.line_count() - 1);
        let start_char = self.rope.line_to_char(line);
        let end_char = self.rope.byte_to_char(self.line_end(line));
        self.rope.char_to_byte((start_char + column).min(end_char))
    }

    pub fn offset_to_utf16(&self, offset: usize) -> usize {
        self.rope.char_to_utf16_cu(self.rope.byte_to_char(offset))
    }

    pub fn offset_from_utf16(&self, utf16: usize) -> usize {
        let utf16 = utf16.min(self.rope.len_utf16_cu());
        self.rope.char_to_byte(self.rope.utf16_cu_to_char(utf16))
    }

    /// Next grapheme boundary after `offset` (CRLF counts as one grapheme).
    pub fn next_grapheme(&self, offset: usize) -> usize {
        let len = self.len();
        if offset >= len {
            return len;
        }
        let (mut chunk, mut chunk_start, _, _) = self.rope.chunk_at_byte(offset);
        let mut cursor = GraphemeCursor::new(offset, len, true);
        loop {
            match cursor.next_boundary(chunk, chunk_start) {
                Ok(boundary) => return boundary.unwrap_or(len),
                Err(GraphemeIncomplete::NextChunk) => {
                    chunk_start += chunk.len();
                    chunk = self.rope.chunk_at_byte(chunk_start).0;
                }
                Err(GraphemeIncomplete::PreContext(at)) => {
                    let (context, start, _, _) = self.rope.chunk_at_byte(at - 1);
                    cursor.provide_context(context, start);
                }
                Err(err) => unreachable!("grapheme cursor: {err:?}"),
            }
        }
    }

    /// Previous grapheme boundary before `offset`.
    pub fn prev_grapheme(&self, offset: usize) -> usize {
        if offset == 0 {
            return 0;
        }
        let (mut chunk, mut chunk_start, _, _) = self.rope.chunk_at_byte(offset);
        let mut cursor = GraphemeCursor::new(offset, self.len(), true);
        loop {
            match cursor.prev_boundary(chunk, chunk_start) {
                Ok(boundary) => return boundary.unwrap_or(0),
                Err(GraphemeIncomplete::PrevChunk) => {
                    (chunk, chunk_start, _, _) = self.rope.chunk_at_byte(chunk_start - 1);
                }
                Err(GraphemeIncomplete::PreContext(at)) => {
                    let (context, start, _, _) = self.rope.chunk_at_byte(at - 1);
                    cursor.provide_context(context, start);
                }
                Err(err) => unreachable!("grapheme cursor: {err:?}"),
            }
        }
    }
    /// End of the word (or punctuation run) at or after `offset`, skipping leading whitespace.
    pub fn next_word_end(&self, offset: usize) -> usize {
        let mut idx = self.rope.byte_to_char(offset);
        let mut run = None;
        for c in self.rope.chars_at(idx) {
            let class = char_class(c);
            match run {
                None if class == CharClass::Space => {}
                None => run = Some(class),
                Some(run) if run != class => break,
                Some(_) => {}
            }
            idx += 1;
        }
        self.rope.char_to_byte(idx)
    }

    /// Start of the word (or punctuation run) at or before `offset`, skipping trailing whitespace.
    pub fn prev_word_start(&self, offset: usize) -> usize {
        let mut idx = self.rope.byte_to_char(offset);
        let mut chars = self.rope.chars_at(idx);
        let mut run = None;
        while let Some(c) = chars.prev() {
            let class = char_class(c);
            match run {
                None if class == CharClass::Space => {}
                None => run = Some(class),
                Some(run) if run != class => break,
                Some(_) => {}
            }
            idx -= 1;
        }
        self.rope.char_to_byte(idx)
    }

    /// Replaces `range` with `text` and records it for undo.
    /// Consecutive single-line insertions coalesce into one undo step.
    pub fn edit(&mut self, range: Range<usize>, text: &str) {
        if range.is_empty() && text.is_empty() {
            return;
        }
        let old = self.slice(range.clone());
        self.replace(range.clone(), text);
        self.redo.clear();
        if self.saved_at.is_some_and(|saved| saved > self.undo.len()) {
            self.saved_at = None;
        }

        if old.is_empty()
            && !text.contains('\n')
            && Some(self.undo.len()) != self.saved_at
            && let Some([last]) = self.undo.last_mut().map(Vec::as_mut_slice)
            && last.old.is_empty()
            && !last.new.contains('\n')
            && last.offset + last.new.len() == range.start
        {
            last.new.push_str(text);
            return;
        }
        self.undo.push(vec![Edit {
            offset: range.start,
            old,
            new: text.to_owned(),
        }]);
    }

    /// Replaces the whole text with what is now on disk, as one undo step,
    /// and marks the buffer saved. Returns `false` if nothing differed.
    pub fn reload(&mut self, text: &str) -> bool {
        if self.rope == text {
            self.saved_at = Some(self.undo.len());
            return false;
        }
        self.edit(0..self.len(), text);
        self.saved_at = Some(self.undo.len());
        self.line_ending = Self::new(text).line_ending;
        true
    }

    /// Applies several edits as one undo step. Ranges refer to the current
    /// text and must not overlap; inserts at the same offset keep their order.
    pub fn edit_batch(
        &mut self,
        mut edits: Vec<(Range<usize>, String)>,
    ) -> Result<(), OverlappingEdits> {
        edits.sort_by_key(|(range, _)| range.start);
        if edits.windows(2).any(|pair| pair[0].0.end > pair[1].0.start) {
            return Err(OverlappingEdits);
        }
        edits.retain(|(range, text)| !(range.is_empty() && text.is_empty()));
        if edits.is_empty() {
            return Ok(());
        }
        // Back to front, so earlier ranges stay valid.
        let mut transaction = Vec::with_capacity(edits.len());
        for (range, text) in edits.into_iter().rev() {
            let old = self.slice(range.clone());
            self.replace(range.clone(), &text);
            transaction.push(Edit {
                offset: range.start,
                old,
                new: text,
            });
        }
        self.redo.clear();
        if self.saved_at.is_some_and(|saved| saved > self.undo.len()) {
            self.saved_at = None;
        }
        self.undo.push(transaction);
        Ok(())
    }

    /// Reverts the last undo step; returns the restored range of its
    /// last-in-text edit (where the cursor belongs).
    pub fn undo(&mut self) -> Option<Range<usize>> {
        let transaction = self.undo.pop()?;
        for edit in transaction.iter().rev() {
            self.replace(edit.offset..edit.offset + edit.new.len(), &edit.old);
        }
        // Edits apply back to front, so the first one is last in the text and,
        // being reverted last, its offset is already final.
        let first = &transaction[0];
        let restored = first.offset..first.offset + first.old.len();
        self.redo.push(transaction);
        Some(restored)
    }

    /// Reapplies the last undone step; returns the reinserted range of its
    /// last-in-text edit.
    pub fn redo(&mut self) -> Option<Range<usize>> {
        let transaction = self.redo.pop()?;
        for edit in &transaction {
            self.replace(edit.offset..edit.offset + edit.old.len(), &edit.new);
        }
        let first = &transaction[0];
        // Later edits sit earlier in the text and shift the first one.
        let shift: isize = transaction[1..]
            .iter()
            .map(|edit| edit.new.len() as isize - edit.old.len() as isize)
            .sum();
        let inserted = first.offset.saturating_add_signed(shift)
            ..first.offset.saturating_add_signed(shift) + first.new.len();
        self.undo.push(transaction);
        Some(inserted)
    }

    fn replace(&mut self, range: Range<usize>, text: &str) {
        let start_point = self.point(range.start);
        let old_end_point = self.point(range.end);
        let start_utf16 = self.point_in(range.start, Encoding::Utf16);
        let old_end_utf16 = self.point_in(range.end, Encoding::Utf16);
        let start = self.rope.byte_to_char(range.start);
        let end = self.rope.byte_to_char(range.end);
        self.rope.remove(start..end);
        self.rope.insert(start, text);
        let new_end = range.start + text.len();
        self.changes.push(Change {
            start: range.start,
            old_end: range.end,
            new_end,
            start_point,
            old_end_point,
            new_end_point: self.point(new_end),
            start_utf16,
            old_end_utf16,
        });
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Space,
    Word,
    Punctuation,
}

fn char_class(c: char) -> CharClass {
    if c.is_whitespace() {
        CharClass::Space
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punctuation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_end_excludes_crlf_and_lf() {
        let buffer = Buffer::new("ab\r\ncd\nef");
        assert_eq!(buffer.line_count(), 3);
        assert_eq!(buffer.line_end(0), 2);
        assert_eq!(buffer.line_text(0), "ab");
        assert_eq!(buffer.line_end(1), 6);
        assert_eq!(buffer.line_text(2), "ef");
    }

    #[test]
    fn ranges_mapped_through_edits_do_not_absorb_text_typed_at_their_edges() {
        let mut buffer = Buffer::new("aa BAD bb");
        let range = 3..6; // "BAD"
        let map = |change: Change, range: Range<usize>| {
            change.map_offset(range.start, Bias::Right)..change.map_offset(range.end, Bias::Left)
        };
        let mut edit = |at: Range<usize>, text: &str| {
            buffer.edit(at, text);
            buffer.drain_changes().next().unwrap()
        };
        // Typing just before and just after leaves the range on "BAD".
        let range = map(edit(3..3, "xx"), range);
        assert_eq!(range, 5..8);
        let range = map(edit(8..8, "yy"), range);
        assert_eq!(range, 5..8);
        // Edits earlier in the text shift it; edits inside resize it.
        let range = map(edit(0..2, ""), range);
        assert_eq!(range, 3..6);
        let range = map(edit(4..5, "ZZZ"), range);
        assert_eq!(range, 3..8);
        // Deleting the whole range collapses it.
        let range = map(edit(2..9, ""), range);
        assert!(range.is_empty());
    }

    #[test]
    fn only_lf_starts_a_line() {
        let buffer = Buffer::new("a\rb\u{2028}c\nd");
        assert_eq!(buffer.line_count(), 2);
        assert_eq!(buffer.point(buffer.len()), Point { row: 1, column: 1 });
    }

    #[test]
    fn edits_record_old_and_new_points() {
        let mut buffer = Buffer::new("ab\r\ncd");
        buffer.edit(1..5, "X\nY");
        assert_eq!(buffer.slice(0..buffer.len()), "aX\nYd");
        let changes: Vec<Change> = buffer.drain_changes().collect();
        assert_eq!(
            changes,
            [Change {
                start: 1,
                old_end: 5,
                new_end: 4,
                start_point: Point { row: 0, column: 1 },
                old_end_point: Point { row: 1, column: 1 },
                new_end_point: Point { row: 1, column: 1 },
                start_utf16: Point { row: 0, column: 1 },
                old_end_utf16: Point { row: 1, column: 1 },
            }]
        );
        buffer.undo();
        assert_eq!(buffer.drain_changes().count(), 1);
    }

    #[test]
    fn line_ending_follows_first_line_break() {
        assert_eq!(Buffer::new("a\r\nb\n").line_ending(), "\r\n");
        assert_eq!(Buffer::new("a\nb\r\n").line_ending(), "\n");
        assert_eq!(Buffer::new("no breaks").line_ending(), "\n");
    }

    #[test]
    fn points_convert_in_both_encodings_and_clamp_out_of_range() {
        // '😀' is 4 UTF-8 bytes and 2 UTF-16 units.
        let buffer = Buffer::new("x\r\na😀b\n");
        let b = 3 + 1 + 4; // offset of 'b'
        assert_eq!(
            buffer.point_in(b, Encoding::Utf8),
            Point { row: 1, column: 5 }
        );
        assert_eq!(
            buffer.point_in(b, Encoding::Utf16),
            Point { row: 1, column: 3 }
        );
        assert_eq!(
            buffer.offset_of(Point { row: 1, column: 3 }, Encoding::Utf16),
            b
        );
        assert_eq!(
            buffer.offset_of(Point { row: 1, column: 5 }, Encoding::Utf8),
            b
        );
        // Inside the emoji snaps to its start.
        assert_eq!(
            buffer.offset_of(Point { row: 1, column: 2 }, Encoding::Utf8),
            4
        );
        assert_eq!(
            buffer.offset_of(Point { row: 1, column: 2 }, Encoding::Utf16),
            4
        );
        // Past the line end stops before the line break, not inside it.
        assert_eq!(
            buffer.offset_of(Point { row: 0, column: 99 }, Encoding::Utf16),
            1
        );
        assert_eq!(
            buffer.offset_of(Point { row: 9, column: 0 }, Encoding::Utf8),
            buffer.len()
        );
    }

    #[test]
    fn graphemes_treat_crlf_and_combining_marks_as_one() {
        // "e\u{301}" is e + combining acute: 3 bytes, one grapheme.
        let buffer = Buffer::new("e\u{301}\r\nx");
        assert_eq!(buffer.next_grapheme(0), 3);
        assert_eq!(buffer.next_grapheme(3), 5);
        assert_eq!(buffer.prev_grapheme(6), 5);
        assert_eq!(buffer.prev_grapheme(5), 3);
        assert_eq!(buffer.prev_grapheme(3), 0);
        assert_eq!(buffer.next_grapheme(6), 6);
    }

    #[test]
    fn utf16_round_trips_through_astral_chars() {
        // '😀' is 4 UTF-8 bytes and 2 UTF-16 units.
        let buffer = Buffer::new("a😀b");
        assert_eq!(buffer.offset_to_utf16(5), 3);
        assert_eq!(buffer.offset_from_utf16(3), 5);
        assert_eq!(buffer.offset_from_utf16(99), buffer.len());
    }
    #[test]
    fn word_motion_stops_at_class_changes_and_skips_spaces() {
        let buffer = Buffer::new("foo  bar.baz");
        assert_eq!(buffer.next_word_end(0), 3);
        assert_eq!(buffer.next_word_end(3), 8);
        assert_eq!(buffer.next_word_end(8), 9);
        assert_eq!(buffer.next_word_end(12), 12);
        assert_eq!(buffer.prev_word_start(12), 9);
        assert_eq!(buffer.prev_word_start(9), 8);
        assert_eq!(buffer.prev_word_start(8), 5);
        assert_eq!(buffer.prev_word_start(3), 0);
    }

    #[test]
    fn offset_at_clamps_column_to_line_end() {
        let buffer = Buffer::new("long line\nab\r\n");
        assert_eq!(buffer.offset_at(1, 8), 12);
        assert_eq!(buffer.column(12), 2);
    }

    #[test]
    fn typing_coalesces_into_one_undo_step_and_tracks_dirty() {
        let mut buffer = Buffer::new("x");
        buffer.edit(1..1, "a");
        buffer.edit(2..2, "b");
        buffer.edit(3..3, "\n");
        assert!(buffer.is_dirty());
        assert_eq!(buffer.undo(), Some(3..3));
        assert_eq!(buffer.undo(), Some(1..1));
        assert_eq!(buffer.slice(0..buffer.len()), "x");
        assert!(!buffer.is_dirty());
        assert_eq!(buffer.redo(), Some(1..3));
        assert!(buffer.is_dirty());
    }

    #[test]
    fn batch_edits_apply_and_undo_as_one_step() {
        let text = "def f():\n    os.pa\n";
        let mut buffer = Buffer::new(text);
        // Complete `os.pa` → `os.path` and add an import at the top.
        buffer
            .edit_batch(vec![(16..18, "path".into()), (0..0, "import os\n".into())])
            .unwrap();
        let edited = "import os\ndef f():\n    os.path\n";
        assert_eq!(buffer.slice(0..buffer.len()), edited);

        // The cursor goes back to the completion, not the import.
        assert_eq!(buffer.undo(), Some(16..18));
        assert_eq!(buffer.slice(0..buffer.len()), text);
        assert_eq!(buffer.redo(), Some(26..30));
        assert_eq!(buffer.slice(0..buffer.len()), edited);
        assert_eq!(buffer.slice(26..30), "path");
    }

    #[test]
    fn reload_is_undoable_and_leaves_the_buffer_clean() {
        let mut buffer = Buffer::new("old\n");
        assert!(!buffer.reload("old\n"), "identical text is not an edit");
        assert!(buffer.reload("new\r\n"));
        assert_eq!(buffer.slice(0..buffer.len()), "new\r\n");
        assert_eq!(buffer.line_ending(), "\r\n");
        assert!(!buffer.is_dirty());
        buffer.undo();
        assert_eq!(buffer.slice(0..buffer.len()), "old\n");
        assert!(buffer.is_dirty(), "undoing a reload diverges from disk");
    }

    #[test]
    fn batch_inserts_at_one_offset_keep_order_and_overlaps_are_rejected() {
        let mut buffer = Buffer::new("x");
        buffer
            .edit_batch(vec![(0..0, "a".into()), (0..0, "b".into())])
            .unwrap();
        assert_eq!(buffer.slice(0..buffer.len()), "abx");
        assert_eq!(
            buffer.edit_batch(vec![(0..2, "".into()), (1..3, "".into())]),
            Err(OverlappingEdits)
        );
        assert_eq!(buffer.slice(0..buffer.len()), "abx");
    }

    #[test]
    fn new_edit_after_undo_past_save_point_stays_dirty() {
        let mut buffer = Buffer::new("");
        buffer.edit(0..0, "a");
        buffer.saved_at = Some(1); // as if saved here
        buffer.undo();
        buffer.edit(0..0, "b");
        assert!(buffer.is_dirty());
        buffer.undo();
        assert!(
            buffer.is_dirty(),
            "save point was discarded with the redo stack"
        );
    }
}
