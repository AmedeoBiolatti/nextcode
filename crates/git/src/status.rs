//! `git status --porcelain=v2 --branch -z` output.

use std::io;

use serde::{Deserialize, Serialize};

/// How a file differs on one side (index vs HEAD, or worktree vs index).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Modified,
    TypeChanged,
    Added,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    /// A merge conflict, to be resolved in the worktree.
    Conflicted,
}

impl Change {
    /// One-letter code, as in `git status --short`.
    pub fn letter(self) -> char {
        match self {
            Self::Modified => 'M',
            Self::TypeChanged => 'T',
            Self::Added => 'A',
            Self::Deleted => 'D',
            Self::Renamed => 'R',
            Self::Copied => 'C',
            Self::Untracked => 'U',
            Self::Conflicted => '!',
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            b'M' => Some(Self::Modified),
            b'T' => Some(Self::TypeChanged),
            b'A' => Some(Self::Added),
            b'D' => Some(Self::Deleted),
            b'R' => Some(Self::Renamed),
            b'C' => Some(Self::Copied),
            b'U' => Some(Self::Conflicted),
            _ => None,
        }
    }
}

/// One changed path. A file can be both staged and changed again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// Relative to the repository root, `/`-separated.
    pub path: String,
    /// The source of a rename or copy.
    pub orig_path: Option<String>,
    /// Index vs HEAD.
    pub staged: Option<Change>,
    /// Worktree vs index (`Untracked`, `Conflicted` live here).
    pub unstaged: Option<Change>,
}

impl Entry {
    /// The change against HEAD, staged and unstaged parts together.
    pub fn combined_change(&self) -> Change {
        match (self.staged, self.unstaged) {
            (Some(change @ (Change::Added | Change::Renamed | Change::Copied)), _) => change,
            (Some(Change::Deleted), _) | (_, Some(Change::Deleted)) => Change::Deleted,
            (Some(change), _) | (None, Some(change)) => change,
            (None, None) => Change::Modified,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Status {
    /// `None` when HEAD is detached.
    pub branch: Option<String>,
    /// No commit yet on the branch.
    pub unborn: bool,
    /// The checked-out commit; `None` before the first commit.
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Sorted by path.
    pub entries: Vec<Entry>,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Parses porcelain v2 records (NUL-terminated, as with `-z`).
pub fn parse(output: &[u8]) -> io::Result<Status> {
    let mut status = Status::default();
    let mut records = output
        .split(|&byte| byte == 0)
        .filter(|record| !record.is_empty())
        .map(String::from_utf8_lossy);
    while let Some(record) = records.next() {
        let (kind, rest) = record.split_once(' ').unwrap_or((&record, ""));
        match kind {
            "#" => {
                let (key, value) = rest.split_once(' ').unwrap_or((rest, ""));
                match key {
                    "branch.oid" => {
                        status.unborn = value == "(initial)";
                        status.head = (!status.unborn).then(|| value.to_owned());
                    }
                    "branch.head" if value != "(detached)" => {
                        status.branch = Some(value.to_owned());
                    }
                    "branch.upstream" => status.upstream = Some(value.to_owned()),
                    "branch.ab" => {
                        let mut counts = value.split(' ');
                        let mut count = |sign: char| {
                            counts
                                .next()
                                .and_then(|count| count.strip_prefix(sign)?.parse().ok())
                                .unwrap_or(0)
                        };
                        status.ahead = count('+');
                        status.behind = count('-');
                    }
                    _ => {}
                }
            }
            // Ordinary: XY sub mH mI mW hH hI path
            "1" => {
                let fields: Vec<&str> = rest.splitn(8, ' ').collect();
                let [xy, _, _, _, _, _, _, path] = fields.as_slice() else {
                    return Err(invalid(format!("bad status record: {record}")));
                };
                status.entries.push(tracked(xy, path, None)?);
            }
            // Renamed or copied: XY sub mH mI mW hH hI Xscore path, then origPath
            "2" => {
                let fields: Vec<&str> = rest.splitn(9, ' ').collect();
                let [xy, _, _, _, _, _, _, _, path] = fields.as_slice() else {
                    return Err(invalid(format!("bad status record: {record}")));
                };
                let orig = records
                    .next()
                    .ok_or_else(|| invalid("rename without its source path"))?;
                status
                    .entries
                    .push(tracked(xy, path, Some(orig.into_owned()))?);
            }
            // Unmerged: XY sub m1 m2 m3 mW h1 h2 h3 path
            "u" => {
                let fields: Vec<&str> = rest.splitn(10, ' ').collect();
                let Some(path) = fields.get(9) else {
                    return Err(invalid(format!("bad status record: {record}")));
                };
                status.entries.push(Entry {
                    path: (*path).to_owned(),
                    orig_path: None,
                    staged: None,
                    unstaged: Some(Change::Conflicted),
                });
            }
            "?" => status.entries.push(Entry {
                path: rest.to_owned(),
                orig_path: None,
                staged: None,
                unstaged: Some(Change::Untracked),
            }),
            // Ignored files are not listed without --ignored.
            "!" => {}
            _ => return Err(invalid(format!("unknown status record: {record}"))),
        }
    }
    status.entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(status)
}

fn tracked(xy: &str, path: &str, orig_path: Option<String>) -> io::Result<Entry> {
    let &[x, y] = xy.as_bytes() else {
        return Err(invalid(format!("bad status code {xy}")));
    };
    Ok(Entry {
        path: path.to_owned(),
        orig_path,
        staged: Change::from_code(x),
        unstaged: Change::from_code(y),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_records_become_entries_and_branch_info() {
        let output = concat!(
            "# branch.oid 1234abcd\0",
            "# branch.head main\0",
            "# branch.upstream origin/main\0",
            "# branch.ab +2 -1\0",
            "1 .M N... 100644 100644 100644 aaa bbb src/main.rs\0",
            "1 A. N... 000000 100644 100644 000 ccc new file.txt\0",
            "2 R. N... 100644 100644 100644 ddd ddd R100 docs/new.md\0docs/old.md\0",
            "u UU N... 100644 100644 100644 100644 e f g conflict.rs\0",
            "? notes/todo.txt\0",
        );
        let status = parse(output.as_bytes()).unwrap();
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        assert_eq!((status.ahead, status.behind), (2, 1));
        assert!(!status.unborn);
        let summary: Vec<(&str, Option<Change>, Option<Change>)> = status
            .entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry.staged, entry.unstaged))
            .collect();
        assert_eq!(
            summary,
            [
                ("conflict.rs", None, Some(Change::Conflicted)),
                ("docs/new.md", Some(Change::Renamed), None),
                ("new file.txt", Some(Change::Added), None),
                ("notes/todo.txt", None, Some(Change::Untracked)),
                ("src/main.rs", None, Some(Change::Modified)),
            ]
        );
        assert_eq!(status.entries[1].orig_path.as_deref(), Some("docs/old.md"));
    }

    #[test]
    fn fresh_repositories_and_detached_heads_are_recognized() {
        let fresh = parse(b"# branch.oid (initial)\0# branch.head main\0").unwrap();
        assert!(fresh.unborn);
        assert_eq!(fresh.branch.as_deref(), Some("main"));
        let detached = parse(b"# branch.oid abc\0# branch.head (detached)\0").unwrap();
        assert_eq!(detached.branch, None);
    }

    #[test]
    fn malformed_records_are_errors() {
        assert!(parse(b"1 M\0").is_err());
        assert!(parse(b"2 R. N... 1 1 1 a b R100 only-new\0").is_err());
        assert!(parse(b"Z what\0").is_err());
    }
}
