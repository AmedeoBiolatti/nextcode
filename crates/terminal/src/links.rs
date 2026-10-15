//! Links in terminal output: URLs and file locations such as
//! `src/main.rs:10:5`, `C:\src\app.py:3` or Python's
//! `File "app.py", line 3`.

use std::ops::Range;

/// What a link opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    Url(String),
    /// A path as printed (relative paths are the reader's to resolve), with
    /// a 1-based line and column when given.
    File {
        path: String,
        line: Option<u32>,
        column: Option<u32>,
    },
}

/// A link in a line of text, by `char` index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub range: Range<usize>,
    pub target: LinkTarget,
}

const SCHEMES: &[&str] = &["https://", "http://", "file://", "ftp://"];

/// Every link in `line`, in order, not overlapping.
pub fn find_links(line: &str) -> Vec<Link> {
    let chars: Vec<char> = line.chars().collect();
    // URLs win over tracebacks, which win over plain locations (a URL's
    // path, or `File "a.py"` inside a traceback, is not a link of its own).
    let mut kept: Vec<Link> = Vec::new();
    for link in urls(&chars)
        .into_iter()
        .chain(tracebacks(&chars))
        .chain(locations(&chars))
    {
        if !kept.iter().any(|known| overlaps(&known.range, &link.range)) {
            kept.push(link);
        }
    }
    kept.sort_by_key(|link| link.range.start);
    kept
}

fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

fn starts_with(chars: &[char], at: usize, prefix: &str) -> bool {
    let mut index = at;
    for expected in prefix.chars() {
        match chars.get(index) {
            Some(c) if c.eq_ignore_ascii_case(&expected) => index += 1,
            _ => return false,
        }
    }
    true
}

/// Trailing punctuation that ends a sentence rather than the link, and
/// closing brackets without their opening one.
fn trim_end(chars: &[char], range: Range<usize>) -> Range<usize> {
    let mut end = range.end;
    while end > range.start {
        let last = chars[end - 1];
        let unbalanced = |open: char, close: char| {
            last == close && {
                let inner = &chars[range.start..end];
                inner.iter().filter(|&&c| c == open).count()
                    < inner.iter().filter(|&&c| c == close).count()
            }
        };
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"')
            || unbalanced('(', ')')
            || unbalanced('[', ']')
            || unbalanced('{', '}')
        {
            end -= 1;
        } else {
            break;
        }
    }
    range.start..end
}

fn urls(chars: &[char]) -> Vec<Link> {
    let mut links = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let Some(scheme) = SCHEMES
            .iter()
            .find(|scheme| starts_with(chars, index, scheme))
        else {
            index += 1;
            continue;
        };
        // Part of a longer word (`xhttps://`) is not a URL start.
        if index > 0 && chars[index - 1].is_alphanumeric() {
            index += 1;
            continue;
        }
        let mut end = index + scheme.len();
        while end < chars.len()
            && !chars[end].is_whitespace()
            && !matches!(chars[end], '"' | '<' | '>' | '`' | '\u{0}')
        {
            end += 1;
        }
        let range = trim_end(chars, index..end);
        if range.len() > scheme.len() {
            links.push(Link {
                target: LinkTarget::Url(chars[range.clone()].iter().collect()),
                range: range.clone(),
            });
        }
        index = end.max(index + 1);
    }
    links
}

/// Python's `File "path", line N`.
fn tracebacks(chars: &[char]) -> Vec<Link> {
    let mut links = Vec::new();
    let mut index = 0;
    while index + 6 <= chars.len() {
        if !starts_with(chars, index, "File \"") {
            index += 1;
            continue;
        }
        let path_start = index + 6;
        let Some(quote) = chars[path_start..].iter().position(|&c| c == '"') else {
            break;
        };
        let path_end = path_start + quote;
        let mut cursor = path_end + 1;
        if chars.get(cursor) == Some(&',') {
            cursor += 1;
        }
        while chars.get(cursor) == Some(&' ') {
            cursor += 1;
        }
        let line = starts_with(chars, cursor, "line ")
            .then(|| digits(chars, cursor + 5))
            .flatten();
        if let Some((line, end)) = line
            && path_end > path_start
        {
            links.push(Link {
                range: index..end,
                target: LinkTarget::File {
                    path: chars[path_start..path_end].iter().collect(),
                    line: Some(line),
                    column: None,
                },
            });
            index = end;
        } else {
            index = path_end + 1;
        }
    }
    links
}

/// A run of ASCII digits at `at`: its value and end.
fn digits(chars: &[char], at: usize) -> Option<(u32, usize)> {
    let end = at
        + chars[at.min(chars.len())..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
    let value: String = chars[at..end].iter().collect();
    Some((value.parse().ok()?, end))
}

fn path_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '/' | '\\' | '.' | '_' | '-' | '~' | '+' | '@' | ':')
}

/// `path`, `path:line` or `path:line:column`, for paths with a separator
/// or with a line number and an extension (`main.rs:3`). Bare words like
/// `README.md` are left alone: too many false positives.
fn locations(chars: &[char]) -> Vec<Link> {
    let mut links = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if !path_char(chars[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < chars.len() && path_char(chars[index]) {
            index += 1;
        }
        let token = trim_end(chars, start..index);
        if let Some(link) = location(chars, token) {
            links.push(link);
        }
    }
    links
}

fn location(chars: &[char], token: Range<usize>) -> Option<Link> {
    let text = &chars[token.clone()];
    // Peel `:column` and `:line` off the end.
    let mut numbers = Vec::new();
    let mut path_end = text.len();
    for _ in 0..2 {
        let digits = text[..path_end]
            .iter()
            .rev()
            .take_while(|c| c.is_ascii_digit())
            .count();
        if digits == 0 || path_end <= digits || text[path_end - digits - 1] != ':' {
            break;
        }
        let value: String = text[path_end - digits..path_end].iter().collect();
        numbers.push(value.parse::<u32>().ok()?);
        path_end -= digits + 1;
    }
    numbers.reverse();
    let path: String = text[..path_end].iter().collect();
    // A drive letter's colon is the only one a path keeps.
    let drive = path.len() >= 3
        && path.as_bytes()[0].is_ascii_alphabetic()
        && path.as_bytes()[1] == b':'
        && matches!(path.as_bytes()[2], b'\\' | b'/');
    let body = if drive { &path[2..] } else { &path[..] };
    if body.contains(':') || !body.chars().any(char::is_alphabetic) {
        return None;
    }
    let separator = body.contains(['/', '\\']);
    let name = body.rsplit(['/', '\\']).next().unwrap_or(body);
    let extension = name
        .rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && ext.chars().any(char::is_alphabetic));
    let line = numbers.first().copied();
    // `//` alone, `./`, `..`: not files.
    let trivial = name.is_empty() || name.chars().all(|c| c == '.');
    if trivial || !(separator && (extension || line.is_some()) || extension && line.is_some()) {
        return None;
    }
    Some(Link {
        range: token,
        target: LinkTarget::File {
            path,
            line,
            column: numbers.get(1).copied(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(line: &str) -> Vec<(String, LinkTarget)> {
        let chars: Vec<char> = line.chars().collect();
        find_links(line)
            .into_iter()
            .map(|link| (chars[link.range].iter().collect(), link.target))
            .collect()
    }

    fn file(path: &str, line: Option<u32>, column: Option<u32>) -> LinkTarget {
        LinkTarget::File {
            path: path.into(),
            line,
            column,
        }
    }

    #[test]
    fn urls_end_before_trailing_punctuation_and_unbalanced_brackets() {
        let url = |s: &str| LinkTarget::Url(s.into());
        assert_eq!(
            found("see https://example.com/a?b=1#c."),
            [(
                "https://example.com/a?b=1#c".into(),
                url("https://example.com/a?b=1#c")
            )]
        );
        assert_eq!(
            found("(docs: https://en.wikipedia.org/wiki/Rust_(language))"),
            [(
                "https://en.wikipedia.org/wiki/Rust_(language)".into(),
                url("https://en.wikipedia.org/wiki/Rust_(language)")
            )]
        );
        assert_eq!(
            found("<http://localhost:8000/>")[0].0,
            "http://localhost:8000/"
        );
        assert!(found("https:// alone").is_empty());
    }

    #[test]
    fn compiler_and_tool_locations() {
        assert_eq!(
            found("  --> crates/ui/src/main.rs:10:5"),
            [(
                "crates/ui/src/main.rs:10:5".into(),
                file("crates/ui/src/main.rs", Some(10), Some(5))
            )]
        );
        assert_eq!(
            found("tests/test_a.py:12: AssertionError"),
            [(
                "tests/test_a.py:12".into(),
                file("tests/test_a.py", Some(12), None)
            )]
        );
        assert_eq!(
            found(r"error at C:\src\app\main.py:7"),
            [(
                r"C:\src\app\main.py:7".into(),
                file(r"C:\src\app\main.py", Some(7), None)
            )]
        );
        assert_eq!(found("main.rs:3")[0].1, file("main.rs", Some(3), None));
        assert_eq!(
            found("modified:   src/lib.rs")[0].1,
            file("src/lib.rs", None, None)
        );
    }

    #[test]
    fn python_tracebacks_link_the_whole_reference() {
        assert_eq!(
            found(r#"  File "/app/run.py", line 42, in main"#),
            [(
                r#"File "/app/run.py", line 42"#.into(),
                file("/app/run.py", Some(42), None)
            )]
        );
    }

    #[test]
    fn ordinary_words_numbers_and_times_are_not_links() {
        for text in [
            "README.md",
            "version 1.2.3",
            "at 12:30:15",
            "a ratio of 3:2",
            "./",
            "and/or",
            "x::y::z",
        ] {
            assert!(found(text).is_empty(), "{text}: {:?}", found(text));
        }
    }
}
