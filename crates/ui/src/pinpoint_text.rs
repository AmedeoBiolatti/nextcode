//! Pinpoint down to the letter: code ranges as agents and references name
//! them (`src/main.rs:12:5-12:18`), and the shared helpers text owners
//! (editor text, notebook cells, terminal grid, selectable text) use to
//! report where characters are on screen.
//!
//! Lines are 1-based, columns are 1-based characters, the end column is
//! inclusive (the last highlighted character). No column: whole line(s);
//! a column without an end column: to the end of the (end) line.

use std::ops::Range;

use gpui::{App, Bounds, Entity, Pixels, TextLayout, Window, px, size};
use text::Buffer;

use crate::{
    pinpoint::{CodeWant, TerminalRef, TerminalWant},
    shell::Shell,
    workspace::Workspace,
};

/// A range of a file or notebook cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CodeSpan {
    pub line: u32,
    pub column: Option<u32>,
    pub end_line: Option<u32>,
    pub end_column: Option<u32>,
}

impl CodeSpan {
    /// The buffer bytes it covers; `None` when its line is not in `buffer`.
    pub fn offsets(&self, buffer: &Buffer) -> Option<Range<usize>> {
        let count = buffer.line_count();
        let line = (self.line as usize).checked_sub(1).filter(|&l| l < count)?;
        let end_line = self
            .end_line
            .map_or(line, |l| (l as usize).saturating_sub(1).min(count - 1))
            .max(line);
        let start = match self.column {
            Some(column) => buffer.offset_at(line, (column as usize).saturating_sub(1)),
            None => buffer.line_start(line),
        };
        let end = match self.end_column {
            Some(column) => buffer.offset_at(end_line, column as usize),
            None => buffer.line_end(end_line),
        };
        Some(start..end.max(start))
    }

    /// The span of a selection: the line alone for a cursor, whole lines
    /// when it runs from a line start to a line start, else its first and
    /// last characters (a trailing line break left out).
    pub fn of_range(buffer: &Buffer, range: Range<usize>) -> Self {
        let line = |offset| buffer.line_of(offset) as u32 + 1;
        let column = |offset| buffer.column(offset) as u32 + 1;
        if range.is_empty() {
            return Self {
                line: line(range.start),
                ..Self::default()
            };
        }
        let (start_line, end_line) = (buffer.line_of(range.start), buffer.line_of(range.end));
        if buffer.line_start(start_line) == range.start
            && buffer.line_start(end_line) == range.end
            && end_line > start_line
        {
            let last = end_line as u32;
            return Self {
                line: line(range.start),
                end_line: (last > line(range.start)).then_some(last),
                ..Self::default()
            };
        }
        // The last character, a trailing line break left out.
        let mut last = range.end - 1;
        if buffer.line_start(end_line) == range.end {
            let line_end = buffer.line_end(end_line - 1);
            if line_end > range.start {
                last = line_end - 1;
            }
        }
        Self {
            line: line(range.start),
            column: Some(column(range.start)),
            end_line: Some(line(last)),
            end_column: Some(column(last)),
        }
    }
}

impl From<&CodeWant> for CodeSpan {
    fn from(want: &CodeWant) -> Self {
        Self {
            line: want.line,
            column: want.column,
            end_line: want.end_line,
            end_column: want.end_column,
        }
    }
}

impl CodeSpan {
    /// This span of `file` (notebook cell `cell`) as a target.
    pub fn want(self, file: String, cell: Option<usize>) -> CodeWant {
        CodeWant {
            file,
            cell,
            line: self.line,
            column: self.column,
            end_line: self.end_line,
            end_column: self.end_column,
        }
    }
}

/// `rects` clipped to `clip` (empty ones dropped), and whether any part
/// was cut off (a pixel's sliver of a glyph row does not count).
pub fn clip_rects(
    rects: impl IntoIterator<Item = Bounds<Pixels>>,
    clip: Bounds<Pixels>,
) -> (Vec<Bounds<Pixels>>, bool) {
    let mut shown_rects = Vec::new();
    let mut clipped = false;
    for rect in rects {
        let shown = rect.intersect(&clip);
        if shown.size.width <= Pixels::ZERO || shown.size.height <= Pixels::ZERO {
            clipped = true;
            continue;
        }
        clipped |= rect.size.width - shown.size.width > px(1.)
            || rect.size.height - shown.size.height > px(1.);
        shown_rects.push(shown);
    }
    (shown_rects, clipped)
}

/// What the picker takes at byte `index` of `text`: the run of word
/// characters (letters, digits, `_`) there, else that one character.
pub fn word_at(text: &str, index: usize) -> Range<usize> {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let Some(ch) = text[index..].chars().next() else {
        return index..index;
    };
    if !is_word(ch) {
        return index..index + ch.len_utf8();
    }
    let start = text[..index]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| is_word(c))
        .last()
        .map_or(index, |(i, _)| i);
    let end = text[index..]
        .char_indices()
        .find(|&(_, c)| !is_word(c))
        .map_or(text.len(), |(i, _)| index + i);
    start..end
}

/// Shows the code `want` names in `window`'s shown project (opening the
/// file, scrolling it to the middle) without moving focus or selections.
pub fn reveal_code(window: &mut Window, cx: &mut App, want: &CodeWant) -> Result<(), String> {
    let workspace = shown_workspace(window, cx)?;
    workspace.update(cx, |workspace, cx| {
        workspace.pinpoint_reveal(&want.file, want.cell, CodeSpan::from(want), window, cx)
    })
}

/// Shows the terminal `want` names (the Terminal panel, its tab, and for a
/// `text` match the lowest line holding it, from the history if needed).
pub fn reveal_terminal(
    window: &mut Window,
    cx: &mut App,
    want: &TerminalWant,
) -> Result<(), String> {
    let workspace = shown_workspace(window, cx)?;
    let index = match want.terminal {
        TerminalRef::Index(index) => Some(index),
        TerminalRef::Active => None,
    };
    workspace.update(cx, |workspace, cx| {
        workspace.pinpoint_reveal_terminal(index, want.text.as_deref(), cx)
    })
}

fn shown_workspace(window: &Window, cx: &App) -> Result<Entity<Workspace>, String> {
    window
        .root::<Shell>()
        .flatten()
        .and_then(|shell| shell.read(cx).active_workspace())
        .ok_or_else(|| "this window shows no project".to_owned())
}

/// The `nth` (1-based) occurrence of `needle` in `text`.
pub fn nth_match(text: &str, needle: &str, nth: usize) -> Option<Range<usize>> {
    if needle.is_empty() {
        return None;
    }
    let (at, _) = text.match_indices(needle).nth(nth.max(1) - 1)?;
    Some(at..at + needle.len())
}

/// The reference of `range` of the text of element `path`, by content
/// and occurrence (stable while the text scrolls).
pub fn range_reference(path: &str, text: &str, range: Range<usize>) -> String {
    let needle = &text[range.clone()];
    let nth = text[..range.start].match_indices(needle).count() + 1;
    crate::pinpoint::text_reference(path, needle, nth)
}

/// Window rects of `range` of `text` laid out as `layout` (one per row it
/// wraps to), and whether some of it has no position (cut off).
pub fn layout_rects(
    layout: &TextLayout,
    text: &str,
    range: Range<usize>,
) -> (Vec<Bounds<Pixels>>, bool) {
    let height = layout.line_height();
    let mut rects: Vec<Bounds<Pixels>> = Vec::new();
    let mut missing = false;
    let end = range.end.min(text.len());
    let start = range.start.min(end);
    let mut line_start = text[..start].rfind('\n').map_or(0, |at| at + 1);
    for (at, ch) in text[start..end].char_indices() {
        let at = start + at;
        if ch == '\n' {
            line_start = at + 1;
            continue;
        }
        let (Some(position), Some(line)) = (
            layout.position_for_index(at),
            layout.line_layout_for_index(at),
        ) else {
            missing = true;
            continue;
        };
        let shaped = &line.unwrapped_layout;
        let index = at - line_start;
        let width = shaped.x_for_index(index + ch.len_utf8()) - shaped.x_for_index(index);
        let rect = Bounds::new(position, size(width.max(Pixels::ZERO), height));
        match rects.last_mut() {
            Some(last)
                if last.top() == rect.top() && (last.right() - rect.left()).abs() < px(1.) =>
            {
                last.size.width = rect.right() - last.left();
            }
            _ => rects.push(rect),
        }
    }
    (rects, missing)
}

/// In prepaint of a selectable text inside pinned element `path`: adds
/// `text` to the node's text, reports the parts of wanted ranges in it,
/// and the word under the picker.
pub fn record_layout_text(window: &Window, path: &str, text: &str, layout: &TextLayout) {
    if path.is_empty() {
        return;
    }
    let Some(offset) = crate::pinpoint::append_text(window, path, text) else {
        return;
    };
    let mask = window.content_mask().bounds;
    for (id, range) in crate::pinpoint::wanted_text(window, path) {
        let (start, end) = (range.start.max(offset), range.end.min(offset + text.len()));
        if start >= end {
            continue;
        }
        let (rects, missing) = layout_rects(layout, text, start - offset..end - offset);
        let (rects, clipped) = clip_rects(rects, mask);
        crate::pinpoint::report(window, id, rects, clipped || missing);
    }
    if let Some(pointer) = crate::pinpoint::picking(window)
        && layout.bounds().contains(&pointer)
        && mask.contains(&pointer)
        && let Ok(index) = layout.index_for_position(pointer)
    {
        let word = word_at(text, index);
        if text[word.clone()].trim().is_empty() {
            return;
        }
        let (rects, _) = layout_rects(layout, text, word.clone());
        crate::pinpoint::report_pick(
            window,
            crate::pinpoint::PickText {
                reference: range_reference(path, text, word),
                path: path.to_owned().into(),
                rects,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pinpoint::{code_reference, parse_code_reference};

    fn span(
        line: u32,
        column: Option<u32>,
        end_line: Option<u32>,
        end_column: Option<u32>,
    ) -> CodeSpan {
        CodeSpan {
            line,
            column,
            end_line,
            end_column,
        }
    }

    #[test]
    fn spans_cover_chars_with_inclusive_ends() {
        let buffer = Buffer::new("fn main() {\n    let é = \"日本\";\n}\n");
        let text = |s: CodeSpan| s.offsets(&buffer).map(|r| buffer.slice(r));
        assert_eq!(text(span(1, Some(4), None, Some(7))).unwrap(), "main");
        assert_eq!(text(span(1, None, None, None)).unwrap(), "fn main() {");
        // Columns count characters, not bytes.
        assert_eq!(text(span(2, Some(9), None, Some(9))).unwrap(), "é");
        assert_eq!(text(span(2, Some(13), None, Some(16))).unwrap(), "\"日本\"");
        assert_eq!(
            text(span(2, Some(5), None, None)).unwrap(),
            "let é = \"日本\";"
        );
        assert_eq!(
            text(span(1, None, Some(3), None)).unwrap(),
            "fn main() {\n    let é = \"日本\";\n}"
        );
        assert_eq!(
            text(span(1, Some(11), Some(3), Some(1))).unwrap(),
            "{\n    let é = \"日本\";\n}"
        );
        // Past the end of a line: clamped; past the last line: nothing.
        assert_eq!(text(span(3, Some(1), None, Some(99))).unwrap(), "}");
        assert_eq!(text(span(9, None, None, None)), None);
        assert_eq!(text(span(0, None, None, None)), None);
    }

    #[test]
    fn selections_become_references_that_select_the_same_text() {
        let buffer = Buffer::new("fn main() {\n    let é = 1;\n}\n");
        let reference = |range: Range<usize>, cell: Option<usize>| {
            code_reference(&CodeSpan::of_range(&buffer, range).want("src/a.rs".into(), cell))
        };
        assert_eq!(reference(3..7, None), "src/a.rs:1:4-1:7");
        assert_eq!(reference(5..5, None), "src/a.rs:1", "a cursor: its line");
        // Whole lines (a line-start to line-start selection).
        assert_eq!(reference(0..12, None), "src/a.rs:1");
        assert_eq!(reference(0..buffer.line_start(2), None), "src/a.rs:1-2");
        // A trailing line break is left out.
        assert_eq!(reference(3..12, None), "src/a.rs:1:4-1:11");
        let multi = buffer.line_start(1) + 4..buffer.line_start(2) + 1;
        assert_eq!(reference(multi.clone(), None), "src/a.rs:2:5-3:1");
        assert_eq!(reference(16..22, Some(3)), "src/a.rs#cell=3:2:5-2:9");
        // Parsed back as a target, they select the same text (multibyte
        // and Windows paths included).
        for range in [3..7, multi, 16..22, 20..22] {
            for (file, cell) in [
                ("src/a.rs", None),
                (r"C:\w\a.rs", None),
                ("n.ipynb", Some(2)),
            ] {
                let text = code_reference(
                    &CodeSpan::of_range(&buffer, range.clone()).want(file.into(), cell),
                );
                let want = parse_code_reference(&text).expect(&text);
                assert_eq!((want.file.as_str(), want.cell), (file, cell));
                assert_eq!(
                    CodeSpan::from(&want).offsets(&buffer),
                    Some(range.clone()),
                    "{text}"
                );
            }
        }
    }
}
