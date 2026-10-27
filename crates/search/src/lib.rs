//! UI-agnostic project-wide text search and replace.
//!
//! [`search`] walks a directory tree (honoring ignore files) on background
//! threads and streams per-file results over a channel; [`search_text`]
//! searches one in-memory text the same way. [`Replacer`] turns reported
//! matches into edits, re-verified against the current text.

use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{DirEntry, WalkBuilder, WalkState};
use serde::{Deserialize, Serialize};

mod replace;

pub use replace::{Replacement, Replacer, Stale, Target, apply_replacements};

/// Lines longer than this many bytes are truncated around their first match.
const MAX_LINE_LEN: usize = 400;
/// Bytes of context kept before the first match in a truncated line.
const TRUNCATED_CONTEXT: usize = 100;

/// What to search for and where.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchQuery {
    /// The text or regular expression to search for.
    pub pattern: String,
    /// When false, `pattern` is matched literally.
    pub regex: bool,
    /// When false, matching ignores case.
    pub case_sensitive: bool,
    /// Match only at word boundaries.
    pub whole_word: bool,
    /// Glob patterns (gitignore syntax, relative to the root) of files to
    /// search. Empty means all files.
    pub include: Vec<String>,
    /// Glob patterns (gitignore syntax, relative to the root) of files or
    /// directories to skip.
    pub exclude: Vec<String>,
}

/// A single matching line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineMatch {
    /// 0-based line number.
    pub line: usize,
    /// The line without its terminator. Lines longer than ~400 bytes are
    /// truncated around the first match.
    pub text: String,
    /// Sorted byte ranges of the matches within `text`, char-boundary aligned.
    pub ranges: Vec<Range<usize>>,
    /// Byte column of the first match in the original line.
    pub column: usize,
}

/// All matching lines of one file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMatches {
    /// Absolute path of the file.
    pub path: PathBuf,
    /// Matching lines in ascending line order.
    pub lines: Vec<LineMatch>,
}

/// Streamed search output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchEvent {
    /// A file with at least one match.
    File(FileMatches),
    /// Always the last event of a search.
    Done {
        /// Number of files whose contents were searched.
        files_searched: usize,
        /// Number of individual matches reported.
        matches: usize,
        /// Whether `max_matches` was reached, so results may be incomplete.
        limit_hit: bool,
    },
}

/// Resource limits for a search.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchLimits {
    /// Stop after reporting this many matches.
    pub max_matches: usize,
    /// Files larger than this many bytes are skipped.
    pub max_file_size: u64,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            max_matches: 10_000,
            max_file_size: 10 * 1024 * 1024,
        }
    }
}

/// An invalid [`SearchQuery`].
#[derive(Debug)]
pub enum QueryError {
    /// The search pattern failed to compile.
    InvalidPattern(String),
    /// An include or exclude glob is malformed.
    InvalidGlob(String),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPattern(err) => write!(f, "invalid search pattern: {err}"),
            Self::InvalidGlob(err) => write!(f, "invalid glob: {err}"),
        }
    }
}

impl std::error::Error for QueryError {}

/// Cancels the search when dropped (or via [`SearchHandle::cancel`]).
#[derive(Debug)]
pub struct SearchHandle {
    cancelled: Arc<AtomicBool>,
}

impl SearchHandle {
    /// Stops the search; a final [`SearchEvent::Done`] is still sent.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

impl Drop for SearchHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Starts searching `root` on background threads and returns immediately.
///
/// Emits one [`SearchEvent::File`] per matching file as found, then exactly
/// one [`SearchEvent::Done`].
pub fn search(
    root: &Path,
    query: &SearchQuery,
    limits: SearchLimits,
) -> Result<(SearchHandle, UnboundedReceiver<SearchEvent>), QueryError> {
    let root = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let filter = PathFilter::new(&root, query)?;
    let matcher = if query.pattern.is_empty() {
        None
    } else {
        Some(build_matcher(query)?)
    };

    let cancelled = Arc::new(AtomicBool::new(false));
    let handle = SearchHandle {
        cancelled: cancelled.clone(),
    };
    let (tx, rx) = unbounded();
    let Some(matcher) = matcher else {
        tx.unbounded_send(EMPTY_DONE).ok();
        return Ok((handle, rx));
    };

    let state = Arc::new(SharedState {
        filter,
        matcher,
        limits,
        cancelled,
        tx,
        files_searched: AtomicUsize::new(0),
        reserved: AtomicUsize::new(0),
        reported: AtomicUsize::new(0),
        limit_hit: AtomicBool::new(false),
    });
    let fallback_tx = state.tx.clone();
    let spawned = std::thread::Builder::new()
        .name("project-search".into())
        .spawn(move || run(&state));
    if spawned.is_err() {
        fallback_tx.unbounded_send(EMPTY_DONE).ok();
    }
    Ok((handle, rx))
}

const EMPTY_DONE: SearchEvent = SearchEvent::Done {
    files_searched: 0,
    matches: 0,
    limit_hit: false,
};

/// Searches `text` (a file's contents, e.g. an unsaved editor buffer) like
/// [`search`] searches a file: matching lines in order, without limits.
pub fn search_text(query: &SearchQuery, text: &str) -> Result<Vec<LineMatch>, QueryError> {
    if query.pattern.is_empty() {
        return Ok(Vec::new());
    }
    let matcher = build_matcher(query)?;
    Ok(text
        .split_inclusive('\n')
        .enumerate()
        .filter_map(|(line, raw)| {
            let mut ranges = Vec::new();
            matcher
                .find_iter(raw.as_bytes(), |m| {
                    ranges.push(m.start()..m.end());
                    true
                })
                .ok()?;
            (!ranges.is_empty()).then(|| line_match(line, raw.as_bytes(), ranges))
        })
        .collect())
}

/// The include/exclude globs of a [`SearchQuery`] (and the `.git`
/// directory, never searched), for paths under a root.
#[derive(Debug)]
pub struct PathFilter {
    root: PathBuf,
    include: Gitignore,
    exclude: Gitignore,
}

impl PathFilter {
    pub fn new(root: &Path, query: &SearchQuery) -> Result<Self, QueryError> {
        Ok(Self {
            root: root.to_path_buf(),
            include: build_globs(root, &query.include)?,
            exclude: build_globs(root, &query.exclude)?,
        })
    }

    /// Whether a search would look at the file `path`: under the root, not
    /// excluded (itself or a parent directory), and included. Ignore files
    /// are not consulted.
    pub fn matches(&self, path: &Path) -> bool {
        let Some(rel) = self.relative(path) else {
            return false;
        };
        let in_git = rel
            .parent()
            .is_some_and(|dir| dir.components().any(|c| c.as_os_str() == ".git"));
        !in_git
            && !self
                .exclude
                .matched_path_or_any_parents(rel, false)
                .is_ignore()
            && self.is_included(path)
    }

    /// Path relative to the root for glob matching; a root file matches by name.
    fn relative<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        let rel = path.strip_prefix(&self.root).ok()?;
        if rel.as_os_str().is_empty() {
            path.file_name().map(Path::new)
        } else {
            Some(rel)
        }
    }

    fn is_excluded(&self, entry: &DirEntry) -> bool {
        if entry.depth() == 0 {
            return false;
        }
        let is_dir = entry.file_type().is_some_and(|ty| ty.is_dir());
        if is_dir && entry.file_name() == ".git" {
            return true;
        }
        self.relative(entry.path())
            .is_some_and(|rel| self.exclude.matched(rel, is_dir).is_ignore())
    }

    fn is_included(&self, path: &Path) -> bool {
        self.include.is_empty()
            || self.relative(path).is_some_and(|rel| {
                self.include
                    .matched_path_or_any_parents(rel, false)
                    .is_ignore()
            })
    }
}

fn build_globs(root: &Path, globs: &[String]) -> Result<Gitignore, QueryError> {
    let mut builder = GitignoreBuilder::new(root);
    // Report `[abc` as an error instead of silently matching it literally.
    builder.allow_unclosed_class(false);
    for glob in globs.iter().map(|glob| glob.trim()) {
        if glob.is_empty() {
            continue;
        }
        builder
            .add_line(None, glob)
            .map_err(|err| QueryError::InvalidGlob(err.to_string()))?;
    }
    builder
        .build()
        .map_err(|err| QueryError::InvalidGlob(err.to_string()))
}

fn build_matcher(query: &SearchQuery) -> Result<RegexMatcher, QueryError> {
    RegexMatcherBuilder::new()
        .fixed_strings(!query.regex)
        .case_insensitive(!query.case_sensitive)
        .word(query.whole_word)
        // `^`/`$` anchor at line boundaries (before `\r\n` too); matches never
        // span lines.
        .multi_line(true)
        .crlf(true)
        .line_terminator(Some(b'\n'))
        .build(&query.pattern)
        .map_err(|err| QueryError::InvalidPattern(err.to_string()))
}

struct SharedState {
    filter: PathFilter,
    matcher: RegexMatcher,
    limits: SearchLimits,
    cancelled: Arc<AtomicBool>,
    tx: UnboundedSender<SearchEvent>,
    files_searched: AtomicUsize,
    /// Matches claimed against `max_matches` (may exceed it transiently).
    reserved: AtomicUsize,
    /// Matches actually sent to the receiver.
    reported: AtomicUsize,
    limit_hit: AtomicBool,
}

impl SharedState {
    fn should_stop(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed) || self.limit_hit.load(Ordering::Relaxed)
    }

    /// Claims up to `wanted` matches; returns how many may be reported.
    fn reserve(&self, wanted: usize) -> usize {
        let max = self.limits.max_matches;
        let previous = self.reserved.fetch_add(wanted, Ordering::Relaxed);
        let granted = wanted.min(max.saturating_sub(previous));
        if previous + wanted >= max {
            self.limit_hit.store(true, Ordering::Relaxed);
        }
        granted
    }

    fn search_file(&self, searcher: &mut Searcher, entry: DirEntry) {
        if !entry.file_type().is_some_and(|ty| ty.is_file())
            || !self.filter.is_included(entry.path())
        {
            return;
        }
        match entry.metadata() {
            Ok(meta) if meta.len() <= self.limits.max_file_size => {}
            _ => return,
        }
        self.files_searched.fetch_add(1, Ordering::Relaxed);
        let mut sink = FileSink {
            state: self,
            lines: Vec::new(),
            binary: false,
        };
        if searcher
            .search_path(&self.matcher, entry.path(), &mut sink)
            .is_err()
            || sink.binary
            || sink.lines.is_empty()
            || self.cancelled.load(Ordering::Relaxed)
        {
            return;
        }
        let count = sink.lines.iter().map(|line| line.ranges.len()).sum();
        self.reported.fetch_add(count, Ordering::Relaxed);
        let event = SearchEvent::File(FileMatches {
            path: entry.into_path(),
            lines: sink.lines,
        });
        if self.tx.unbounded_send(event).is_err() {
            // Receiver dropped: nobody is listening any more.
            self.cancelled.store(true, Ordering::Relaxed);
        }
    }
}

fn run(state: &Arc<SharedState>) {
    let mut builder = WalkBuilder::new(&state.filter.root);
    builder
        .hidden(false)
        .require_git(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true);
    let filter_state = state.clone();
    builder.filter_entry(move |entry| !filter_state.filter.is_excluded(entry));
    builder.build_parallel().run(|| {
        let state = state.clone();
        let mut searcher = SearcherBuilder::new()
            .line_number(true)
            .binary_detection(BinaryDetection::quit(b'\x00'))
            .build();
        Box::new(move |entry| {
            if state.should_stop() {
                return WalkState::Quit;
            }
            if let Ok(entry) = entry {
                state.search_file(&mut searcher, entry);
            }
            if state.should_stop() {
                WalkState::Quit
            } else {
                WalkState::Continue
            }
        })
    });
    state
        .tx
        .unbounded_send(SearchEvent::Done {
            files_searched: state.files_searched.load(Ordering::Relaxed),
            matches: state.reported.load(Ordering::Relaxed),
            limit_hit: state.limit_hit.load(Ordering::Relaxed),
        })
        .ok();
}

struct FileSink<'a> {
    state: &'a SharedState,
    lines: Vec<LineMatch>,
    binary: bool,
}

impl Sink for FileSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self.state.should_stop() {
            return Ok(false);
        }
        let raw = mat.bytes();
        let mut ranges = Vec::new();
        self.state
            .matcher
            .find_iter(raw, |m| {
                ranges.push(m.start()..m.end());
                !self.state.cancelled.load(Ordering::Relaxed)
            })
            .map_err(std::io::Error::other)?;
        if ranges.is_empty() {
            return Ok(true);
        }
        let granted = self.state.reserve(ranges.len());
        if granted == 0 {
            return Ok(false);
        }
        ranges.truncate(granted);
        let line = mat
            .line_number()
            .map_or(0, |number| number.saturating_sub(1) as usize);
        self.lines.push(line_match(line, raw, ranges));
        Ok(!self.state.should_stop())
    }

    fn binary_data(&mut self, _: &Searcher, _: u64) -> Result<bool, Self::Error> {
        self.binary = true;
        Ok(false)
    }
}

/// Builds a [`LineMatch`] from a raw line (terminator included) and the byte
/// ranges of its matches.
fn line_match(line: usize, raw: &[u8], ranges: Vec<Range<usize>>) -> LineMatch {
    let content = &raw[..content_len(raw)];
    let column = ranges.first().map_or(0, |range| range.start);
    let ranges = ranges
        .into_iter()
        .map(|range| range.start.min(content.len())..range.end.min(content.len()));
    let (text, ranges) = decode(content, ranges);
    let (text, ranges) = truncate(text, ranges);
    LineMatch {
        line,
        text,
        ranges,
        column,
    }
}

/// Length of a raw line without its `\n` or `\r\n` terminator.
fn content_len(raw: &[u8]) -> usize {
    let content = raw.strip_suffix(b"\n").unwrap_or(raw);
    content.strip_suffix(b"\r").unwrap_or(content).len()
}

/// Converts `bytes` to a `String`, replacing invalid UTF-8 and remapping
/// `ranges` onto the decoded text so they stay char-boundary aligned.
fn decode(bytes: &[u8], ranges: impl Iterator<Item = Range<usize>>) -> (String, Vec<Range<usize>>) {
    if let Ok(text) = std::str::from_utf8(bytes) {
        let ranges = ranges
            .map(|range| floor_char(text, range.start)..ceil_char(text, range.end))
            .collect();
        return (text.to_owned(), ranges);
    }
    let mut text = String::with_capacity(bytes.len());
    let mut mapped = Vec::new();
    let mut cursor = 0;
    for range in ranges {
        let start = range.start.max(cursor);
        let end = range.end.max(start);
        text.push_str(&String::from_utf8_lossy(&bytes[cursor..start]));
        let mapped_start = text.len();
        text.push_str(&String::from_utf8_lossy(&bytes[start..end]));
        mapped.push(mapped_start..text.len());
        cursor = end;
    }
    text.push_str(&String::from_utf8_lossy(&bytes[cursor..]));
    (text, mapped)
}

/// Shortens lines longer than [`MAX_LINE_LEN`] to a window around the first
/// match, dropping or clipping ranges outside it.
fn truncate(text: String, ranges: Vec<Range<usize>>) -> (String, Vec<Range<usize>>) {
    let Some(first) = ranges.first().cloned() else {
        return (text, ranges);
    };
    if text.len() <= MAX_LINE_LEN {
        return (text, ranges);
    }
    let start = floor_char(&text, first.start.saturating_sub(TRUNCATED_CONTEXT));
    let end = floor_char(&text, (start + MAX_LINE_LEN).min(text.len())).max(first.end);
    let ranges = ranges
        .into_iter()
        .filter(|range| range.start < end && (range.end > start || range == &first))
        .map(|range| range.start.max(start) - start..range.end.min(end) - start)
        .collect();
    (text[start..end].to_owned(), ranges)
}

fn floor_char(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests;
