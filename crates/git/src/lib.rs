//! Git through its command line: status, staging (whole files or hunks),
//! commits, branches, remotes, worktrees, history, comparisons between
//! revisions, and file diffs.
//!
//! Commands go through a [`Runner`], so the same code drives `git` on this
//! machine or inside WSL behind the agent.

mod diff;
mod history;
mod operations;
mod patch;
mod status;

use std::{io, sync::Arc};

use serde::Serialize;

pub use diff::{FULL_FILE, FileDiff, LineKind, Row, SideLine, SplitRow, diff, side_by_side};
pub use history::{Commit, GraphRow, graph};
pub use operations::Worktree;
pub use patch::{Hunk, HunkAction, Patch, parse_patch};
pub use status::{Change, Entry, Status};

/// A file that differs between two revisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileChange {
    /// Repository-relative, `/`-separated; the new path for renames.
    pub path: String,
    pub orig_path: Option<String>,
    pub change: Change,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefKind {
    Branch,
    Remote,
    Tag,
}

/// A branch or tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ref {
    /// Short name (`main`, `origin/main`, `v1.0`).
    pub name: String,
    pub kind: RefKind,
    /// Abbreviated commit id.
    pub target: String,
    /// The checked-out branch.
    pub head: bool,
}

/// One side of a comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The file doesn't exist on this side (before a root commit).
    Empty,
    /// A commit, branch, tag or other revision.
    Revision(String),
    /// The working tree.
    Worktree,
}

/// What a finished `git` invocation produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    /// `None` when killed by a signal.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Runs `git` with arguments (and optional stdin) where the repository
/// lives. Implementations must not let git prompt (set
/// `GIT_TERMINAL_PROMPT=0`); an `io::Error` means git could not be run at
/// all (`NotFound`: not installed).
pub trait Runner: Send + Sync {
    fn run_git(&self, args: &[String], input: Option<&[u8]>) -> io::Result<Output>;
}

/// Context lines around each change in diffs.
pub const DIFF_CONTEXT: usize = 3;

/// A repository, addressed by its top-level directory.
#[derive(Clone)]
pub struct Repository {
    runner: Arc<dyn Runner>,
    root: String,
    prefix: String,
}

impl Repository {
    /// The repository containing `dir`, or `None` outside any repository.
    pub fn discover(runner: Arc<dyn Runner>, dir: &str) -> io::Result<Option<Self>> {
        let args = ["-C", dir, "rev-parse", "--show-toplevel", "--show-prefix"].map(String::from);
        let output = runner.run_git(&args, None).map_err(|err| {
            if err.kind() == io::ErrorKind::NotFound {
                io::Error::new(
                    err.kind(),
                    format!("git is not installed or not on PATH ({err})"),
                )
            } else {
                err
            }
        })?;
        if output.code != Some(0) {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return if stderr.contains("not a git repository") {
                Ok(None)
            } else {
                Err(failure("rev-parse", &output))
            };
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut lines = stdout.lines();
        let root = lines.next().unwrap_or_default().trim().to_owned();
        let prefix = lines.next().unwrap_or_default().trim().to_owned();
        Ok(Some(Self {
            runner,
            root,
            prefix,
        }))
    }

    /// Top-level directory as git prints it (`/`-separated, also on Windows).
    pub fn root(&self) -> &str {
        &self.root
    }

    /// Where the discovered directory sits in the repository: empty at the
    /// top level, else a root-relative path ending in `/`.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    fn git(&self, args: &[&str]) -> io::Result<Output> {
        self.git_with_input(args, None)
    }

    fn git_with_input(&self, args: &[&str], input: Option<&[u8]>) -> io::Result<Output> {
        let mut full: Vec<String> = [
            "-C",
            &self.root,
            "--no-optional-locks",
            "-c",
            "core.quotepath=false",
        ]
        .map(String::from)
        .to_vec();
        full.extend(args.iter().map(|&arg| arg.to_owned()));
        self.runner.run_git(&full, input)
    }

    /// Runs a command that must succeed.
    fn checked(&self, args: &[&str]) -> io::Result<Output> {
        self.checked_with_input(args, None)
    }

    fn checked_with_input(&self, args: &[&str], input: Option<&[u8]>) -> io::Result<Output> {
        let output = self.git_with_input(args, input)?;
        if output.code == Some(0) {
            Ok(output)
        } else {
            Err(failure(args[0], &output))
        }
    }

    fn with_paths<'a>(command: &[&'a str], paths: &'a [String]) -> Vec<&'a str> {
        let mut args = command.to_vec();
        args.push("--");
        args.extend(paths.iter().map(String::as_str));
        args
    }

    pub fn status(&self) -> io::Result<Status> {
        let output = self.checked(&[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
        ])?;
        status::parse(&output.stdout)
    }

    /// Stages the paths' current contents, including deletions.
    pub fn stage(&self, paths: &[String]) -> io::Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        self.checked(&Self::with_paths(&["add", "-A"], paths))
            .map(drop)
    }

    /// Takes the paths out of the next commit, keeping worktree changes.
    pub fn unstage(&self, paths: &[String]) -> io::Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let unborn = self.git(&["rev-parse", "--verify", "-q", "HEAD"])?.code != Some(0);
        let command: &[&str] = if unborn {
            // Nothing to reset to: just drop the paths from the index.
            &["rm", "--cached", "-r", "-q"]
        } else {
            &["reset", "-q"]
        };
        self.checked(&Self::with_paths(command, paths)).map(drop)
    }

    /// Throws away worktree changes: tracked files go back to their staged
    /// (or committed) contents, untracked files are deleted.
    pub fn discard(&self, entries: &[Entry]) -> io::Result<()> {
        let (untracked, tracked): (Vec<&Entry>, Vec<&Entry>) = entries
            .iter()
            .filter(|entry| entry.unstaged.is_some())
            .partition(|entry| entry.unstaged == Some(Change::Untracked));
        let paths = |entries: Vec<&Entry>| -> Vec<String> {
            entries
                .into_iter()
                .map(|entry| entry.path.clone())
                .collect()
        };
        let tracked = paths(tracked);
        if !tracked.is_empty() {
            self.checked(&Self::with_paths(&["checkout", "-q"], &tracked))?;
        }
        let untracked = paths(untracked);
        if !untracked.is_empty() {
            self.checked(&Self::with_paths(&["clean", "-f", "-q"], &untracked))?;
        }
        Ok(())
    }

    /// Commits what is staged (or, with `amend`, replaces the last commit
    /// with it); returns the new commit's `<hash> <subject>`.
    pub fn commit(&self, message: &str, amend: bool) -> io::Result<String> {
        if message.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the commit message is empty",
            ));
        }
        let mut args = vec!["commit", "-q", "-m", message];
        if amend {
            args.push("--amend");
        }
        self.checked(&args)?;
        let output = self.checked(&["log", "-1", "--format=%h %s"])?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// A blob's bytes (`HEAD:path`, `:path` for the index); `None` if absent.
    pub fn blob(&self, spec: &str) -> io::Result<Option<Vec<u8>>> {
        let output = self.git(&["cat-file", "blob", spec])?;
        Ok((output.code == Some(0)).then_some(output.stdout))
    }

    /// The diff shown for `entry`: staged (HEAD → index) or unstaged (index
    /// → worktree), with `context` lines ([`FULL_FILE`] for whole files).
    /// `worktree` reads the file's current contents (`None` if it was
    /// deleted); it is only called for unstaged diffs.
    pub fn file_diff(
        &self,
        entry: &Entry,
        staged: bool,
        context: usize,
        worktree: impl FnOnce() -> io::Result<Option<Vec<u8>>>,
    ) -> io::Result<FileDiff> {
        let index = self.blob(&format!(":{}", entry.path))?;
        let (old, new) = if staged {
            let source = entry.orig_path.as_deref().unwrap_or(&entry.path);
            (self.blob(&format!("HEAD:{source}"))?, index)
        } else {
            (index, worktree()?)
        };
        Ok(diff(old.as_deref(), new.as_deref(), context))
    }

    /// Up to `limit` commits, newest first in graph order: those reachable
    /// from HEAD, or from every branch and tag with `all`. Empty before the
    /// first commit.
    pub fn log(&self, limit: usize, all: bool) -> io::Result<Vec<Commit>> {
        let limit = format!("-n{limit}");
        let mut args = vec!["log", "--topo-order", history::LOG_FORMAT, &limit];
        if all {
            args.push("--all");
        }
        let output = self.git(&args)?;
        if output.code != Some(0) {
            // A repository without commits has no HEAD to log.
            let unborn = self.git(&["rev-parse", "--verify", "-q", "HEAD"])?.code != Some(0);
            return if unborn {
                Ok(Vec::new())
            } else {
                Err(failure("log", &output))
            };
        }
        history::parse_log(&output.stdout)
    }

    /// The commit `revision` names.
    pub fn show(&self, revision: &str) -> io::Result<Commit> {
        let id = self.resolve(revision)?;
        let output = self.checked(&["log", "-1", history::LOG_FORMAT, &id])?;
        history::parse_log(&output.stdout)?
            .into_iter()
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no commit {revision}")))
    }

    /// Local branches, remote branches and tags.
    pub fn refs(&self) -> io::Result<Vec<Ref>> {
        let output = self.checked(&[
            "for-each-ref",
            "--format=%(refname)%1f%(objectname:short)%1f%(HEAD)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ])?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut fields = line.split('\x1f');
                let (full, target, head) = (fields.next()?, fields.next()?, fields.next()?);
                let (kind, name) = if let Some(name) = full.strip_prefix("refs/heads/") {
                    (RefKind::Branch, name)
                } else if let Some(name) = full.strip_prefix("refs/remotes/") {
                    (RefKind::Remote, name)
                } else {
                    (RefKind::Tag, full.strip_prefix("refs/tags/")?)
                };
                // `origin/HEAD` just points at another remote branch.
                (!name.ends_with("/HEAD")).then(|| Ref {
                    name: name.to_owned(),
                    kind,
                    target: target.to_owned(),
                    head: head == "*",
                })
            })
            .collect())
    }

    /// The full commit id `revision` names.
    pub fn resolve(&self, revision: &str) -> io::Result<String> {
        check_revision(revision)?;
        let spec = format!("{revision}^{{commit}}");
        let output = self.git(&["rev-parse", "--verify", "-q", &spec])?;
        if output.code != Some(0) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown revision `{revision}`"),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// Files that differ from `base` to `target` (`Worktree`: the working
    /// tree including staged changes; untracked files are not listed).
    pub fn changes(&self, base: &str, target: &Side) -> io::Result<Vec<FileChange>> {
        check_revision(base)?;
        let mut args = vec!["diff", "--name-status", "-z", "-M", base];
        match target {
            Side::Revision(target) => {
                check_revision(target)?;
                args.push(target);
            }
            Side::Worktree => {}
            Side::Empty => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cannot compare against nothing",
                ));
            }
        }
        args.push("--");
        parse_name_status(&self.checked(&args)?.stdout)
    }

    /// What `commit` changed relative to its first parent (everything, for
    /// a root commit).
    pub fn commit_changes(&self, commit: &Commit) -> io::Result<Vec<FileChange>> {
        let output = match commit.parents.first() {
            Some(parent) => self.checked(&[
                "diff",
                "--name-status",
                "-z",
                "-M",
                parent,
                &commit.id,
                "--",
            ])?,
            None => self.checked(&[
                "diff-tree",
                "--root",
                "-r",
                "--no-commit-id",
                "--name-status",
                "-z",
                "-M",
                &commit.id,
            ])?,
        };
        parse_name_status(&output.stdout)
    }

    /// The diff of one changed file between two sides. `worktree` reads the
    /// file's working-tree contents when a side is `Worktree`.
    pub fn revision_diff(
        &self,
        old: &Side,
        new: &Side,
        change: &FileChange,
        context: usize,
        worktree: impl FnOnce() -> io::Result<Option<Vec<u8>>>,
    ) -> io::Result<FileDiff> {
        let mut worktree = Some(worktree);
        let mut read = |side: &Side, path: &str| -> io::Result<Option<Vec<u8>>> {
            match side {
                Side::Empty => Ok(None),
                Side::Revision(revision) => {
                    check_revision(revision)?;
                    self.blob(&format!("{revision}:{path}"))
                }
                Side::Worktree => worktree.take().map_or(Ok(None), |read| read()),
            }
        };
        let old_path = change.orig_path.as_deref().unwrap_or(&change.path);
        let old = read(old, old_path)?;
        let new = read(new, &change.path)?;
        Ok(diff(old.as_deref(), new.as_deref(), context))
    }
}

/// Revisions starting with `-` would be read as options.
fn check_revision(revision: &str) -> io::Result<()> {
    if revision.is_empty() || revision.starts_with('-') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid revision `{revision}`"),
        ));
    }
    Ok(())
}

/// `git diff --name-status -z` records: `X\0path\0`, or `Rnn\0old\0new\0`
/// for renames and copies.
fn parse_name_status(output: &[u8]) -> io::Result<Vec<FileChange>> {
    let mut fields = output
        .split(|&byte| byte == 0)
        .filter(|field| !field.is_empty())
        .map(String::from_utf8_lossy);
    let mut changes = Vec::new();
    while let Some(status) = fields.next() {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad diff record {status}"),
            )
        };
        let change = match status.as_bytes().first() {
            Some(b'M') => Change::Modified,
            Some(b'A') => Change::Added,
            Some(b'D') => Change::Deleted,
            Some(b'T') => Change::TypeChanged,
            Some(b'R') => Change::Renamed,
            Some(b'C') => Change::Copied,
            Some(b'U') => Change::Conflicted,
            _ => return Err(invalid()),
        };
        let first = fields.next().ok_or_else(invalid)?.into_owned();
        let (path, orig_path) = if matches!(change, Change::Renamed | Change::Copied) {
            (fields.next().ok_or_else(invalid)?.into_owned(), Some(first))
        } else {
            (first, None)
        };
        changes.push(FileChange {
            path,
            orig_path,
            change,
        });
    }
    Ok(changes)
}

fn failure(command: &str, output: &Output) -> io::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let details = [stderr.trim(), stdout.trim()]
        .into_iter()
        .find(|text| !text.is_empty())
        .unwrap_or("no output");
    io::Error::other(format!("git {command} failed: {details}"))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, process::Command};

    use super::*;

    struct LocalGit;

    impl Runner for LocalGit {
        fn run_git(&self, args: &[String], input: Option<&[u8]>) -> io::Result<Output> {
            use std::{io::Write, process::Stdio};
            let mut child = Command::new("git")
                .args(args)
                .env("GIT_TERMINAL_PROMPT", "0")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let mut stdin = child.stdin.take().expect("piped stdin");
            if let Some(input) = input {
                stdin.write_all(input)?;
            }
            drop(stdin);
            let output = child.wait_with_output()?;
            Ok(Output {
                code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
            })
        }
    }

    fn temp_repo(name: &str) -> (PathBuf, Repository) {
        let dir = std::env::temp_dir().join(format!("next_code_git_{name}_{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.name", "Test"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "commit.gpgsign", "false"]);
        // Keep byte-exact checkouts regardless of the machine's settings.
        run(&["config", "core.autocrlf", "false"]);
        let sub = dir.join("src");
        fs::create_dir_all(&sub).unwrap();
        let repo = Repository::discover(Arc::new(LocalGit), sub.to_str().unwrap())
            .unwrap()
            .expect("a repository");
        (dir, repo)
    }

    fn state(repo: &Repository) -> Vec<(String, Option<Change>, Option<Change>)> {
        repo.status()
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| (entry.path, entry.staged, entry.unstaged))
            .collect()
    }

    #[test]
    fn staging_committing_and_discarding_drive_real_git() {
        let (dir, repo) = temp_repo("flow");
        assert!(repo.status().unwrap().unborn);
        fs::write(dir.join("src").join("a.txt"), "one\n").unwrap();
        fs::write(dir.join("b é.txt"), "bee\n").unwrap();
        assert_eq!(
            state(&repo),
            [
                ("b é.txt".into(), None, Some(Change::Untracked)),
                ("src/a.txt".into(), None, Some(Change::Untracked)),
            ]
        );

        repo.stage(&["src/a.txt".into(), "b é.txt".into()]).unwrap();
        repo.unstage(&["b é.txt".into()]).unwrap();
        assert_eq!(
            state(&repo)[1],
            ("src/a.txt".into(), Some(Change::Added), None)
        );
        let summary = repo.commit("first commit", false).unwrap();
        assert!(summary.ends_with(" first commit"), "{summary}");
        let status = repo.status().unwrap();
        assert!(!status.unborn);
        assert_eq!(status.branch.as_deref(), Some("main"));

        fs::write(dir.join("src").join("a.txt"), "one\ntwo\n").unwrap();
        let entry = repo.status().unwrap().entries[1].clone();
        let unstaged = repo
            .file_diff(&entry, false, DIFF_CONTEXT, || {
                fs::read(dir.join("src").join("a.txt")).map(Some)
            })
            .unwrap();
        assert_eq!((unstaged.added, unstaged.removed), (1, 0));
        repo.stage(std::slice::from_ref(&entry.path)).unwrap();
        let staged = repo
            .file_diff(&entry, true, DIFF_CONTEXT, || unreachable!())
            .unwrap();
        assert_eq!((staged.added, staged.removed), (1, 0));

        repo.unstage(std::slice::from_ref(&entry.path)).unwrap();
        let entries = repo.status().unwrap().entries;
        repo.discard(&entries).unwrap();
        assert!(repo.status().unwrap().entries.is_empty());
        assert_eq!(
            fs::read_to_string(dir.join("src").join("a.txt")).unwrap(),
            "one\n"
        );
        assert!(!dir.join("b é.txt").exists(), "untracked files are removed");

        let err = repo.commit("nothing staged", false).unwrap_err();
        assert!(err.to_string().contains("git commit failed"), "{err}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn directories_outside_repositories_have_none() {
        let dir = std::env::temp_dir().join(format!("next_code_git_none_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let found = Repository::discover(Arc::new(LocalGit), dir.to_str().unwrap()).unwrap();
        assert!(found.is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn history_refs_and_comparisons_follow_branches_and_renames() {
        let (dir, repo) = temp_repo("history");
        assert_eq!(repo.log(10, true).unwrap(), [], "no commits yet");
        let commit = |message: &str| {
            repo.stage(&[".".into()]).unwrap();
            repo.commit(message, false).unwrap();
        };
        fs::write(dir.join("old.txt"), "same\n").unwrap();
        fs::write(dir.join("keep.txt"), "v1\n").unwrap();
        commit("root");
        let git = |args: &[&str]| repo.git(args).unwrap();
        git(&["checkout", "-q", "-b", "feature"]);
        git(&["mv", "old.txt", "new.txt"]);
        fs::write(dir.join("keep.txt"), "v2\n").unwrap();
        commit("feature work");
        git(&["checkout", "-q", "main"]);
        fs::write(dir.join("main.txt"), "m\n").unwrap();
        commit("main work");
        git(&["tag", "v1"]);

        let all = repo.log(10, true).unwrap();
        assert_eq!(all.len(), 3);
        let rows = graph(&all);
        assert_eq!(
            rows.iter().map(|row| row.width).max(),
            Some(2),
            "two branches"
        );
        assert_eq!(repo.log(10, false).unwrap().len(), 2, "HEAD only");

        let refs = repo.refs().unwrap();
        let names: Vec<(&str, RefKind, bool)> = refs
            .iter()
            .map(|r| (r.name.as_str(), r.kind, r.head))
            .collect();
        assert_eq!(
            names,
            [
                ("feature", RefKind::Branch, false),
                ("main", RefKind::Branch, true),
                ("v1", RefKind::Tag, false),
            ]
        );
        assert_eq!(repo.resolve("v1").unwrap(), repo.resolve("main").unwrap());
        assert_eq!(
            repo.resolve("nope").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            repo.resolve("--all").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );

        // main ↔ feature: main.txt only on main, keep.txt edited, old.txt renamed.
        let mut changes = repo
            .changes("main", &Side::Revision("feature".into()))
            .unwrap();
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        let summary: Vec<(&str, Option<&str>, Change)> = changes
            .iter()
            .map(|c| (c.path.as_str(), c.orig_path.as_deref(), c.change))
            .collect();
        assert_eq!(
            summary,
            [
                ("keep.txt", None, Change::Modified),
                ("main.txt", None, Change::Deleted),
                ("new.txt", Some("old.txt"), Change::Renamed),
            ]
        );
        let between = |change: &FileChange| {
            repo.revision_diff(
                &Side::Revision("main".into()),
                &Side::Revision("feature".into()),
                change,
                DIFF_CONTEXT,
                || unreachable!(),
            )
            .unwrap()
        };
        let keep = between(&changes[0]);
        assert_eq!((keep.added, keep.removed), (1, 1));
        assert!(
            between(&changes[2]).rows.is_empty(),
            "a pure rename has no line changes"
        );

        // The working tree against a revision includes unstaged edits.
        fs::write(dir.join("keep.txt"), "v3\n").unwrap();
        let worktree = repo.changes("v1", &Side::Worktree).unwrap();
        assert_eq!(worktree.len(), 1);
        let diff = repo
            .revision_diff(
                &Side::Revision("v1".into()),
                &Side::Worktree,
                &worktree[0],
                DIFF_CONTEXT,
                || fs::read(dir.join("keep.txt")).map(Some),
            )
            .unwrap();
        assert_eq!((diff.added, diff.removed), (1, 1));

        // Commit contents: the root adds everything; others diff their parent.
        let root = all.iter().find(|c| c.parents.is_empty()).unwrap();
        assert_eq!(repo.commit_changes(root).unwrap().len(), 2);
        let main_work = all.iter().find(|c| c.subject == "main work").unwrap();
        assert_eq!(
            repo.commit_changes(main_work).unwrap(),
            [FileChange {
                path: "main.txt".into(),
                orig_path: None,
                change: Change::Added,
            }]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hunks_are_staged_unstaged_and_discarded_individually() {
        let (dir, repo) = temp_repo("hunks");
        let file = dir.join("f.txt");
        let original: String = (1..=20).map(|n| format!("line {n}\r\n")).collect();
        fs::write(&file, &original).unwrap();
        repo.stage(&["f.txt".into()]).unwrap();
        repo.commit("base", false).unwrap();
        // Two separate edits, far apart: two hunks. CRLF must survive.
        let edited = original
            .replace("line 2\r\n", "line two\r\n")
            .replace("line 18\r\n", "line eighteen\r\n");
        fs::write(&file, &edited).unwrap();
        let entry = repo.status().unwrap().entries[0].clone();

        let unstaged = repo
            .index_patch(&entry, false, DIFF_CONTEXT)
            .unwrap()
            .unwrap();
        assert_eq!(unstaged.hunks.len(), 2);
        let rows = unstaged.to_file_diff();
        assert_eq!((rows.added, rows.removed), (2, 2));
        repo.apply_hunk(&unstaged, 1, HunkAction::Stage).unwrap();
        let entry = repo.status().unwrap().entries[0].clone();
        assert_eq!(
            (entry.staged, entry.unstaged),
            (Some(Change::Modified), Some(Change::Modified))
        );
        let staged = repo
            .index_patch(&entry, true, DIFF_CONTEXT)
            .unwrap()
            .unwrap();
        assert_eq!(staged.hunks.len(), 1);
        assert!(
            staged.hunks[0].header.starts_with("@@ -15"),
            "{}",
            staged.hunks[0].header
        );

        // Unstage it again, then discard the first edit from the worktree.
        repo.apply_hunk(&staged, 0, HunkAction::Unstage).unwrap();
        let unstaged = repo
            .index_patch(&entry, false, DIFF_CONTEXT)
            .unwrap()
            .unwrap();
        repo.apply_hunk(&unstaged, 0, HunkAction::Discard).unwrap();
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            original.replace("line 18\r\n", "line eighteen\r\n")
        );
        let entry = repo.status().unwrap().entries[0].clone();
        assert_eq!(entry.staged, None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn branches_remotes_and_worktrees_round_trip() {
        let (dir, repo) = temp_repo("branches");
        fs::write(dir.join("a.txt"), "a\n").unwrap();
        repo.stage(&["a.txt".into()]).unwrap();
        repo.commit("first", false).unwrap();

        // Branches: create from HEAD, switch, amend, delete.
        repo.create_branch("feature/x", None, true).unwrap();
        assert_eq!(repo.status().unwrap().branch.as_deref(), Some("feature/x"));
        assert_eq!(
            repo.create_branch("bad name", None, false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let summary = repo.commit("first, reworded", true).unwrap();
        assert!(summary.ends_with("first, reworded"), "{summary}");
        let main = repo
            .refs()
            .unwrap()
            .into_iter()
            .find(|r| r.name == "main")
            .unwrap();
        repo.switch_branch(&main).unwrap();
        assert!(
            repo.delete_branch("feature/x", false).is_err(),
            "unmerged amend"
        );
        repo.delete_branch("feature/x", true).unwrap();

        // A bare remote: first push sets the upstream, then fetch/pull.
        // Built from git's own root path: `canonicalize` gives `\\?\` paths
        // on Windows, which git cannot use.
        let remote = PathBuf::from(format!("{}.remote.git", repo.root()));
        fs::remove_dir_all(&remote).ok();
        let git = |args: &[&str]| repo.git(args).unwrap();
        assert!(repo.push(&repo.status().unwrap()).is_err(), "no remote yet");
        let bare = Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&remote)
            .status()
            .unwrap();
        assert!(bare.success());
        git(&["remote", "add", "origin", remote.to_str().unwrap()]);
        repo.push(&repo.status().unwrap()).unwrap();
        let status = repo.status().unwrap();
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        repo.fetch().unwrap();
        repo.pull().unwrap();
        assert!(
            repo.refs()
                .unwrap()
                .iter()
                .any(|r| r.kind == RefKind::Remote && r.name == "origin/main")
        );

        // A worktree on a new branch next to the repository.
        let path = repo.default_worktree_path("wt/one");
        assert!(path.ends_with("-wt-one"), "{path}");
        repo.add_worktree(&path, "wt-one", true, None).unwrap();
        let worktrees = repo.worktrees().unwrap();
        assert_eq!(worktrees.len(), 2);
        assert!(worktrees[0].current);
        assert_eq!(worktrees[1].branch.as_deref(), Some("wt-one"));
        repo.remove_worktree(&path, false).unwrap();
        assert_eq!(repo.worktrees().unwrap().len(), 1);

        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&remote).unwrap();
    }
}
