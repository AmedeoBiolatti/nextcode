//! Multiple selections: the sorted, merged selection set, multi-cursor
//! commands (next/all occurrences, cursors above/below, column selection)
//! and the saved editor state.

use serde::{Deserialize, Serialize};

use super::*;

actions!(
    editor,
    [
        SelectNextOccurrence,
        SelectAllOccurrences,
        AddCursorAbove,
        AddCursorBelow,
        CollapseSelections,
    ]
);

/// One selection: `anchor` stays put while extending, `head` is the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Selection {
    pub anchor: usize,
    pub head: usize,
    /// Column (in chars) that vertical motion tries to keep.
    pub goal: Option<usize>,
    /// A head at a wrap position is drawn at the end of the upper row
    /// (after End, or a click past a row's end) instead of at the start of
    /// the next one.
    pub at_row_end: bool,
}

impl Selection {
    pub fn cursor(offset: usize) -> Self {
        Self::new(offset, offset)
    }

    pub fn new(anchor: usize, head: usize) -> Self {
        Self {
            anchor,
            head,
            goal: None,
            at_row_end: false,
        }
    }

    pub fn start(&self) -> usize {
        self.anchor.min(self.head)
    }

    pub fn end(&self) -> usize {
        self.anchor.max(self.head)
    }

    pub fn range(&self) -> Range<usize> {
        self.start()..self.end()
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    pub fn reversed(&self) -> bool {
        self.head < self.anchor
    }

    /// The same direction over `range`.
    fn with_range(self, range: Range<usize>) -> Self {
        let (anchor, head) = if self.reversed() {
            (range.end, range.start)
        } else {
            (range.start, range.end)
        };
        Self {
            anchor,
            head,
            goal: self.goal,
            at_row_end: self.at_row_end && head == self.head,
        }
    }
}

/// Selections sorted by start, never overlapping. Ranges that overlap, or
/// touch where one of them is a bare cursor, merge; touching non-empty
/// selections (adjacent occurrences) stay apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SelectionSet {
    items: Vec<Selection>,
    /// The newest selection: the one commands with a single target use
    /// (completions, go to definition, scrolling) and Escape keeps.
    primary: usize,
}

impl SelectionSet {
    pub fn single(selection: Selection) -> Self {
        Self {
            items: vec![selection],
            primary: 0,
        }
    }

    /// Sorts and merges `items`; `primary` indexes into `items`.
    pub fn new(items: Vec<Selection>, primary: usize) -> Self {
        assert!(!items.is_empty(), "at least one selection");
        let mut tagged: Vec<(Selection, bool)> = items
            .into_iter()
            .enumerate()
            .map(|(i, selection)| (selection, i == primary))
            .collect();
        tagged.sort_by_key(|(selection, _)| (selection.start(), selection.end()));
        let mut merged: Vec<(Selection, bool)> = Vec::with_capacity(tagged.len());
        for (selection, is_primary) in tagged {
            if let Some((last, last_primary)) = merged.last_mut() {
                let overlaps = selection.start() < last.end()
                    || (selection.start() == last.end()
                        && (selection.is_empty() || last.is_empty()));
                if overlaps {
                    let range = last.start()..last.end().max(selection.end());
                    let direction = if is_primary { selection } else { *last };
                    *last = direction.with_range(range);
                    *last_primary |= is_primary;
                    continue;
                }
            }
            merged.push((selection, is_primary));
        }
        let primary = merged.iter().position(|(_, p)| *p).unwrap_or(0);
        Self {
            items: merged.into_iter().map(|(selection, _)| selection).collect(),
            primary,
        }
    }

    pub fn all(&self) -> &[Selection] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn primary(&self) -> Selection {
        self.items[self.primary]
    }

    pub fn primary_index(&self) -> usize {
        self.primary
    }

    /// Each selection replaced by `f`'s, then re-merged.
    pub fn map(&self, f: impl FnMut(&Selection) -> Selection) -> Self {
        Self::new(self.items.iter().map(f).collect(), self.primary)
    }

    /// `selection` added as the new primary.
    pub fn with(&self, selection: Selection) -> Self {
        let mut items = self.items.clone();
        items.push(selection);
        let primary = items.len() - 1;
        Self::new(items, primary)
    }

    /// The primary selection replaced (e.g. while dragging it).
    pub fn with_primary(&self, selection: Selection) -> Self {
        let mut items = self.items.clone();
        items[self.primary] = selection;
        Self::new(items, self.primary)
    }

    /// Without the selection at `index`; needs at least two.
    pub fn without(&self, index: usize) -> Self {
        let mut items = self.items.clone();
        items.remove(index);
        let primary = match self.primary {
            primary if primary == index => items.len() - 1,
            primary if primary > index => primary - 1,
            primary => primary,
        };
        Self { items, primary }
    }

    /// Clamped to `buffer`, on character boundaries (e.g. after edits elsewhere).
    pub fn clamped(&self, buffer: &Buffer) -> Self {
        let clamp = |offset: usize| {
            let offset = offset.min(buffer.len());
            buffer.offset_of(buffer.point(offset), Encoding::Utf8)
        };
        self.map(|s| Selection {
            anchor: clamp(s.anchor),
            head: clamp(s.head),
            ..*s
        })
    }
}

/// What a tab restores: selections, scroll position and folds. Columns are
/// in chars; positions past the end are clamped on restore.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorState {
    pub selections: Vec<SelectionState>,
    /// Index of the primary selection in `selections`.
    #[serde(default)]
    pub primary: usize,
    /// First visible line.
    #[serde(default)]
    pub scroll_line: usize,
    /// Folded regions as `(header line, last hidden line)`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folds: Vec<(usize, usize)>,
}

/// A selection as 0-based `(line, column)` pairs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionState {
    pub anchor: (usize, usize),
    pub head: (usize, usize),
}

/// Whether `c` belongs to an identifier.
pub(super) fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Non-overlapping occurrences of `needle` in `haystack`; `whole_word`
/// skips those touching word characters.
pub(super) fn occurrences(haystack: &str, needle: &str, whole_word: bool) -> Vec<Range<usize>> {
    if needle.is_empty() {
        return Vec::new();
    }
    haystack
        .match_indices(needle)
        .map(|(start, _)| start..start + needle.len())
        .filter(|range| !whole_word || is_whole_word(haystack, range))
        .collect()
}

/// Whether `range` of `text` has no word character just before or after it.
pub(super) fn is_whole_word(text: &str, range: &Range<usize>) -> bool {
    !text[..range.start]
        .chars()
        .next_back()
        .is_some_and(is_word_char)
        && !text[range.end..].chars().next().is_some_and(is_word_char)
}

impl Editor {
    pub(super) fn primary(&self) -> Selection {
        self.selections.primary()
    }

    /// Replaces the selections without editing; closes what depended on the
    /// old cursor position and opens folds a cursor moved into.
    pub(super) fn set_selections(&mut self, selections: SelectionSet, cx: &mut Context<Self>) {
        self.selections = selections;
        self.reveal_selections();
        self.autoscroll = true;
        self.auto_closed.clear();
        self.update_completion_menu(cx);
        cx.notify();
    }

    pub fn state(&self) -> EditorState {
        let position = |offset: usize| {
            let line = self.buffer.line_of(offset);
            (line, self.buffer.column(offset))
        };
        let line_height = self
            .layout
            .as_ref()
            .map_or(px(theme::EDITOR_LINE_HEIGHT), |layout| layout.line_height);
        EditorState {
            selections: self
                .selections
                .all()
                .iter()
                .map(|s| SelectionState {
                    anchor: position(s.anchor),
                    head: position(s.head),
                })
                .collect(),
            primary: self.selections.primary_index(),
            scroll_line: self.pending_scroll_line.unwrap_or_else(|| {
                let row = (self.scroll.y / line_height).round() as usize;
                self.display.row_line(row)
            }),
            folds: self.folded_lines(),
        }
    }

    pub fn restore_state(&mut self, state: &EditorState, cx: &mut Context<Self>) {
        let offset = |(line, column): (usize, usize)| self.buffer.offset_at(line, column);
        let items: Vec<Selection> = state
            .selections
            .iter()
            .map(|s| Selection::new(offset(s.anchor), offset(s.head)))
            .collect();
        if !items.is_empty() {
            let primary = state.primary.min(items.len() - 1);
            self.selections = SelectionSet::new(items, primary);
        }
        self.restore_folds(&state.folds);
        self.reveal_selections();
        self.pending_scroll_line = Some(state.scroll_line.min(self.buffer.line_count() - 1));
        self.autoscroll = false;
        self.center_cursor = false;
        self.completion = None;
        cx.notify();
    }

    /// The word around `offset` (touching it on either side), if any.
    pub(super) fn word_around(&self, offset: usize) -> Option<Range<usize>> {
        let line = self.buffer.line_of(offset);
        let line_start = self.buffer.line_start(line);
        let text = self.buffer.line_text(line);
        let at = offset - line_start;
        let start = text[..at]
            .char_indices()
            .rev()
            .take_while(|(_, c)| is_word_char(*c))
            .last()
            .map_or(at, |(i, _)| i);
        let end = at
            + text[at..]
                .chars()
                .take_while(|c| is_word_char(*c))
                .map(char::len_utf8)
                .sum::<usize>();
        (start < end).then(|| line_start + start..line_start + end)
    }

    /// Expands every empty selection to its word. Returns `false` when the
    /// primary cursor touches no word.
    fn expand_cursors_to_words(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(primary_word) = self.word_around(self.primary().head) else {
            return false;
        };
        let selections = self.selections.map(|s| {
            if s.is_empty()
                && let Some(word) = self.word_around(s.head)
            {
                Selection::new(word.start, word.end)
            } else {
                *s
            }
        });
        self.occurrence_word = Some(self.buffer.slice(primary_word));
        self.set_selections(selections, cx);
        true
    }

    /// The primary selection's text and whether occurrences must be whole
    /// words (the search started from a cursor on a word, like VS Code).
    fn occurrence_query(&self) -> Option<(String, bool)> {
        let range = self.primary().range();
        if range.is_empty() {
            return None;
        }
        let text = self.buffer.slice(range);
        let whole_word = self.occurrence_word.as_deref() == Some(text.as_str());
        Some((text, whole_word))
    }

    /// Ctrl+D: selects the word at the cursor, then adds the next occurrence
    /// of the primary selection (wrapping around).
    pub(super) fn select_next_occurrence(
        &mut self,
        _: &SelectNextOccurrence,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.primary().is_empty() {
            self.expand_cursors_to_words(cx);
            return;
        }
        let Some((needle, whole_word)) = self.occurrence_query() else {
            return;
        };
        let text = self.text();
        let found = occurrences(&text, &needle, whole_word);
        let after = self.primary().end();
        let taken = |range: &Range<usize>| {
            self.selections
                .all()
                .iter()
                .any(|s| s.start() < range.end && range.start < s.end())
        };
        let next = found
            .iter()
            .filter(|range| range.start >= after)
            .chain(found.iter().filter(|range| range.start < after))
            .find(|range| !taken(range));
        if let Some(range) = next {
            let selections = self.selections.with(Selection::new(range.start, range.end));
            self.set_selections(selections, cx);
        }
    }

    /// Ctrl+Shift+L: a selection on every occurrence of the primary
    /// selection (or of the word at the cursor).
    pub(super) fn select_all_occurrences(
        &mut self,
        _: &SelectAllOccurrences,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.primary().is_empty() && !self.expand_cursors_to_words(cx) {
            return;
        }
        let Some((needle, whole_word)) = self.occurrence_query() else {
            return;
        };
        let primary_start = self.primary().start();
        let found = occurrences(&self.text(), &needle, whole_word);
        if found.is_empty() {
            return;
        }
        let primary = found
            .iter()
            .position(|range| range.start == primary_start)
            .unwrap_or(0);
        let items = found
            .into_iter()
            .map(|range| Selection::new(range.start, range.end))
            .collect();
        self.set_selections(SelectionSet::new(items, primary), cx);
    }

    fn add_cursor_vertically(&mut self, lines: isize, cx: &mut Context<Self>) {
        let primary = self.primary();
        let goal = primary
            .goal
            .unwrap_or_else(|| self.buffer.column(primary.head));
        let line = self.buffer.line_of(primary.head);
        // Lines hidden in folds are skipped.
        let Some(line) = self.display.shown_line(line, lines > 0) else {
            return;
        };
        let offset = self.buffer.offset_at(line, goal);
        let selections = self.selections.with(Selection {
            goal: Some(goal),
            ..Selection::cursor(offset)
        });
        self.set_selections(selections, cx);
    }

    pub(super) fn add_cursor_above(
        &mut self,
        _: &AddCursorAbove,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.add_cursor_vertically(-1, cx);
    }

    pub(super) fn add_cursor_below(
        &mut self,
        _: &AddCursorBelow,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.add_cursor_vertically(1, cx);
    }

    /// Escape: back to the primary selection alone.
    pub(super) fn collapse_selections(
        &mut self,
        _: &CollapseSelections,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_selections(SelectionSet::single(self.primary()), cx);
    }

    /// A selection per line from `anchor` to `head` (line, char column):
    /// Shift+Alt+drag. Lines shorter than both columns are skipped.
    pub(super) fn column_selection(
        &self,
        anchor: (usize, usize),
        head: (usize, usize),
    ) -> SelectionSet {
        let (top, bottom) = (anchor.0.min(head.0), anchor.0.max(head.0));
        let min_column = anchor.1.min(head.1);
        let line_chars = |line: usize| {
            let end = self.buffer.line_end(line);
            self.buffer.column(end)
        };
        let mut items = Vec::new();
        let mut primary = 0;
        for line in top..=bottom {
            if line != head.0 && line != anchor.0 && line_chars(line) < min_column
                || self.display.is_hidden(line)
            {
                continue;
            }
            if line == head.0 {
                primary = items.len();
            }
            items.push(Selection {
                goal: Some(head.1),
                ..Selection::new(
                    self.buffer.offset_at(line, anchor.1),
                    self.buffer.offset_at(line, head.1),
                )
            });
        }
        SelectionSet::new(items, primary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranges(set: &SelectionSet) -> Vec<Range<usize>> {
        set.all().iter().map(Selection::range).collect()
    }

    #[test]
    fn overlapping_and_touching_cursors_merge_but_adjacent_selections_do_not() {
        let set = SelectionSet::new(
            vec![
                Selection::new(8, 12),
                Selection::new(0, 4),
                Selection::new(3, 6),
                Selection::cursor(12),
                Selection::new(13, 14),
                Selection::new(20, 22),
                Selection::new(22, 24),
            ],
            3,
        );
        assert_eq!(ranges(&set), [0..6, 8..12, 13..14, 20..22, 22..24]);
        assert_eq!(
            set.primary_index(),
            1,
            "the primary cursor merged into 8..12"
        );
    }

    #[test]
    fn merging_keeps_the_primary_direction() {
        let set = SelectionSet::new(vec![Selection::new(0, 5), Selection::new(9, 3)], 1);
        assert_eq!(set.len(), 1);
        assert_eq!(set.primary(), Selection::new(9, 0));
    }

    #[test]
    fn occurrences_respect_whole_words() {
        let text = "foo food foo_bar (foo)";
        assert_eq!(occurrences(text, "foo", false).len(), 4);
        assert_eq!(occurrences(text, "foo", true), [0..3, 18..21]);
        assert!(occurrences(text, "", false).is_empty());
    }
}
