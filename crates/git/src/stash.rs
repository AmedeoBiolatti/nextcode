//! Git's stash (PyCharm's Stash Changes / Unstash Changes): list, push,
//! apply or pop (conflicts reported like cherry-picks), drop, and the files
//! of a stash with the sides their diffs compare.

use std::io;

use serde::Serialize;

use crate::{ApplyOutcome, Change, FileChange, Repository, Side, failure, parse_name_status};

/// One entry of the stash list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stash {
    /// Position in the list: `stash@{index}`, newest first.
    pub index: usize,
    /// The stash commit's full id.
    pub id: String,
    /// The branch it was made on; `None` on a detached HEAD.
    pub branch: Option<String>,
    /// The message given, else git's `<hash> <subject>` of the commit it
    /// was made on.
    pub message: String,
    /// Unix seconds.
    pub date: i64,
}

/// A file of a stash and the two sides its diff compares: the commit the
/// stash was made on → the stashed working tree, or nothing → the stashed
/// untracked file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StashFile {
    #[serde(flatten)]
    pub change: FileChange,
    pub old: Side,
    pub new: Side,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn stash_ref(index: usize) -> String {
    format!("stash@{{{index}}}")
}

/// `On <branch>: <message>` or `WIP on <branch>: <hash> <subject>`.
fn parse_subject(subject: &str) -> (Option<String>, String) {
    let rest = subject
        .strip_prefix("WIP on ")
        .or_else(|| subject.strip_prefix("On "));
    match rest.and_then(|rest| rest.split_once(": ")) {
        // Branch names cannot contain `:`.
        Some((branch, message)) => (
            (branch != "(no branch)").then(|| branch.to_owned()),
            message.to_owned(),
        ),
        None => (None, subject.to_owned()),
    }
}

impl Repository {
    /// The stash list, newest first.
    pub fn stash_list(&self) -> io::Result<Vec<Stash>> {
        let output = self.checked(&["stash", "list", "--format=%gd%x1f%H%x1f%ct%x1f%gs"])?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut fields = line.splitn(4, '\x1f');
                let (name, id, date, subject) = (
                    fields.next()?,
                    fields.next()?,
                    fields.next()?,
                    fields.next()?,
                );
                let index = name
                    .strip_prefix("stash@{")?
                    .strip_suffix('}')?
                    .parse()
                    .ok()?;
                let (branch, message) = parse_subject(subject);
                Some(Stash {
                    index,
                    id: id.to_owned(),
                    branch,
                    message,
                    date: date.parse().unwrap_or_default(),
                })
            })
            .collect())
    }

    fn stash_top(&self) -> io::Result<Option<String>> {
        let output = self.git(&["rev-parse", "-q", "--verify", "refs/stash"])?;
        Ok((output.code == Some(0))
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned()))
    }

    /// Stashes local changes (only `paths`' when given) and reverts them
    /// in the working tree; `include_untracked` stashes and removes
    /// untracked files too. An empty message lets git describe the stash.
    /// Fails when there is nothing to stash.
    pub fn stash_push(
        &self,
        message: &str,
        include_untracked: bool,
        paths: Option<&[String]>,
    ) -> io::Result<()> {
        if paths.is_some_and(<[String]>::is_empty) {
            return Err(invalid("no files are selected to stash"));
        }
        let before = self.stash_top()?;
        let mut args = vec!["stash", "push", "-q"];
        if include_untracked {
            args.push("--include-untracked");
        }
        let message = message.trim();
        if !message.is_empty() {
            args.extend(["-m", message]);
        }
        let args = match paths {
            Some(paths) => Self::with_paths(&args, paths),
            None => args,
        };
        self.checked(&args)?;
        if self.stash_top()? == before {
            return Err(invalid("no local changes to stash"));
        }
        Ok(())
    }

    /// Applies `stash@{index}` to the working tree; `pop` drops it once
    /// applied without conflicts (git keeps it otherwise), and
    /// `reinstate_index` restores what was staged as staged.
    pub fn stash_apply(
        &self,
        index: usize,
        pop: bool,
        reinstate_index: bool,
    ) -> io::Result<ApplyOutcome> {
        let name = stash_ref(index);
        let mut args = vec!["stash", if pop { "pop" } else { "apply" }, "-q"];
        if reinstate_index {
            args.push("--index");
        }
        args.push(&name);
        let output = self.git(&args)?;
        if output.code == Some(0) {
            return Ok(ApplyOutcome::Done { empty: 0 });
        }
        let paths = self.conflicted_paths()?;
        if paths.is_empty() {
            Err(failure("stash", &output))
        } else {
            Ok(ApplyOutcome::Conflicts { paths })
        }
    }

    /// Deletes `stash@{index}`.
    pub fn stash_drop(&self, index: usize) -> io::Result<()> {
        self.checked(&["stash", "drop", "-q", &stash_ref(index)])
            .map(drop)
    }

    /// The files `stash@{index}` holds: its working-tree changes against
    /// the commit it was made on, then its untracked files. The sides name
    /// the stash by (abbreviated) commit id, so they stay right when the
    /// list shifts and read well as diff titles.
    pub fn stash_changes(&self, index: usize) -> io::Result<Vec<StashFile>> {
        let full = self.resolve(&stash_ref(index))?;
        let id = full[..full.len().min(12)].to_owned();
        let base = format!("{id}^1");
        let output = self.checked(&["diff", "--name-status", "-z", "-M", &base, &id, "--"])?;
        let mut files: Vec<StashFile> = parse_name_status(&output.stdout)?
            .into_iter()
            .map(|change| StashFile {
                change,
                old: Side::Revision(base.clone()),
                new: Side::Revision(id.clone()),
            })
            .collect();
        let untracked = format!("{id}^3");
        if self.git(&["rev-parse", "-q", "--verify", &untracked])?.code == Some(0) {
            let output = self.checked(&["ls-tree", "-r", "-z", "--name-only", &untracked])?;
            files.extend(
                output
                    .stdout
                    .split(|&byte| byte == 0)
                    .filter(|path| !path.is_empty())
                    .map(|path| StashFile {
                        change: FileChange {
                            path: String::from_utf8_lossy(path).into_owned(),
                            orig_path: None,
                            change: Change::Untracked,
                        },
                        old: Side::Empty,
                        new: Side::Revision(untracked.clone()),
                    }),
            );
        }
        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::tests::{state, temp_repo};

    fn commit_all(repo: &Repository, message: &str) {
        repo.stage(&[".".into()]).unwrap();
        repo.commit(message, false).unwrap();
    }

    #[test]
    fn subjects_give_branch_and_message() {
        assert_eq!(
            parse_subject("On main: fix it"),
            (Some("main".into()), "fix it".into())
        );
        assert_eq!(
            parse_subject("WIP on feature/x: 1a2b3c4 subject: with colon"),
            (
                Some("feature/x".into()),
                "1a2b3c4 subject: with colon".into()
            )
        );
        assert_eq!(
            parse_subject("On (no branch): detached"),
            (None, "detached".into())
        );
    }

    #[test]
    fn stash_push_apply_pop_and_drop_round_trip_with_untracked_files() {
        let (dir, repo) = temp_repo("stash_round_trip");
        fs::write(dir.join("a.txt"), "one\n").unwrap();
        commit_all(&repo, "first");
        assert!(repo.stash_list().unwrap().is_empty());
        assert!(
            repo.stash_push("", false, None)
                .unwrap_err()
                .to_string()
                .contains("no local changes")
        );

        fs::write(dir.join("a.txt"), "two\n").unwrap();
        fs::write(dir.join("new.txt"), "new\n").unwrap();
        repo.stash_push("my work", true, None).unwrap();
        assert!(state(&repo).is_empty(), "everything stashed");
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
        assert!(!dir.join("new.txt").exists());

        let list = repo.stash_list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].index, 0);
        assert_eq!(list[0].message, "my work");
        assert_eq!(list[0].branch.as_deref(), Some("main"));
        assert!(list[0].date > 0);

        let files = repo.stash_changes(0).unwrap();
        let summary: Vec<(&str, Change, &Side)> = files
            .iter()
            .map(|file| (file.change.path.as_str(), file.change.change, &file.old))
            .collect();
        assert_eq!(
            summary,
            [
                (
                    "a.txt",
                    Change::Modified,
                    &Side::Revision(format!("{}^1", &list[0].id[..12]))
                ),
                ("new.txt", Change::Untracked, &Side::Empty),
            ]
        );
        // The stashed untracked file reads from the stash's third parent.
        let diff = repo
            .revision_diff(&files[1].old, &files[1].new, &files[1].change, 3, || {
                Ok(None)
            })
            .unwrap();
        assert_eq!(diff.added, 1);

        // Apply keeps the stash; pop removes it.
        assert_eq!(
            repo.stash_apply(0, false, false).unwrap(),
            ApplyOutcome::Done { empty: 0 }
        );
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "two\n");
        assert_eq!(fs::read_to_string(dir.join("new.txt")).unwrap(), "new\n");
        assert_eq!(repo.stash_list().unwrap().len(), 1);
        repo.rollback(&repo.status().unwrap().entries).unwrap();
        assert!(state(&repo).is_empty());
        repo.stash_apply(0, true, false).unwrap();
        assert!(repo.stash_list().unwrap().is_empty());
        assert_eq!(fs::read_to_string(dir.join("new.txt")).unwrap(), "new\n");

        // Without untracked files, only tracked changes go; then drop.
        repo.stash_push("", false, None).unwrap();
        assert_eq!(
            state(&repo),
            [("new.txt".to_owned(), None, Some(Change::Untracked))]
        );
        let list = repo.stash_list().unwrap();
        assert!(list[0].message.ends_with("first"), "{:?}", list[0]);
        repo.stash_drop(0).unwrap();
        assert!(repo.stash_list().unwrap().is_empty());
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
    }

    #[test]
    fn stashing_chosen_paths_and_reinstating_the_index() {
        let (dir, repo) = temp_repo("stash_paths");
        fs::write(dir.join("a.txt"), "a\n").unwrap();
        fs::write(dir.join("b.txt"), "b\n").unwrap();
        commit_all(&repo, "first");
        fs::write(dir.join("a.txt"), "a2\n").unwrap();
        fs::write(dir.join("b.txt"), "b2\n").unwrap();
        repo.stage(&["a.txt".into()]).unwrap();
        repo.stash_push("only a", false, Some(&["a.txt".into()]))
            .unwrap();
        assert_eq!(
            state(&repo),
            [("b.txt".to_owned(), None, Some(Change::Modified))]
        );
        repo.stash_apply(0, true, true).unwrap();
        assert_eq!(
            state(&repo),
            [
                ("a.txt".to_owned(), Some(Change::Modified), None),
                ("b.txt".to_owned(), None, Some(Change::Modified)),
            ]
        );
        assert!(repo.stash_push("", false, Some(&[])).is_err());
    }

    #[test]
    fn applying_a_stash_over_a_conflicting_commit_reports_the_paths() {
        let (dir, repo) = temp_repo("stash_conflict");
        fs::write(dir.join("a.txt"), "one\n").unwrap();
        commit_all(&repo, "first");
        fs::write(dir.join("a.txt"), "stashed\n").unwrap();
        repo.stash_push("", false, None).unwrap();
        fs::write(dir.join("a.txt"), "committed\n").unwrap();
        commit_all(&repo, "second");
        assert_eq!(
            repo.stash_apply(0, true, false).unwrap(),
            ApplyOutcome::Conflicts {
                paths: vec!["a.txt".into()]
            }
        );
        // Pop keeps the stash when it conflicts.
        assert_eq!(repo.stash_list().unwrap().len(), 1);
        assert_eq!(
            state(&repo),
            [("a.txt".to_owned(), None, Some(Change::Conflicted))]
        );
        assert!(repo.stash_drop(5).is_err());
    }
}
