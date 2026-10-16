//! Syntax colors in diffs: each side of the diff is rebuilt from the lines
//! the diff shows (context and removed lines for the old side, context and
//! added lines for the new one) and highlighted like the editor highlights
//! a file. With "Whole file" that is the exact file; with "Changes only"
//! each hunk is parsed with its context lines (tree-sitter recovers from
//! the gaps, so only constructs spanning a gap can be off).

use std::{collections::HashMap, ops::Range};

use git::{LineKind, Row};
use gpui::{FontStyle, FontWeight, HighlightStyle, StrikethroughStyle, StyledText, UnderlineStyle};
use syntax::{Highlight, Language, Syntax};
use text::Buffer;

use crate::theme::SyntaxStyle;

/// Highlights of one line, as byte ranges within the line.
pub type LineSpans = Vec<(Range<usize>, Highlight)>;

/// Highlights per 1-based line number of each side.
#[derive(Default)]
pub struct DiffHighlights {
    old: HashMap<usize, LineSpans>,
    new: HashMap<usize, LineSpans>,
}

impl DiffHighlights {
    /// Highlights both sides of `rows`; empty without a known language.
    pub fn compute(language: Option<Language>, rows: &[Row]) -> Self {
        let Some(language) = language else {
            return Self::default();
        };
        let side = |old: bool| {
            let lines = rows.iter().filter_map(|row| match row {
                Row::Line {
                    kind,
                    old: old_line,
                    new: new_line,
                    text,
                } => match (old, kind) {
                    (true, LineKind::Added) | (false, LineKind::Removed) => None,
                    (true, _) => old_line.map(|line| (line, text.as_str())),
                    (false, _) => new_line.map(|line| (line, text.as_str())),
                },
                _ => None,
            });
            highlight_lines(language, lines)
        };
        Self {
            old: side(true),
            new: side(false),
        }
    }

    /// The highlights of `line` (1-based) on the old or new side.
    pub fn line(&self, old: bool, line: usize) -> &[(Range<usize>, Highlight)] {
        let side = if old { &self.old } else { &self.new };
        side.get(&line).map_or(&[], Vec::as_slice)
    }
}

/// Highlights `lines` (line number and text, in order) as one text.
fn highlight_lines<'a>(
    language: Language,
    lines: impl Iterator<Item = (usize, &'a str)>,
) -> HashMap<usize, LineSpans> {
    let mut text = String::new();
    let mut starts = Vec::new();
    for (line, content) in lines {
        starts.push((line, text.len(), content.len()));
        text.push_str(content);
        text.push('\n');
    }
    if text.is_empty() {
        return HashMap::new();
    }
    let buffer = Buffer::new(&text);
    let spans = Syntax::new(language, &buffer).highlights(&buffer, 0..text.len());
    let mut result = HashMap::with_capacity(starts.len());
    let mut first = 0;
    for (line, start, len) in starts {
        let end = start + len;
        // Spans are sorted and don't overlap: skip those that end before
        // this line.
        while first < spans.len() && spans[first].range.end <= start {
            first += 1;
        }
        let line_spans: LineSpans = spans[first..]
            .iter()
            .take_while(|span| span.range.start < end)
            .map(|span| {
                (
                    span.range.start.max(start) - start..span.range.end.min(end) - start,
                    span.highlight,
                )
            })
            .filter(|(range, _)| !range.is_empty())
            .collect();
        if !line_spans.is_empty() {
            result.insert(line, line_spans);
        }
    }
    result
}

/// `text` with tabs as four spaces, colored by `spans` (byte ranges of the
/// original text) in `styles`.
pub fn styled_line(
    text: &str,
    spans: &[(Range<usize>, Highlight)],
    styles: &[SyntaxStyle],
) -> StyledText {
    let (shown, spans) = expand_tabs(text, spans);
    let highlights: Vec<(Range<usize>, HighlightStyle)> = spans
        .into_iter()
        .filter_map(|(range, highlight)| {
            Some((range, highlight_style(styles.get(highlight.index())?)))
        })
        .collect();
    StyledText::new(shown).with_highlights(highlights)
}

/// `text` with tabs as four spaces, and `spans` moved to match.
fn expand_tabs(text: &str, spans: &[(Range<usize>, Highlight)]) -> (String, LineSpans) {
    // `offsets[i]`: where byte `i` of `text` lands in the shown text.
    let mut shown = String::with_capacity(text.len());
    let mut offsets = Vec::with_capacity(text.len() + 1);
    for (index, ch) in text.char_indices() {
        offsets.resize(index + 1, shown.len());
        if ch == '\t' {
            shown.push_str("    ");
        } else {
            shown.push(ch);
        }
    }
    offsets.resize(text.len() + 1, shown.len());
    let spans = spans
        .iter()
        .filter_map(|(range, highlight)| {
            let (start, end) = (*offsets.get(range.start)?, *offsets.get(range.end)?);
            (start < end).then_some((start..end, *highlight))
        })
        .collect();
    (shown, spans)
}

fn highlight_style(style: &SyntaxStyle) -> HighlightStyle {
    HighlightStyle {
        color: style.color,
        font_weight: (style.bold == Some(true)).then_some(FontWeight::BOLD),
        font_style: (style.italic == Some(true)).then_some(FontStyle::Italic),
        underline: (style.underline == Some(true)).then(|| UnderlineStyle {
            thickness: gpui::px(1.),
            color: style.color,
            wavy: false,
        }),
        strikethrough: (style.strikethrough == Some(true)).then(|| StrikethroughStyle {
            thickness: gpui::px(1.),
            color: style.color,
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: LineKind, old: Option<usize>, new: Option<usize>, text: &str) -> Row {
        Row::Line {
            kind,
            old,
            new,
            text: text.into(),
        }
    }

    fn scopes(highlights: &DiffHighlights, old: bool, number: usize, text: &str) -> Vec<String> {
        highlights
            .line(old, number)
            .iter()
            .map(|(range, highlight)| format!("{}={}", &text[range.clone()], highlight.scope()))
            .collect()
    }

    #[test]
    fn each_side_is_highlighted_with_its_own_lines() {
        let rows = vec![
            line(LineKind::Context, Some(1), Some(1), "def f():"),
            line(LineKind::Removed, Some(2), None, "    return 1"),
            line(LineKind::Added, None, Some(2), "    return \"one\""),
        ];
        let highlights = DiffHighlights::compute(Some(Language::Python), &rows);
        let old = scopes(&highlights, true, 2, "    return 1");
        assert!(
            old.iter().any(|s| s.starts_with("return=keyword")),
            "{old:?}"
        );
        assert!(old.iter().any(|s| s.starts_with("1=")), "{old:?}");
        let new = scopes(&highlights, false, 2, "    return \"one\"");
        assert!(new.iter().any(|s| s.contains("=string")), "{new:?}");
        // Context lines are highlighted on both sides; `def` on line 1.
        assert!(
            scopes(&highlights, true, 1, "def f():")
                .iter()
                .any(|s| s.starts_with("def=keyword"))
        );
        assert!(
            scopes(&highlights, false, 1, "def f():")
                .iter()
                .any(|s| s.starts_with("def=keyword"))
        );
    }

    #[test]
    fn tabs_shift_the_colored_ranges() {
        let keyword = Highlight::from_scope("keyword").unwrap();
        let (shown, spans) = expand_tabs("\treturn\tx", &[(1..7, keyword)]);
        assert_eq!(shown, "    return    x");
        assert_eq!(spans, [(4..10, keyword)]);
    }

    #[test]
    fn unknown_languages_have_no_highlights() {
        let rows = vec![line(LineKind::Context, Some(1), Some(1), "anything")];
        assert!(
            DiffHighlights::compute(None, &rows)
                .line(false, 1)
                .is_empty()
        );
    }
}
