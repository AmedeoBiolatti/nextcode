//! Where a note sits: the anchored lines' text and a little context, so the
//! note can be found again after the file changed ([`reanchor`]).

use serde::{Deserialize, Serialize};

/// Lines of the anchored text kept (a longer range keeps its first lines).
const MAX_LINES: usize = 40;
/// Characters kept per line (and compared).
const MAX_LINE_CHARS: usize = 300;
/// Context lines kept above and below.
const CONTEXT_LINES: usize = 2;
/// Snippet lines compared when looking for an edited snippet.
const FUZZY_LINES: usize = 12;
/// Below this many non-blank characters a snippet (`}`, `else {`) says little
/// by itself: its context decides.
const WEAK_SNIPPET: usize = 8;
const FUZZY_ACCEPT: f64 = 0.6;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// 1-based, inclusive.
    pub start_line: usize,
    pub end_line: usize,
    /// 1-based character columns of a selection within the lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_column: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_column: Option<usize>,
    /// The anchored lines when anchored, `\n`-joined, without `\r`.
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnchorState {
    /// The text is still at the stored lines.
    Exact,
    /// The same text elsewhere (lines inserted or removed above).
    Moved,
    /// Similar text (the snippet was edited).
    Fuzzy,
    /// Not found: the stored lines (clamped to the file); keep the note.
    Orphaned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchored {
    /// 1-based, inclusive.
    pub start_line: usize,
    pub end_line: usize,
    pub state: AnchorState,
}

fn lines(text: &str) -> Vec<&str> {
    text.lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

fn cut(line: &str) -> &str {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((index, _)) => &line[..index],
        None => line,
    }
}

/// What lines are compared by: cut, then trimmed (re-indenting keeps the
/// anchor).
fn norm(line: &str) -> &str {
    cut(line).trim()
}

/// An anchor on lines `start_line..=end_line` (1-based, clamped to the
/// text) of `text`, with optional 1-based columns of a selection.
pub fn anchor_from(
    text: &str,
    start_line: usize,
    end_line: usize,
    columns: Option<(usize, usize)>,
) -> Anchor {
    let lines = lines(text);
    let last = lines.len().max(1);
    let start = start_line.clamp(1, last);
    let end = end_line.clamp(start, last);
    let range = |from: usize, to: usize| -> Vec<String> {
        lines
            .get(from..to.min(lines.len()))
            .unwrap_or_default()
            .iter()
            .map(|line| cut(line).to_owned())
            .collect()
    };
    let kept_end = end.min(start + MAX_LINES - 1);
    Anchor {
        start_line: start,
        end_line: end,
        start_column: columns.map(|(start, _)| start),
        end_column: columns.map(|(_, end)| end),
        text: range(start - 1, kept_end).join("\n"),
        before: range(start.saturating_sub(1 + CONTEXT_LINES), start - 1),
        after: range(end, end + CONTEXT_LINES),
    }
}

/// Where `anchor` is in `text` now: at its lines if their text is still
/// there, else the nearest identical text, else the most similar lines
/// (context included), else orphaned at its old lines.
pub fn reanchor(anchor: &Anchor, text: &str) -> Anchored {
    let file = lines(text);
    let normalized: Vec<&str> = file.iter().map(|line| norm(line)).collect();
    let count = normalized.len();
    let span = anchor.end_line.saturating_sub(anchor.start_line) + 1;
    let stored = anchor.start_line.max(1) - 1;
    let at = |start: usize, state| {
        let last = count.max(1);
        let start_line = (start + 1).min(last);
        Anchored {
            start_line,
            end_line: (start + span).clamp(start_line, last),
            state,
        }
    };
    if anchor.text.is_empty() && anchor.before.is_empty() && anchor.after.is_empty() {
        // Nothing to look for (a hand-written anchor): trust the lines.
        let state = if stored < count {
            AnchorState::Exact
        } else {
            AnchorState::Orphaned
        };
        return at(stored, state);
    }
    let snippet: Vec<&str> = anchor.text.split('\n').map(norm).collect();
    let weak = snippet
        .iter()
        .map(|line| line.chars().filter(|c| !c.is_whitespace()).count())
        .sum::<usize>()
        < WEAK_SNIPPET;
    let context = |start: usize| context_score(anchor, &normalized, start, span);
    let distance = |start: usize| start.abs_diff(stored);

    let exact: Vec<usize> = (0..=count.saturating_sub(snippet.len()))
        .filter(|&start| {
            start + snippet.len() <= count
                && snippet
                    .iter()
                    .zip(&normalized[start..])
                    .all(|(want, have)| want == have)
        })
        .collect();
    let trusted = |start: usize| !weak || context(start).is_none_or(|score| score >= 0.5);
    if exact.contains(&stored) && trusted(stored) {
        return at(stored, AnchorState::Exact);
    }
    let best = exact.iter().copied().min_by(|&a, &b| {
        let (ca, cb) = (context(a).unwrap_or(0.), context(b).unwrap_or(0.));
        let by_context = cb.total_cmp(&ca);
        let by_distance = distance(a).cmp(&distance(b));
        if weak {
            // Rounded: near-equal contexts fall back to distance.
            let (ra, rb) = ((ca * 10.).round(), (cb * 10.).round());
            rb.total_cmp(&ra).then(by_distance)
        } else {
            by_distance.then(by_context)
        }
    });
    if let Some(start) = best.filter(|&start| trusted(start)) {
        let state = if start == stored {
            AnchorState::Exact
        } else {
            AnchorState::Moved
        };
        return at(start, state);
    }

    match fuzzy(anchor, &snippet, &normalized, weak, stored) {
        Some(start) => at(start, AnchorState::Fuzzy),
        None => at(stored.min(count.saturating_sub(1)), AnchorState::Orphaned),
    }
}

/// How well the stored context lines match those around `start`, 0..=1;
/// `None` without stored context.
fn context_score(anchor: &Anchor, file: &[&str], start: usize, span: usize) -> Option<f64> {
    let mut total = 0.;
    let mut compared = 0;
    let before = anchor.before.len();
    for (index, want) in anchor.before.iter().enumerate() {
        // `before` ends right above the anchor.
        let have = (start + index)
            .checked_sub(before)
            .and_then(|line| file.get(line));
        total += have.map_or(0., |have| similarity(norm(want), have));
        compared += 1;
    }
    for (index, want) in anchor.after.iter().enumerate() {
        let have = file.get(start + span + index);
        total += have.map_or(0., |have| similarity(norm(want), have));
        compared += 1;
    }
    (compared > 0).then(|| total / compared as f64)
}

/// The start of the lines most like the snippet (and its context), if alike
/// enough.
fn fuzzy(
    anchor: &Anchor,
    snippet: &[&str],
    file: &[&str],
    weak: bool,
    stored: usize,
) -> Option<usize> {
    let snippet = &snippet[..snippet.len().min(FUZZY_LINES)];
    let wanted: Vec<Vec<u64>> = snippet.iter().map(|line| bigrams(line)).collect();
    let span = anchor.end_line.saturating_sub(anchor.start_line) + 1;
    let mut best: Option<(f64, usize)> = None;
    let mut cache: Vec<Option<Vec<u64>>> = vec![None; file.len()];
    for start in 0..file.len() {
        let mut body = 0.;
        for (index, want) in snippet.iter().enumerate() {
            // Even perfect remaining lines could not reach the bar: skip.
            if (body + (snippet.len() - index) as f64) / (snippet.len() as f64) < 0.5 {
                break;
            }
            let Some(have) = file.get(start + index) else {
                break;
            };
            let have_grams = cache[start + index].get_or_insert_with(|| bigrams(have));
            body += dice(want, &wanted[index], have, have_grams);
        }
        let body = body / snippet.len() as f64;
        if body < 0.5 {
            continue;
        }
        let context = context_score(anchor, file, start, span);
        if weak && context.is_none_or(|score| score < 0.5) {
            continue;
        }
        let score = match context {
            Some(context) => 0.7 * body + 0.3 * context,
            None => body,
        };
        // A hair per line of distance: equal scores go to the nearest.
        let score = score - start.abs_diff(stored) as f64 * 1e-9;
        if score >= FUZZY_ACCEPT - 1e-3 && best.is_none_or(|(best, _)| score > best) {
            best = Some((score, start));
        }
    }
    best.map(|(_, start)| start)
}

fn bigrams(line: &str) -> Vec<u64> {
    let chars: Vec<char> = line.chars().collect();
    let mut grams: Vec<u64> = chars
        .windows(2)
        .map(|pair| (u64::from(pair[0]) << 32) | u64::from(pair[1]))
        .collect();
    grams.sort_unstable();
    grams
}

/// Sørensen–Dice similarity of two lines' character bigrams, 0..=1.
fn dice(a: &str, a_grams: &[u64], b: &str, b_grams: &[u64]) -> f64 {
    if a_grams.is_empty() || b_grams.is_empty() {
        return if a == b { 1. } else { 0. };
    }
    let (mut i, mut j, mut shared) = (0, 0, 0);
    while i < a_grams.len() && j < b_grams.len() {
        match a_grams[i].cmp(&b_grams[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                shared += 1;
                i += 1;
                j += 1;
            }
        }
    }
    2. * shared as f64 / (a_grams.len() + b_grams.len()) as f64
}

fn similarity(a: &str, b: &str) -> f64 {
    dice(a, &bigrams(a), b, &bigrams(b))
}
