//! Bracket matching: the pair at the primary cursor (highlighted) and
//! Ctrl+Shift+\ to jump between them.

use syntax::Highlight;

use super::*;

actions!(editor, [JumpToBracket]);

/// How far the text scan (used where the syntax tree has no bracket
/// tokens) looks for a partner, in bytes each way.
const SCAN_LIMIT: usize = 64 * 1024;

const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];

/// The bracket pair (both offsets) for a `(buffer version, primary head)`.
pub(super) type BracketCache = ((u64, usize), Option<(usize, usize)>);

/// Offset of the bracket matching the one at `at` in `text` (offsets into
/// `text`), skipping brackets inside `skip` ranges (strings, comments).
pub(super) fn scan_partner(text: &str, at: usize, skip: &[Range<usize>]) -> Option<usize> {
    let bracket = text[at..].chars().next()?;
    let skipped = |offset: usize| {
        let i = skip.partition_point(|range| range.end <= offset);
        skip.get(i).is_some_and(|range| range.start <= offset)
    };
    if skipped(at) {
        return None;
    }
    let mut depth = 0usize;
    if let Some(&(open, close)) = PAIRS.iter().find(|(open, _)| *open == bracket) {
        for (i, c) in text[at + 1..].char_indices() {
            let offset = at + 1 + i;
            if (c == open || c == close) && !skipped(offset) {
                if c == open {
                    depth += 1;
                } else if depth == 0 {
                    return Some(offset);
                } else {
                    depth -= 1;
                }
            }
        }
    } else if let Some(&(open, close)) = PAIRS.iter().find(|(_, close)| *close == bracket) {
        for (offset, c) in text[..at].char_indices().rev() {
            if (c == open || c == close) && !skipped(offset) {
                if c == close {
                    depth += 1;
                } else if depth == 0 {
                    return Some(offset);
                } else {
                    depth -= 1;
                }
            }
        }
    }
    None
}

fn is_bracket(c: char) -> bool {
    PAIRS.iter().any(|&(open, close)| c == open || c == close)
}

impl Editor {
    /// The partner of the bracket at `offset`: from the syntax tree when it
    /// has a token there, else a text scan that skips strings and comments.
    fn partner_of(&self, offset: usize) -> Option<usize> {
        if !self.char_at(offset).is_some_and(is_bracket) {
            return None;
        }
        let Some(syntax) = &self.syntax else {
            return self.scan_partner_around(offset, &[]);
        };
        if let Some(partner) = syntax.bracket_partner(offset) {
            return partner;
        }
        let start = offset.saturating_sub(SCAN_LIMIT);
        let end = (offset + SCAN_LIMIT).min(self.buffer.len());
        let start = self
            .buffer
            .offset_of(self.buffer.point(start), Encoding::Utf8);
        let quiet = [
            Highlight::from_scope("string"),
            Highlight::from_scope("comment"),
        ];
        let skip: Vec<Range<usize>> = syntax
            .highlights(&self.buffer, start..end)
            .into_iter()
            .filter(|span| {
                quiet
                    .iter()
                    .flatten()
                    .any(|scope| span.highlight.scope().starts_with(scope.scope()))
            })
            .map(|span| span.range)
            .collect();
        self.scan_partner_around(offset, &skip)
    }

    /// Text scan within `SCAN_LIMIT` of `offset`; `skip` holds buffer ranges.
    fn scan_partner_around(&self, offset: usize, skip: &[Range<usize>]) -> Option<usize> {
        let start = offset.saturating_sub(SCAN_LIMIT);
        let start = self
            .buffer
            .offset_of(self.buffer.point(start), Encoding::Utf8);
        let end = (offset + SCAN_LIMIT).min(self.buffer.len());
        let end = self
            .buffer
            .offset_of(self.buffer.point(end), Encoding::Utf8);
        let text = self.buffer.slice(start..end);
        let skip: Vec<Range<usize>> = skip
            .iter()
            .map(|range| range.start.max(start) - start..range.end.min(end).max(start) - start)
            .collect();
        scan_partner(&text, offset - start, &skip).map(|partner| partner + start)
    }

    /// The bracket pair touching the primary cursor: the bracket after it,
    /// else the one before it. Offsets of both brackets.
    pub(super) fn bracket_pair(&self) -> Option<(usize, usize)> {
        let head = self.primary().head;
        if let Some(partner) = self.partner_of(head) {
            return Some((head, partner));
        }
        let before = self.buffer.prev_grapheme(head);
        (before < head)
            .then(|| self.partner_of(before))
            .flatten()
            .map(|partner| (before, partner))
    }

    /// Cached `bracket_pair` for painting: recomputed when the buffer or
    /// the primary cursor changes.
    pub(super) fn cached_bracket_pair(&mut self) -> Option<(usize, usize)> {
        let key = (self.version, self.primary().head);
        match self.bracket_cache {
            Some((cached, pair)) if cached == key => pair,
            _ => {
                let pair = self.bracket_pair();
                self.bracket_cache = Some((key, pair));
                pair
            }
        }
    }

    /// The innermost bracket pair enclosing `offset`, by text scan.
    fn enclosing_open(&self, offset: usize) -> Option<usize> {
        let start = offset.saturating_sub(SCAN_LIMIT);
        let start = self
            .buffer
            .offset_of(self.buffer.point(start), Encoding::Utf8);
        let text = self.buffer.slice(start..offset);
        let mut depth = 0usize;
        for (i, c) in text.char_indices().rev() {
            if PAIRS.iter().any(|(_, close)| *close == c) {
                depth += 1;
            } else if PAIRS.iter().any(|(open, _)| *open == c) {
                if depth == 0 {
                    let candidate = start + i;
                    if self.partner_of(candidate).is_some_and(|end| end >= offset) {
                        return Some(candidate);
                    }
                } else {
                    depth -= 1;
                }
            }
        }
        None
    }

    /// Ctrl+Shift+\: jumps to the partner of the bracket at each cursor, or
    /// to the closer of the enclosing pair.
    pub(super) fn jump_to_bracket(
        &mut self,
        _: &JumpToBracket,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selections = self.selections.map(|s| {
            let head = s.head;
            let before = self.buffer.prev_grapheme(head);
            let target = self
                .partner_of(head)
                .or_else(|| (before < head).then(|| self.partner_of(before)).flatten())
                .or_else(|| {
                    self.enclosing_open(head)
                        .and_then(|open| self.partner_of(open))
                });
            Selection::cursor(target.unwrap_or(head))
        });
        self.set_selections(selections, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_scan_matches_nested_pairs_and_skips_ranges() {
        let text = "f(a, \")\", [b(c)])";
        let open = 1;
        let close = text.len() - 1;
        let string = 5..8;
        let skip = std::slice::from_ref(&string);
        assert_eq!(scan_partner(text, open, skip), Some(close));
        assert_eq!(scan_partner(text, close, skip), Some(open));
        assert_eq!(scan_partner(text, open, &[]), Some(6), "without skipping");
        assert_eq!(scan_partner(text, 6, skip), None, "inside the string");
        assert_eq!(scan_partner(text, 10, &[]), Some(15));
        assert_eq!(scan_partner(text, 0, &[]), None, "not a bracket");
    }
}
