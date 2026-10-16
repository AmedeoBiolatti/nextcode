//! Changed line ranges of a file's current text against a base version
//! (HEAD or the index): the editor's gutter markers, Rollback and Stage
//! Hunk, and blame lines following edits.
//!
//! Texts are split into lines at `\n`, so a trailing line break starts an
//! extra empty last line (as the editor counts lines). Lines are compared
//! without a trailing `\r`: a CRLF checkout of an LF blob has no changes.

use std::{io, ops::Range, time::Instant};

use serde::Serialize;
use similar::{Algorithm, DiffOp, capture_diff_slices_deadline};

use crate::Repository;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LineChangeKind {
    /// Lines only in the current text.
    Added,
    /// Lines that replace base lines.
    Modified,
    /// Base lines removed; they sat before current line `new.start`.
    Deleted,
}

/// One run of changed lines (0-based line ranges, end exclusive).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LineChange {
    pub kind: LineChangeKind,
    /// Lines of the current text (empty for deletions).
    pub new: Range<usize>,
    /// Lines of the base text (empty for additions).
    pub old: Range<usize>,
}

impl LineChange {
    /// Whether the change touches current line `line`; a deletion touches
    /// the line after it (and the last line, at the end of the text).
    pub fn touches(&self, line: usize, line_count: usize) -> bool {
        if self.new.is_empty() {
            line == self.new.start || (self.new.start >= line_count && line + 1 == line_count)
        } else {
            self.new.contains(&line)
        }
    }

    /// Whether the change touches any of `lines`.
    pub fn touches_range(&self, lines: &Range<usize>, line_count: usize) -> bool {
        if self.new.is_empty() {
            lines.clone().any(|line| self.touches(line, line_count))
        } else {
            self.new.start < lines.end && lines.start < self.new.end
        }
    }
}

/// The lines of `text` as compared (no trailing `\r`).
fn compared(text: &str) -> Vec<&str> {
    text.split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

/// The runs of lines where `current` differs from `base`, in order. With a
/// `deadline` the diff may stop refining early (the result is still a
/// correct, possibly coarser, set of changes).
pub fn line_changes(base: &str, current: &str, deadline: Option<Instant>) -> Vec<LineChange> {
    let old = compared(base);
    let new = compared(current);
    let ops = capture_diff_slices_deadline(Algorithm::Myers, &old, &new, deadline);
    let mut changes: Vec<LineChange> = Vec::new();
    let mut pending: Option<(Range<usize>, Range<usize>)> = None;
    let mut flush = |pending: &mut Option<(Range<usize>, Range<usize>)>| {
        if let Some((old, new)) = pending.take() {
            let kind = if old.is_empty() {
                LineChangeKind::Added
            } else if new.is_empty() {
                LineChangeKind::Deleted
            } else {
                LineChangeKind::Modified
            };
            changes.push(LineChange { kind, new, old });
        }
    };
    for op in ops {
        if let DiffOp::Equal { .. } = op {
            flush(&mut pending);
            continue;
        }
        let (old_range, new_range) = (op.old_range(), op.new_range());
        pending = Some(match pending.take() {
            Some((old, new)) => (old.start..old_range.end, new.start..new_range.end),
            None => (old_range, new_range),
        });
    }
    flush(&mut pending);
    changes
}

/// All lines of `current` as one addition (a file new to the base).
pub fn all_added(current: &str) -> Vec<LineChange> {
    let lines = current.split('\n').count();
    vec![LineChange {
        kind: LineChangeKind::Added,
        new: 0..lines,
        old: 0..0,
    }]
}

/// `into` with its lines `into_lines` replaced by lines `from_lines` of
/// `from`. Inserted lines take `into`'s line ending (CRLF if it has any),
/// and the last line never ends with a stray `\r`.
pub fn replace_lines(
    into: &str,
    into_lines: Range<usize>,
    from: &str,
    from_lines: Range<usize>,
) -> String {
    let crlf = into.contains("\r\n") || (!into.contains('\n') && from.contains("\r\n"));
    let target: Vec<&str> = into.split('\n').collect();
    let source: Vec<&str> = from.split('\n').collect();
    let start = into_lines.start.min(target.len());
    let end = into_lines.end.clamp(start, target.len());
    let inserted = &source[from_lines.start.min(source.len())..from_lines.end.min(source.len())];
    let mut lines: Vec<String> = Vec::with_capacity(target.len() + inserted.len());
    lines.extend(target[..start].iter().map(|line| (*line).to_owned()));
    lines.extend(
        inserted
            .iter()
            .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned()),
    );
    lines.extend(target[end..].iter().map(|line| (*line).to_owned()));
    if lines.is_empty() {
        return String::new();
    }
    // Line endings around the edit: the line before it, the inserted
    // lines, and the (possibly new) last line.
    let last = lines.len() - 1;
    let touched = start.saturating_sub(1)..(start + inserted.len()).min(lines.len());
    for index in touched.chain([last]) {
        let line = &mut lines[index];
        let has_cr = line.ends_with('\r');
        if index == last {
            if has_cr {
                line.pop();
            }
        } else if crlf && !has_cr {
            line.push('\r');
        }
    }
    lines.join("\n")
}

/// `current` with `change` rolled back to the base's lines.
pub fn revert_change(current: &str, base: &str, change: &LineChange) -> String {
    replace_lines(current, change.new.clone(), base, change.old.clone())
}

/// `base` with `change` applied: what the index holds after staging only
/// that change.
pub fn apply_change(base: &str, current: &str, change: &LineChange) -> String {
    replace_lines(base, change.old.clone(), current, change.new.clone())
}

/// The smallest single edit turning `old` into `new`: the byte range of
/// `old` to replace and the text to put there (both on char boundaries).
pub fn minimal_edit(old: &str, new: &str) -> (Range<usize>, String) {
    let mut prefix = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(prefix) || !new.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let max_suffix = old.len().min(new.len()) - prefix;
    let mut suffix = old
        .bytes()
        .rev()
        .zip(new.bytes().rev())
        .take(max_suffix)
        .take_while(|(a, b)| a == b)
        .count();
    while !old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix) {
        suffix -= 1;
    }
    (
        prefix..old.len() - suffix,
        new[prefix..new.len() - suffix].to_owned(),
    )
}

/// Where an open file's base version lives, as found by
/// [`Repository::base_objects`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "object")]
pub enum BaseObject {
    /// The base is this blob.
    Blob(String),
    /// Not in HEAD but staged: every line counts as added.
    NewFile,
    /// Untracked, ignored, or outside the repository: no markers.
    None,
}

impl Repository {
    /// The base blob of each of `paths` (repository-relative): the index
    /// entry with `index`, else HEAD's (a file only in the index is
    /// [`BaseObject::NewFile`]). One or two git runs for all paths.
    pub fn base_objects(&self, paths: &[String], index: bool) -> io::Result<Vec<BaseObject>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let staged = || -> io::Result<Vec<(String, String)>> {
            let output = self.checked(&Self::with_paths(&["ls-files", "-s", "-z"], paths))?;
            // `<mode> <object> <stage>\t<path>`; conflicted files have
            // stages 1-3 only and get no base.
            Ok(records(&output.stdout)
                .filter_map(|(meta, path)| {
                    let mut fields = meta.split(' ');
                    let (_, object, stage) = (fields.next()?, fields.next()?, fields.next()?);
                    (stage == "0").then(|| (path.to_owned(), object.to_owned()))
                })
                .collect())
        };
        let find = |entries: &[(String, String)], path: &str| {
            entries
                .iter()
                .find(|(entry, _)| entry == path)
                .map(|(_, object)| object.clone())
        };
        if index {
            let entries = staged()?;
            return Ok(paths
                .iter()
                .map(|path| find(&entries, path).map_or(BaseObject::None, BaseObject::Blob))
                .collect());
        }
        let mut args = vec!["ls-tree", "-z", "HEAD", "--"];
        args.extend(paths.iter().map(String::as_str));
        let output = self.git(&args)?;
        // An unborn HEAD fails: nothing is committed yet.
        let committed: Vec<(String, String)> = if output.code == Some(0) {
            records(&output.stdout)
                .filter_map(|(meta, path)| {
                    // `<mode> blob <object>\t<path>`
                    let mut fields = meta.split(' ');
                    let (_, kind, object) = (fields.next()?, fields.next()?, fields.next()?);
                    (kind == "blob").then(|| (path.to_owned(), object.to_owned()))
                })
                .collect()
        } else {
            Vec::new()
        };
        let missing = paths.iter().any(|path| find(&committed, path).is_none());
        let in_index = if missing { staged()? } else { Vec::new() };
        Ok(paths
            .iter()
            .map(|path| match find(&committed, path) {
                Some(object) => BaseObject::Blob(object),
                None if find(&in_index, path).is_some() => BaseObject::NewFile,
                None => BaseObject::None,
            })
            .collect())
    }

    /// Replaces `path`'s index entry with `contents` (keeping its mode),
    /// without touching the working tree: Stage Hunk from the editor.
    pub fn stage_contents(&self, path: &str, contents: &[u8]) -> io::Result<()> {
        let listed = self.checked(&["ls-files", "-s", "-z", "--", path])?;
        let mode = records(&listed.stdout)
            .find(|(_, listed)| *listed == path)
            .and_then(|(meta, _)| meta.split(' ').next().map(str::to_owned))
            .unwrap_or_else(|| "100644".to_owned());
        // The contents are already in the index's form: no clean filters.
        let hashed = self.checked_with_input(
            &["hash-object", "-w", "--no-filters", "--stdin"],
            Some(contents),
        )?;
        let object = String::from_utf8_lossy(&hashed.stdout).trim().to_owned();
        let info = format!("{mode},{object},{path}");
        self.checked(&["update-index", "--add", "--cacheinfo", &info])
            .map(drop)
    }
}

/// `-z` records of `<meta>\t<path>`.
fn records(output: &[u8]) -> impl Iterator<Item = (&str, &str)> {
    output
        .split(|&byte| byte == 0)
        .filter_map(|record| std::str::from_utf8(record).ok())
        .filter_map(|record| record.split_once('\t'))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{Duration, Instant},
    };

    use super::*;
    use crate::tests::temp_repo;
    use LineChangeKind::*;

    fn change(kind: LineChangeKind, new: Range<usize>, old: Range<usize>) -> LineChange {
        LineChange { kind, new, old }
    }

    #[test]
    fn additions_modifications_and_deletions_are_separate_runs() {
        let base = "a\nb\nc\nd\ne\n";
        let current = "a\nB\nc\nnew\nd\n";
        assert_eq!(
            line_changes(base, current, None),
            [
                change(Modified, 1..2, 1..2),
                change(Added, 3..4, 3..3),
                change(Deleted, 5..5, 4..5),
            ]
        );
        assert!(line_changes(base, base, None).is_empty());
    }

    #[test]
    fn changes_at_the_start_and_end_of_the_file() {
        // Deleted first line: before current line 0.
        assert_eq!(
            line_changes("x\na\nb", "a\nb", None),
            [change(Deleted, 0..0, 0..1)]
        );
        // Added first line.
        assert_eq!(
            line_changes("a\nb", "x\na\nb", None),
            [change(Added, 0..1, 0..0)]
        );
        // Deleted last line (no trailing newline): past the last line.
        let deleted = line_changes("a\nb\nc", "a\nb", None);
        assert_eq!(deleted, [change(Deleted, 2..2, 2..3)]);
        assert!(deleted[0].touches(1, 2));
        assert!(!deleted[0].touches(0, 2));
        // Modified last line.
        assert_eq!(
            line_changes("a\nb", "a\nB", None),
            [change(Modified, 1..2, 1..2)]
        );
    }

    #[test]
    fn crlf_and_lf_versions_of_the_same_lines_have_no_changes() {
        assert!(line_changes("a\nb\n", "a\r\nb\r\n", None).is_empty());
        assert_eq!(
            line_changes("a\nb\n", "a\r\nB\r\n", None),
            [change(Modified, 1..2, 1..2)]
        );
    }

    #[test]
    fn a_missing_or_added_final_newline_is_a_change_of_the_last_line() {
        // Adding the final line break starts an empty last line.
        assert_eq!(
            line_changes("a\nb", "a\nb\n", None),
            [change(Added, 2..3, 2..2)]
        );
        assert_eq!(
            line_changes("a\nb\n", "a\nb", None),
            [change(Deleted, 2..2, 2..3)]
        );
    }

    #[test]
    fn a_large_file_with_a_deadline_still_reports_its_changes() {
        let base: String = (0..200_000).map(|i| format!("line {i}\n")).collect();
        let current = base.replacen("line 100000\n", "changed\n", 1) + "tail\n";
        let started = Instant::now();
        let changes = line_changes(
            &base,
            &current,
            Some(Instant::now() + Duration::from_millis(200)),
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            changes,
            [
                change(Modified, 100_000..100_001, 100_000..100_001),
                change(Added, 200_000..200_001, 200_000..200_000),
            ]
        );
    }

    #[test]
    fn reverting_each_change_restores_the_base() {
        let cases = [
            ("a\nb\nc\nd\ne\n", "a\nB\nc\nnew\nd\n"),
            ("x\na\nb", "a\nb"),
            ("a\nb", "x\ny\na\nb"),
            ("a\nb\nc", "a\nb"),
            ("a", "a\nb"),
            ("a\nb", "a\nb\n"),
            ("a\nb\n", "a\nb"),
            ("a\r\nb\r\nc", "a\r\nB\r\nc\r\nd"),
            ("one", "two"),
            ("", "added\nlines\n"),
        ];
        for (base, current) in cases {
            // Rolling back the changes from last to first keeps the earlier
            // ranges valid.
            let mut text = current.to_owned();
            for change in line_changes(base, current, None).iter().rev() {
                text = revert_change(&text, base, change);
            }
            assert_eq!(text, base, "reverting {current:?} to {base:?}");
        }
    }

    #[test]
    fn applying_each_change_to_the_base_gives_the_current_text() {
        let cases = [
            ("a\nb\nc\nd\ne\n", "a\nB\nc\nnew\nd\n"),
            ("a\nb\nc", "a\nb"),
            ("a", "a\nb"),
            ("a\nb", "a\nb\n"),
            ("a\r\nb\r\nc", "a\r\nB\r\nc\r\nd"),
        ];
        for (base, current) in cases {
            let mut text = base.to_owned();
            for change in line_changes(base, current, None).iter().rev() {
                text = apply_change(&text, current, change);
            }
            assert_eq!(text, current, "applying {base:?} -> {current:?}");
        }
        // Lines staged into a CRLF blob from an LF buffer get CRLF.
        let base = "a\r\nb\r\n";
        let current = "a\nX\nb\n";
        let changes = line_changes(base, current, None);
        assert_eq!(apply_change(base, current, &changes[0]), "a\r\nX\r\nb\r\n");
    }

    #[test]
    fn minimal_edits_cover_only_the_difference() {
        assert_eq!(minimal_edit("abc", "abc"), (3..3, String::new()));
        assert_eq!(minimal_edit("a\nb\nc", "a\nX\nc"), (2..3, "X".into()));
        assert_eq!(minimal_edit("aXa", "aa"), (1..2, String::new()));
        // Never splits a character.
        assert_eq!(minimal_edit("é", "è"), (0..2, "è".into()));
    }

    #[test]
    fn base_objects_follow_head_or_the_index() {
        let (dir, repo) = temp_repo("base_objects");
        fs::write(dir.join("a.txt"), "one\n").unwrap();
        repo.stage(&["a.txt".into()]).unwrap();
        let paths = [
            "a.txt".to_owned(),
            "new.txt".to_owned(),
            "free.txt".to_owned(),
        ];
        // Unborn HEAD: a staged file is new.
        assert_eq!(
            repo.base_objects(&paths, false).unwrap(),
            [BaseObject::NewFile, BaseObject::None, BaseObject::None]
        );
        repo.commit("first", false).unwrap();
        fs::write(dir.join("a.txt"), "two\n").unwrap();
        fs::write(dir.join("new.txt"), "new\n").unwrap();
        fs::write(dir.join("free.txt"), "untracked\n").unwrap();
        repo.stage(&["a.txt".into(), "new.txt".into()]).unwrap();
        let head = repo.base_objects(&paths, false).unwrap();
        let BaseObject::Blob(committed) = &head[0] else {
            panic!("{head:?}");
        };
        assert_eq!(repo.blob(committed).unwrap().unwrap(), b"one\n");
        assert_eq!(head[1..], [BaseObject::NewFile, BaseObject::None]);
        let index = repo.base_objects(&paths, true).unwrap();
        let BaseObject::Blob(staged) = &index[0] else {
            panic!("{index:?}");
        };
        assert_eq!(repo.blob(staged).unwrap().unwrap(), b"two\n");
        assert!(matches!(index[1], BaseObject::Blob(_)));
        assert_eq!(index[2], BaseObject::None);
    }

    #[test]
    fn staging_contents_replaces_only_the_index_entry() {
        let (dir, repo) = temp_repo("stage_contents");
        fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        repo.stage(&["a.txt".into()]).unwrap();
        repo.commit("first", false).unwrap();
        let current = "one\nTWO\nthree\nfour\n";
        fs::write(dir.join("a.txt"), current).unwrap();
        let base = "one\ntwo\nthree\n";
        let changes = line_changes(base, current, None);
        assert_eq!(changes.len(), 2);
        // Stage only the second change (the added line).
        repo.stage_contents("a.txt", apply_change(base, current, &changes[1]).as_bytes())
            .unwrap();
        assert_eq!(
            repo.blob(":a.txt").unwrap().unwrap(),
            b"one\ntwo\nthree\nfour\n"
        );
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), current);
    }
}
