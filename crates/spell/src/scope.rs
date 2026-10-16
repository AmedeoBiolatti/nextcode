//! Which parts of a document are spell checked: comments, strings and
//! docstrings of code (from tree-sitter highlights), Markdown outside code
//! and link targets, and every line of plain text and commit messages.

use std::{ops::Range, path::Path};

use syntax::{Highlight, HighlightSpan, Language, Syntax};
use text::Buffer;

/// How a document is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Every line (plain text, commit messages).
    Prose,
    /// Everything but code blocks, link targets and fence info strings.
    Markdown,
    /// Comments, docstrings and strings.
    Code,
}

/// File names whose text is prose: git's message files.
const MESSAGE_FILES: &[&str] = &["COMMIT_EDITMSG", "MERGE_MSG", "TAG_EDITMSG", "SQUASH_MSG"];
/// Extensions of prose files.
const PROSE_EXTENSIONS: &[&str] = &["txt", "text", "rst", "adoc", "asciidoc", "org"];

impl Scope {
    /// The scope for a file of `language` at `path`; `None` when it is not
    /// checked (code with `check_code` off, unknown non-prose files).
    pub fn of(language: Option<Language>, path: Option<&Path>, check_code: bool) -> Option<Self> {
        match language {
            Some(Language::Markdown) => Some(Self::Markdown),
            Some(_) => check_code.then_some(Self::Code),
            None => {
                let path = path?;
                let name = path.file_name()?.to_str()?;
                let prose = MESSAGE_FILES.contains(&name)
                    || path
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .is_some_and(|extension| {
                            PROSE_EXTENSIONS
                                .iter()
                                .any(|known| extension.eq_ignore_ascii_case(known))
                        });
                prose.then_some(Self::Prose)
            }
        }
    }
}

/// Highlights whose text is checked in code.
fn checked_in_code(highlight: Highlight) -> bool {
    matches!(highlight.scope(), "comment" | "comment.doc" | "string")
}

/// Highlights whose text is skipped in Markdown.
fn skipped_in_markdown(highlight: Highlight) -> bool {
    matches!(highlight.scope(), "markup.raw" | "markup.link" | "label")
}

/// Appends the byte ranges of `lines` to check, each within one line and
/// sorted. Code and Markdown need `syntax` (without it nothing / every
/// line is checked).
pub fn segments(
    buffer: &Buffer,
    syntax: Option<&Syntax>,
    scope: Scope,
    lines: Range<usize>,
    out: &mut Vec<Range<usize>>,
) {
    let lines = lines.start..lines.end.min(buffer.line_count());
    if lines.is_empty() {
        return;
    }
    let start = buffer.line_start(lines.start);
    let end = buffer.line_end(lines.end - 1);
    match (scope, syntax) {
        (Scope::Code, None) => {}
        (Scope::Prose, _) | (Scope::Markdown, None) => {
            for line in lines {
                push(out, buffer.line_start(line)..buffer.line_end(line));
            }
        }
        (Scope::Code, Some(syntax)) => {
            let spans = syntax.highlights(buffer, start..end);
            let mut pending: Option<(Range<usize>, bool)> = None;
            for span in spans.iter().filter(|span| checked_in_code(span.highlight)) {
                let comment = span.highlight.scope().starts_with("comment");
                match &mut pending {
                    // Adjacent spans (a doc comment's marker and text) join.
                    Some((range, _)) if range.end == span.range.start => range.end = span.range.end,
                    _ => {
                        if let Some((range, comment)) = pending.take() {
                            split_lines(buffer, range, comment, out);
                        }
                        pending = Some((span.range.clone(), comment));
                    }
                }
            }
            if let Some((range, comment)) = pending {
                split_lines(buffer, range, comment, out);
            }
        }
        (Scope::Markdown, Some(syntax)) => {
            let spans = syntax.highlights(buffer, start..end);
            let skipped: Vec<&HighlightSpan> = spans
                .iter()
                .filter(|span| skipped_in_markdown(span.highlight))
                .collect();
            let mut next = 0;
            for line in lines {
                let mut from = buffer.line_start(line);
                let line_end = buffer.line_end(line);
                while next < skipped.len() && skipped[next].range.end <= from {
                    next += 1;
                }
                for span in skipped[next..]
                    .iter()
                    .take_while(|span| span.range.start < line_end)
                {
                    push(out, from..span.range.start.max(from));
                    from = from.max(span.range.end);
                }
                if from < line_end {
                    push(out, from..line_end);
                }
            }
        }
    }
}

/// `range` cut at line breaks; comment pieces start after their markers.
fn split_lines(buffer: &Buffer, range: Range<usize>, comment: bool, out: &mut Vec<Range<usize>>) {
    let first = buffer.line_of(range.start);
    let last = buffer.line_of(range.end.max(range.start + 1) - 1);
    for line in first..=last {
        let start = range.start.max(buffer.line_start(line));
        let end = range.end.min(buffer.line_end(line));
        if start >= end {
            continue;
        }
        let start = if comment {
            let text: String = buffer.chunks(start..end).collect();
            let markers = text.len()
                - text
                    .trim_start_matches(|c: char| {
                        matches!(c, '/' | '#' | '*' | '!' | '-' | ';' | '%') || c.is_whitespace()
                    })
                    .len();
            start + markers
        } else {
            start
        };
        push(out, start..end);
    }
}

fn push(out: &mut Vec<Range<usize>>, range: Range<usize>) {
    if !range.is_empty() {
        out.push(range);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BUILT_IN, Checker, dictionary};

    /// The misspelled words `scope` finds in `text`, in order.
    fn typos(language: Option<Language>, scope: Scope, text: &str) -> Vec<String> {
        let buffer = Buffer::new(text);
        let syntax = language.map(|language| Syntax::new(language, &buffer));
        let mut ranges = Vec::new();
        segments(
            &buffer,
            syntax.as_ref(),
            scope,
            0..buffer.line_count(),
            &mut ranges,
        );
        let checker = Checker::new(vec![dictionary(BUILT_IN, None).unwrap()], Vec::new());
        let mut found = Vec::new();
        let mut words = Vec::new();
        for range in ranges {
            let piece = buffer.slice(range);
            words.clear();
            checker.check(&piece, &mut words);
            found.extend(words.iter().map(|word| piece[word.clone()].to_owned()));
        }
        found
    }

    #[test]
    fn python_comments_docstrings_and_strings_but_not_identifiers() {
        let text = "\"\"\"Modul docstrng.\n\nSecond paragrph.\n\"\"\"\n\
                    def fetch_usr(argumnt):\n    \
                    '''Retreive the user.'''\n    \
                    # Coment about the argumnt\n    \
                    mesage = f\"Helo {argumnt}\\n\"\n    \
                    return mesage  # trailling\n";
        assert_eq!(
            typos(Some(Language::Python), Scope::Code, text),
            [
                "Modul",
                "docstrng",
                "paragrph",
                "Retreive",
                "Coment",
                "argumnt",
                "Helo",
                "trailling"
            ]
        );
    }

    #[test]
    fn rust_line_doc_and_block_comments_and_strings() {
        let text = "/// Retuns the valu.\n//! Crate levl docs.\nfn main() {\n    \
                    let mispeled = \"wrold\"; // inlin\n    /* blokc */\n}\n";
        assert_eq!(
            typos(Some(Language::Rust), Scope::Code, text),
            ["Retuns", "valu", "levl", "wrold", "inlin", "blokc"]
        );
    }

    #[test]
    fn javascript_comments_strings_and_templates() {
        let text = "// Compute the totl\nconst mesage = `Helo ${userNmae} wrld`;\n\
                    const other = 'singl';\n/** Docs heer */\n";
        assert_eq!(
            typos(Some(Language::JavaScript), Scope::Code, text),
            ["totl", "Helo", "wrld", "singl", "heer"]
        );
    }

    #[test]
    fn markdown_prose_without_code_or_link_targets() {
        let text = "# Titel\n\nSome prose with a tpyo and `inlin` code.\n\n\
                    ```python\nmispeled = 1\n```\n\n    indnted code\n\n\
                    [label]: https://exmaple.com/pathh\n- item lsit\n";
        assert_eq!(
            typos(Some(Language::Markdown), Scope::Markdown, text),
            ["Titel", "tpyo", "lsit"]
        );
    }

    #[test]
    fn prose_checks_every_line_and_code_without_syntax_nothing() {
        assert_eq!(
            typos(None, Scope::Prose, "Fix teh bug\n\nMore detials"),
            ["teh", "detials"]
        );
        assert!(typos(None, Scope::Code, "# teh").is_empty());
    }

    #[test]
    fn scope_by_language_path_and_setting() {
        let path = |p: &str| Some(Path::new(p)).map(|p| p.to_path_buf());
        let of = |language, p: &str, code| Scope::of(language, path(p).as_deref(), code);
        assert_eq!(of(Some(Language::Python), "a.py", true), Some(Scope::Code));
        assert_eq!(of(Some(Language::Python), "a.py", false), None);
        assert_eq!(
            of(Some(Language::Markdown), "a.md", false),
            Some(Scope::Markdown)
        );
        assert_eq!(of(None, "notes.TXT", false), Some(Scope::Prose));
        assert_eq!(of(None, ".git/COMMIT_EDITMSG", false), Some(Scope::Prose));
        assert_eq!(of(None, "Main.java", true), None);
        assert_eq!(Scope::of(None, None, true), None);
    }

    #[test]
    fn only_requested_lines_are_returned() {
        let buffer = Buffer::new("one\ntwo\nthree\n");
        let mut ranges = Vec::new();
        segments(&buffer, None, Scope::Prose, 1..10, &mut ranges);
        assert_eq!(ranges, [4..7, 8..13]);
    }
}
