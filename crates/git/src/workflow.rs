//! PyCharm's git workflow: committing chosen files without a staging area,
//! rollback, ignoring, merge/rebase with conflict detection, branch
//! renames, Update Project and pushes with an explicit target.

use std::io;

use serde::Serialize;

use crate::{Change, Entry, Repository, check_revision, failure};

/// How a merge, rebase or update ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Done,
    /// Stopped on conflicts: resolve them, then commit (merge) or continue
    /// (rebase), or abort.
    Conflicts,
}

/// A multi-step operation git is in the middle of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    None,
    Merging,
    Rebasing,
    CherryPicking,
    Reverting,
}

impl OperationState {
    /// While merging, cherry-picking or reverting, commits must include the
    /// whole index (git refuses partial commits then).
    pub fn needs_full_commit(self) -> bool {
        matches!(self, Self::Merging | Self::CherryPicking | Self::Reverting)
    }
}

/// Where pushing a local branch goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PushTarget {
    pub remote: String,
    /// Branch name on the remote.
    pub branch: String,
    /// The branch has no upstream yet: the push sets it (`-u`).
    pub new: bool,
}

/// A local branch's upstream and how far apart they are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BranchTracking {
    pub name: String,
    /// Short name (`origin/main`); `None` without one.
    pub upstream: Option<String>,
    /// Commits on the branch that the upstream lacks.
    pub ahead: u32,
    /// Commits on the upstream that the branch lacks.
    pub behind: u32,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

/// `git branch -d` refused: the branch has commits no other branch has.
pub fn not_fully_merged(err: &io::Error) -> bool {
    err.to_string().contains("not fully merged")
}

/// `.gitignore` contents with root-anchored patterns for `paths` appended
/// (paths already listed are skipped).
pub fn ignore_entries(existing: &str, paths: &[String]) -> String {
    let mut text = existing.to_owned();
    for path in paths {
        let mut pattern = String::from("/");
        for (index, c) in path.chars().enumerate() {
            let special = matches!(c, '*' | '?' | '[' | '\\') || (index == 0 && c == '#');
            if special {
                pattern.push('\\');
            }
            pattern.push(c);
        }
        if pattern.ends_with(' ') {
            pattern.insert(pattern.len() - 1, '\\');
        }
        if text.lines().any(|line| line.trim_end() == pattern) {
            continue;
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&pattern);
        text.push('\n');
    }
    text
}

impl Repository {
    /// The operation in progress, from git's pseudo-refs (one `cat-file`
    /// run).
    pub fn operation_state(&self) -> io::Result<OperationState> {
        let refs = [
            ("REBASE_HEAD", OperationState::Rebasing),
            ("MERGE_HEAD", OperationState::Merging),
            ("CHERRY_PICK_HEAD", OperationState::CherryPicking),
            ("REVERT_HEAD", OperationState::Reverting),
        ];
        let input: String = refs.iter().map(|(name, _)| format!("{name}\n")).collect();
        let output =
            self.checked_with_input(&["cat-file", "--batch-check"], Some(input.as_bytes()))?;
        // One `<id> <type> <size>` or `<name> missing` line per name, in
        // request order.
        let text = String::from_utf8_lossy(&output.stdout);
        for (line, (_, state)) in text.lines().zip(refs) {
            if line.ends_with(" missing") {
                continue;
            }
            // git leaves REBASE_HEAD behind after a rebase ends.
            if state == OperationState::Rebasing
                && self.git(&["rebase", "--show-current-patch"])?.code != Some(0)
            {
                continue;
            }
            return Ok(state);
        }
        Ok(OperationState::None)
    }

    /// Puts untracked files under version control (PyCharm's Add to VCS).
    pub fn add(&self, paths: &[String]) -> io::Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        self.checked(&Self::with_paths(&["add"], paths)).map(drop)
    }

    /// Commits exactly the changes of `entries` (their working-tree
    /// contents; untracked ones are added first), leaving what is staged
    /// for other files alone: `git commit --only`. With `amend`, replaces
    /// the last commit (`entries` may then be empty: message only). While
    /// merging (or cherry-picking/reverting) git allows no partial commit,
    /// so the entries are staged and the whole index is committed.
    /// Returns the new commit's `<hash> <subject>`.
    pub fn commit_entries(
        &self,
        message: &str,
        entries: &[Entry],
        amend: bool,
    ) -> io::Result<String> {
        if message.trim().is_empty() {
            return Err(invalid("the commit message is empty"));
        }
        if entries.is_empty() && !amend {
            return Err(invalid("no changes are selected for the commit"));
        }
        let mut paths: Vec<String> = Vec::new();
        for entry in entries {
            paths.push(entry.path.clone());
            if let Some(orig) = &entry.orig_path {
                paths.push(orig.clone());
            }
        }
        let state = self.operation_state()?;
        if state == OperationState::Rebasing {
            return Err(invalid(
                "a rebase is in progress: resolve the conflicts, then Continue Rebase",
            ));
        }
        if state.needs_full_commit() {
            if amend {
                return Err(invalid("cannot amend while a merge is in progress"));
            }
            if entries
                .iter()
                .any(|entry| entry.unstaged == Some(Change::Conflicted))
            {
                return Err(invalid(
                    "resolve the merge conflicts (Mark Resolved) before committing",
                ));
            }
            self.stage(&paths)?;
            self.checked(&["commit", "-q", "-m", message])?;
            return self.head_summary();
        }
        let untracked: Vec<String> = entries
            .iter()
            .filter(|entry| entry.unstaged == Some(Change::Untracked))
            .map(|entry| entry.path.clone())
            .collect();
        if !untracked.is_empty() {
            self.checked(&Self::with_paths(&["add"], &untracked))?;
        }
        let mut args = vec!["commit", "-q", "-m", message];
        if amend {
            args.push("--amend");
        }
        args.push("--only");
        let args = Self::with_paths(&args, &paths);
        if let Err(err) = self.checked(&args) {
            // Leave untracked files as they were.
            if !untracked.is_empty() {
                let _ = self.checked(&Self::with_paths(
                    &["rm", "--cached", "-q", "-r", "--ignore-unmatch"],
                    &untracked,
                ));
            }
            return Err(err);
        }
        self.head_summary()
    }

    fn head_summary(&self) -> io::Result<String> {
        let output = self.checked(&["log", "-1", "--format=%h %s"])?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// The last commit's full message (what Amend starts from); `None`
    /// before the first commit.
    pub fn head_message(&self) -> io::Result<Option<String>> {
        let output = self.git(&["log", "-1", "--format=%B"])?;
        Ok((output.code == Some(0)).then(|| {
            String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_owned()
        }))
    }

    /// Reverts `entries` to HEAD: modified and deleted files get their
    /// committed contents back (index and working tree), added files leave
    /// version control (the file stays, unversioned), renames return to
    /// the old name, and untracked files are deleted.
    pub fn rollback(&self, entries: &[Entry]) -> io::Result<()> {
        let mut restore = Vec::new();
        let mut unversion = Vec::new();
        let mut remove = Vec::new();
        let mut delete = Vec::new();
        for entry in entries {
            match (entry.staged, entry.unstaged) {
                (_, Some(Change::Untracked)) => delete.push(entry.path.clone()),
                (Some(Change::Added), _) => unversion.push(entry.path.clone()),
                (Some(Change::Renamed | Change::Copied), _) => {
                    if let Some(orig) = &entry.orig_path {
                        restore.push(orig.clone());
                    }
                    remove.push(entry.path.clone());
                }
                _ => restore.push(entry.path.clone()),
            }
        }
        if !unversion.is_empty() {
            self.checked(&Self::with_paths(
                &["rm", "-q", "-r", "--cached", "-f"],
                &unversion,
            ))?;
        }
        if !remove.is_empty() {
            self.checked(&Self::with_paths(&["rm", "-q", "-r", "-f"], &remove))?;
        }
        if !restore.is_empty() {
            self.checked(&Self::with_paths(&["checkout", "-q", "HEAD"], &restore))?;
        }
        if !delete.is_empty() {
            self.checked(&Self::with_paths(&["clean", "-f", "-q"], &delete))?;
        }
        Ok(())
    }

    /// Runs a merge-like command; a failure that left conflicts is
    /// [`Outcome::Conflicts`], any other failure an error.
    fn conflicting(&self, args: &[&str]) -> io::Result<Outcome> {
        let output = self.git(args)?;
        if output.code == Some(0) {
            return Ok(Outcome::Done);
        }
        let conflicted = self
            .status()?
            .entries
            .iter()
            .any(|entry| entry.unstaged == Some(Change::Conflicted));
        if conflicted {
            Ok(Outcome::Conflicts)
        } else {
            Err(failure(args[0], &output))
        }
    }

    /// Merges `branch` into the current branch.
    pub fn merge(&self, branch: &str) -> io::Result<Outcome> {
        check_revision(branch)?;
        self.conflicting(&["merge", "--no-edit", "-q", branch])
    }

    /// Rebases `branch` (default: the current one, else it is checked out
    /// first) onto `onto`.
    pub fn rebase(&self, onto: &str, branch: Option<&str>) -> io::Result<Outcome> {
        check_revision(onto)?;
        let mut args = vec!["-c", "core.editor=true", "rebase", "-q", onto];
        if let Some(branch) = branch {
            check_revision(branch)?;
            args.push(branch);
        }
        self.conflicting_rebase(&args)
    }

    fn conflicting_rebase(&self, args: &[&str]) -> io::Result<Outcome> {
        let output = self.git(args)?;
        if output.code == Some(0) {
            return Ok(Outcome::Done);
        }
        if self.operation_state()? == OperationState::Rebasing {
            Ok(Outcome::Conflicts)
        } else {
            Err(failure("rebase", &output))
        }
    }

    /// Continues a rebase after its conflicts were resolved and staged.
    pub fn continue_rebase(&self) -> io::Result<Outcome> {
        self.conflicting_rebase(&["-c", "core.editor=true", "rebase", "--continue"])
    }

    /// Abandons the operation in progress, restoring the state before it.
    pub fn abort(&self, state: OperationState) -> io::Result<()> {
        let command = match state {
            OperationState::None => return Ok(()),
            OperationState::Merging => "merge",
            OperationState::Rebasing => "rebase",
            OperationState::CherryPicking => "cherry-pick",
            OperationState::Reverting => "revert",
        };
        self.checked(&[command, "--abort"]).map(drop)
    }

    /// Deletes a branch on its remote (`origin/x` → `git push origin
    /// --delete x`).
    pub fn delete_remote_branch(&self, name: &str) -> io::Result<()> {
        check_revision(name)?;
        let (remote, branch) = name
            .split_once('/')
            .ok_or_else(|| invalid(format!("`{name}` is not a remote branch")))?;
        self.checked(&["push", "--quiet", remote, "--delete", branch])
            .map(drop)
    }

    /// Renames local branch `old` to `new`.
    pub fn rename_branch(&self, old: &str, new: &str) -> io::Result<()> {
        check_revision(old)?;
        if new.starts_with('-') || self.git(&["check-ref-format", "--branch", new])?.code != Some(0)
        {
            return Err(invalid(format!("`{new}` is not a valid branch name")));
        }
        self.checked(&["branch", "-m", old, new]).map(drop)
    }

    /// PyCharm's Update Project: fetches, then merges (or rebases onto)
    /// the current branch's upstream.
    pub fn update(&self, rebase: bool) -> io::Result<Outcome> {
        self.fetch()?;
        let upstream = self.git(&["rev-parse", "--abbrev-ref", "@{upstream}"])?;
        if upstream.code != Some(0) {
            return Err(invalid(
                "the current branch has no tracked branch to update from; push it first",
            ));
        }
        if rebase {
            self.conflicting_rebase(&["-c", "core.editor=true", "rebase", "-q", "@{upstream}"])
        } else {
            self.conflicting(&["merge", "--no-edit", "-q", "@{upstream}"])
        }
    }

    /// Fast-forwards a local branch that is not checked out to its
    /// upstream's state on the remote (fails if it diverged).
    pub fn update_branch(&self, branch: &str) -> io::Result<()> {
        check_revision(branch)?;
        let config = |key: &str| -> io::Result<Option<String>> {
            let key = format!("branch.{branch}.{key}");
            let output = self.git(&["config", "--get", &key])?;
            Ok((output.code == Some(0))
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned()))
        };
        let (Some(remote), Some(merge)) = (config("remote")?, config("merge")?) else {
            return Err(invalid(format!("{branch} has no tracked branch")));
        };
        let refspec = format!("{merge}:refs/heads/{branch}");
        self.checked(&["fetch", "-q", &remote, &refspec]).map(drop)
    }

    /// Where `branch` pushes: its upstream, else a same-named branch on
    /// `origin` (or the only remote), set as upstream by the push.
    pub fn push_target(&self, branch: &str) -> io::Result<PushTarget> {
        check_revision(branch)?;
        let config = |key: &str| -> io::Result<Option<String>> {
            let key = format!("branch.{branch}.{key}");
            let output = self.git(&["config", "--get", &key])?;
            Ok((output.code == Some(0))
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned()))
        };
        if let (Some(remote), Some(merge)) = (config("remote")?, config("merge")?)
            && remote != "."
        {
            return Ok(PushTarget {
                remote,
                branch: merge
                    .strip_prefix("refs/heads/")
                    .unwrap_or(&merge)
                    .to_owned(),
                new: false,
            });
        }
        let remotes = self.remotes()?;
        let remote = remotes
            .iter()
            .find(|r| *r == "origin")
            .or(remotes.first())
            .ok_or_else(|| invalid("the repository has no remote to push to"))?;
        Ok(PushTarget {
            remote: remote.clone(),
            branch: branch.to_owned(),
            new: true,
        })
    }

    /// Commits a push of `branch` to `target` would send, newest first, as
    /// `<short hash> <subject>` (at most 100).
    pub fn outgoing(&self, branch: &str, target: &PushTarget) -> io::Result<Vec<String>> {
        check_revision(branch)?;
        let remote_ref = format!("refs/remotes/{}/{}", target.remote, target.branch);
        let known = self
            .git(&["rev-parse", "--verify", "-q", &remote_ref])?
            .code
            == Some(0);
        let mut args = vec!["log", "-n100", "--format=%h %s", branch];
        let not_remotes = format!("--remotes={}", target.remote);
        if known {
            args.extend(["--not", &remote_ref]);
        } else {
            args.extend(["--not", &not_remotes]);
        }
        let output = self.git(&args)?;
        if output.code != Some(0) {
            // An unborn branch has nothing to push.
            return Ok(Vec::new());
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect())
    }

    /// Pushes `branch` to `target`, setting the upstream when it is new.
    pub fn push_branch(&self, branch: &str, target: &PushTarget) -> io::Result<()> {
        check_revision(branch)?;
        check_revision(&target.remote)?;
        let refspec = format!("refs/heads/{branch}:refs/heads/{}", target.branch);
        let mut args = vec!["push", "--quiet"];
        if target.new {
            args.push("-u");
        }
        args.extend([target.remote.as_str(), refspec.as_str()]);
        self.checked(&args).map(drop)
    }

    /// Every local branch with its upstream and ahead/behind counts.
    pub fn branch_tracking(&self) -> io::Result<Vec<BranchTracking>> {
        let output = self.checked(&[
            "for-each-ref",
            "--format=%(refname:short)%1f%(upstream:short)%1f%(upstream:track,nobracket)",
            "refs/heads",
        ])?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut fields = line.split('\x1f');
                let name = fields.next()?.to_owned();
                let upstream = fields.next().filter(|u| !u.is_empty()).map(str::to_owned);
                let track = fields.next().unwrap_or_default();
                let count = |word: &str| {
                    track
                        .split(", ")
                        .find_map(|part| part.strip_prefix(word)?.trim().parse().ok())
                        .unwrap_or(0)
                };
                Some(BranchTracking {
                    name,
                    upstream,
                    ahead: count("ahead "),
                    behind: count("behind "),
                })
            })
            .collect())
    }

    /// Branches checked out most recently (from HEAD's reflog), newest
    /// first, without duplicates; may include deleted branches.
    pub fn recent_branches(&self, limit: usize) -> io::Result<Vec<String>> {
        let output = self.git(&["reflog", "show", "-n500", "--format=%gs", "HEAD", "--"])?;
        if output.code != Some(0) {
            return Ok(Vec::new());
        }
        let mut recent: Vec<String> = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Some(rest) = line.strip_prefix("checkout: moving from ") else {
                continue;
            };
            let Some((_, to)) = rest.rsplit_once(" to ") else {
                continue;
            };
            if !recent.iter().any(|name| name == to) {
                recent.push(to.to_owned());
                if recent.len() == limit {
                    break;
                }
            }
        }
        Ok(recent)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use super::*;
    use crate::tests::{state, temp_repo};

    fn run(dir: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn entries(repo: &Repository, paths: &[&str]) -> Vec<Entry> {
        repo.status()
            .unwrap()
            .entries
            .into_iter()
            .filter(|entry| paths.contains(&entry.path.as_str()))
            .collect()
    }

    fn committed_files(dir: &std::path::Path) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["show", "--name-status", "--format=", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn commit_includes_only_the_chosen_files_and_keeps_other_staging() {
        let (dir, repo) = temp_repo("commit_only");
        // The first commit, with --only, in an unborn repository.
        fs::write(dir.join("a.txt"), "a\n").unwrap();
        fs::write(dir.join("b.txt"), "b\n").unwrap();
        fs::write(dir.join("c.txt"), "c\n").unwrap();
        let chosen = entries(&repo, &["a.txt", "b.txt"]);
        repo.commit_entries("first", &chosen, false).unwrap();
        assert_eq!(committed_files(&dir), "A\ta.txt\nA\tb.txt\n");
        assert_eq!(
            state(&repo),
            [("c.txt".into(), None, Some(Change::Untracked))]
        );

        // a: modified and staged, b: modified only, c: untracked, d: staged new.
        fs::write(dir.join("a.txt"), "a2\n").unwrap();
        run(&dir, &["add", "a.txt"]);
        fs::write(dir.join("b.txt"), "b2\n").unwrap();
        fs::write(dir.join("d.txt"), "d\n").unwrap();
        run(&dir, &["add", "d.txt"]);
        // Commit b and the untracked c only.
        let chosen = entries(&repo, &["b.txt", "c.txt"]);
        let summary = repo.commit_entries("second", &chosen, false).unwrap();
        assert!(summary.ends_with(" second"), "{summary}");
        assert_eq!(committed_files(&dir), "M\tb.txt\nA\tc.txt\n");
        assert_eq!(
            state(&repo),
            [
                ("a.txt".into(), Some(Change::Modified), None),
                ("d.txt".into(), Some(Change::Added), None),
            ],
            "what was staged for other files stays staged"
        );

        // Amend with one more file; the message changes too.
        let chosen = entries(&repo, &["d.txt"]);
        repo.commit_entries("second, amended", &chosen, true)
            .unwrap();
        assert_eq!(committed_files(&dir), "M\tb.txt\nA\tc.txt\nA\td.txt\n");
        // Amending the message only.
        repo.commit_entries("reworded", &[], true).unwrap();
        assert_eq!(committed_files(&dir), "M\tb.txt\nA\tc.txt\nA\td.txt\n");
        assert!(repo.head_summary().unwrap().ends_with(" reworded"));
        assert_eq!(
            state(&repo),
            [("a.txt".into(), Some(Change::Modified), None)]
        );

        // Deletions and an empty selection.
        fs::remove_file(dir.join("b.txt")).unwrap();
        let chosen = entries(&repo, &["b.txt"]);
        repo.commit_entries("drop b", &chosen, false).unwrap();
        assert_eq!(committed_files(&dir), "D\tb.txt\n");
        let err = repo.commit_entries("nothing", &[], false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rollback_returns_every_kind_of_change_to_head() {
        let (dir, repo) = temp_repo("rollback");
        fs::write(dir.join("m.txt"), "m\n").unwrap();
        fs::write(dir.join("d.txt"), "d\n").unwrap();
        fs::write(dir.join("r.txt"), "r\n").unwrap();
        run(&dir, &["add", "-A"]);
        run(&dir, &["commit", "-q", "-m", "base"]);

        fs::write(dir.join("m.txt"), "changed\n").unwrap();
        run(&dir, &["add", "m.txt"]);
        fs::write(dir.join("m.txt"), "changed again\n").unwrap();
        fs::remove_file(dir.join("d.txt")).unwrap();
        fs::write(dir.join("added.txt"), "new\n").unwrap();
        run(&dir, &["add", "added.txt"]);
        run(&dir, &["mv", "r.txt", "renamed.txt"]);
        fs::write(dir.join("untracked.txt"), "?\n").unwrap();

        let all = repo.status().unwrap().entries;
        assert_eq!(all.len(), 5, "{all:?}");
        repo.rollback(&all).unwrap();
        assert_eq!(
            state(&repo),
            [("added.txt".into(), None, Some(Change::Untracked))],
            "an added file leaves version control but stays on disk"
        );
        assert_eq!(fs::read_to_string(dir.join("m.txt")).unwrap(), "m\n");
        assert_eq!(fs::read_to_string(dir.join("d.txt")).unwrap(), "d\n");
        assert_eq!(fs::read_to_string(dir.join("r.txt")).unwrap(), "r\n");
        assert!(!dir.join("renamed.txt").exists());
        assert!(!dir.join("untracked.txt").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn branches_merge_rebase_rename_and_delete() {
        let (dir, repo) = temp_repo("branch_ops");
        fs::write(dir.join("f.txt"), "base\n").unwrap();
        run(&dir, &["add", "-A"]);
        run(&dir, &["commit", "-q", "-m", "base"]);

        // New branch from main, checked out, with its own commit.
        repo.create_branch("feature", Some("main"), true).unwrap();
        fs::write(dir.join("g.txt"), "feature\n").unwrap();
        run(&dir, &["add", "-A"]);
        run(&dir, &["commit", "-q", "-m", "feature work"]);
        run(&dir, &["switch", "-q", "main"]);
        assert_eq!(
            repo.recent_branches(5).unwrap(),
            ["main", "feature"],
            "newest checkout first"
        );

        // Unmerged: plain delete refuses, which the UI turns into a question.
        let err = repo.delete_branch("feature", false).unwrap_err();
        assert!(not_fully_merged(&err), "{err}");
        repo.rename_branch("feature", "topic").unwrap();
        assert!(repo.rename_branch("topic", "bad..name").is_err());

        // A clean merge.
        assert_eq!(repo.merge("topic").unwrap(), Outcome::Done);
        assert!(dir.join("g.txt").exists());
        repo.delete_branch("topic", false).unwrap();

        // A conflicting merge stops with conflicts, then aborts.
        repo.create_branch("other", None, false).unwrap();
        fs::write(dir.join("f.txt"), "main side\n").unwrap();
        run(&dir, &["commit", "-q", "-am", "main side"]);
        run(&dir, &["switch", "-q", "other"]);
        fs::write(dir.join("f.txt"), "other side\n").unwrap();
        run(&dir, &["commit", "-q", "-am", "other side"]);
        run(&dir, &["switch", "-q", "main"]);
        assert_eq!(repo.merge("other").unwrap(), Outcome::Conflicts);
        assert_eq!(repo.operation_state().unwrap(), OperationState::Merging);
        assert_eq!(
            state(&repo),
            [("f.txt".into(), None, Some(Change::Conflicted))]
        );
        // Committing needs the conflict resolved; then the merge commits.
        let conflicted = repo.status().unwrap().entries;
        assert!(repo.commit_entries("merge", &conflicted, false).is_err());
        fs::write(dir.join("f.txt"), "resolved\n").unwrap();
        repo.stage(&["f.txt".into()]).unwrap();
        let resolved = repo.status().unwrap().entries;
        repo.commit_entries("merge other", &resolved, false)
            .unwrap();
        assert_eq!(repo.operation_state().unwrap(), OperationState::None);
        let parents = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["rev-list", "--parents", "-n1", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&parents.stdout).split(' ').count(),
            3,
            "a merge commit"
        );

        // Checkout-and-rebase a branch onto main with a conflict, then abort.
        repo.create_branch("rebased", Some("main~1"), false)
            .unwrap();
        run(&dir, &["switch", "-q", "rebased"]);
        fs::write(dir.join("f.txt"), "rebased side\n").unwrap();
        run(&dir, &["commit", "-q", "-am", "rebased side"]);
        run(&dir, &["switch", "-q", "main"]);
        assert_eq!(
            repo.rebase("main", Some("rebased")).unwrap(),
            Outcome::Conflicts
        );
        assert_eq!(repo.operation_state().unwrap(), OperationState::Rebasing);
        // Resolve and continue.
        fs::write(dir.join("f.txt"), "rebase resolved\n").unwrap();
        repo.stage(&["f.txt".into()]).unwrap();
        assert_eq!(repo.continue_rebase().unwrap(), Outcome::Done);
        assert_eq!(repo.operation_state().unwrap(), OperationState::None);
        assert_eq!(repo.status().unwrap().branch.as_deref(), Some("rebased"));

        // A clean rebase of the current branch, and an abort.
        repo.create_branch("clean", Some("main"), true).unwrap();
        assert_eq!(repo.rebase("main", None).unwrap(), Outcome::Done);
        repo.create_branch("late", Some("main~2"), true).unwrap();
        fs::write(dir.join("f.txt"), "late side\n").unwrap();
        run(&dir, &["commit", "-q", "-am", "late side"]);
        assert_eq!(repo.rebase("main", None).unwrap(), Outcome::Conflicts);
        repo.abort(OperationState::Rebasing).unwrap();
        assert_eq!(repo.operation_state().unwrap(), OperationState::None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn push_sets_the_upstream_once_and_update_merges_or_rebases() {
        let (dir, repo) = temp_repo("push_update");
        // git's own `C:/…` form: canonicalized `\\?\` paths are not valid
        // remote URLs on Windows.
        let remote = std::path::PathBuf::from(format!("{}_remote.git", repo.root()));
        fs::remove_dir_all(&remote).ok();
        run(
            dir.parent().unwrap(),
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                remote.to_str().unwrap(),
            ],
        );
        assert!(repo.push_target("main").is_err(), "no remote yet");
        run(&dir, &["remote", "add", "origin", remote.to_str().unwrap()]);
        fs::write(dir.join("f.txt"), "one\n").unwrap();
        run(&dir, &["add", "-A"]);
        run(&dir, &["commit", "-q", "-m", "one"]);
        fs::write(dir.join("f.txt"), "two\n").unwrap();
        run(&dir, &["commit", "-q", "-am", "two"]);

        let target = repo.push_target("main").unwrap();
        assert_eq!(
            target,
            PushTarget {
                remote: "origin".into(),
                branch: "main".into(),
                new: true
            }
        );
        let outgoing = repo.outgoing("main", &target).unwrap();
        assert_eq!(outgoing.len(), 2);
        assert!(outgoing[0].ends_with(" two"), "{outgoing:?}");
        repo.push_branch("main", &target).unwrap();
        let status = repo.status().unwrap();
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));

        // With the upstream set, only new commits are outgoing.
        fs::write(dir.join("f.txt"), "three\n").unwrap();
        run(&dir, &["commit", "-q", "-am", "three"]);
        let target = repo.push_target("main").unwrap();
        assert!(!target.new);
        assert_eq!(repo.outgoing("main", &target).unwrap().len(), 1);
        let tracking = repo.branch_tracking().unwrap();
        assert_eq!(
            tracking,
            [BranchTracking {
                name: "main".into(),
                upstream: Some("origin/main".into()),
                ahead: 1,
                behind: 0
            }]
        );
        repo.push_branch("main", &target).unwrap();
        assert!(repo.outgoing("main", &target).unwrap().is_empty());

        // Another clone pushes; Update merges it in (and rebases next time).
        let other = std::path::PathBuf::from(format!("{}_other", repo.root()));
        fs::remove_dir_all(&other).ok();
        run(
            dir.parent().unwrap(),
            &[
                "clone",
                "-q",
                remote.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        run(&other, &["config", "user.name", "Other"]);
        run(&other, &["config", "user.email", "other@example.com"]);
        fs::write(other.join("o.txt"), "other\n").unwrap();
        run(&other, &["add", "-A"]);
        run(&other, &["commit", "-q", "-m", "from other"]);
        run(&other, &["push", "-q"]);
        fs::write(dir.join("local.txt"), "local\n").unwrap();
        run(&dir, &["add", "-A"]);
        run(&dir, &["commit", "-q", "-m", "local"]);
        assert_eq!(repo.update(true).unwrap(), Outcome::Done);
        assert!(dir.join("o.txt").exists());
        let status = repo.status().unwrap();
        assert_eq!((status.ahead, status.behind), (1, 0), "rebased on top");

        // A non-current tracked branch fast-forwards with Update.
        repo.create_branch("behind", Some("origin/main~1"), false)
            .unwrap();
        run(
            &dir,
            &["branch", "-q", "--set-upstream-to=origin/main", "behind"],
        );
        repo.update_branch("behind").unwrap();
        let tracking = repo.branch_tracking().unwrap();
        let behind = tracking.iter().find(|b| b.name == "behind").unwrap();
        assert_eq!((behind.ahead, behind.behind), (0, 0));

        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&remote).unwrap();
        fs::remove_dir_all(&other).unwrap();
    }

    #[test]
    fn ignored_paths_are_anchored_escaped_and_not_repeated() {
        assert_eq!(
            ignore_entries("target\n", &["build/out.log".into()]),
            "target\n/build/out.log\n"
        );
        assert_eq!(
            ignore_entries("a", &["#x[1]*.txt".into(), "a".into()]),
            "a\n/\\#x\\[1]\\*.txt\n/a\n"
        );
        assert_eq!(ignore_entries("/a\n", &["a".into()]), "/a\n");
        assert_eq!(ignore_entries("", &["s ".into()]), "/s\\ \n");
    }
}
