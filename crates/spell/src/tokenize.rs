//! Splitting text into the words to look up.
//!
//! Text is cut at whitespace into chunks. Chunks that are URLs, e-mail
//! addresses, paths, `@tags`, `$variables`, `%formats`, `&entities;` or
//! `#hex` colors are skipped whole, and so is text between backticks
//! (inline code, fenced blocks in plain text). The rest splits into runs of
//! letters and digits (apostrophes inside a word stay: "don't"); a run with
//! a digit (`utf8`, `x86`, `sha256`, `0xff`) is skipped. Runs split again
//! at camelCase humps (`parseHTTPRequest` → parse, HTTP, Request); `_`,
//! `-` and `.` already separate them. Of the parts, those shorter than
//! three letters, all-caps ones up to four letters (`TODO`, `JSON`) and
//! long runs of hex letters (`deadbeef`) are skipped, and so are words in
//! scripts other than Latin.

use std::ops::Range;

/// Words shorter than this are never checked.
const MIN_LETTERS: usize = 3;
/// All-caps words up to this length are taken for acronyms.
const MAX_ACRONYM: usize = 4;
/// Runs of only `a-f` this long or longer are taken for hashes.
const MIN_HEX_WORD: usize = 8;

/// Appends the byte ranges (within `text`) of the words to check.
pub fn words(text: &str, out: &mut Vec<Range<usize>>) {
    let mut start = 0;
    for code in code_spans(text) {
        chunks(text, start..code.start, out);
        start = code.end;
    }
    chunks(text, start..text.len(), out);
}

/// Text between matching backtick runs on one line (inline code), and from
/// a line's run of three or more backticks to the next such run (a fence).
fn code_spans(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i] == b'`' {
            i += 1;
        }
        let run = i - start;
        let line_end = text[i..].find('\n').map_or(text.len(), |n| i + n);
        let closing = find_run(bytes, i..line_end, run, false);
        let end = match closing {
            Some(end) => end,
            None if run >= 3 => {
                find_run(bytes, line_end..bytes.len(), run, true).unwrap_or(text.len())
            }
            None => continue,
        };
        spans.push(start..end);
        i = end;
    }
    spans
}

/// The end of the first run of exactly `len` backticks in `range` (with
/// `at_least`, `len` or more).
fn find_run(bytes: &[u8], range: Range<usize>, len: usize, at_least: bool) -> Option<usize> {
    let mut i = range.start;
    while i < range.end {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let start = i;
        while i < range.end && bytes[i] == b'`' {
            i += 1;
        }
        let run = i - start;
        if run == len || (at_least && run > len) {
            return Some(i);
        }
    }
    None
}

/// Whitespace-separated chunks of `text[range]`.
fn chunks(text: &str, range: Range<usize>, out: &mut Vec<Range<usize>>) {
    let piece = &text[range.clone()];
    let mut chunk_start = None;
    for (i, c) in piece.char_indices() {
        if c.is_whitespace() {
            if let Some(start) = chunk_start.take() {
                chunk(text, range.start + start..range.start + i, out);
            }
        } else if chunk_start.is_none() {
            chunk_start = Some(i);
        }
    }
    if let Some(start) = chunk_start {
        chunk(text, range.start + start..range.end, out);
    }
}

fn chunk(text: &str, range: Range<usize>, out: &mut Vec<Range<usize>>) {
    const LEADING: &[char] = &[
        '"', '\'', '‘', '“', '(', '[', '{', '<', '*', '_', '~', '#', '>',
    ];
    const TRAILING: &[char] = &[
        '"', '\'', '’', '”', ')', ']', '}', '>', '.', ',', ';', ':', '!', '?', '*', '_', '~',
    ];
    let raw = &text[range.clone()];
    if let Some(hex) = raw.strip_prefix('#')
        && (3..=8).contains(&hex.len())
        && hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return;
    }
    let trimmed = raw.trim_start_matches(LEADING);
    let start = range.start + (raw.len() - trimmed.len());
    let trimmed = trimmed.trim_end_matches(TRAILING);
    if trimmed.is_empty() || skipped_chunk(trimmed) {
        return;
    }
    runs(text, start..start + trimmed.len(), out);
}

/// URLs, e-mail addresses and `@tags`, paths and escapes, variables,
/// format specifiers and HTML entities.
fn skipped_chunk(chunk: &str) -> bool {
    if chunk.contains("://") || chunk.starts_with("www.") || chunk.contains('@') {
        return true;
    }
    if chunk.contains('\\')
        || ["/", "~/", "./", "../"]
            .iter()
            .any(|prefix| chunk.starts_with(prefix))
    {
        return true;
    }
    if let Some(last) = chunk.rfind('/') {
        // `and/or` is prose; `src/lib.rs` and `a/b/c` are paths.
        if chunk.matches('/').count() >= 2 || chunk[last..].contains('.') {
            return true;
        }
    }
    // `&amp;` lost its `;` to the trailing punctuation.
    chunk.starts_with(['$', '%', '&'])
}

/// Letter/digit runs of `text[range]`, with apostrophes inside words.
fn runs(text: &str, range: Range<usize>, out: &mut Vec<Range<usize>>) {
    let piece = &text[range.clone()];
    let mut run_start: Option<usize> = None;
    let mut chars = piece.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let inner_apostrophe = matches!(c, '\'' | '’')
            && run_start.is_some()
            && chars.peek().is_some_and(|&(_, next)| next.is_alphabetic());
        if c.is_alphanumeric() || inner_apostrophe {
            run_start.get_or_insert(i);
        } else if let Some(start) = run_start.take() {
            run(text, range.start + start..range.start + i, out);
        }
    }
    if let Some(start) = run_start {
        run(text, range.start + start..range.end, out);
    }
}

fn run(text: &str, range: Range<usize>, out: &mut Vec<Range<usize>>) {
    let word = &text[range.clone()];
    if word
        .chars()
        .any(|c| c.is_numeric() || (c.is_alphabetic() && !latin(c)))
    {
        return;
    }
    // Hump boundaries: `aB` (before B) and `ABc` (before B).
    let mut part_start = 0;
    let mut previous: Option<char> = None;
    let mut chars = word.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if let Some(previous) = previous {
            let next_lower = chars.peek().is_some_and(|&(_, next)| next.is_lowercase());
            let hump = c.is_uppercase()
                && (previous.is_lowercase() || (previous.is_uppercase() && next_lower));
            if hump {
                part(text, range.start + part_start..range.start + i, out);
                part_start = i;
            }
        }
        previous = Some(c);
    }
    part(text, range.start + part_start..range.end, out);
}

fn part(text: &str, range: Range<usize>, out: &mut Vec<Range<usize>>) {
    let word = &text[range.clone()];
    let base = strip_possessive(word);
    let letters = base.chars().filter(|c| c.is_alphabetic()).count();
    if letters < MIN_LETTERS {
        return;
    }
    if letters <= MAX_ACRONYM && base.chars().all(|c| !c.is_lowercase()) {
        return;
    }
    if letters >= MIN_HEX_WORD && base.chars().all(|c| c.is_ascii_hexdigit()) {
        return;
    }
    out.push(range);
}

/// `word` without a trailing `'s`.
pub(crate) fn strip_possessive(word: &str) -> &str {
    word.strip_suffix("'s")
        .or_else(|| word.strip_suffix("’s"))
        .unwrap_or(word)
}

/// Latin letters (ASCII and the Latin-1/Extended-A/B blocks).
fn latin(c: char) -> bool {
    c.is_ascii_alphabetic() || ('\u{00C0}'..='\u{024F}').contains(&c) && c != '×' && c != '÷'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(text: &str) -> Vec<&str> {
        let mut ranges = Vec::new();
        words(text, &mut ranges);
        ranges.into_iter().map(|range| &text[range]).collect()
    }

    #[test]
    fn prose_splits_at_spaces_and_punctuation() {
        assert_eq!(
            split("Hello, world! (This is \"quoted\".) well-known and/or"),
            ["Hello", "world", "This", "quoted", "well", "known", "and"]
        );
    }

    #[test]
    fn identifiers_split_at_humps_underscores_and_dots() {
        assert_eq!(
            split("parseHTTPRequest snake_case_name XMLHttpRequest os.path.join getID"),
            [
                "parse", "Request", "snake", "case", "name", "Http", "Request", "path", "join",
                "get"
            ]
        );
    }

    #[test]
    fn short_words_acronyms_and_words_with_digits_are_skipped() {
        assert_eq!(
            split("an ox TODO JSON HTTPS utf8 x86_64 sha256 0xff 3rd Win32 README"),
            ["HTTPS", "README"]
        );
    }

    #[test]
    fn urls_emails_paths_and_hashes_are_skipped() {
        assert_eq!(
            split(
                "see https://exmaple.com/pth?q=1 or www.exmaple.org, mail jon@exmaple.com \
                 src/lib.rs /usr/bin ~/notes C:\\Users\\Foo ./run a/b/c #fafafa #fff deadbeefcafe 61e5f35"
            ),
            ["see", "mail"]
        );
    }

    #[test]
    fn tags_variables_formats_and_entities_are_skipped() {
        assert_eq!(
            split("@param $HOME ${VAR} %s %(name)s &amp; plain"),
            ["plain"]
        );
    }

    #[test]
    fn inline_code_and_fences_are_skipped() {
        assert_eq!(
            split("use `fooBarr` and ``a ` b`` here\n```\nlet mispeled = 1;\n```\nafter"),
            ["use", "and", "here", "after"]
        );
        // An unmatched backtick is just punctuation.
        assert_eq!(split("it`s okay"), ["okay"]);
    }

    #[test]
    fn apostrophes_stay_inside_words() {
        // `API's` is an acronym and `it's` is short once `'s` is taken off.
        assert_eq!(
            split("don't 'quoted' user’s API's it's"),
            ["don't", "quoted", "user’s"]
        );
    }

    #[test]
    fn ranges_are_byte_offsets_with_non_ascii_text() {
        let text = "naïve café — résumé 日本語 Привет";
        let mut ranges = Vec::new();
        words(text, &mut ranges);
        let found: Vec<&str> = ranges.iter().map(|r| &text[r.clone()]).collect();
        assert_eq!(found, ["naïve", "café", "résumé"]);
        assert_eq!(ranges[1], 7..12);
    }
}
