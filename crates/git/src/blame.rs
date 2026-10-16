//! Line authorship (`git blame --porcelain`), optionally of unsaved
//! contents, with each commit's full message.

use std::{collections::HashMap, io};

use serde::Serialize;

use crate::Repository;

/// A commit that last changed some lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlameCommit {
    pub id: String,
    pub author: String,
    pub author_mail: String,
    /// Author date, seconds since the epoch.
    pub time: i64,
    pub summary: String,
    /// Full message (subject and body).
    pub message: String,
}

/// Who last changed each line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Blame {
    pub commits: Vec<BlameCommit>,
    /// Per line of the blamed text: index into `commits`, `None` when the
    /// line is not committed.
    pub lines: Vec<Option<usize>>,
}

const UNCOMMITTED: &str = "0000000000000000000000000000000000000000";

impl Repository {
    /// Blames `path` (repository-relative) as of `contents` (unsaved
    /// editor text; lines git cannot find in HEAD are not committed), or
    /// as in the working tree when `None`.
    pub fn blame(&self, path: &str, contents: Option<&[u8]>) -> io::Result<Blame> {
        let mut args = vec!["blame", "--porcelain"];
        if contents.is_some() {
            args.extend(["--contents", "-"]);
        }
        args.extend(["--", path]);
        let output = self.checked_with_input(&args, contents)?;
        let mut blame = parse_porcelain(&String::from_utf8_lossy(&output.stdout))?;
        let ids: Vec<&str> = blame.commits.iter().map(|c| c.id.as_str()).collect();
        let messages = self.messages(&ids)?;
        for commit in &mut blame.commits {
            if let Some(message) = messages.get(&commit.id) {
                commit.message = message.clone();
            }
        }
        Ok(blame)
    }

    /// Full messages of commits, read in one `cat-file --batch`.
    fn messages(&self, ids: &[&str]) -> io::Result<HashMap<String, String>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let input = ids.join("\n") + "\n";
        let output = self.checked_with_input(&["cat-file", "--batch"], Some(input.as_bytes()))?;
        let mut messages = HashMap::new();
        let mut rest = output.stdout.as_slice();
        // `<id> <type> <size>\n<size bytes>\n` per object.
        while let Some(newline) = rest.iter().position(|&byte| byte == b'\n') {
            let header = String::from_utf8_lossy(&rest[..newline]).into_owned();
            rest = &rest[newline + 1..];
            let mut fields = header.split(' ');
            let (Some(id), Some(_), Some(size)) = (fields.next(), fields.next(), fields.next())
            else {
                continue; // `<id> missing`
            };
            let size: usize = size
                .parse()
                .map_err(|_| io::Error::other(format!("bad cat-file header {header:?}")))?;
            let object = &rest[..size.min(rest.len())];
            rest = rest.get(size + 1..).unwrap_or_default();
            let text = String::from_utf8_lossy(object);
            let message = text.split_once("\n\n").map_or("", |(_, message)| message);
            messages.insert(id.to_owned(), message.trim_end().to_owned());
        }
        Ok(messages)
    }
}

/// `git blame --porcelain`: per line `<id> <orig> <final> [<count>]`, the
/// commit's headers the first time it appears, then `\t<line>`.
fn parse_porcelain(output: &str) -> io::Result<Blame> {
    let mut blame = Blame::default();
    let mut index_of: HashMap<String, Option<usize>> = HashMap::new();
    let mut current: Option<Option<usize>> = None;
    for line in output.lines() {
        if line.starts_with('\t') {
            continue;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        if key.len() != 40 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            // A header of the current commit.
            if let Some(Some(index)) = current {
                let commit = &mut blame.commits[index];
                match key {
                    "author" => commit.author = value.to_owned(),
                    "author-mail" => commit.author_mail = value.trim_matches(['<', '>']).to_owned(),
                    "author-time" => commit.time = value.parse().unwrap_or(0),
                    "summary" => commit.summary = value.to_owned(),
                    _ => {}
                }
            }
            continue;
        }
        let id = key;
        let final_line: usize = value
            .split(' ')
            .nth(1)
            .and_then(|line| line.parse().ok())
            .filter(|&line| line > 0)
            .ok_or_else(|| io::Error::other(format!("bad blame line {line:?}")))?;
        let index = *index_of.entry(id.to_owned()).or_insert_with(|| {
            (id != UNCOMMITTED).then(|| {
                blame.commits.push(BlameCommit {
                    id: id.to_owned(),
                    author: String::new(),
                    author_mail: String::new(),
                    time: 0,
                    summary: String::new(),
                    message: String::new(),
                });
                blame.commits.len() - 1
            })
        });
        current = Some(index);
        if blame.lines.len() < final_line {
            blame.lines.resize(final_line, None);
        }
        blame.lines[final_line - 1] = index;
    }
    Ok(blame)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::tests::temp_repo;

    fn commit(dir: &std::path::Path, repo: &Repository, text: &str, message: &str) -> String {
        fs::write(dir.join("f.txt"), text).unwrap();
        repo.stage(&["f.txt".into()]).unwrap();
        repo.commit(message, false).unwrap();
        repo.resolve("HEAD").unwrap()
    }

    #[test]
    fn blame_names_each_lines_commit_and_marks_unsaved_lines_uncommitted() {
        let (dir, repo) = temp_repo("blame");
        let first = commit(&dir, &repo, "one\ntwo\nthree\n", "first\n\nwith a body");
        let second = commit(&dir, &repo, "one\nTWO\nthree\n", "second");
        let blame = repo.blame("f.txt", None).unwrap();
        let ids: Vec<Option<&str>> = blame
            .lines
            .iter()
            .map(|line| line.map(|index| blame.commits[index].id.as_str()))
            .collect();
        assert_eq!(
            ids,
            [
                Some(first.as_str()),
                Some(second.as_str()),
                Some(first.as_str())
            ]
        );
        let oldest = blame.commits.iter().find(|c| c.id == first).unwrap();
        assert_eq!(oldest.author, "Test");
        assert_eq!(oldest.author_mail, "test@example.com");
        assert_eq!(oldest.summary, "first");
        assert_eq!(oldest.message, "first\n\nwith a body");
        assert!(oldest.time > 1_600_000_000);

        // Unsaved contents: an inserted line is not committed and later
        // lines keep their commits.
        let blame = repo
            .blame("f.txt", Some(b"zero\none\nTWO\nthree\n"))
            .unwrap();
        let ids: Vec<Option<&str>> = blame
            .lines
            .iter()
            .map(|line| line.map(|index| blame.commits[index].id.as_str()))
            .collect();
        assert_eq!(
            ids,
            [
                None,
                Some(first.as_str()),
                Some(second.as_str()),
                Some(first.as_str())
            ]
        );
    }

    #[test]
    fn blaming_an_untracked_file_fails() {
        let (dir, repo) = temp_repo("blame_untracked");
        commit(&dir, &repo, "a\n", "first");
        fs::write(dir.join("new.txt"), "x\n").unwrap();
        assert!(repo.blame("new.txt", None).is_err());
    }
}
