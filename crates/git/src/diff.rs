//! Line diffs between two versions of a file, grouped into hunks.

use serde::Serialize;
use similar::{ChangeTag, TextDiff};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Row {
    Line {
        kind: LineKind,
        /// 1-based line in the old version (absent for added lines).
        old: Option<usize>,
        /// 1-based line in the new version (absent for removed lines).
        new: Option<usize>,
        /// Without its line break.
        text: String,
    },
    /// Unchanged lines left out between hunks.
    Skipped { lines: usize },
    /// Start of hunk `index` of a git patch (see `Patch::to_file_diff`).
    Hunk { index: usize, header: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FileDiff {
    pub rows: Vec<Row>,
    pub added: usize,
    pub removed: usize,
    /// One side is not text; `rows` is empty.
    pub binary: bool,
}

/// Context that shows whole files (no skipped lines).
pub const FULL_FILE: usize = 1 << 30;

/// One side of a side-by-side row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SideLine {
    pub kind: LineKind,
    /// 1-based line on this side.
    pub line: usize,
    pub text: String,
}

/// A row of a side-by-side diff: old on the left, new on the right.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitRow {
    Pair {
        left: Option<SideLine>,
        right: Option<SideLine>,
    },
    Skipped {
        lines: usize,
    },
    Hunk {
        index: usize,
        header: String,
    },
}

/// Lays unified rows side by side: unchanged lines face each other, and a
/// run of removals faces the run of additions that replaced it.
pub fn side_by_side(rows: &[Row]) -> Vec<SplitRow> {
    let mut out = Vec::new();
    let mut removed: Vec<SideLine> = Vec::new();
    let mut added: Vec<SideLine> = Vec::new();
    let flush =
        |out: &mut Vec<SplitRow>, removed: &mut Vec<SideLine>, added: &mut Vec<SideLine>| {
            let count = removed.len().max(added.len());
            let mut left = removed.drain(..);
            let mut right = added.drain(..);
            for _ in 0..count {
                out.push(SplitRow::Pair {
                    left: left.next(),
                    right: right.next(),
                });
            }
        };
    for row in rows {
        match row {
            Row::Line {
                kind: LineKind::Removed,
                old,
                text,
                ..
            } => {
                // Removals after additions start a new change block.
                if !added.is_empty() {
                    flush(&mut out, &mut removed, &mut added);
                }
                removed.push(SideLine {
                    kind: LineKind::Removed,
                    line: old.unwrap_or_default(),
                    text: text.clone(),
                });
            }
            Row::Line {
                kind: LineKind::Added,
                new,
                text,
                ..
            } => added.push(SideLine {
                kind: LineKind::Added,
                line: new.unwrap_or_default(),
                text: text.clone(),
            }),
            Row::Line {
                kind: LineKind::Context,
                old,
                new,
                text,
            } => {
                flush(&mut out, &mut removed, &mut added);
                let side = |line: &Option<usize>| SideLine {
                    kind: LineKind::Context,
                    line: line.unwrap_or_default(),
                    text: text.clone(),
                };
                out.push(SplitRow::Pair {
                    left: Some(side(old)),
                    right: Some(side(new)),
                });
            }
            Row::Skipped { lines } => {
                flush(&mut out, &mut removed, &mut added);
                out.push(SplitRow::Skipped { lines: *lines });
            }
            Row::Hunk { index, header } => {
                flush(&mut out, &mut removed, &mut added);
                out.push(SplitRow::Hunk {
                    index: *index,
                    header: header.clone(),
                });
            }
        }
    }
    flush(&mut out, &mut removed, &mut added);
    out
}

/// Unified diff of `old` → `new` with `context` unchanged lines around
/// each change. `None` sides are missing files (diffed as empty). Line
/// endings are ignored, like git does when it converts CRLF on checkout.
pub fn diff(old: Option<&[u8]>, new: Option<&[u8]>, context: usize) -> FileDiff {
    let text = |side: Option<&[u8]>| -> Option<String> {
        let bytes = side.unwrap_or_default();
        if bytes.contains(&0) {
            return None;
        }
        String::from_utf8(bytes.to_vec())
            .ok()
            .map(|text| text.replace("\r\n", "\n"))
    };
    let (Some(old), Some(new)) = (text(old), text(new)) else {
        return FileDiff {
            binary: true,
            ..FileDiff::default()
        };
    };
    let diff = TextDiff::from_lines(&old, &new);
    let mut result = FileDiff::default();
    let old_lines = diff.old_slices().len();
    let mut shown_until = 0; // old lines covered by earlier rows
    for group in diff.grouped_ops(context) {
        let first_old = group.first().map_or(0, |op| op.old_range().start);
        if first_old > shown_until {
            result.rows.push(Row::Skipped {
                lines: first_old - shown_until,
            });
        }
        for op in &group {
            for change in diff.iter_changes(op) {
                let kind = match change.tag() {
                    ChangeTag::Equal => LineKind::Context,
                    ChangeTag::Insert => {
                        result.added += 1;
                        LineKind::Added
                    }
                    ChangeTag::Delete => {
                        result.removed += 1;
                        LineKind::Removed
                    }
                };
                result.rows.push(Row::Line {
                    kind,
                    old: change.old_index().map(|index| index + 1),
                    new: change.new_index().map(|index| index + 1),
                    text: change
                        .value()
                        .trim_end_matches('\n')
                        .trim_end_matches('\r')
                        .to_owned(),
                });
            }
        }
        shown_until = group.last().map_or(shown_until, |op| op.old_range().end);
    }
    if !result.rows.is_empty() && shown_until < old_lines {
        result.rows.push(Row::Skipped {
            lines: old_lines - shown_until,
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(diff: &FileDiff) -> Vec<String> {
        diff.rows
            .iter()
            .map(|row| match row {
                Row::Line { kind, text, .. } => {
                    let sign = match kind {
                        LineKind::Context => ' ',
                        LineKind::Added => '+',
                        LineKind::Removed => '-',
                    };
                    format!("{sign}{text}")
                }
                Row::Skipped { lines } => format!("…{lines}"),
                Row::Hunk { header, .. } => header.clone(),
            })
            .collect()
    }

    #[test]
    fn hunks_keep_context_and_mark_skipped_lines() {
        let old: String = (1..=10).map(|n| format!("line {n}\n")).collect();
        let new = old.replace("line 5\n", "line five\n");
        let diff = diff(Some(old.as_bytes()), Some(new.as_bytes()), 1);
        assert_eq!(
            lines(&diff),
            ["…3", " line 4", "-line 5", "+line five", " line 6", "…4"]
        );
        assert_eq!((diff.added, diff.removed), (1, 1));
        let Row::Line { old, new, .. } = &diff.rows[3] else {
            panic!()
        };
        assert_eq!((*old, *new), (None, Some(5)));
    }

    #[test]
    fn side_by_side_faces_replacements_and_pads_uneven_runs() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nB\nB2\nd\ne\n";
        let full = diff(Some(old.as_bytes()), Some(new.as_bytes()), FULL_FILE);
        assert!(
            !full
                .rows
                .iter()
                .any(|row| matches!(row, Row::Skipped { .. }))
        );
        fn text(side: &Option<SideLine>) -> Option<(usize, &str)> {
            side.as_ref().map(|s| (s.line, s.text.as_str()))
        }
        let split = side_by_side(&full.rows);
        let pairs: Vec<_> = split
            .iter()
            .map(|row| match row {
                SplitRow::Pair { left, right } => (text(left), text(right)),
                SplitRow::Skipped { .. } | SplitRow::Hunk { .. } => {
                    panic!("similar diffs have no skips or hunk rows in whole-file mode")
                }
            })
            .collect();
        assert_eq!(
            pairs,
            [
                (Some((1, "a")), Some((1, "a"))),
                (Some((2, "b")), Some((2, "B"))),
                (Some((3, "c")), Some((3, "B2"))),
                (Some((4, "d")), Some((4, "d"))),
                (None, Some((5, "e"))),
            ]
        );
    }

    #[test]
    fn missing_sides_are_empty_and_binary_is_flagged() {
        let added = diff(None, Some(b"a\r\nb\r\n"), 3);
        assert_eq!(lines(&added), ["+a", "+b"]);
        assert!(diff(Some(b"same\n"), Some(b"same\n"), 3).rows.is_empty());
        let binary = diff(Some(b"\0\x01"), Some(b"text"), 3);
        assert!(binary.binary && binary.rows.is_empty());
    }
}
