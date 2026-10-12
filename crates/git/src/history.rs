//! Commit history: `git log` records and the lane layout of the commit graph.

use std::io;

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Commit {
    pub id: String,
    /// First parent first; empty for root commits.
    pub parents: Vec<String>,
    pub author: String,
    pub email: String,
    /// Author time, seconds since the Unix epoch.
    pub time: i64,
    /// Branches and tags pointing here (`HEAD -> main`, `origin/main`, `tag: v1`).
    pub refs: Vec<String>,
    pub subject: String,
}

impl Commit {
    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(8)]
    }
}

/// `git log --format` producing what [`parse_log`] reads.
pub(crate) const LOG_FORMAT: &str = "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%at%x1f%D%x1f%s%x1e";

pub(crate) fn parse_log(output: &[u8]) -> io::Result<Vec<Commit>> {
    String::from_utf8_lossy(output)
        .split('\x1e')
        .map(|record| record.trim_start_matches(['\n', '\r']))
        .filter(|record| !record.is_empty())
        .map(|record| {
            let fields: Vec<&str> = record.split('\x1f').collect();
            let [id, parents, author, email, time, refs, subject] = fields.as_slice() else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bad log record: {record:?}"),
                ));
            };
            Ok(Commit {
                id: (*id).to_owned(),
                parents: parents.split_whitespace().map(str::to_owned).collect(),
                author: (*author).to_owned(),
                email: (*email).to_owned(),
                time: time.parse().unwrap_or_default(),
                refs: refs
                    .split(", ")
                    .filter(|r| !r.is_empty())
                    .map(str::to_owned)
                    .collect(),
                subject: (*subject).to_owned(),
            })
        })
        .collect()
}

/// How one commit's row of the graph is drawn. Lanes are columns; each
/// segment runs from a lane at one edge of the half-row to a lane at the
/// other (the node sits in the middle of the row at `column`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GraphRow {
    pub column: usize,
    /// Top edge lane → middle lane (lanes ending in the node, or passing).
    pub top: Vec<(usize, usize)>,
    /// Middle lane → bottom edge lane (edges to parents, or passing).
    pub bottom: Vec<(usize, usize)>,
    /// Number of lanes the row uses.
    pub width: usize,
}

/// Lays out commits (newest first, children before parents, as from
/// `git log --topo-order`) into lanes.
pub fn graph(commits: &[Commit]) -> Vec<GraphRow> {
    // The commit each lane is waiting for.
    let mut lanes: Vec<Option<&str>> = Vec::new();
    let mut rows = Vec::with_capacity(commits.len());
    for commit in commits {
        let id = commit.id.as_str();
        let column = lanes
            .iter()
            .position(|lane| *lane == Some(id))
            .or_else(|| lanes.iter().position(Option::is_none))
            .unwrap_or_else(|| {
                lanes.push(None);
                lanes.len() - 1
            });
        let mut row = GraphRow {
            column,
            ..GraphRow::default()
        };
        for (lane, waiting) in lanes.iter().enumerate() {
            match waiting {
                Some(waiting) if *waiting == id => row.top.push((lane, column)),
                Some(_) => row.top.push((lane, lane)),
                None => {}
            }
        }
        for lane in lanes.iter_mut() {
            if *lane == Some(id) {
                *lane = None;
            }
        }

        // Lanes this row starts (towards parents not awaited yet).
        let mut started = Vec::new();
        let mut parents = commit.parents.iter().map(String::as_str);
        if let Some(first) = parents.next() {
            match lanes.iter().position(|lane| *lane == Some(first)) {
                // Another child already leads to this parent: join it.
                Some(lane) => row.bottom.push((column, lane)),
                None => {
                    lanes[column] = Some(first);
                    started.push(column);
                    row.bottom.push((column, column));
                }
            }
        }
        for parent in parents {
            let lane = match lanes.iter().position(|lane| *lane == Some(parent)) {
                Some(lane) => lane,
                None => {
                    let free = lanes
                        .iter()
                        .enumerate()
                        .position(|(lane, waiting)| waiting.is_none() && lane != column);
                    let lane = free.unwrap_or_else(|| {
                        lanes.push(None);
                        lanes.len() - 1
                    });
                    lanes[lane] = Some(parent);
                    started.push(lane);
                    lane
                }
            };
            row.bottom.push((column, lane));
        }
        for (lane, waiting) in lanes.iter().enumerate() {
            if waiting.is_some() && !started.contains(&lane) {
                row.bottom.push((lane, lane));
            }
        }
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
        row.width = row
            .top
            .iter()
            .chain(&row.bottom)
            .flat_map(|&(a, b)| [a, b])
            .chain([column])
            .max()
            .map_or(1, |max| max + 1);
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(id: &str, parents: &[&str]) -> Commit {
        Commit {
            id: id.into(),
            parents: parents.iter().map(|p| (*p).into()).collect(),
            author: String::new(),
            email: String::new(),
            time: 0,
            refs: Vec::new(),
            subject: String::new(),
        }
    }

    #[test]
    fn log_records_are_parsed_with_refs_and_parents() {
        let output = "aaaa\x1fbbbb cccc\x1fAda\x1fada@x\x1f1700000000\x1fHEAD -> main, tag: v1\x1fMerge it\x1e\n\
                      bbbb\x1f\x1fBob\x1fbob@x\x1f1600000000\x1f\x1fRoot, with comma\x1e\n";
        let commits = parse_log(output.as_bytes()).unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].parents, ["bbbb", "cccc"]);
        assert_eq!(commits[0].refs, ["HEAD -> main", "tag: v1"]);
        assert_eq!(commits[1].parents, Vec::<String>::new());
        assert_eq!(commits[1].subject, "Root, with comma");
        assert!(parse_log(b"too\x1ffew\x1e").is_err());
    }

    #[test]
    fn a_branch_and_merge_use_two_lanes_and_rejoin() {
        // m merges feature (f) into main (b); both fork from a.
        //   m
        //   |\
        //   b f
        //   |/
        //   a
        let commits = [
            commit("m", &["b", "f"]),
            commit("f", &["a"]),
            commit("b", &["a"]),
            commit("a", &[]),
        ];
        let rows = graph(&commits);
        assert_eq!(rows[0].column, 0);
        assert_eq!(rows[0].bottom, [(0, 0), (0, 1)]);
        // f sits in lane 1 while lane 0 waits for b.
        assert_eq!(rows[1].column, 1);
        assert_eq!(rows[1].top, [(0, 0), (1, 1)]);
        // b's parent a is already awaited by lane 1: lane 0 joins it,
        // and lane 1 carries on below.
        assert_eq!(rows[2].column, 0);
        assert_eq!(rows[2].bottom, [(0, 1), (1, 1)]);
        // a ends both: its row has a node at lane 1 and nothing below.
        assert_eq!(rows[3].column, 1);
        assert!(rows[3].bottom.is_empty());
        assert!(rows.iter().all(|row| row.width <= 2));
    }

    #[test]
    fn linear_history_stays_in_one_lane() {
        let commits = [commit("c", &["b"]), commit("b", &["a"]), commit("a", &[])];
        let rows = graph(&commits);
        assert!(rows.iter().all(|row| row.column == 0 && row.width == 1));
    }
}
