//! The repository log as the Git tool window lists it: filters (branches,
//! text, authors, dates, paths) turned into `git log` arguments, pages of
//! commits, the changes of a commit selection, and what a commit belongs to.

use std::io;

use crate::{
    Commit, FileChange, GraphRow, Repository, Side, check_revision, failure,
    history::{self, LOG_FORMAT},
    parse_name_status,
};

/// What the log lists. The default lists every commit of every branch,
/// remote branch and tag (plus a detached HEAD).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogFilter {
    /// Branches (or other revisions) whose history is listed; empty: all.
    pub revisions: Vec<String>,
    /// Case-insensitive text found in the subject, the author's name or
    /// email, or the start of the commit id.
    pub text: String,
    /// Commits by any of these authors (case-insensitive substrings of the
    /// name or email).
    pub authors: Vec<String>,
    /// Commit time bounds (inclusive, seconds since the Unix epoch), as
    /// `git log --since/--until` read the committer date.
    pub since: Option<i64>,
    pub until: Option<i64>,
    /// Repository-relative paths: commits touching any of them, with the
    /// graph simplified to those commits.
    pub paths: Vec<String>,
}

/// Commits git lists per run while a text filter looks for matches.
const SCAN_CHUNK: usize = 1000;

/// One page of the log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogPage {
    pub commits: Vec<Commit>,
    /// The `skip` that continues after this page; `None` once git has
    /// listed everything.
    pub next: Option<usize>,
}

/// The files a selection of commits changed together: from the oldest
/// commit's first parent (`base`) to the newest commit (`target`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionChanges {
    /// `Side::Empty` when the oldest commit is a root commit.
    pub base: Side,
    pub target: Side,
    pub changes: Vec<FileChange>,
}

impl LogFilter {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Whether the text, author or date filters pick commits whose parents
    /// may be left out, so lanes cannot connect them; see [`Self::graph`].
    pub fn hides_parents(&self) -> bool {
        !self.text.trim().is_empty()
            || !self.authors.is_empty()
            || self.since.is_some()
            || self.until.is_some()
    }

    /// `git log` arguments listing `count` commits after the first `skip`.
    /// The text filter is applied to the result ([`Self::matches`]): git
    /// cannot match a subject, an author or a hash in one pattern.
    pub fn args(&self, skip: usize, count: usize) -> io::Result<Vec<String>> {
        let mut args: Vec<String> = ["log", "--topo-order", "--parents", LOG_FORMAT]
            .map(String::from)
            .to_vec();
        args.push(format!("--skip={skip}"));
        args.push(format!("-n{count}"));
        if !self.authors.is_empty() {
            args.extend(["--fixed-strings", "--regexp-ignore-case"].map(String::from));
            args.extend(
                self.authors
                    .iter()
                    .map(|author| format!("--author={author}")),
            );
        }
        if let Some(since) = self.since {
            args.push(format!("--since=@{since}"));
        }
        if let Some(until) = self.until {
            args.push(format!("--until=@{until}"));
        }
        if self.revisions.is_empty() {
            args.extend(["--branches", "--remotes", "--tags", "HEAD"].map(String::from));
        } else {
            for revision in &self.revisions {
                check_revision(revision)?;
                args.push(revision.clone());
            }
        }
        args.push("--".into());
        args.extend(self.paths.iter().cloned());
        Ok(args)
    }

    /// Whether `commit` matches the text filter.
    pub fn matches(&self, commit: &Commit) -> bool {
        let text = self.text.trim().to_lowercase();
        text.is_empty()
            || commit.id.starts_with(&text)
            || [&commit.subject, &commit.author, &commit.email]
                .iter()
                .any(|field| field.to_lowercase().contains(&text))
    }

    /// The graph rows of `commits` listed with this filter: lanes, or a
    /// single line when the filter hides parents.
    pub fn graph(&self, commits: &[Commit]) -> Vec<GraphRow> {
        if !self.hides_parents() {
            return history::graph(commits);
        }
        commits
            .iter()
            .enumerate()
            .map(|(index, commit)| GraphRow {
                column: 0,
                top: if index > 0 { vec![(0, 0)] } else { Vec::new() },
                bottom: if commit.parents.is_empty() {
                    Vec::new()
                } else {
                    vec![(0, 0)]
                },
                width: 1,
            })
            .collect()
    }
}

impl Repository {
    /// Up to `limit` commits matching `filter` (newest first in graph
    /// order), starting after the first `skip` commits git lists. With a
    /// text filter git's output is scanned in chunks until `limit` match,
    /// so a page may hold a few more. Empty before the first commit.
    pub fn log_page(&self, filter: &LogFilter, skip: usize, limit: usize) -> io::Result<LogPage> {
        let filtering = !filter.text.trim().is_empty();
        let chunk = if filtering {
            limit.max(SCAN_CHUNK)
        } else {
            limit
        };
        let mut page = LogPage {
            commits: Vec::new(),
            next: Some(skip),
        };
        while let Some(skip) = page.next {
            if page.commits.len() >= limit {
                break;
            }
            let args = filter.args(skip, chunk)?;
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let output = self.git(&args)?;
            if output.code != Some(0) {
                // A repository without commits has no HEAD to log.
                let unborn = self.git(&["rev-parse", "--verify", "-q", "HEAD"])?.code != Some(0);
                if unborn && filter.revisions.is_empty() {
                    return Ok(LogPage::default());
                }
                return Err(failure("log", &output));
            }
            let commits = history::parse_log(&output.stdout)?;
            let listed = commits.len();
            page.next = (listed == chunk).then_some(skip + listed);
            page.commits
                .extend(commits.into_iter().filter(|commit| filter.matches(commit)));
        }
        Ok(page)
    }

    /// The files changed from `oldest`'s first parent to `newest`: one
    /// commit's changes when they are the same commit.
    pub fn selection_changes(
        &self,
        newest: &Commit,
        oldest: &Commit,
    ) -> io::Result<SelectionChanges> {
        let target = Side::Revision(newest.id.clone());
        let (base, from) = match oldest.parents.first() {
            Some(parent) => (Side::Revision(parent.clone()), parent.clone()),
            None => (Side::Empty, self.empty_tree()?),
        };
        check_revision(&from)?;
        check_revision(&newest.id)?;
        let output =
            self.checked(&["diff", "--name-status", "-z", "-M", &from, &newest.id, "--"])?;
        Ok(SelectionChanges {
            base,
            target,
            changes: parse_name_status(&output.stdout)?,
        })
    }

    /// The id of the empty tree in this repository's hash format.
    fn empty_tree(&self) -> io::Result<String> {
        let output =
            self.checked_with_input(&["hash-object", "-t", "tree", "--stdin"], Some(&[]))?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// The commit's full message (subject and body).
    pub fn commit_message(&self, id: &str) -> io::Result<String> {
        check_revision(id)?;
        let output = self.checked(&["log", "-1", "--format=%B", id])?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned())
    }

    /// Local and remote branches containing `id` (short names).
    pub fn branches_containing(&self, id: &str) -> io::Result<Vec<String>> {
        check_revision(id)?;
        let output = self.checked(&[
            "for-each-ref",
            "--contains",
            id,
            "--format=%(refname)",
            "refs/heads",
            "refs/remotes",
        ])?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|name| !name.ends_with("/HEAD"))
            .filter_map(|name| {
                name.strip_prefix("refs/heads/")
                    .or_else(|| name.strip_prefix("refs/remotes/"))
            })
            .map(str::to_owned)
            .collect())
    }

    /// The configured `user.name`, if any (the log's "me" author).
    pub fn user_name(&self) -> io::Result<Option<String>> {
        let output = self.git(&["config", "user.name"])?;
        let name = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        Ok((output.code == Some(0) && !name.is_empty()).then_some(name))
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, process::Command};

    use super::*;
    use crate::{Change, tests::temp_repo};

    const DAY: i64 = 86_400;
    const T0: i64 = 1_600_000_000;

    /// Runs git in `dir` as `author` at `time` (author and committer).
    fn git_as(dir: &Path, author: &str, time: i64, args: &[&str]) {
        let date = format!("@{time} +0000");
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", author)
            .env(
                "GIT_AUTHOR_EMAIL",
                format!("{}@example.com", author.to_lowercase()),
            )
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn commit_as(dir: &Path, file: &str, author: &str, time: i64, message: &[&str]) {
        let path = dir.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = fs::read_to_string(&path).unwrap_or_default();
        text.push_str(message[0]);
        text.push('\n');
        fs::write(&path, text).unwrap();
        git_as(dir, author, time, &["add", "-A"]);
        let mut args = vec!["commit", "-q"];
        for paragraph in message {
            args.extend(["-m", paragraph]);
        }
        git_as(dir, author, time, &args);
    }

    /// main: 12 commits by Alice (even) and Bob (odd), the sixth being
    /// "fix: crash"; feature (from main 11): 3 commits by Carol in
    /// `lib/`; then main 12 (with a body) and a merge of feature by Dave.
    fn history(name: &str) -> (std::path::PathBuf, Repository) {
        let (dir, repo) = temp_repo(name);
        for n in 0..12 {
            let author = if n % 2 == 0 { "Alice" } else { "Bob" };
            let subject = if n == 5 {
                "fix: crash".to_owned()
            } else {
                format!("main {n}")
            };
            commit_as(&dir, "main.txt", author, T0 + n * DAY, &[&subject]);
        }
        git_as(&dir, "Test", T0, &["switch", "-q", "-c", "feature"]);
        for n in 0..3 {
            let subject = format!("feature {n}");
            commit_as(&dir, "lib/x.txt", "Carol", T0 + (12 + n) * DAY, &[&subject]);
        }
        git_as(&dir, "Test", T0, &["switch", "-q", "main"]);
        commit_as(
            &dir,
            "main.txt",
            "Bob",
            T0 + 15 * DAY,
            &["main 12", "body line"],
        );
        git_as(
            &dir,
            "Dave",
            T0 + 16 * DAY,
            &["merge", "-q", "--no-ff", "-m", "Merge feature", "feature"],
        );
        (dir, repo)
    }

    fn subjects(commits: &[Commit]) -> Vec<&str> {
        commits.iter().map(|c| c.subject.as_str()).collect()
    }

    #[test]
    fn filters_become_log_arguments() {
        let default = LogFilter::default().args(0, 100).unwrap();
        assert_eq!(
            default[4..],
            [
                "--skip=0",
                "-n100",
                "--branches",
                "--remotes",
                "--tags",
                "HEAD",
                "--"
            ]
        );
        let filter = LogFilter {
            revisions: vec!["main".into(), "origin/dev".into()],
            text: "ignored by git".into(),
            authors: vec!["Ada".into(), "bob@x".into()],
            since: Some(100),
            until: Some(200),
            paths: vec!["src/a b.rs".into()],
        };
        assert_eq!(
            filter.args(40, 20).unwrap()[4..],
            [
                "--skip=40",
                "-n20",
                "--fixed-strings",
                "--regexp-ignore-case",
                "--author=Ada",
                "--author=bob@x",
                "--since=@100",
                "--until=@200",
                "main",
                "origin/dev",
                "--",
                "src/a b.rs",
            ]
        );
        assert!(filter.hides_parents());
        assert!(
            !LogFilter {
                paths: vec!["a".into()],
                revisions: vec!["main".into()],
                ..LogFilter::default()
            }
            .hides_parents()
        );
        let option = LogFilter {
            revisions: vec!["--output=x".into()],
            ..LogFilter::default()
        };
        assert!(option.args(0, 1).is_err());
    }

    #[test]
    fn pages_continue_where_the_previous_one_ended() {
        let (dir, repo) = history("log_pages");
        let filter = LogFilter::default();
        let all = repo.log_page(&filter, 0, 1000).unwrap();
        assert_eq!(all.commits.len(), 17);
        assert_eq!(all.next, None);
        assert_eq!(all.commits[0].subject, "Merge feature");
        let mut paged = Vec::new();
        let mut nexts = Vec::new();
        let mut skip = Some(0);
        while let Some(from) = skip {
            let page = repo.log_page(&filter, from, 5).unwrap();
            assert!(page.commits.len() <= 5);
            paged.extend(page.commits);
            nexts.push(page.next);
            skip = page.next;
        }
        assert_eq!(nexts, [Some(5), Some(10), Some(15), None]);
        assert_eq!(paged, all.commits);
        // The merge's two parents need two lanes.
        let rows = filter.graph(&all.commits);
        assert_eq!(rows.iter().map(|row| row.width).max(), Some(2));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn author_date_text_path_and_branch_filters_pick_commits() {
        let (dir, repo) = history("log_filters");
        let log = |filter: LogFilter| repo.log_page(&filter, 0, 1000).unwrap().commits;
        let all = log(LogFilter::default());

        let carol = log(LogFilter {
            authors: vec!["carol".into()],
            ..LogFilter::default()
        });
        assert_eq!(subjects(&carol), ["feature 2", "feature 1", "feature 0"]);
        let two = log(LogFilter {
            authors: vec!["ALICE".into(), "carol@example".into()],
            ..LogFilter::default()
        });
        assert_eq!(two.len(), 6 + 3);
        assert!(
            two.iter()
                .all(|c| c.author == "Alice" || c.author == "Carol")
        );

        let (since, until) = (T0 + 10 * DAY, T0 + 13 * DAY);
        let dated = log(LogFilter {
            since: Some(since),
            until: Some(until),
            ..LogFilter::default()
        });
        let expected: Vec<&Commit> = all
            .iter()
            .filter(|c| (since..=until).contains(&c.time))
            .collect();
        assert_eq!(dated.iter().collect::<Vec<_>>(), expected);
        assert_eq!(dated.len(), 4, "main 10, main 11, feature 0, feature 1");
        // Such filters drop parents: the graph is one line.
        let filter = LogFilter {
            since: Some(since),
            ..LogFilter::default()
        };
        assert!(filter.graph(&dated).iter().all(|row| row.column == 0));

        let text = |text: &str| {
            log(LogFilter {
                text: text.into(),
                ..LogFilter::default()
            })
        };
        assert_eq!(subjects(&text("FIX")), ["fix: crash"]);
        assert_eq!(text("dave@").len(), 1, "author email");
        let third = all.iter().find(|c| c.subject == "main 3").unwrap();
        assert_eq!(subjects(&text(&third.id[..7])), ["main 3"]);
        assert_eq!(text("main").len(), 12);
        assert!(text("no such thing").is_empty());

        let lib = log(LogFilter {
            paths: vec!["lib".into()],
            ..LogFilter::default()
        });
        assert_eq!(subjects(&lib), ["feature 2", "feature 1", "feature 0"]);
        // Parents are rewritten to the listed commits, so lanes connect.
        let ids: Vec<&str> = lib.iter().map(|c| c.id.as_str()).collect();
        assert!(
            lib.iter()
                .flat_map(|c| &c.parents)
                .all(|p| ids.contains(&p.as_str()))
        );

        let feature = log(LogFilter {
            revisions: vec!["feature".into()],
            ..LogFilter::default()
        });
        assert_eq!(feature.len(), 15);
        assert_eq!(feature[0].subject, "feature 2");
        let main_only = log(LogFilter {
            revisions: vec!["main".into()],
            paths: vec!["main.txt".into()],
            ..LogFilter::default()
        });
        assert_eq!(main_only.len(), 13);
        assert!(
            repo.log_page(
                &LogFilter {
                    revisions: vec!["nope".into()],
                    ..LogFilter::default()
                },
                0,
                10
            )
            .is_err()
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn text_filters_scan_past_a_page_of_unmatched_commits() {
        let (dir, repo) = history("log_scan");
        let filter = LogFilter {
            text: "feature 0".into(),
            ..LogFilter::default()
        };
        let page = repo.log_page(&filter, 0, 1).unwrap();
        assert_eq!(subjects(&page.commits), ["feature 0"]);
        assert_eq!(page.next, None, "git listed everything in one scan");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn selections_span_from_the_oldest_parent_to_the_newest_commit() {
        let (dir, repo) = history("log_selection");
        let all = repo
            .log_page(&LogFilter::default(), 0, 100)
            .unwrap()
            .commits;
        let find = |subject: &str| all.iter().find(|c| c.subject == subject).unwrap();
        let (newest, oldest) = (find("feature 2"), find("feature 0"));
        let span = repo.selection_changes(newest, oldest).unwrap();
        assert_eq!(span.base, Side::Revision(find("main 11").id.clone()));
        assert_eq!(span.target, Side::Revision(newest.id.clone()));
        let changes: Vec<(&str, Change)> = span
            .changes
            .iter()
            .map(|c| (c.path.as_str(), c.change))
            .collect();
        assert_eq!(changes, [("lib/x.txt", Change::Added)]);

        // Across the merge: main.txt and lib/x.txt both changed since main 11.
        let merged = repo
            .selection_changes(find("Merge feature"), find("main 12"))
            .unwrap();
        let paths: Vec<&str> = merged.changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["lib/x.txt", "main.txt"]);

        // From the root commit: everything is added, against nothing.
        let root = repo
            .selection_changes(find("main 1"), find("main 0"))
            .unwrap();
        assert_eq!(root.base, Side::Empty);
        assert_eq!(root.changes.len(), 1);
        assert_eq!(root.changes[0].change, Change::Added);

        assert_eq!(
            repo.branches_containing(&oldest.id).unwrap(),
            ["feature", "main"]
        );
        assert_eq!(
            repo.branches_containing(&find("main 12").id).unwrap(),
            ["main"]
        );
        assert_eq!(
            repo.commit_message(&find("main 12").id).unwrap(),
            "main 12\n\nbody line"
        );
        assert_eq!(repo.user_name().unwrap().as_deref(), Some("Test"));
        fs::remove_dir_all(dir).ok();
    }
}
