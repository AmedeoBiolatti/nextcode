//! Operations on commits from the log: cherry-pick, revert, reset, undo
//! and reword the last commit, tags, detached checkouts and patches.

use std::io;

use serde::Serialize;

use crate::{OperationState, Repository, check_revision, failure};

/// How a cherry-pick, revert, stash apply or unshelve ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "result")]
pub enum ApplyOutcome {
    /// Every commit was applied; `empty` were skipped because their
    /// changes were already there.
    Done { empty: usize },
    /// Git stopped on conflicts in `paths`: resolve and commit (the
    /// message is prepared), or [`Repository::abort`].
    Conflicts { paths: Vec<String> },
}

/// `git reset` modes, as PyCharm's "Reset Current Branch to Here" offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetMode {
    /// Moves the branch only; changes stay staged.
    Soft,
    /// Moves the branch and resets the index; changes stay in files.
    Mixed,
    /// Discards every change.
    Hard,
    /// Moves the branch, keeping local changes; fails if they conflict.
    Keep,
}

impl ResetMode {
    pub const ALL: [Self; 4] = [Self::Soft, Self::Mixed, Self::Hard, Self::Keep];

    fn flag(self) -> &'static str {
        match self {
            Self::Soft => "--soft",
            Self::Mixed => "--mixed",
            Self::Hard => "--hard",
            Self::Keep => "--keep",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Soft => "Soft",
            Self::Mixed => "Mixed",
            Self::Hard => "Hard",
            Self::Keep => "Keep",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Soft => "Files and index are not changed; the changes are kept staged.",
            Self::Mixed => "Files are not changed; the index is reset.",
            Self::Hard => "Files and index are reset: all changes are lost.",
            Self::Keep => "Files differing from HEAD are kept; fails if they would be overwritten.",
        }
    }
}

/// The two sequencer commands that can stop on conflicts.
#[derive(Clone, Copy)]
enum Apply {
    CherryPick,
    Revert,
}

impl Apply {
    fn command(self) -> &'static str {
        match self {
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
        }
    }

    /// The state git is left in when the command stops.
    fn state(self) -> OperationState {
        match self {
            Self::CherryPick => OperationState::CherryPicking,
            Self::Revert => OperationState::Reverting,
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_owned())
}

impl Repository {
    /// Applies the commits on top of HEAD in the given order (oldest
    /// first, as they were made). Merge commits are refused.
    pub fn cherry_pick(&self, ids: &[String]) -> io::Result<ApplyOutcome> {
        for id in ids {
            if self.parent_count(id)? > 1 {
                return Err(invalid(&format!(
                    "{} is a merge commit; cherry-picking merges is not supported",
                    &id[..id.len().min(8)]
                )));
            }
        }
        self.apply(Apply::CherryPick, &[], ids)
    }

    /// Commits the inverse of each commit, in the given order (newest
    /// first, so later changes are undone before the ones they build on).
    /// A merge commit is reverted against its first parent, alone.
    pub fn revert(&self, ids: &[String]) -> io::Result<ApplyOutcome> {
        let mut merges = 0;
        for id in ids {
            if self.parent_count(id)? > 1 {
                merges += 1;
            }
        }
        match merges {
            0 => self.apply(Apply::Revert, &["--no-edit"], ids),
            _ if ids.len() == 1 => self.apply(Apply::Revert, &["--no-edit", "-m", "1"], ids),
            _ => Err(invalid("revert a merge commit on its own")),
        }
    }

    fn parent_count(&self, id: &str) -> io::Result<usize> {
        check_revision(id)?;
        let output = self.checked(&["rev-list", "--parents", "-n1", id])?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .count()
            .saturating_sub(1))
    }

    fn apply(&self, apply: Apply, options: &[&str], ids: &[String]) -> io::Result<ApplyOutcome> {
        if ids.is_empty() {
            return Ok(ApplyOutcome::Done { empty: 0 });
        }
        let mut args = vec![apply.command()];
        args.extend(options);
        for id in ids {
            check_revision(id)?;
            args.push(id);
        }
        let mut output = self.git(&args)?;
        let mut empty = 0;
        while output.code != Some(0) {
            if self.operation_state()? != apply.state() {
                return Err(failure(apply.command(), &output));
            }
            let paths = self.conflicted_paths()?;
            if !paths.is_empty() {
                return Ok(ApplyOutcome::Conflicts { paths });
            }
            // Stopped on a commit whose changes are already here: skip it
            // and go on with the rest.
            empty += 1;
            output = self.git(&[apply.command(), "--skip"])?;
        }
        Ok(ApplyOutcome::Done { empty })
    }

    pub(crate) fn conflicted_paths(&self) -> io::Result<Vec<String>> {
        let output = self.checked(&["diff", "--name-only", "--diff-filter=U", "-z"])?;
        Ok(output
            .stdout
            .split(|&byte| byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect())
    }

    /// Moves the current branch (or detached HEAD) to `id`.
    pub fn reset(&self, id: &str, mode: ResetMode) -> io::Result<()> {
        check_revision(id)?;
        self.checked(&["reset", "-q", mode.flag(), id]).map(drop)
    }

    /// Whether a remote branch contains `id`.
    pub fn is_pushed(&self, id: &str) -> io::Result<bool> {
        check_revision(id)?;
        let output = self.checked(&[
            "for-each-ref",
            "--count=1",
            "--contains",
            id,
            "--format=%(refname)",
            "refs/remotes",
        ])?;
        Ok(!output.stdout.trim_ascii().is_empty())
    }

    /// Takes back the last commit, keeping its changes (a soft reset to
    /// its parent). Refused for a pushed commit and the first commit.
    pub fn undo_last_commit(&self) -> io::Result<()> {
        let head = self.resolve("HEAD")?;
        if self.is_pushed(&head)? {
            return Err(invalid(
                "the last commit is already pushed; revert it instead",
            ));
        }
        if self.parent_count(&head)? == 0 {
            return Err(invalid("the first commit cannot be undone"));
        }
        self.checked(&["reset", "-q", "--soft", "HEAD~1"]).map(drop)
    }

    /// Replaces the last commit's message, leaving its contents (and
    /// whatever is staged) alone; returns `<hash> <subject>`.
    pub fn reword_head(&self, message: &str) -> io::Result<String> {
        if message.trim().is_empty() {
            return Err(invalid("the commit message is empty"));
        }
        self.checked(&[
            "commit",
            "-q",
            "--amend",
            "--only",
            "--allow-empty",
            "-m",
            message,
        ])?;
        let output = self.checked(&["log", "-1", "--format=%h %s"])?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// Creates tag `name` at `at`; annotated when `message` is given.
    pub fn create_tag(&self, name: &str, at: &str, message: Option<&str>) -> io::Result<()> {
        let full = format!("refs/tags/{name}");
        if name.is_empty()
            || name.starts_with('-')
            || self.git(&["check-ref-format", &full])?.code != Some(0)
        {
            return Err(invalid(&format!("`{name}` is not a valid tag name")));
        }
        check_revision(at)?;
        let mut args = vec!["tag"];
        if let Some(message) = message.filter(|m| !m.trim().is_empty()) {
            args.extend(["-a", "-m", message]);
        }
        args.extend([name, at]);
        self.checked(&args).map(drop)
    }

    /// Checks out `id` as a detached HEAD.
    pub fn checkout_revision(&self, id: &str) -> io::Result<()> {
        check_revision(id)?;
        self.checked(&["switch", "-q", "--detach", id]).map(drop)
    }

    /// The commits as one mailbox of patches (`git format-patch`), in the
    /// given order (oldest first). Merge commits have no patch.
    pub fn create_patch(&self, ids: &[String]) -> io::Result<Vec<u8>> {
        let mut patch = Vec::new();
        for id in ids {
            check_revision(id)?;
            let output = self.checked(&[
                "format-patch",
                "--stdout",
                "--no-merges",
                "--no-color",
                "-1",
                id,
            ])?;
            patch.extend_from_slice(&output.stdout);
        }
        if patch.is_empty() {
            return Err(invalid("merge commits have no patch"));
        }
        Ok(patch)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;
    use crate::{
        Change, Commit, LogFilter,
        tests::{state, temp_repo},
    };

    fn write(dir: &Path, file: &str, text: &str) {
        fs::write(dir.join(file), text).unwrap();
    }

    fn commit(repo: &Repository, message: &str) -> String {
        repo.stage(&[".".into()]).unwrap();
        repo.commit(message, false).unwrap();
        repo.resolve("HEAD").unwrap()
    }

    fn git(repo: &Repository, args: &[&str]) {
        let output = repo.git(args).unwrap();
        assert_eq!(output.code, Some(0), "git {args:?}: {output:?}");
    }

    fn head_log(repo: &Repository) -> Vec<Commit> {
        let filter = LogFilter {
            revisions: vec!["HEAD".into()],
            ..LogFilter::default()
        };
        repo.log_page(&filter, 0, 100).unwrap().commits
    }

    fn subjects(repo: &Repository) -> Vec<String> {
        head_log(repo).into_iter().map(|c| c.subject).collect()
    }

    fn read(dir: &Path, file: &str) -> String {
        fs::read_to_string(dir.join(file)).unwrap()
    }

    #[test]
    fn cherry_picks_apply_skip_empty_and_stop_on_conflicts() {
        let (dir, repo) = temp_repo("cherry");
        write(&dir, "t.txt", "base\n");
        write(&dir, "c.txt", "base\n");
        commit(&repo, "base");
        git(&repo, &["switch", "-q", "-c", "topic"]);
        write(&dir, "t.txt", "one\n");
        let t1 = commit(&repo, "t1");
        write(&dir, "t.txt", "two\n");
        let t2 = commit(&repo, "t2");
        write(&dir, "c.txt", "theirs\n");
        let theirs = commit(&repo, "theirs");
        git(&repo, &["switch", "-q", "main"]);
        write(&dir, "c.txt", "ours\n");
        commit(&repo, "ours");

        assert_eq!(
            repo.cherry_pick(&[t1.clone(), t2.clone()]).unwrap(),
            ApplyOutcome::Done { empty: 0 }
        );
        assert_eq!(subjects(&repo)[..3], ["t2", "t1", "ours"]);
        assert_eq!(read(&dir, "t.txt"), "two\n");
        // Already applied: skipped, nothing committed.
        let before = repo.resolve("HEAD").unwrap();
        assert_eq!(
            repo.cherry_pick(std::slice::from_ref(&t2)).unwrap(),
            ApplyOutcome::Done { empty: 1 }
        );
        assert_eq!(repo.resolve("HEAD").unwrap(), before);

        assert_eq!(
            repo.cherry_pick(std::slice::from_ref(&theirs)).unwrap(),
            ApplyOutcome::Conflicts {
                paths: vec!["c.txt".into()]
            }
        );
        assert!(read(&dir, "c.txt").contains("<<<<<<<"));
        assert_eq!(
            repo.operation_state().unwrap(),
            OperationState::CherryPicking
        );
        repo.abort(OperationState::CherryPicking).unwrap();
        assert_eq!(repo.resolve("HEAD").unwrap(), before);
        assert_eq!(read(&dir, "c.txt"), "ours\n");
        assert!(state(&repo).is_empty());
        assert_eq!(repo.operation_state().unwrap(), OperationState::None);

        // Merge commits are refused.
        git(&repo, &["switch", "-q", "-c", "side"]);
        write(&dir, "s.txt", "s\n");
        commit(&repo, "s");
        git(&repo, &["switch", "-q", "main"]);
        git(
            &repo,
            &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
        );
        let merge = repo.resolve("HEAD").unwrap();
        git(&repo, &["switch", "-q", "topic"]);
        assert!(repo.cherry_pick(&[merge]).is_err());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn reverts_undo_commits_merges_and_stop_on_conflicts() {
        let (dir, repo) = temp_repo("revert");
        write(&dir, "f.txt", "a\n");
        commit(&repo, "a");
        write(&dir, "f.txt", "b\n");
        let b = commit(&repo, "b");
        write(&dir, "f.txt", "c\n");
        let c = commit(&repo, "c");
        write(&dir, "g.txt", "g\n");
        let g = commit(&repo, "g");

        assert_eq!(
            repo.revert(&[g.clone(), c.clone()]).unwrap(),
            ApplyOutcome::Done { empty: 0 }
        );
        assert_eq!(subjects(&repo)[..2], ["Revert \"c\"", "Revert \"g\""]);
        assert_eq!(read(&dir, "f.txt"), "b\n");
        assert!(!dir.join("g.txt").exists());

        // `b` is overwritten by `c` and its revert's parent: conflict.
        write(&dir, "f.txt", "d\n");
        let head = commit(&repo, "d");
        assert_eq!(
            repo.revert(std::slice::from_ref(&b)).unwrap(),
            ApplyOutcome::Conflicts {
                paths: vec!["f.txt".into()]
            }
        );
        assert_eq!(repo.operation_state().unwrap(), OperationState::Reverting);
        repo.abort(OperationState::Reverting).unwrap();
        assert_eq!(repo.resolve("HEAD").unwrap(), head);

        // A merge reverts against its first parent.
        git(&repo, &["switch", "-q", "-c", "side"]);
        write(&dir, "s.txt", "s\n");
        commit(&repo, "s");
        git(&repo, &["switch", "-q", "main"]);
        git(
            &repo,
            &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
        );
        let merge = repo.resolve("HEAD").unwrap();
        assert!(
            repo.revert(&[merge.clone(), head]).is_err(),
            "merge not alone"
        );
        assert_eq!(
            repo.revert(&[merge]).unwrap(),
            ApplyOutcome::Done { empty: 0 }
        );
        assert!(!dir.join("s.txt").exists());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn resets_move_the_branch_in_each_mode() {
        let (dir, repo) = temp_repo("reset");
        write(&dir, "f.txt", "1\n");
        let first = commit(&repo, "1");
        write(&dir, "f.txt", "2\n");
        let second = commit(&repo, "2");
        let modified = Some(Change::Modified);

        repo.reset(&first, ResetMode::Soft).unwrap();
        assert_eq!(repo.resolve("HEAD").unwrap(), first);
        assert_eq!(state(&repo), [("f.txt".into(), modified, None)]);

        repo.reset(&second, ResetMode::Hard).unwrap();
        repo.reset(&first, ResetMode::Mixed).unwrap();
        assert_eq!(state(&repo), [("f.txt".into(), None, modified)]);
        assert_eq!(read(&dir, "f.txt"), "2\n");

        repo.reset(&second, ResetMode::Hard).unwrap();
        repo.reset(&first, ResetMode::Keep).unwrap();
        assert_eq!(read(&dir, "f.txt"), "1\n");
        assert!(state(&repo).is_empty());
        repo.reset(&second, ResetMode::Keep).unwrap();
        write(&dir, "f.txt", "local\n");
        assert!(
            repo.reset(&first, ResetMode::Keep).is_err(),
            "keep refuses to overwrite local changes"
        );
        repo.reset(&first, ResetMode::Hard).unwrap();
        assert_eq!(read(&dir, "f.txt"), "1\n");
        assert!(state(&repo).is_empty());
        assert!(repo.reset("--hard", ResetMode::Hard).is_err());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn the_last_commit_is_undone_unless_pushed_or_first() {
        let (dir, repo) = temp_repo("undo");
        write(&dir, "f.txt", "1\n");
        let first = commit(&repo, "1");
        assert!(repo.undo_last_commit().is_err(), "first commit");
        write(&dir, "f.txt", "2\n");
        commit(&repo, "2");
        repo.undo_last_commit().unwrap();
        assert_eq!(repo.resolve("HEAD").unwrap(), first);
        assert_eq!(read(&dir, "f.txt"), "2\n", "changes kept");
        assert_eq!(
            state(&repo),
            [("f.txt".into(), Some(Change::Modified), None)]
        );

        // git's own `/`-separated root: canonical Windows paths (`\\?\C:`)
        // are not valid git remotes.
        let remote_path = format!("{}.remote.git", repo.root());
        let remote = std::path::PathBuf::from(&remote_path);
        fs::remove_dir_all(&remote).ok();
        let remote_path = remote_path.as_str();
        git(&repo, &["init", "-q", "--bare", remote_path]);
        git(&repo, &["remote", "add", "origin", remote_path]);
        let pushed = commit(&repo, "2 again");
        assert!(!repo.is_pushed(&pushed).unwrap());
        git(&repo, &["push", "-q", "-u", "origin", "main"]);
        assert!(repo.is_pushed(&pushed).unwrap());
        assert!(repo.undo_last_commit().is_err(), "pushed");
        fs::remove_dir_all(dir).ok();
        fs::remove_dir_all(remote).ok();
    }

    #[test]
    fn rewording_tags_detached_checkouts_and_patches() {
        let (dir, repo) = temp_repo("reword");
        write(&dir, "f.txt", "1\n");
        let first = commit(&repo, "first");
        write(&dir, "f.txt", "2\n");
        commit(&repo, "second");

        // Staged changes stay staged: only the message changes.
        write(&dir, "f.txt", "3\n");
        repo.stage(&["f.txt".into()]).unwrap();
        let summary = repo.reword_head("second, reworded\n\nwith a body").unwrap();
        assert!(summary.ends_with(" second, reworded"), "{summary}");
        assert_eq!(
            repo.commit_message("HEAD").unwrap(),
            "second, reworded\n\nwith a body"
        );
        assert_eq!(
            repo.blob("HEAD:f.txt").unwrap().as_deref(),
            Some(&b"2\n"[..])
        );
        assert_eq!(
            state(&repo),
            [("f.txt".into(), Some(Change::Modified), None)]
        );
        assert!(repo.reword_head("  ").is_err());
        assert_eq!(subjects(&repo).len(), 2, "amended, not added");

        repo.create_tag("v1", &first, None).unwrap();
        repo.create_tag("v2", "HEAD", Some("release two")).unwrap();
        assert!(repo.create_tag("bad name", "HEAD", None).is_err());
        assert!(repo.create_tag("-x", "HEAD", None).is_err());
        assert!(repo.create_tag("v1", "HEAD", None).is_err(), "exists");
        let tags: Vec<String> = repo
            .refs()
            .unwrap()
            .into_iter()
            .filter(|r| r.kind == crate::RefKind::Tag)
            .map(|r| r.name)
            .collect();
        assert_eq!(tags, ["v1", "v2"]);
        assert_eq!(
            repo.commit_message("v2").unwrap(),
            "second, reworded\n\nwith a body"
        );

        let ids: Vec<String> = head_log(&repo).into_iter().rev().map(|c| c.id).collect();
        let patch = String::from_utf8(repo.create_patch(&ids).unwrap()).unwrap();
        let first_at = patch.find("Subject: [PATCH] first").unwrap();
        let second_at = patch.find("Subject: [PATCH] second, reworded").unwrap();
        assert!(first_at < second_at, "oldest first");
        assert!(patch.contains("+2"));

        git(&repo, &["stash", "-q"]);
        repo.checkout_revision(&first).unwrap();
        let status = repo.status().unwrap();
        assert_eq!(status.branch, None, "detached");
        assert_eq!(repo.resolve("HEAD").unwrap(), first);
        fs::remove_dir_all(dir).ok();
    }
}
