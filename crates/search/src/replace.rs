//! Replacing search matches: the replacement text of a match (with `$1` /
//! `${name}` capture expansion in regex mode, literal otherwise, like the
//! editor's find bar), and edits planned against a file's current text,
//! refused when a reported match is no longer there.

use std::fmt;
use std::ops::Range;

use grep_matcher::{Captures, Matcher};
use grep_regex::RegexMatcher;

use crate::{LineMatch, MAX_LINE_LEN, QueryError, SearchQuery, build_matcher, content_len};

/// One match to replace, as a search reported it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    /// 0-based line.
    pub line: usize,
    /// Byte column of the match in the line.
    pub column: usize,
    /// The matched text the search saw.
    pub text: String,
    /// `text` may stop short of the match's end: the search truncated a
    /// long line there.
    pub partial: bool,
}

impl Target {
    /// The non-empty matches of `line` (see [`LineMatch`]) with their
    /// range in `line.text`.
    pub fn of_line(line: &LineMatch) -> Vec<(Range<usize>, Target)> {
        // `text` may be a window of a long line: `column` places its first
        // match in the original line.
        let shift = line
            .column
            .saturating_sub(line.ranges.first().map_or(0, |range| range.start));
        let clipped_end = line.text.len() + 3 >= MAX_LINE_LEN;
        line.ranges
            .iter()
            .filter(|range| !range.is_empty())
            .map(|range| {
                let target = Target {
                    line: line.line,
                    column: shift + range.start,
                    text: line.text[range.clone()].to_owned(),
                    partial: clipped_end && range.end == line.text.len(),
                };
                (range.clone(), target)
            })
            .collect()
    }
}

/// A planned edit: `offsets` of the text (`columns` of `line`) become `text`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replacement {
    pub line: usize,
    pub columns: Range<usize>,
    pub offsets: Range<usize>,
    pub text: String,
}

/// A target that no longer matches: the text changed since the search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stale {
    /// 0-based line of the first target that no longer matches.
    pub line: usize,
}

impl fmt::Display for Stale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "changed since the search (line {} no longer matches)",
            self.line + 1
        )
    }
}

/// A search query paired with its replacement.
#[derive(Clone, Debug)]
pub struct Replacer {
    matcher: RegexMatcher,
    replacement: String,
    /// Regex mode: `$1`, `${name}` and `$$` expand.
    expand: bool,
}

impl Replacer {
    pub fn new(query: &SearchQuery, replacement: &str) -> Result<Self, QueryError> {
        if query.pattern.is_empty() {
            return Err(QueryError::InvalidPattern("the pattern is empty".into()));
        }
        Ok(Self {
            matcher: build_matcher(query)?,
            replacement: replacement.to_owned(),
            expand: query.regex,
        })
    }

    /// The text replacing the match at `range` of `line` (a line without
    /// its terminator, as shown in results); `None` if no match is there.
    pub fn replacement(&self, line: &str, range: Range<usize>) -> Option<String> {
        let found = self.match_at(line.as_bytes(), range.start)?;
        (found == range).then(|| self.expand(line.as_bytes(), range.start))
    }

    /// The edits replacing `targets` in `text` (the file's current
    /// contents), sorted; an error if any target no longer matches there
    /// with the text the search saw.
    pub fn plan(&self, text: &str, targets: &[Target]) -> Result<Vec<Replacement>, Stale> {
        let mut sorted: Vec<&Target> = targets.iter().collect();
        sorted.sort_by_key(|target| (target.line, target.column));
        sorted.dedup_by_key(|target| (target.line, target.column));
        let mut lines = text.split_inclusive('\n').enumerate();
        let mut offset = 0;
        let mut current: Option<(usize, usize, &str, Vec<Range<usize>>)> = None;
        let mut planned = Vec::with_capacity(sorted.len());
        for target in sorted {
            let stale = Stale { line: target.line };
            if current
                .as_ref()
                .is_none_or(|(line, ..)| *line != target.line)
            {
                let (_, raw) = lines
                    .by_ref()
                    .inspect(|(_, raw)| offset += raw.len())
                    .find(|(line, _)| *line == target.line)
                    .ok_or(stale.clone())?;
                let start = offset - raw.len();
                let matches = self.line_matches(raw.as_bytes());
                current = Some((target.line, start, raw, matches));
            }
            let Some((_, start, raw, matches)) = &current else {
                unreachable!("set above");
            };
            let found = matches
                .iter()
                .find(|range| range.start == target.column)
                .filter(|range| {
                    let seen = &raw[(*range).clone()];
                    if target.partial {
                        seen.starts_with(&target.text)
                    } else {
                        seen == target.text
                    }
                })
                .ok_or(stale)?;
            planned.push(Replacement {
                line: target.line,
                columns: found.clone(),
                offsets: start + found.start..start + found.end,
                text: self.expand(raw.as_bytes(), found.start),
            });
        }
        Ok(planned)
    }

    /// Non-empty match ranges of a raw line (terminator included), clipped
    /// to its content like the search's results.
    fn line_matches(&self, raw: &[u8]) -> Vec<Range<usize>> {
        let content = content_len(raw);
        let mut ranges = Vec::new();
        let _ = self.matcher.find_iter(raw, |m| {
            let range = m.start().min(content)..m.end().min(content);
            if !range.is_empty() {
                ranges.push(range);
            }
            true
        });
        ranges
    }

    fn match_at(&self, haystack: &[u8], at: usize) -> Option<Range<usize>> {
        let found = self.matcher.find_at(haystack, at).ok()??;
        Some(found.start()..found.end())
    }

    /// The replacement for the match starting at `at` of `haystack`.
    fn expand(&self, haystack: &[u8], at: usize) -> String {
        if !self.expand {
            return self.replacement.clone();
        }
        let Ok(mut captures) = self.matcher.new_captures() else {
            return self.replacement.clone();
        };
        if !matches!(
            self.matcher.captures_at(haystack, at, &mut captures),
            Ok(true)
        ) {
            return self.replacement.clone();
        }
        let mut expanded = Vec::new();
        captures.interpolate(
            |name| self.matcher.capture_index(name),
            haystack,
            self.replacement.as_bytes(),
            &mut expanded,
        );
        String::from_utf8(expanded)
            .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
    }
}

/// `text` with sorted, non-overlapping `replacements` applied.
pub fn apply_replacements(text: &str, replacements: &[Replacement]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for replacement in replacements {
        out.push_str(&text[cursor..replacement.offsets.start]);
        out.push_str(&replacement.text);
        cursor = replacement.offsets.end;
    }
    out.push_str(&text[cursor..]);
    out
}
