//! A command's terminal output as text: escape sequences stripped across
//! chunk boundaries, readiness regexes matched as output arrives, the last
//! line for display, and the URLs printed (each reported once).

use std::collections::{HashSet, VecDeque};

use regex::Regex;
use terminal::{LinkTarget, find_links};

/// Text kept for regexes that span chunks (and lines).
const WINDOW: usize = 8 * 1024;
/// Longest line kept for URL detection and display.
const MAX_LINE: usize = 4 * 1024;
/// Complete lines kept for display.
const RECENT_LINES: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Escape {
    #[default]
    Ground,
    /// After ESC.
    Esc,
    /// ESC + intermediates (`ESC ( B`): ends at the final byte.
    EscIntermediate,
    /// `ESC [ ... final`.
    Csi,
    /// `ESC ] ...` (OSC) or `ESC P/X/^/_ ...` (DCS, SOS, PM, APC): ends at
    /// BEL or ST (`ESC \`).
    String,
    /// ESC inside a string: `\` ends it.
    StringEsc,
}

/// Turns terminal output bytes into plain text: CSI/OSC/DCS sequences and
/// control characters (except newline and tab) are dropped, carriage
/// returns become line breaks, and UTF-8 split between chunks is joined.
#[derive(Debug, Default)]
pub struct AnsiStripper {
    state: Escape,
    /// The start of a UTF-8 sequence cut off at the end of a chunk.
    partial: Vec<u8>,
}

impl AnsiStripper {
    /// Appends the text of `bytes` to `out`.
    pub fn push(&mut self, bytes: &[u8], out: &mut String) {
        let mut text = Vec::with_capacity(bytes.len() + self.partial.len());
        text.append(&mut self.partial);
        for &byte in bytes {
            self.state = match (self.state, byte) {
                (Escape::Ground, 0x1b) => Escape::Esc,
                (Escape::Ground, b'\n' | b'\t') => {
                    text.push(byte);
                    Escape::Ground
                }
                (Escape::Ground, b'\r') => {
                    text.push(b'\n');
                    Escape::Ground
                }
                (Escape::Ground, 0..=0x1f | 0x7f) => Escape::Ground,
                (Escape::Ground, _) => {
                    text.push(byte);
                    Escape::Ground
                }
                (Escape::Esc, b'[') => Escape::Csi,
                (Escape::Esc, b']' | b'P' | b'X' | b'^' | b'_') => Escape::String,
                (Escape::Esc, 0x20..=0x2f) => Escape::EscIntermediate,
                (Escape::Esc, 0x1b) => Escape::Esc,
                (Escape::Esc, _) => Escape::Ground,
                (Escape::EscIntermediate, 0x20..=0x2f) => Escape::EscIntermediate,
                (Escape::EscIntermediate, _) => Escape::Ground,
                (Escape::Csi, 0x40..=0x7e) => Escape::Ground,
                (Escape::Csi, 0x1b) => Escape::Esc,
                (Escape::Csi, _) => Escape::Csi,
                (Escape::String, 0x07) => Escape::Ground,
                (Escape::String, 0x1b) => Escape::StringEsc,
                (Escape::String, _) => Escape::String,
                (Escape::StringEsc, b'\\') => Escape::Ground,
                (Escape::StringEsc, _) => Escape::String,
            };
        }
        // Keep an incomplete UTF-8 sequence at the end for the next chunk.
        let keep = incomplete_utf8_tail(&text);
        self.partial = text.split_off(text.len() - keep);
        out.push_str(&String::from_utf8_lossy(&text));
    }
}

/// How many bytes at the end of `bytes` start a UTF-8 sequence that is not
/// complete yet.
fn incomplete_utf8_tail(bytes: &[u8]) -> usize {
    for back in 1..=3.min(bytes.len()) {
        let byte = bytes[bytes.len() - back];
        if byte & 0xc0 == 0x80 {
            continue; // continuation byte: look further back
        }
        let needed = match byte {
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => return 0,
        };
        return if back < needed { back } else { 0 };
    }
    0
}

/// What one chunk of output brought.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Seen {
    /// The readiness regex matched (now or earlier).
    pub matched: bool,
    /// http(s) URLs on lines completed by this chunk, not seen before.
    pub urls: Vec<String>,
}

/// Follows one command's output.
#[derive(Debug)]
pub struct OutputWatcher {
    stripper: AnsiStripper,
    pattern: Option<Regex>,
    matched: bool,
    /// The latest text (up to `WINDOW` bytes) for the pattern.
    window: String,
    /// The line being printed.
    line: String,
    /// The last non-empty complete lines, oldest first.
    recent: VecDeque<String>,
    urls: HashSet<String>,
}

impl OutputWatcher {
    /// `pattern` is a readiness regex (validated when the file was parsed).
    pub fn new(pattern: Option<&str>) -> Self {
        Self {
            stripper: AnsiStripper::default(),
            pattern: pattern.and_then(|pattern| Regex::new(pattern).ok()),
            matched: false,
            window: String::new(),
            line: String::new(),
            recent: VecDeque::new(),
            urls: HashSet::new(),
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Seen {
        let mut text = String::new();
        self.stripper.push(bytes, &mut text);
        let mut seen = Seen::default();
        if text.is_empty() {
            seen.matched = self.matched;
            return seen;
        }
        if let Some(pattern) = &self.pattern
            && !self.matched
        {
            self.window.push_str(&text);
            self.matched = pattern.is_match(&self.window);
            if self.window.len() > WINDOW {
                let mut cut = self.window.len() - WINDOW / 2;
                while !self.window.is_char_boundary(cut) {
                    cut += 1;
                }
                self.window.drain(..cut);
            }
        }
        seen.matched = self.matched;
        for (index, piece) in text.split('\n').enumerate() {
            if index > 0 {
                let line = std::mem::take(&mut self.line);
                for url in urls_in(&line) {
                    if self.urls.insert(url.clone()) {
                        seen.urls.push(url);
                    }
                }
                // An interactive login shell (how steps run, so PATH is as
                // in a terminal) prints `logout` as it exits: not the
                // command's output.
                if !line.trim().is_empty() && line.trim() != "logout" {
                    if self.recent.len() == RECENT_LINES {
                        self.recent.pop_front();
                    }
                    self.recent.push_back(line);
                }
            }
            if self.line.len() < MAX_LINE {
                self.line.push_str(piece);
            }
        }
        seen
    }

    /// The line being printed if it has text, else the last complete one.
    pub fn last_line(&self) -> &str {
        self.last_lines(1).pop().unwrap_or_default()
    }

    /// Up to `count` last lines (the one being printed included when it
    /// has text), oldest first, trimmed.
    pub fn last_lines(&self, count: usize) -> Vec<&str> {
        let current = self.line.trim();
        let current = (!current.is_empty() && current != "logout").then_some(current);
        let complete = count.saturating_sub(usize::from(current.is_some()));
        let mut lines: Vec<&str> = self
            .recent
            .iter()
            .skip(self.recent.len().saturating_sub(complete))
            .map(|line| line.trim())
            .collect();
        lines.extend(current);
        lines
    }
}

/// The http(s) URLs in a line of text.
pub fn urls_in(line: &str) -> Vec<String> {
    find_links(line)
        .into_iter()
        .filter_map(|link| match link.target {
            LinkTarget::Url(url) if url.starts_with("http://") || url.starts_with("https://") => {
                Some(url)
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(chunks: &[&[u8]]) -> String {
        let mut stripper = AnsiStripper::default();
        let mut out = String::new();
        for chunk in chunks {
            stripper.push(chunk, &mut out);
        }
        out
    }

    #[test]
    fn strips_escape_sequences_across_chunks() {
        assert_eq!(strip(&[b"\x1b[1;32mgreen\x1b[0m"]), "green");
        assert_eq!(strip(&[b"\x1b[1;3", b"2mgreen\x1b", b"[0m!"]), "green!");
        // OSC title (BEL and ST endings), OSC 8 hyperlink, DCS, charset.
        assert_eq!(
            strip(&[b"\x1b]0;title\x07a\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\ b\x1bP1$r\x1b\\c\x1b(Bd"]),
            "alink bcd"
        );
        assert_eq!(strip(&[b"one\r\ntwo\x08\x07\tx"]), "one\n\ntwo\tx");
        // A UTF-8 character split between chunks.
        let bytes = "héllo ✓".as_bytes();
        assert_eq!(strip(&[&bytes[..2], &bytes[2..8], &bytes[8..]]), "héllo ✓");
    }

    #[test]
    fn matches_a_regex_split_between_chunks_and_colors() {
        let mut watcher = OutputWatcher::new(Some(r"Serving HTTP on .* port (\d+)"));
        assert!(!watcher.push(b"starting\r\nServing HT").matched);
        assert!(!watcher.push(b"TP on \x1b[1m0.0.0.0\x1b[0m po").matched);
        assert!(watcher.push(b"rt 8000 ...").matched);
        // Stays matched.
        assert!(watcher.push(b"\r\nmore").matched);
        assert_eq!(watcher.last_line(), "more");

        let mut lines = OutputWatcher::new(Some("(?m)^ready$"));
        assert!(!lines.push(b"not ready\n").matched);
        assert!(!lines.push(b"rea").matched && lines.push(b"dy\n").matched);
    }

    #[test]
    fn long_output_keeps_matching_new_text() {
        let mut watcher = OutputWatcher::new(Some("DONE"));
        for _ in 0..1000 {
            assert!(!watcher.push(b"0123456789abcdef0123456789abcdef\n").matched);
        }
        assert!(watcher.window.len() <= WINDOW);
        assert!(!watcher.push(b"DO").matched && watcher.push(b"NE").matched);
    }

    #[test]
    fn reports_each_url_once_from_complete_lines() {
        let mut watcher = OutputWatcher::new(None);
        let seen =
            watcher.push(b"Open \x1b[4mhttp://localhost:8000/login?x=1\x1b[0m.\r\nhttp://exa");
        assert_eq!(
            seen.urls,
            vec!["http://localhost:8000/login?x=1".to_owned()]
        );
        let seen = watcher.push(b"mple.com/a (and https://b.example/)\n");
        assert_eq!(
            seen.urls,
            vec![
                "http://example.com/a".to_owned(),
                "https://b.example/".to_owned()
            ]
        );
        let again =
            watcher.push(b"again http://localhost:8000/login?x=1\nfile:///tmp/x ftp://h/\n");
        assert_eq!(again.urls, Vec::<String>::new());
        assert_eq!(watcher.last_line(), "file:///tmp/x ftp://h/");
    }

    #[test]
    fn the_login_shells_logout_is_not_the_last_line() {
        let mut watcher = OutputWatcher::new(None);
        watcher.push(b"all up\r\nlogout");
        assert_eq!(watcher.last_line(), "all up");
        watcher.push(b"\r\n");
        assert_eq!(watcher.last_line(), "all up");
    }

    #[test]
    fn the_last_lines_include_the_one_being_printed() {
        let mut watcher = OutputWatcher::new(None);
        assert_eq!(watcher.last_lines(3), Vec::<&str>::new());
        watcher.push(b"1\n2\n\n3\n4\n5\n6\n7\npartial");
        assert_eq!(watcher.last_lines(3), ["6", "7", "partial"]);
        watcher.push(b"\n");
        assert_eq!(watcher.last_lines(3), ["6", "7", "partial"]);
        assert_eq!(watcher.last_lines(99).len(), RECENT_LINES);
        assert_eq!(watcher.last_line(), "partial");
    }
}
