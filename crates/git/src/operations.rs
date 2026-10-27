//! Branches, remotes and worktrees.

use std::io;

use serde::Serialize;

use crate::{Ref, RefKind, Repository, Status, check_revision};

/// A working tree of the repository (`git worktree list`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Worktree {
    /// As git prints it (`/`-separated, also on Windows).
    pub path: String,
    /// Checked-out commit; `None` for bare repositories.
    pub head: Option<String>,
    /// Checked-out branch (short name); `None` when detached or bare.
    pub branch: Option<String>,
    pub bare: bool,
    pub locked: bool,
    /// Its directory is gone; `git worktree prune` would remove it.
    pub prunable: bool,
    /// The worktree this repository was opened from.
    pub current: bool,
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

impl Repository {
    /// Fails unless `name` is a valid new branch name.
    fn check_branch_name(&self, name: &str) -> io::Result<()> {
        if name.starts_with('-')
            || self.git(&["check-ref-format", "--branch", name])?.code != Some(0)
        {
            return Err(invalid(format!("`{name}` is not a valid branch name")));
        }
        Ok(())
    }

    /// Creates branch `name` at `start` (default HEAD), switching to it
    /// when `switch` is set.
    pub fn create_branch(&self, name: &str, start: Option<&str>, switch: bool) -> io::Result<()> {
        self.check_branch_name(name)?;
        let mut args = if switch {
            vec!["switch", "-q", "-c", name]
        } else {
            vec!["branch", name]
        };
        if let Some(start) = start {
            check_revision(start)?;
            args.push(start);
        }
        self.checked(&args).map(drop)
    }

    /// Checks out a branch. A remote branch (`origin/x`) becomes a local
    /// branch `x` tracking it, unless `x` already exists.
    pub fn switch_branch(&self, branch: &Ref) -> io::Result<()> {
        check_revision(&branch.name)?;
        match branch.kind {
            RefKind::Branch => self.checked(&["switch", "-q", &branch.name]).map(drop),
            RefKind::Remote => {
                let local = branch
                    .name
                    .split_once('/')
                    .map_or(&*branch.name, |(_, name)| name);
                let exists = self
                    .refs()?
                    .iter()
                    .any(|r| r.kind == RefKind::Branch && r.name == local);
                if exists {
                    self.checked(&["switch", "-q", local]).map(drop)
                } else {
                    self.checked(&["switch", "-q", "--track", &branch.name])
                        .map(drop)
                }
            }
            RefKind::Tag => self
                .checked(&["switch", "-q", "--detach", &branch.name])
                .map(drop),
        }
    }

    /// Deletes a local branch; without `force`, only if it is merged.
    pub fn delete_branch(&self, name: &str, force: bool) -> io::Result<()> {
        check_revision(name)?;
        self.checked(&["branch", if force { "-D" } else { "-d" }, name])
            .map(drop)
    }

    pub fn remotes(&self) -> io::Result<Vec<String>> {
        let output = self.checked(&["remote"])?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect())
    }

    /// Fetches every remote, pruning deleted remote branches.
    pub fn fetch(&self) -> io::Result<()> {
        self.checked(&["fetch", "--all", "--prune", "--quiet"])
            .map(drop)
    }

    /// Pulls the current branch's upstream, only if that fast-forwards (a
    /// diverged branch needs a merge or rebase decision).
    pub fn pull(&self) -> io::Result<()> {
        self.checked(&["pull", "--ff-only", "--quiet"]).map(drop)
    }

    /// Pushes the current branch to its upstream, setting it on `origin`
    /// (or the only remote) the first time.
    pub fn push(&self, status: &Status) -> io::Result<()> {
        let branch = status
            .branch
            .as_deref()
            .ok_or_else(|| invalid("cannot push a detached HEAD; create a branch first".into()))?;
        let target = self.push_target(branch)?;
        self.push_branch(branch, &target)
    }

    pub fn worktrees(&self) -> io::Result<Vec<Worktree>> {
        let output = self.checked(&["worktree", "list", "--porcelain"])?;
        let text = String::from_utf8_lossy(&output.stdout);
        Ok(parse_worktrees(&text, self.root()))
    }

    /// A sibling of the repository named after it and `branch`
    /// (`/work/app` + `fix/x` → `/work/app-fix-x`).
    pub fn default_worktree_path(&self, branch: &str) -> String {
        let root = self.root().trim_end_matches('/');
        let (parent, name) = root.rsplit_once('/').unwrap_or(("", root));
        let suffix: String = branch
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        format!("{parent}/{name}-{suffix}")
    }

    /// Adds a worktree at `path` checking out `branch`, creating the branch
    /// (at `start`, default HEAD) when `create` is set.
    pub fn add_worktree(
        &self,
        path: &str,
        branch: &str,
        create: bool,
        start: Option<&str>,
    ) -> io::Result<()> {
        if path.starts_with('-') {
            return Err(invalid(format!("invalid worktree path `{path}`")));
        }
        if create {
            self.check_branch_name(branch)?;
            let mut args = vec!["worktree", "add", "-q", "-b", branch, "--", path];
            if let Some(start) = start {
                check_revision(start)?;
                args.push(start);
            }
            self.checked(&args).map(drop)
        } else {
            check_revision(branch)?;
            self.checked(&["worktree", "add", "-q", "--", path, branch])
                .map(drop)
        }
    }

    /// Removes a worktree; without `force`, only a clean one.
    pub fn remove_worktree(&self, path: &str, force: bool) -> io::Result<()> {
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.extend(["--", path]);
        self.checked(&args).map(drop)
    }
}

/// `git worktree list --porcelain`: blank-line separated records of
/// `worktree <path>`, `HEAD <id>`, `branch refs/heads/x`, `detached`,
/// `bare`, `locked [reason]`, `prunable [reason]`.
fn parse_worktrees(text: &str, current_root: &str) -> Vec<Worktree> {
    // Windows paths (`C:/…`) compare case-insensitively.
    let normalize = |path: &str| {
        let path = path.trim_end_matches('/');
        if path.as_bytes().get(1) == Some(&b':') {
            path.to_lowercase()
        } else {
            path.to_owned()
        }
    };
    text.split("\n\n")
        .filter_map(|record| {
            let mut worktree = Worktree {
                path: String::new(),
                head: None,
                branch: None,
                bare: false,
                locked: false,
                prunable: false,
                current: false,
            };
            for line in record.lines() {
                let (key, value) = line.split_once(' ').unwrap_or((line, ""));
                match key {
                    "worktree" => worktree.path = value.to_owned(),
                    "HEAD" => worktree.head = Some(value.to_owned()),
                    "branch" => {
                        worktree.branch = Some(
                            value
                                .strip_prefix("refs/heads/")
                                .unwrap_or(value)
                                .to_owned(),
                        )
                    }
                    "bare" => worktree.bare = true,
                    "locked" => worktree.locked = true,
                    "prunable" => worktree.prunable = true,
                    _ => {}
                }
            }
            if worktree.path.is_empty() {
                return None;
            }
            // Paths compare case-insensitively: Windows paths may differ in case.
            worktree.current = normalize(&worktree.path) == normalize(current_root);
            Some(worktree)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_records_are_parsed_and_the_current_one_marked() {
        let text = "worktree /work/app\nHEAD abc\nbranch refs/heads/main\n\n\
                    worktree /work/app-fix\nHEAD def\ndetached\nlocked reason\n\n\
                    worktree /gone\nHEAD 123\nbranch refs/heads/old\nprunable gitdir file points to non-existent location\n";
        let worktrees = parse_worktrees(text, "/work/app-fix");
        let summary: Vec<(&str, Option<&str>, bool, bool, bool)> = worktrees
            .iter()
            .map(|w| {
                (
                    w.path.as_str(),
                    w.branch.as_deref(),
                    w.current,
                    w.locked,
                    w.prunable,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("/work/app", Some("main"), false, false, false),
                ("/work/app-fix", None, true, true, false),
                ("/gone", Some("old"), false, false, true),
            ]
        );
    }
}
