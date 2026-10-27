//! Completion: local filtering of server items and the edits to accept one.

use std::{cmp::Ordering, ops::Range};

use lsp::types::{CompletionItem, CompletionTextEdit};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{AtomKind, CaseMatching, Normalization, Pattern},
};
use text::{Buffer, Encoding};

use crate::{resolve_edits, to_range};

/// Start of the identifier ending at `offset` (letters, digits, `_`).
pub fn word_start(buffer: &Buffer, offset: usize) -> usize {
    let line_start = buffer.line_start(buffer.line_of(offset));
    let before = buffer.slice(line_start..offset);
    let word_len: usize = before
        .chars()
        .rev()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .map(char::len_utf8)
        .sum();
    offset - word_len
}

/// An item that matches the typed query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Index into the server's item list.
    pub index: usize,
    pub score: u32,
    /// Byte offsets of matched characters in the item's label.
    pub positions: Vec<usize>,
}

/// Items matching `query` (fuzzy, smart case), best first: items starting
/// with the query exactly as typed, then by match score, then the server's
/// `sortText` order. An empty query keeps every item.
pub fn filter(items: &[CompletionItem], query: &str) -> Vec<Candidate> {
    let sort_key =
        |item: &CompletionItem| item.sort_text.clone().unwrap_or_else(|| item.label.clone());
    let exact_prefix = |item: &CompletionItem| {
        !query.is_empty()
            && item
                .filter_text
                .as_deref()
                .unwrap_or(&item.label)
                .starts_with(query)
    };
    let mut candidates: Vec<Candidate> = if query.is_empty() {
        (0..items.len())
            .map(|index| Candidate {
                index,
                score: 0,
                positions: Vec::new(),
            })
            .collect()
    } else {
        // Fuzzy (not substring) atoms: `osp` should find `os.path`.
        let pattern = Pattern::new(
            query,
            CaseMatching::Smart,
            Normalization::Smart,
            AtomKind::Fuzzy,
        );
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut chars = Vec::new();
        let mut indices = Vec::new();
        items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let haystack = item.filter_text.as_deref().unwrap_or(&item.label);
                let score = pattern.score(Utf32Str::new(haystack, &mut chars), &mut matcher)?;
                indices.clear();
                let positions = if pattern
                    .indices(
                        Utf32Str::new(&item.label, &mut chars),
                        &mut matcher,
                        &mut indices,
                    )
                    .is_some()
                {
                    indices.sort_unstable();
                    indices.dedup();
                    char_indices_to_bytes(&item.label, &indices)
                } else {
                    Vec::new()
                };
                Some(Candidate {
                    index,
                    score,
                    positions,
                })
            })
            .collect()
    };
    candidates.sort_by(|a, b| {
        let (a_item, b_item) = (&items[a.index], &items[b.index]);
        exact_prefix(b_item)
            .cmp(&exact_prefix(a_item))
            .then(b.score.cmp(&a.score))
            .then_with(|| sort_key(a_item).cmp(&sort_key(b_item)))
            .then_with(|| a_item.label.len().cmp(&b_item.label.len()))
            .then(Ordering::Equal)
    });
    candidates
}

fn char_indices_to_bytes(text: &str, char_indices: &[u32]) -> Vec<usize> {
    let mut wanted = char_indices.iter().map(|&i| i as usize).peekable();
    text.char_indices()
        .enumerate()
        .filter_map(|(char_index, (byte, _))| wanted.next_if_eq(&char_index).map(|_| byte))
        .collect()
}

/// What accepting an item does to the buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acceptance {
    /// Main edit plus any additional ones (e.g. auto-imports), for
    /// [`Buffer::edit_batch`].
    pub edits: Vec<(Range<usize>, String)>,
    /// Cursor offset after the edits are applied.
    pub cursor: usize,
}

/// Edits for accepting `item` while the cursor is at `cursor` and the typed
/// word started at `word_start`. Server ranges were computed when the request
/// was sent, so the main edit is stretched to the current cursor to also
/// replace what the user typed since.
pub fn accept(
    buffer: &Buffer,
    item: &CompletionItem,
    word_start: usize,
    cursor: usize,
    encoding: Encoding,
) -> Acceptance {
    let (range, text) = match &item.text_edit {
        Some(CompletionTextEdit::Edit(edit)) => (
            to_range(buffer, edit.range, encoding),
            edit.new_text.clone(),
        ),
        Some(CompletionTextEdit::InsertAndReplace(edit)) => (
            to_range(buffer, edit.insert, encoding),
            edit.new_text.clone(),
        ),
        None => (
            word_start..cursor,
            item.insert_text
                .clone()
                .unwrap_or_else(|| item.label.clone()),
        ),
    };
    let main = range.start..range.end.max(cursor);
    let additional = item
        .additional_text_edits
        .as_deref()
        .map(|edits| resolve_edits(buffer, edits, encoding))
        .unwrap_or_default();
    // Additional edits must not touch the main one; drop any that do.
    let additional: Vec<_> = additional
        .into_iter()
        .filter(|(range, _)| range.end <= main.start || range.start >= main.end)
        .collect();
    let shift: isize = additional
        .iter()
        .filter(|(range, _)| range.end <= main.start)
        .map(|(range, text)| text.len() as isize - range.len() as isize)
        .sum();
    let cursor = (main.start + text.len()).saturating_add_signed(shift);
    let mut edits = vec![(main, text)];
    edits.extend(additional);
    Acceptance { edits, cursor }
}

#[cfg(test)]
mod tests {
    use lsp::types::{InsertReplaceEdit, Position, TextEdit};

    use super::*;

    fn item(label: &str) -> CompletionItem {
        CompletionItem {
            label: label.into(),
            ..CompletionItem::default()
        }
    }

    fn range(line: u32, start: u32, end: u32) -> lsp::types::Range {
        lsp::types::Range::new(Position::new(line, start), Position::new(line, end))
    }

    #[test]
    fn word_start_stops_at_non_identifier_characters() {
        let buffer = Buffer::new("x = os.pa_th2");
        assert_eq!(word_start(&buffer, buffer.len()), 7);
        // Right after `.` the word is empty; right after `os` it is `os`.
        assert_eq!(word_start(&buffer, 7), 7);
        assert_eq!(word_start(&buffer, 6), 4);
    }

    #[test]
    fn filtering_is_fuzzy_ranked_and_respects_sort_text() {
        let mut items = vec![
            item("path"),
            item("pathsep"),
            item("getcwd"),
            item("pardir"),
        ];
        items[1].sort_text = Some("0".into()); // server prefers pathsep
        items[0].sort_text = Some("1".into());

        let ranked: Vec<&str> = filter(&items, "pa")
            .iter()
            .map(|c| items[c.index].label.as_str())
            .collect();
        assert!(!ranked.contains(&"getcwd"));
        assert_eq!(
            &ranked[..2],
            ["pathsep", "path"],
            "equal scores fall back to sortText"
        );

        let fuzzy = filter(&items, "gcw");
        assert_eq!(items[fuzzy[0].index].label, "getcwd");
        assert_eq!(fuzzy[0].positions, [0, 3, 4]);
        assert_eq!(filter(&items, "").len(), 4);

        // Seen with basedpyright on `os.pa`: `PathLike` must not beat `path`.
        let os = vec![item("PathLike"), item("pardir"), item("path")];
        let ranked: Vec<&str> = filter(&os, "pa")
            .iter()
            .map(|c| os[c.index].label.as_str())
            .collect();
        assert_eq!(ranked.last(), Some(&"PathLike"));
        let ranked: Vec<&str> = filter(&os, "Pa")
            .iter()
            .map(|c| os[c.index].label.as_str())
            .collect();
        assert_eq!(ranked.first(), Some(&"PathLike"));
    }

    #[test]
    fn filter_text_is_matched_but_label_is_highlighted() {
        let mut items = vec![item("print(…)")];
        items[0].filter_text = Some("print".into());
        let found = filter(&items, "prn");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].positions, [0, 1, 3]);
    }

    #[test]
    fn accepting_without_text_edit_replaces_the_typed_word() {
        let buffer = Buffer::new("os.pa");
        let accepted = accept(&buffer, &item("path"), 3, 5, Encoding::Utf16);
        assert_eq!(accepted.edits, [(3..5, "path".to_owned())]);
        assert_eq!(accepted.cursor, 7);
    }

    #[test]
    fn server_edits_stretch_to_text_typed_after_the_request() {
        // Requested at `os.p|`, accepted at `os.pat|`.
        let buffer = Buffer::new("os.pat");
        let mut path = item("path");
        path.text_edit = Some(CompletionTextEdit::InsertAndReplace(InsertReplaceEdit {
            new_text: "path".into(),
            insert: range(0, 3, 4),
            replace: range(0, 3, 4),
        }));
        let accepted = accept(&buffer, &path, 3, 6, Encoding::Utf16);
        assert_eq!(accepted.edits, [(3..6, "path".to_owned())]);
    }

    #[test]
    fn additional_edits_shift_the_cursor_and_overlaps_are_dropped() {
        let buffer = Buffer::new("\ndef f():\n    Path");
        let mut path = item("Path");
        path.additional_text_edits = Some(vec![
            TextEdit::new(range(0, 0, 0), "from pathlib import Path\n".into()),
            // Overlaps the main edit; must not be applied.
            TextEdit::new(range(2, 4, 6), "xx".into()),
        ]);
        let cursor = buffer.len();
        let accepted = accept(&buffer, &path, cursor - 4, cursor, Encoding::Utf16);
        assert_eq!(accepted.edits.len(), 2);
        let mut buffer = buffer;
        buffer.edit_batch(accepted.edits).unwrap();
        assert_eq!(
            buffer.slice(0..buffer.len()),
            "from pathlib import Path\n\ndef f():\n    Path"
        );
        assert_eq!(accepted.cursor, buffer.len());
    }
}
