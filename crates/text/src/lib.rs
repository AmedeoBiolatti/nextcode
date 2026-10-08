//! UI-agnostic text buffer.
//!
//! All public offsets are UTF-8 byte offsets into the whole buffer. Callers must
//! pass offsets on char boundaries; grapheme helpers and UTF-16 conversions
//! always return valid boundaries.

use std::{
    fs,
    io::{self, BufWriter, Write},
    ops::Range,
    path::{Path, PathBuf},
};

use ropey::Rope;
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};

/// Characters ropey treats as line breaks (CRLF is handled as a pair).
const LINE_BREAKS: [char; 7] = [
    '\n', '\r', '\u{0B}', '\u{0C}', '\u{85}', '\u{2028}', '\u{2029}',
];

#[derive(Debug, Clone)]
struct Edit {
    offset: usize,
    old: String,
    new: String,
}

pub struct Buffer {
    rope: Rope,
    path: Option<PathBuf>,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    /// Undo-stack depth matching the file on disk; `None` if that state is unreachable.
    saved_at: Option<usize>,
    line_ending: &'static str,
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
        if let Some(last) = chars.prev().filter(|c| LINE_BREAKS.contains(c)) {
            end -= last.len_utf8();
            if last == '\n' && chars.prev() == Some('\r') {
                end -= 1;
            }
        }
        end
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
            && !text.contains(LINE_BREAKS)
            && Some(self.undo.len()) != self.saved_at
            && let Some(last) = self.undo.last_mut()
            && last.old.is_empty()
            && !last.new.contains(LINE_BREAKS)
            && last.offset + last.new.len() == range.start
        {
            last.new.push_str(text);
            return;
        }
        self.undo.push(Edit {
            offset: range.start,
            old,
            new: text.to_owned(),
        });
    }

    /// Reverts the last edit; returns the range of restored text.
    pub fn undo(&mut self) -> Option<Range<usize>> {
        let edit = self.undo.pop()?;
        self.replace(edit.offset..edit.offset + edit.new.len(), &edit.old);
        let restored = edit.offset..edit.offset + edit.old.len();
        self.redo.push(edit);
        Some(restored)
    }

    /// Reapplies the last undone edit; returns the range of reinserted text.
    pub fn redo(&mut self) -> Option<Range<usize>> {
        let edit = self.redo.pop()?;
        self.replace(edit.offset..edit.offset + edit.old.len(), &edit.new);
        let inserted = edit.offset..edit.offset + edit.new.len();
        self.undo.push(edit);
        Some(inserted)
    }

    fn replace(&mut self, range: Range<usize>, text: &str) {
        let start = self.rope.byte_to_char(range.start);
        let end = self.rope.byte_to_char(range.end);
        self.rope.remove(start..end);
        self.rope.insert(start, text);
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
    fn line_ending_follows_first_line_break() {
        assert_eq!(Buffer::new("a\r\nb\n").line_ending(), "\r\n");
        assert_eq!(Buffer::new("a\nb\r\n").line_ending(), "\n");
        assert_eq!(Buffer::new("no breaks").line_ending(), "\n");
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
