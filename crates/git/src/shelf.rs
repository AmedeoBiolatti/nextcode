//! PyCharm's shelf: changes of chosen files saved as a patch outside git
//! (`.next_code/shelf/<id>/shelf.patch` plus `info.json`, under the
//! repository root), then rolled back; unshelving applies the patch again
//! (three-way when it no longer applies cleanly, leaving conflicts).
//!
//! The shelf folder holds a `.gitignore` of `*`, so shelves never show up
//! as changes. Git produces and applies the patches; the files themselves
//! are read and written through [`WorktreeFiles`], where the repository
//! lives (locally, in WSL or in a container).

use std::{
    collections::BTreeSet,
    io,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    ApplyOutcome, Change, Entry, FileChange, FileDiff, Repository, diff, failure, parse_patch,
};

/// Where shelves live, relative to the repository root.
pub const SHELF_DIR: &str = ".next_code/shelf";
const PATCH: &str = "shelf.patch";
const INFO: &str = "info.json";

/// Reads and writes working-tree files where the repository lives (git
/// cannot write arbitrary files). Paths are repository-relative and
/// `/`-separated.
pub trait WorktreeFiles {
    fn read(&self, path: &str) -> io::Result<Vec<u8>>;
    /// Writes `data`, creating missing parent directories.
    fn write(&self, path: &str, data: &[u8]) -> io::Result<()>;
}

/// A shelved change list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shelf {
    /// Its folder's name under [`SHELF_DIR`].
    pub id: String,
    pub message: String,
    /// Unix seconds.
    pub date: i64,
    /// The commit the changes were made on.
    pub base: String,
    pub files: Vec<FileChange>,
}

/// Options shared by every patch the shelf writes: binary-safe, full blob
/// ids (for three-way unshelving and viewing), fixed prefixes, renames as
/// delete + add, whatever the user's diff settings.
const PATCH_ARGS: [&str; 9] = [
    "diff",
    "--binary",
    "--full-index",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-renames",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn check_id(id: &str) -> io::Result<()> {
    if id.is_empty() || id.starts_with('.') || id.contains(['/', '\\']) {
        return Err(invalid(format!("invalid shelf id `{id}`")));
    }
    Ok(())
}

/// A file of shelf `id` (checked); a missing one means the shelf is gone.
fn read_shelf_file(files: &dyn WorktreeFiles, id: &str, name: &str) -> io::Result<Vec<u8>> {
    check_id(id)?;
    files
        .read(&format!("{SHELF_DIR}/{id}/{name}"))
        .map_err(|err| {
            if err.kind() == io::ErrorKind::NotFound {
                io::Error::new(
                    err.kind(),
                    format!("no shelf `{id}` (it was unshelved or deleted)"),
                )
            } else {
                err
            }
        })
}

fn shelf_file(id: &str, name: &str) -> String {
    format!("{SHELF_DIR}/{id}/{name}")
}

impl Repository {
    /// Shelves the changes of `paths` (as status lists them; untracked
    /// files included): writes them as a patch against HEAD under
    /// [`SHELF_DIR`], then rolls the files back to HEAD (new files are
    /// deleted).
    pub fn shelve(
        &self,
        files: &dyn WorktreeFiles,
        paths: &[String],
        message: &str,
    ) -> io::Result<Shelf> {
        let message = message.trim();
        if message.is_empty() {
            return Err(invalid("the shelf needs a name"));
        }
        let status = self.status()?;
        if status.unborn {
            return Err(invalid("nothing is committed yet: commit before shelving"));
        }
        let entries: Vec<Entry> = status
            .entries
            .into_iter()
            .filter(|entry| {
                paths.contains(&entry.path)
                    || entry
                        .orig_path
                        .as_ref()
                        .is_some_and(|orig| paths.contains(orig))
            })
            .collect();
        if entries.is_empty() {
            return Err(invalid("the selected files have no changes to shelve"));
        }
        if let Some(entry) = entries
            .iter()
            .find(|entry| entry.unstaged == Some(Change::Conflicted))
        {
            return Err(invalid(format!(
                "{} has merge conflicts: resolve them before shelving",
                entry.path
            )));
        }
        let base = self.resolve("HEAD")?;
        let (untracked, tracked): (Vec<&Entry>, Vec<&Entry>) = entries
            .iter()
            .partition(|entry| entry.unstaged == Some(Change::Untracked));
        let mut patch = Vec::new();
        if !tracked.is_empty() {
            let mut tracked_paths = Vec::new();
            for entry in &tracked {
                tracked_paths.push(entry.path.clone());
                tracked_paths.extend(entry.orig_path.clone());
            }
            let mut args = PATCH_ARGS.to_vec();
            args.push("HEAD");
            patch = self
                .checked(&Self::with_paths(&args, &tracked_paths))?
                .stdout;
        }
        for entry in &untracked {
            let mut args = PATCH_ARGS.to_vec();
            args.extend(["--no-index", "--", "/dev/null", &entry.path]);
            let output = self.git(&args)?;
            // `--no-index` exits with 1 when the files differ.
            if output.code != Some(1) {
                return Err(failure("diff", &output));
            }
            patch.extend_from_slice(&output.stdout);
        }
        if patch.is_empty() {
            return Err(invalid("the selected files have no changes to shelve"));
        }

        let existing = self.shelf_ids()?;
        let date = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs() as i64);
        let id = (1..)
            .map(|n| match n {
                1 => date.to_string(),
                n => format!("{date}-{n}"),
            })
            .find(|id| !existing.contains(id))
            .unwrap_or_default();
        let mut shelved: Vec<FileChange> = entries
            .iter()
            .map(|entry| FileChange {
                path: entry.path.clone(),
                orig_path: entry.orig_path.clone(),
                change: entry.combined_change(),
            })
            .collect();
        shelved.sort_by(|a, b| a.path.cmp(&b.path));
        let shelf = Shelf {
            id: id.clone(),
            message: message.to_owned(),
            date,
            base,
            files: shelved,
        };
        let ignore = format!("{SHELF_DIR}/.gitignore");
        if files.read(&ignore).is_err() {
            files.write(&ignore, b"*\n")?;
        }
        files.write(&shelf_file(&id, PATCH), &patch)?;
        let info = serde_json::to_vec_pretty(&shelf).map_err(io::Error::other)?;
        files.write(&shelf_file(&id, INFO), &info)?;

        self.rollback(&entries)?;
        // Rollback leaves added files on disk, unversioned: they are in
        // the shelf now.
        let added: Vec<String> = entries
            .iter()
            .filter(|entry| entry.staged == Some(Change::Added))
            .map(|entry| entry.path.clone())
            .collect();
        if !added.is_empty() {
            self.checked(&Self::with_paths(&["clean", "-f", "-q"], &added))?;
        }
        Ok(shelf)
    }

    /// Names of the folders under [`SHELF_DIR`] (ignored files included).
    fn shelf_ids(&self) -> io::Result<BTreeSet<String>> {
        Ok(self
            .shelf_contents()?
            .into_iter()
            .filter_map(|(id, _)| id)
            .collect())
    }

    /// `(Some(id), file)` for each file in a shelf folder.
    fn shelf_contents(&self) -> io::Result<Vec<(Option<String>, String)>> {
        let dir = format!("{SHELF_DIR}/");
        let output = self.checked(&["ls-files", "-z", "--cached", "--others", "--", &dir])?;
        Ok(output
            .stdout
            .split(|&byte| byte == 0)
            .filter_map(|path| {
                let path = String::from_utf8_lossy(path);
                let rest = path.strip_prefix(&dir)?;
                Some(match rest.split_once('/') {
                    Some((id, file)) => (Some(id.to_owned()), file.to_owned()),
                    None => (None, rest.to_owned()),
                })
            })
            .collect())
    }

    /// Every shelf, newest first. Folders without a readable `info.json`
    /// are skipped.
    pub fn shelves(&self, files: &dyn WorktreeFiles) -> io::Result<Vec<Shelf>> {
        let mut shelves: Vec<Shelf> = self
            .shelf_contents()?
            .into_iter()
            .filter_map(|(id, file)| if file == INFO { id } else { None })
            .filter_map(|id| self.shelf(files, &id).ok())
            .collect();
        shelves.sort_by(|a, b| b.date.cmp(&a.date).then_with(|| b.id.cmp(&a.id)));
        Ok(shelves)
    }

    /// The shelf `id`.
    pub fn shelf(&self, files: &dyn WorktreeFiles, id: &str) -> io::Result<Shelf> {
        let info = read_shelf_file(files, id, INFO)?;
        let mut shelf: Shelf = serde_json::from_slice(&info)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        shelf.id = id.to_owned();
        Ok(shelf)
    }

    /// Applies the shelf `id` to the working tree; when the patch no
    /// longer applies cleanly, a three-way merge leaves conflicts in the
    /// files it reports (the shelf is then kept). `remove` deletes the
    /// shelf once fully applied.
    pub fn unshelve(
        &self,
        files: &dyn WorktreeFiles,
        id: &str,
        remove: bool,
    ) -> io::Result<ApplyOutcome> {
        self.shelf(files, id)?;
        let patch = read_shelf_file(files, id, PATCH)?;
        let check = self.git_with_input(
            &["apply", "--check", "--whitespace=nowarn", "-"],
            Some(&patch),
        )?;
        if check.code == Some(0) {
            self.checked_with_input(&["apply", "--whitespace=nowarn", "-"], Some(&patch))?;
        } else {
            let output = self.git_with_input(
                &["apply", "--3way", "--whitespace=nowarn", "-"],
                Some(&patch),
            )?;
            if output.code != Some(0) {
                let paths = self.conflicted_paths()?;
                if paths.is_empty() {
                    return Err(failure("apply", &output));
                }
                return Ok(ApplyOutcome::Conflicts { paths });
            }
        }
        if remove {
            self.delete_shelf(id)?;
        }
        Ok(ApplyOutcome::Done { empty: 0 })
    }

    /// Deletes the shelf `id`.
    pub fn delete_shelf(&self, id: &str) -> io::Result<()> {
        check_id(id)?;
        if !self.shelf_ids()?.contains(id) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no shelf `{id}`"),
            ));
        }
        let dir = format!("{SHELF_DIR}/{id}/");
        // `-x`: the folder is ignored (see the module docs).
        self.checked(&["clean", "-f", "-d", "-x", "-q", "--", &dir])
            .map(drop)
    }

    /// The diff of one shelved file, from the version it was shelved
    /// against to the shelved one, with `context` lines.
    pub fn shelved_file_diff(
        &self,
        files: &dyn WorktreeFiles,
        id: &str,
        path: &str,
        context: usize,
    ) -> io::Result<FileDiff> {
        let patch = read_shelf_file(files, id, PATCH)?;
        let section = file_sections(&patch)
            .into_iter()
            .find(|(name, _)| name == path)
            .map(|(_, section)| section)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{path} is not in shelf `{id}`"),
                )
            })?;
        let mut old_id = None;
        let mut deleted = false;
        for line in section.split(|&byte| byte == b'\n') {
            if line.starts_with(b"@@") {
                break;
            }
            if line == b"GIT binary patch" {
                return Ok(FileDiff {
                    binary: true,
                    ..FileDiff::default()
                });
            }
            if line.starts_with(b"deleted file mode") {
                deleted = true;
            }
            if let Some(range) = line.strip_prefix(b"index ") {
                let range = String::from_utf8_lossy(range);
                let old = range.split("..").next().unwrap_or_default();
                if !old.bytes().all(|byte| byte == b'0') {
                    old_id = Some(old.to_owned());
                }
            }
        }
        let old = match &old_id {
            Some(old_id) => match self.blob(old_id)? {
                Some(old) => Some(old),
                // The shelved-on version is gone: show the patch's hunks.
                None => {
                    return Ok(parse_patch(section)
                        .map(|patch| patch.to_file_diff())
                        .unwrap_or_default());
                }
            },
            None => None,
        };
        let new = if deleted {
            None
        } else {
            Some(
                apply_hunks(old.as_deref().unwrap_or_default(), section).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("the shelved patch of {path} does not apply to its base"),
                    )
                })?,
            )
        };
        Ok(diff(old.as_deref(), new.as_deref(), context))
    }
}

/// Splits a multi-file patch into `(path, section)` per file.
fn file_sections(patch: &[u8]) -> Vec<(String, &[u8])> {
    let mut starts: Vec<usize> = Vec::new();
    let mut offset = 0;
    for line in patch.split_inclusive(|&byte| byte == b'\n') {
        if line.starts_with(b"diff --git ") {
            starts.push(offset);
        }
        offset += line.len();
    }
    starts
        .iter()
        .enumerate()
        .filter_map(|(index, &start)| {
            let end = starts.get(index + 1).copied().unwrap_or(patch.len());
            let section = &patch[start..end];
            let header = section.split(|&byte| byte == b'\n').next()?;
            let header = String::from_utf8_lossy(&header["diff --git ".len()..]);
            Some((header_path(header.trim_end_matches('\r'))?, section))
        })
        .collect()
}

/// The path in `a/<path> b/<path>` (the same on both sides without rename
/// detection), C-quoted when git quotes it.
fn header_path(header: &str) -> Option<String> {
    if header.starts_with('"') {
        let (quoted, _) = unquote(header)?;
        return quoted.strip_prefix("a/").map(str::to_owned);
    }
    let length = header.len().checked_sub(5)? / 2;
    let path = header.get(2..2 + length)?;
    (header.get(..2)? == "a/" && header.get(2 + length..)? == format!(" b/{path}"))
        .then(|| path.to_owned())
}

/// Reads a C-quoted string at the start of `text`; returns it and the rest.
fn unquote(text: &str) -> Option<(String, &str)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut index = 1;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                let value = String::from_utf8_lossy(&out).into_owned();
                return Some((value, &text[index + 1..]));
            }
            b'\\' => {
                index += 1;
                let escaped = *bytes.get(index)?;
                match escaped {
                    b'0'..=b'7' => {
                        let digits = text.get(index..index + 3)?;
                        out.push(u8::from_str_radix(digits, 8).ok()?);
                        index += 2;
                    }
                    b'a' => out.push(7),
                    b'b' => out.push(8),
                    b't' => out.push(b'\t'),
                    b'n' => out.push(b'\n'),
                    b'v' => out.push(11),
                    b'f' => out.push(12),
                    b'r' => out.push(b'\r'),
                    other => out.push(other),
                }
            }
            other => out.push(other),
        }
        index += 1;
    }
    None
}

/// `old` with the unified-diff hunks of `section` applied (lines keep
/// their own endings, so CRLF files stay CRLF); `None` if the hunks don't
/// fit `old`.
fn apply_hunks(old: &[u8], section: &[u8]) -> Option<Vec<u8>> {
    let old_lines: Vec<&[u8]> = old.split_inclusive(|&byte| byte == b'\n').collect();
    let mut out = Vec::with_capacity(old.len());
    let mut position = 0;
    let mut in_hunk = false;
    let mut last_added = false;
    for line in section.split_inclusive(|&byte| byte == b'\n') {
        if line.starts_with(b"@@") {
            let header = String::from_utf8_lossy(line);
            let range = header.split(' ').nth(1)?.trim_start_matches('-');
            let mut parts = range.split(',');
            let start: usize = parts.next()?.parse().ok()?;
            let count: usize = parts.next().map_or(Some(1), |count| count.parse().ok())?;
            // `-n,0` inserts after line n; otherwise the hunk starts at n.
            let target = if count == 0 {
                start
            } else {
                start.checked_sub(1)?
            };
            if target < position || target > old_lines.len() {
                return None;
            }
            for line in &old_lines[position..target] {
                out.extend_from_slice(line);
            }
            position = target;
            in_hunk = true;
            last_added = false;
            continue;
        }
        if !in_hunk {
            continue;
        }
        match line.first() {
            Some(b' ') => {
                out.extend_from_slice(old_lines.get(position)?);
                position += 1;
                last_added = false;
            }
            Some(b'-') => {
                old_lines.get(position)?;
                position += 1;
                last_added = false;
            }
            Some(b'+') => {
                out.extend_from_slice(&line[1..]);
                last_added = true;
            }
            // `\ No newline at end of file` after an added line.
            Some(b'\\') => {
                if last_added && out.last() == Some(&b'\n') {
                    out.pop();
                }
            }
            // The next file's header (sections end there anyway).
            _ => in_hunk = false,
        }
    }
    for line in old_lines.get(position..)? {
        out.extend_from_slice(line);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use super::*;
    use crate::tests::{state, temp_repo};

    struct Dir(PathBuf);

    impl WorktreeFiles for Dir {
        fn read(&self, path: &str) -> io::Result<Vec<u8>> {
            fs::read(self.0.join(path))
        }

        fn write(&self, path: &str, data: &[u8]) -> io::Result<()> {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().expect("a parent"))?;
            fs::write(path, data)
        }
    }

    fn commit_all(repo: &Repository, message: &str) {
        repo.stage(&[".".into()]).unwrap();
        repo.commit(message, false).unwrap();
    }

    fn read(dir: &Path, file: &str) -> Vec<u8> {
        fs::read(dir.join(file)).unwrap()
    }

    #[test]
    fn header_paths_plain_spaced_and_quoted() {
        assert_eq!(header_path("a/x.txt b/x.txt").as_deref(), Some("x.txt"));
        assert_eq!(
            header_path("a/sub/a b.txt b/sub/a b.txt").as_deref(),
            Some("sub/a b.txt")
        );
        assert_eq!(
            header_path(r#""a/sp \"q\"\303\251.txt" "b/sp \"q\"\303\251.txt""#).as_deref(),
            Some("sp \"q\"é.txt")
        );
    }

    #[test]
    fn hunks_apply_at_the_ends_and_keep_missing_final_newlines() {
        let old = b"1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        let section = b"diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n-1\n+one\n 2\n@@ -8,2 +8,2 @@\n 8\n-9\n+nine\n\\ No newline at end of file\n";
        assert_eq!(
            apply_hunks(old, section).unwrap(),
            b"one\n2\n3\n4\n5\n6\n7\n8\nnine"
        );
        let insert = b"@@ -0,0 +1 @@\n+zero\n";
        assert_eq!(apply_hunks(b"a\n", insert).unwrap(), b"zero\na\n");
        assert!(apply_hunks(b"a\n", b"@@ -5,1 +5,1 @@\n-x\n+y\n").is_none());
    }

    #[test]
    fn shelving_rolls_back_and_unshelving_restores_every_kind_of_change() {
        let (dir, repo) = temp_repo("shelf_round_trip");
        let files = Dir(dir.clone());
        fs::write(dir.join("keep.txt"), "keep\n").unwrap();
        fs::write(dir.join("mod.txt"), "a\nb\nc\n").unwrap();
        fs::write(dir.join("gone.txt"), "gone\n").unwrap();
        fs::write(dir.join("bin.dat"), [0u8, 1, 2]).unwrap();
        commit_all(&repo, "first");

        fs::write(dir.join("keep.txt"), "keep 2\n").unwrap();
        fs::write(dir.join("mod.txt"), "a\nB\nc\nd").unwrap();
        fs::remove_file(dir.join("gone.txt")).unwrap();
        fs::write(dir.join("new file.txt"), "new\n").unwrap();
        fs::write(dir.join("staged.txt"), "staged\n").unwrap();
        repo.add(&["staged.txt".into()]).unwrap();
        fs::write(dir.join("bin.dat"), [0u8, 9, 9, 9]).unwrap();
        let shelved: Vec<String> = [
            "mod.txt",
            "gone.txt",
            "new file.txt",
            "staged.txt",
            "bin.dat",
        ]
        .map(String::from)
        .to_vec();

        assert!(repo.shelve(&files, &shelved, " ").is_err());
        let shelf = repo.shelve(&files, &shelved, "My shelf").unwrap();
        assert_eq!(shelf.message, "My shelf");
        assert_eq!(shelf.base, repo.resolve("HEAD").unwrap());
        // Only the unselected file is left; the shelf folder is ignored.
        assert_eq!(
            state(&repo),
            [("keep.txt".to_owned(), None, Some(Change::Modified))]
        );
        assert_eq!(read(&dir, "mod.txt"), b"a\nb\nc\n");
        assert_eq!(read(&dir, "gone.txt"), b"gone\n");
        assert!(!dir.join("new file.txt").exists());
        assert!(!dir.join("staged.txt").exists());

        let list = repo.shelves(&files).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0], shelf);
        let changes: Vec<(&str, Change)> = shelf
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.change))
            .collect();
        assert_eq!(
            changes,
            [
                ("bin.dat", Change::Modified),
                ("gone.txt", Change::Deleted),
                ("mod.txt", Change::Modified),
                ("new file.txt", Change::Untracked),
                ("staged.txt", Change::Added),
            ]
        );

        // Viewing shows the same diff as the working tree did.
        let view = repo
            .shelved_file_diff(&files, &shelf.id, "mod.txt", 3)
            .unwrap();
        assert_eq!(view, diff(Some(b"a\nb\nc\n"), Some(b"a\nB\nc\nd"), 3));
        let view = repo
            .shelved_file_diff(&files, &shelf.id, "new file.txt", 3)
            .unwrap();
        assert_eq!(view, diff(None, Some(b"new\n"), 3));
        let view = repo
            .shelved_file_diff(&files, &shelf.id, "gone.txt", 3)
            .unwrap();
        assert_eq!(view, diff(Some(b"gone\n"), None, 3));
        assert!(
            repo.shelved_file_diff(&files, &shelf.id, "bin.dat", 3)
                .unwrap()
                .binary
        );
        assert!(
            repo.shelved_file_diff(&files, &shelf.id, "keep.txt", 3)
                .is_err()
        );

        // Keep: applied, still listed.
        assert_eq!(
            repo.unshelve(&files, &shelf.id, false).unwrap(),
            ApplyOutcome::Done { empty: 0 }
        );
        assert_eq!(read(&dir, "mod.txt"), b"a\nB\nc\nd");
        assert!(!dir.join("gone.txt").exists());
        assert_eq!(read(&dir, "new file.txt"), b"new\n");
        assert_eq!(read(&dir, "staged.txt"), b"staged\n");
        assert_eq!(read(&dir, "bin.dat"), [0u8, 9, 9, 9]);
        assert_eq!(repo.shelves(&files).unwrap().len(), 1);

        // Again with remove, after rolling back: applied and gone.
        repo.rollback(&repo.status().unwrap().entries).unwrap();
        repo.unshelve(&files, &shelf.id, true).unwrap();
        assert_eq!(read(&dir, "mod.txt"), b"a\nB\nc\nd");
        assert!(repo.shelves(&files).unwrap().is_empty());
        assert!(!dir.join(SHELF_DIR).join(&shelf.id).exists());
        assert!(repo.delete_shelf(&shelf.id).is_err());
        assert!(repo.unshelve(&files, "../x", false).is_err());
    }

    #[test]
    fn unshelving_over_conflicting_changes_reports_them_and_keeps_the_shelf() {
        let (dir, repo) = temp_repo("shelf_conflict");
        let files = Dir(dir.clone());
        fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        commit_all(&repo, "first");
        fs::write(dir.join("a.txt"), "one\nshelved\nthree\n").unwrap();
        let shelf = repo
            .shelve(&files, &["a.txt".into()], "conflicting")
            .unwrap();
        fs::write(dir.join("a.txt"), "one\ncommitted\nthree\n").unwrap();
        commit_all(&repo, "second");

        assert_eq!(
            repo.unshelve(&files, &shelf.id, true).unwrap(),
            ApplyOutcome::Conflicts {
                paths: vec!["a.txt".into()]
            }
        );
        assert_eq!(
            state(&repo),
            [("a.txt".to_owned(), None, Some(Change::Conflicted))]
        );
        let text = fs::read_to_string(dir.join("a.txt")).unwrap();
        assert!(
            text.contains("shelved") && text.contains("committed"),
            "{text}"
        );
        assert_eq!(repo.shelves(&files).unwrap().len(), 1);
        repo.delete_shelf(&shelf.id).unwrap();
        assert!(repo.shelves(&files).unwrap().is_empty());
    }

    #[test]
    fn crlf_files_round_trip_byte_for_byte() {
        let (dir, repo) = temp_repo("shelf_crlf");
        let files = Dir(dir.clone());
        let original = b"one\r\ntwo\r\nthree\r\n";
        let changed = b"one\r\nTWO\r\nthree\r\nfour\r\n";
        fs::write(dir.join("w.txt"), original).unwrap();
        commit_all(&repo, "first");
        fs::write(dir.join("w.txt"), changed).unwrap();
        fs::write(dir.join("u.txt"), b"x\r\ny\r\n").unwrap();
        let shelf = repo
            .shelve(&files, &["w.txt".into(), "u.txt".into()], "crlf")
            .unwrap();
        assert_eq!(read(&dir, "w.txt"), original);
        let view = repo
            .shelved_file_diff(&files, &shelf.id, "w.txt", 3)
            .unwrap();
        assert_eq!(view, diff(Some(original), Some(changed), 3));
        repo.unshelve(&files, &shelf.id, true).unwrap();
        assert_eq!(read(&dir, "w.txt"), changed);
        assert_eq!(read(&dir, "u.txt"), b"x\r\ny\r\n");
    }

    #[test]
    fn two_shelves_in_the_same_second_get_distinct_ids() {
        let (dir, repo) = temp_repo("shelf_ids");
        let files = Dir(dir.clone());
        fs::write(dir.join("a.txt"), "a\n").unwrap();
        commit_all(&repo, "first");
        fs::write(dir.join("a.txt"), "a1\n").unwrap();
        let first = repo.shelve(&files, &["a.txt".into()], "one").unwrap();
        fs::write(dir.join("a.txt"), "a2\n").unwrap();
        let second = repo.shelve(&files, &["a.txt".into()], "two").unwrap();
        assert_ne!(first.id, second.id);
        let names: Vec<String> = repo
            .shelves(&files)
            .unwrap()
            .into_iter()
            .map(|shelf| shelf.message)
            .collect();
        assert_eq!(names.len(), 2);
        assert!(repo.shelve(&files, &["a.txt".into()], "none").is_err());
    }
}
