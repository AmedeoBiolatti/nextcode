//! Diffs as git itself produces them (`git diff`), parsed into hunks so
//! single hunks can be staged, unstaged or discarded with `git apply`.

use std::io;

use serde::Serialize;

use crate::{Entry, FileDiff, LineKind, Repository, Row};

/// One `@@` hunk with its raw text (header line included).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub header: String,
    pub old_start: usize,
    pub new_start: usize,
    text: Vec<u8>,
}

/// A single-file patch from `git diff`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    /// Everything before the first hunk (`diff --git`, `---`, `+++`, …).
    header: Vec<u8>,
    pub hunks: Vec<Hunk>,
}

/// What to do with one hunk of an index diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HunkAction {
    /// Add an unstaged hunk to the index.
    Stage,
    /// Take a staged hunk out of the index (the worktree keeps it).
    Unstage,
    /// Revert an unstaged hunk in the worktree.
    Discard,
}

fn parse_range(range: &str) -> Option<usize> {
    range.split(',').next()?.parse().ok()
}

/// Splits `git diff` output for one file into header and hunks; `None`
/// when there is no textual change (no output, or a binary file).
pub fn parse_patch(output: &[u8]) -> Option<Patch> {
    let mut lines = output.split_inclusive(|&b| b == b'\n').peekable();
    let mut header = Vec::new();
    while let Some(line) = lines.peek() {
        if line.starts_with(b"@@") {
            break;
        }
        header.extend_from_slice(line);
        lines.next();
    }
    let mut hunks: Vec<Hunk> = Vec::new();
    for line in lines {
        if line.starts_with(b"@@") {
            let text = String::from_utf8_lossy(line);
            let mut parts = text.split(' ');
            let _ = parts.next();
            let old_start = parts
                .next()
                .and_then(|r| parse_range(r.trim_start_matches('-')))?;
            let new_start = parts
                .next()
                .and_then(|r| parse_range(r.trim_start_matches('+')))?;
            hunks.push(Hunk {
                header: text.trim_end().to_owned(),
                old_start,
                new_start,
                text: line.to_vec(),
            });
        } else if let Some(hunk) = hunks.last_mut() {
            hunk.text.extend_from_slice(line);
        }
    }
    (!hunks.is_empty()).then_some(Patch { header, hunks })
}

impl Patch {
    /// Rows to display: a `Hunk` row before each hunk, `Skipped` for the
    /// unchanged lines between hunks.
    pub fn to_file_diff(&self) -> FileDiff {
        let mut diff = FileDiff::default();
        let mut old_end = 0;
        for (index, hunk) in self.hunks.iter().enumerate() {
            // `-0,0` (a new file) starts before line 1.
            let (mut old, mut new) = (hunk.old_start.max(1), hunk.new_start.max(1));
            let gap = hunk.old_start.saturating_sub(old_end + 1);
            if gap > 0 {
                diff.rows.push(Row::Skipped { lines: gap });
            }
            diff.rows.push(Row::Hunk {
                index,
                header: hunk.header.clone(),
            });
            for line in hunk.text.split_inclusive(|&b| b == b'\n').skip(1) {
                let (sign, rest) = line.split_first().unwrap_or((&b' ', &[]));
                let text = String::from_utf8_lossy(rest)
                    .trim_end_matches('\n')
                    .trim_end_matches('\r')
                    .to_owned();
                match sign {
                    b' ' => {
                        diff.rows.push(Row::Line {
                            kind: LineKind::Context,
                            old: Some(old),
                            new: Some(new),
                            text,
                        });
                        old += 1;
                        new += 1;
                    }
                    b'-' => {
                        diff.rows.push(Row::Line {
                            kind: LineKind::Removed,
                            old: Some(old),
                            new: None,
                            text,
                        });
                        diff.removed += 1;
                        old += 1;
                    }
                    b'+' => {
                        diff.rows.push(Row::Line {
                            kind: LineKind::Added,
                            old: None,
                            new: Some(new),
                            text,
                        });
                        diff.added += 1;
                        new += 1;
                    }
                    // `\ No newline at end of file`.
                    _ => {}
                }
            }
            old_end = old.saturating_sub(1);
        }
        diff
    }

    /// The patch reduced to one hunk.
    fn single(&self, hunk: usize) -> Option<Vec<u8>> {
        let hunk = self.hunks.get(hunk)?;
        let mut patch = self.header.clone();
        patch.extend_from_slice(&hunk.text);
        Some(patch)
    }
}

impl Repository {
    /// `git diff` of `entry`: staged (HEAD → index) or unstaged (index →
    /// worktree) with `context` lines. `None` without textual changes
    /// (e.g. untracked or binary files).
    pub fn index_patch(
        &self,
        entry: &Entry,
        staged: bool,
        context: usize,
    ) -> io::Result<Option<Patch>> {
        let context = format!("-U{}", context.min(i32::MAX as usize));
        let mut args = vec![
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            &context,
        ];
        if staged {
            args.extend(["--cached", "-M"]);
        }
        args.push("--");
        if staged && let Some(orig) = &entry.orig_path {
            args.push(orig);
        }
        args.push(&entry.path);
        Ok(parse_patch(&self.checked(&args)?.stdout))
    }

    /// Applies one hunk of a patch from [`Repository::index_patch`]: stage
    /// and discard take hunks of the unstaged patch, unstage of the staged one.
    pub fn apply_hunk(&self, patch: &Patch, hunk: usize, action: HunkAction) -> io::Result<()> {
        let text = patch.single(hunk).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("no hunk {hunk}"))
        })?;
        let args: &[&str] = match action {
            HunkAction::Stage => &["apply", "--cached", "--whitespace=nowarn", "-"],
            HunkAction::Unstage => &["apply", "--cached", "-R", "--whitespace=nowarn", "-"],
            HunkAction::Discard => &["apply", "-R", "--whitespace=nowarn", "-"],
        };
        self.checked_with_input(args, Some(&text)).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "diff --git a/f.txt b/f.txt\nindex 1..2 100644\n--- a/f.txt\n+++ b/f.txt\n\
                         @@ -2,3 +2,3 @@ fn x\n b\n-c\n+C\n d\n\
                         @@ -10,2 +10,3 @@\n j\n k\n+l\n\\ No newline at end of file\n";

    #[test]
    fn hunks_keep_their_raw_text_and_become_rows() {
        let patch = parse_patch(PATCH.as_bytes()).unwrap();
        assert_eq!(patch.hunks.len(), 2);
        assert_eq!(
            (patch.hunks[1].old_start, patch.hunks[1].new_start),
            (10, 10)
        );
        let single = String::from_utf8(patch.single(1).unwrap()).unwrap();
        assert!(single.starts_with("diff --git a/f.txt b/f.txt\n"));
        assert!(single.ends_with("+l\n\\ No newline at end of file\n"));
        assert!(!single.contains("-c\n"), "only the chosen hunk");

        let diff = patch.to_file_diff();
        assert_eq!((diff.added, diff.removed), (2, 1));
        assert_eq!(diff.rows[0], Row::Skipped { lines: 1 });
        assert!(matches!(diff.rows[1], Row::Hunk { index: 0, .. }));
        // Between hunks: lines 5..=9 of the old file are not shown.
        assert!(diff.rows.contains(&Row::Skipped { lines: 5 }));
        assert!(parse_patch(b"").is_none());
        assert!(parse_patch(b"Binary files a/x and b/x differ\n").is_none());
    }
}
