//! Indentation guides: one vertical line per indentation level in the
//! leading whitespace of each line, the one of the block holding the
//! cursor highlighted (VS Code's guides).

use std::ops::Range;

use syntax::Language;
use text::Buffer;

/// Lines read past the visible ones to find the non-blank lines around a
/// blank one.
const LOOKAROUND: usize = 100;

/// Guides of the visible lines.
pub(super) struct IndentGuides {
    /// First line of `indents`.
    pub first: usize,
    /// Indentation columns of each line from `first`; a blank line takes
    /// one from the lines around it.
    pub indents: Vec<usize>,
    /// Columns per level.
    pub unit: usize,
    /// The highlighted guide: its column and the lines it spans (only the
    /// part among `indents`).
    pub active: Option<(usize, Range<usize>)>,
}

impl IndentGuides {
    /// Guides for `lines`, the highlighted one around `cursor_line`.
    /// `unit` is the columns of one level; a tab counts as one column (it
    /// is drawn as one space).
    pub fn new(
        buffer: &Buffer,
        lines: Range<usize>,
        cursor_line: Option<usize>,
        unit: usize,
        language: Option<Language>,
    ) -> Self {
        let indents = resolve(
            lines.clone(),
            buffer.line_count(),
            unit.max(1),
            off_side(language),
            |line| leading_columns(buffer, line),
        );
        let active = cursor_line.and_then(|line| active_guide(&indents, lines.start, line, unit));
        Self {
            first: lines.start,
            indents,
            unit: unit.max(1),
            active,
        }
    }

    /// Guide columns of `line`, left to right.
    pub fn columns(&self, line: usize) -> impl Iterator<Item = usize> + '_ {
        let indent = line
            .checked_sub(self.first)
            .and_then(|index| self.indents.get(index))
            .copied()
            .unwrap_or(0);
        (0..indent.div_ceil(self.unit)).map(|level| level * self.unit)
    }

    /// Whether the guide at `column` is highlighted on `line`.
    pub fn is_active(&self, line: usize, column: usize) -> bool {
        self.active
            .as_ref()
            .is_some_and(|(active, lines)| *active == column && lines.contains(&line))
    }
}

/// Languages whose blocks end by dedenting (no closing bracket line).
fn off_side(language: Option<Language>) -> bool {
    matches!(language, Some(Language::Python | Language::Yaml))
}

/// Leading whitespace columns of `line`, `None` when it is blank.
fn leading_columns(buffer: &Buffer, line: usize) -> Option<usize> {
    let range = buffer.line_start(line)..buffer.line_end(line);
    let mut columns = 0;
    for chunk in buffer.chunks(range) {
        for c in chunk.chars() {
            match c {
                ' ' | '\t' => columns += 1,
                '\r' => {}
                _ => return Some(columns),
            }
        }
    }
    None
}

/// Indentation of each of `lines` from `indent_of` (`None` = blank). A
/// blank line takes VS Code's choice from the non-blank lines around it
/// (`above`, `below`, read up to [`LOOKAROUND`] lines away): inside a
/// block that opens above, one level past `above`; between lines of one
/// level, that level; after a block that closed, `below`, plus one level
/// where a closing bracket line follows.
fn resolve(
    lines: Range<usize>,
    line_count: usize,
    unit: usize,
    off_side: bool,
    indent_of: impl Fn(usize) -> Option<usize>,
) -> Vec<usize> {
    let start = lines.start.saturating_sub(LOOKAROUND);
    let end = (lines.end + LOOKAROUND).min(line_count);
    let read: Vec<Option<usize>> = (start..end).map(&indent_of).collect();
    let mut above = vec![None; read.len()];
    let mut last = None;
    for (i, indent) in read.iter().enumerate() {
        above[i] = last;
        if indent.is_some() {
            last = *indent;
        }
    }
    let mut below = vec![None; read.len()];
    let mut next = None;
    for (i, indent) in read.iter().enumerate().rev() {
        below[i] = next;
        if indent.is_some() {
            next = *indent;
        }
    }
    (lines.start..lines.end.min(line_count))
        .map(|line| {
            let i = line - start;
            read[i].unwrap_or_else(|| match (above[i], below[i]) {
                (Some(above), Some(below)) if above < below => (above + unit).min(below),
                (Some(above), Some(below)) if above == below => below,
                (Some(above), Some(below)) if off_side => below.min(above),
                (Some(above), Some(below)) => (below + unit).min(above),
                _ => 0,
            })
        })
        .collect()
}

/// The guide of the block holding `cursor` and the lines it spans among
/// `indents` (lines from `first`): the cursor line's deepest guide, or the
/// one opened by the cursor line when the next line is deeper (a block
/// header).
fn active_guide(
    indents: &[usize],
    first: usize,
    cursor: usize,
    unit: usize,
) -> Option<(usize, Range<usize>)> {
    let unit = unit.max(1);
    let index = cursor.checked_sub(first)?;
    let indent = *indents.get(index)?;
    let (column, anchor) = match indents.get(index + 1) {
        Some(&next) if next > indent => (indent, index + 1),
        _ if indent == 0 => return None,
        _ => ((indent - 1) / unit * unit, index),
    };
    let inside = |i: usize| indents[i] > column;
    let mut top = anchor;
    while top > 0 && inside(top - 1) {
        top -= 1;
    }
    let mut bottom = anchor + 1;
    while bottom < indents.len() && inside(bottom) {
        bottom += 1;
    }
    Some((column, first + top..first + bottom))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn indents(text: &str, off_side: bool) -> Vec<usize> {
        let lines: Vec<&str> = text.lines().collect();
        resolve(0..lines.len(), lines.len(), 4, off_side, |line| {
            let line = lines[line];
            let trimmed = line.trim_start();
            (!trimmed.is_empty()).then(|| line.len() - trimmed.len())
        })
    }

    #[test]
    fn blank_lines_take_the_level_of_the_block_around_them() {
        let rust =
            "fn a() {\n    let x = 1;\n\n    if x {\n\n        y();\n\n    }\n\n}\n\nfn b() {}";
        assert_eq!(indents(rust, false), [0, 4, 4, 4, 8, 8, 8, 4, 4, 0, 0, 0]);
        let python = "def a():\n\n    x = 1\n        \n    return x\n\n\ndef b():\n    pass";
        assert_eq!(indents(python, true), [0, 4, 4, 4, 4, 0, 0, 0, 4]);
    }

    #[test]
    fn guides_sit_at_each_level_of_the_indentation() {
        let guides = IndentGuides {
            first: 10,
            indents: vec![0, 4, 9],
            unit: 4,
            active: None,
        };
        assert_eq!(guides.columns(10).count(), 0);
        assert_eq!(guides.columns(11).collect::<Vec<_>>(), [0]);
        // A partial level still gets its guide.
        assert_eq!(guides.columns(12).collect::<Vec<_>>(), [0, 4, 8]);
        assert_eq!(guides.columns(13).count(), 0);
    }

    #[test]
    fn the_active_guide_is_the_cursor_blocks() {
        //            0  1  2  3  4  5  6
        let indents = [0, 4, 8, 8, 4, 4, 0];
        // Inside the inner block: the guide at 4 over lines 2-3.
        assert_eq!(active_guide(&indents, 0, 3, 4), Some((4, 2..4)));
        // On a header: the block it opens.
        assert_eq!(active_guide(&indents, 0, 1, 4), Some((4, 2..4)));
        assert_eq!(active_guide(&indents, 0, 0, 4), Some((0, 1..6)));
        // A body line after the inner block.
        assert_eq!(active_guide(&indents, 0, 5, 4), Some((0, 1..6)));
        // Top level, no deeper line next: none.
        assert_eq!(active_guide(&indents, 0, 6, 4), None);
        // Lines are offset by `first`; a cursor outside them has none.
        assert_eq!(active_guide(&indents, 20, 23, 4), Some((4, 22..24)));
        assert_eq!(active_guide(&indents, 20, 3, 4), None);
    }
}
