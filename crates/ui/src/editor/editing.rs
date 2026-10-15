//! Edits at every selection: the one edit primitive (with undo history of
//! selections), typing with auto-closing pairs, Enter with auto-indent,
//! deletion, clipboard.

use super::*;

/// Replacement for one selection and where that selection lands, as
/// offsets from the start of the inserted text (may point past it, into
/// unchanged text before the next edit).
pub(super) struct SelectionEdit {
    pub range: Range<usize>,
    pub text: String,
    pub anchor: usize,
    pub head: usize,
}

impl SelectionEdit {
    /// `text` over `range`, cursor after it.
    pub fn insert(range: Range<usize>, text: String) -> Self {
        let end = text.len();
        Self {
            range,
            text,
            anchor: end,
            head: end,
        }
    }
}

/// Selections before and after one undo step of the buffer.
#[derive(Clone)]
pub(super) struct HistoryEntry {
    transaction: u64,
    before: SelectionSet,
    after: SelectionSet,
}

/// Undo steps whose selections are remembered.
const HISTORY_LIMIT: usize = 1000;

/// Opening characters that auto-close in `language`, with their closers.
fn auto_close_pairs(language: Option<Language>) -> &'static [(char, char)] {
    const BRACKETS: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}')];
    const QUOTES: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}'), ('"', '"'), ('\'', '\'')];
    const WITH_BACKTICK: &[(char, char)] = &[
        ('(', ')'),
        ('[', ']'),
        ('{', '}'),
        ('"', '"'),
        ('\'', '\''),
        ('`', '`'),
    ];
    // Rust uses `'` for lifetimes; Markdown prose has apostrophes.
    const DOUBLE_ONLY: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}'), ('"', '"')];
    const MARKDOWN: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}'), ('`', '`')];
    match language {
        None => BRACKETS,
        Some(Language::Rust) => DOUBLE_ONLY,
        Some(Language::Markdown) => MARKDOWN,
        Some(
            Language::JavaScript
            | Language::TypeScript
            | Language::Tsx
            | Language::Go
            | Language::Bash
            | Language::Sql,
        ) => WITH_BACKTICK,
        Some(Language::Json) => DOUBLE_ONLY,
        Some(_) => QUOTES,
    }
}

/// Whether an opener auto-closes before `next` (VS Code's `autoCloseBefore`).
fn closes_before(next: Option<char>) -> bool {
    next.is_none_or(|c| c.is_whitespace() || ";:.,=}])>".contains(c))
}

/// The closing bracket for an opening one.
pub(super) fn closer_of(open: char) -> Option<char> {
    match open {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        _ => None,
    }
}

impl Editor {
    /// Applies `edits` (sorted, non-overlapping) as one undo step and sets the
    /// selections computed from the edited buffer.
    pub(super) fn commit(
        &mut self,
        edits: Vec<(Range<usize>, String)>,
        after: impl FnOnce(&Buffer) -> SelectionSet,
        cx: &mut Context<Self>,
    ) {
        let edits: Vec<_> = edits
            .into_iter()
            .filter(|(range, text)| self.buffer.slice(range.clone()) != *text)
            .collect();
        let before = self.selections.clone();
        let was_dirty = self.buffer.is_dirty();
        let transaction = self.buffer.transaction_id();
        let edited = !edits.is_empty();
        if edited && self.buffer.edit_batch(edits).is_err() {
            return;
        }
        self.selections = after(&self.buffer).clamped(&self.buffer);
        if edited {
            self.record_history(transaction, before);
        }
        self.marked = None;
        self.autoscroll = true;
        self.finish_change(was_dirty, cx);
    }

    /// Remembers the selections around the buffer's latest undo step, if
    /// one was added (or grown by coalesced typing) since `transaction`.
    pub(super) fn record_history(&mut self, transaction: Option<u64>, before: SelectionSet) {
        let Some(id) = self.buffer.transaction_id() else {
            return;
        };
        let after = self.selections.clone();
        if Some(id) != transaction {
            if self.history.len() >= HISTORY_LIMIT {
                self.history.remove(0);
            }
            self.history.push(HistoryEntry {
                transaction: id,
                before,
                after,
            });
        } else if let Some(last) = self.history.last_mut()
            && last.transaction == id
        {
            last.after = after;
        }
    }

    /// Applies one edit per selection; ranges may overlap when deleting
    /// (e.g. two cursors in one word), in which case they merge.
    pub(super) fn edit_selections(&mut self, edits: Vec<SelectionEdit>, cx: &mut Context<Self>) {
        let primary = self.selections.primary_index();
        let mut tagged: Vec<(SelectionEdit, bool)> = edits
            .into_iter()
            .enumerate()
            .map(|(i, edit)| (edit, i == primary))
            .collect();
        tagged.sort_by_key(|(edit, _)| edit.range.start);
        let mut merged: Vec<(SelectionEdit, bool)> = Vec::with_capacity(tagged.len());
        for (edit, is_primary) in tagged {
            if let Some((last, last_primary)) = merged.last_mut()
                && edit.range.start < last.range.end
            {
                if edit.text.is_empty() && last.text.is_empty() {
                    last.range.end = last.range.end.max(edit.range.end);
                    *last_primary |= is_primary;
                }
                continue;
            }
            merged.push((edit, is_primary));
        }
        let mut shift = 0isize;
        let mut items = Vec::with_capacity(merged.len());
        let mut primary = 0;
        for (edit, is_primary) in &merged {
            let start = edit.range.start.saturating_add_signed(shift);
            if *is_primary {
                primary = items.len();
            }
            items.push(Selection::new(start + edit.anchor, start + edit.head));
            shift += edit.text.len() as isize - edit.range.len() as isize;
        }
        let edits = merged
            .into_iter()
            .map(|(edit, _)| (edit.range, edit.text))
            .collect();
        self.commit(edits, |_| SelectionSet::new(items, primary), cx);
    }

    /// Replaces every selection with `text` (typing without helpers).
    pub(super) fn insert_at_selections(&mut self, text: &str, cx: &mut Context<Self>) {
        let edits = self
            .selections
            .all()
            .iter()
            .map(|s| SelectionEdit::insert(s.range(), text.to_owned()))
            .collect();
        self.edit_selections(edits, cx);
    }

    /// IME input over `range` at the primary cursor; other cursors get the
    /// same replacement around their own heads. Returns where the text
    /// landed at the primary cursor.
    pub(super) fn replace_composition(
        &mut self,
        range: Range<usize>,
        text: &str,
        cx: &mut Context<Self>,
    ) -> Range<usize> {
        let primary = self.primary();
        let back = primary.head.saturating_sub(range.start);
        let ahead = range.end.saturating_sub(primary.head);
        let primary_index = self.selections.primary_index();
        let clamp = |offset: usize| {
            let offset = offset.min(self.buffer.len());
            self.buffer
                .offset_of(self.buffer.point(offset), Encoding::Utf8)
        };
        let edits = self
            .selections
            .all()
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let range = if i == primary_index {
                    range.clone()
                } else if !s.is_empty() && self.marked.is_none() {
                    s.range()
                } else {
                    clamp(s.head.saturating_sub(back))..clamp(s.head + ahead)
                };
                SelectionEdit::insert(range, text.to_owned())
            })
            .collect();
        self.edit_selections(edits, cx);
        let head = self.primary().head;
        head.saturating_sub(text.len())..head
    }

    /// Deletes each selection, or `fallback` from its cursor when empty.
    pub(super) fn delete_selections_or(
        &mut self,
        fallback: impl Fn(&Self, usize) -> Range<usize>,
        cx: &mut Context<Self>,
    ) {
        let edits = self
            .selections
            .all()
            .iter()
            .map(|s| {
                let range = if s.is_empty() {
                    fallback(self, s.head)
                } else {
                    s.range()
                };
                SelectionEdit::insert(range, String::new())
            })
            .collect();
        self.edit_selections(edits, cx);
    }

    /// Backspace: also removes the closer of an empty pair (`(|)`).
    pub(super) fn backspace_range(&self, head: usize) -> Range<usize> {
        let start = self.buffer.prev_grapheme(head);
        if theme::auto_close_brackets()
            && !self.single_line
            && let Some(before) = self.buffer.slice(start..head).chars().next()
            && start + before.len_utf8() == head
            && let Some(after) = self.char_at(head)
            && auto_close_pairs(self.language())
                .iter()
                .any(|&(open, close)| open == before && close == after)
        {
            return start..head + after.len_utf8();
        }
        start..head
    }

    pub(super) fn char_at(&self, offset: usize) -> Option<char> {
        self.buffer.chunk_at(offset).chars().next()
    }

    pub(super) fn char_before(&self, offset: usize) -> Option<char> {
        let start = self.buffer.prev_grapheme(offset);
        self.buffer.slice(start..offset).chars().next_back()
    }

    /// Typed text at every selection, with auto-closing pairs: an opener
    /// inserts its closer (or wraps a selection), a closer steps over one
    /// that was auto-inserted.
    pub(super) fn type_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let mut chars = text.chars();
        let typed = match (chars.next(), chars.next()) {
            (Some(c), None) if theme::auto_close_brackets() && !self.single_line => c,
            _ => return self.insert_at_selections(text, cx),
        };
        let pairs = auto_close_pairs(self.language());
        let opener = pairs.iter().find(|(open, _)| *open == typed).copied();
        let is_closer = pairs.iter().any(|(_, close)| *close == typed);
        // Per selection: whether its edit auto-inserted a closer.
        let mut closed = Vec::with_capacity(self.selections.len());
        let mut handled = false;
        let edits: Vec<SelectionEdit> = self
            .selections
            .all()
            .iter()
            .map(|s| {
                let range = s.range();
                let next = self.char_at(s.head);
                closed.push(false);
                if !s.is_empty()
                    && let Some((open, close)) = opener
                {
                    // Wrap the selection, keeping it on the wrapped text.
                    handled = true;
                    let inner = self.buffer.slice(range.clone());
                    let len = inner.len();
                    let (anchor, head) = if s.reversed() {
                        (1 + len, 1)
                    } else {
                        (1, 1 + len)
                    };
                    return SelectionEdit {
                        range,
                        text: format!("{open}{inner}{close}"),
                        anchor,
                        head,
                    };
                }
                if s.is_empty()
                    && is_closer
                    && next == Some(typed)
                    && self.auto_closed.contains(&s.head)
                {
                    // Step over the closer typed for us.
                    handled = true;
                    return SelectionEdit {
                        range: s.head..s.head,
                        text: String::new(),
                        anchor: typed.len_utf8(),
                        head: typed.len_utf8(),
                    };
                }
                if s.is_empty()
                    && let Some((open, close)) = opener
                    && closes_before(next)
                {
                    let before = self.char_before(s.head);
                    let quote = open == close;
                    let blocked =
                        quote && before.is_some_and(|c| is_word_char(c) || c == '\\' || c == open);
                    if !blocked {
                        handled = true;
                        *closed.last_mut().expect("pushed above") = true;
                        return SelectionEdit {
                            range,
                            text: format!("{open}{close}"),
                            anchor: open.len_utf8(),
                            head: open.len_utf8(),
                        };
                    }
                }
                SelectionEdit::insert(range, text.to_owned())
            })
            .collect();
        if !handled {
            return self.insert_at_selections(text, cx);
        }
        self.edit_selections(edits, cx);
        // Each new closer sits at its cursor; `finish_change` already mapped
        // the pending ones through this edit.
        if self.selections.len() == closed.len() {
            let heads: Vec<usize> = self.selections.all().iter().map(|s| s.head).collect();
            for (head, closed) in heads.into_iter().zip(closed) {
                if closed && !self.auto_closed.contains(&head) {
                    self.auto_closed.push(head);
                }
            }
        }
    }

    /// The indentation for a new line split at `offset`: the line's indent,
    /// one level deeper after an opener (or Python's `:`), and whether the
    /// closer right after `offset` goes on its own line.
    pub(super) fn newline_indent(&self, offset: usize) -> (String, Option<String>) {
        let line = self.buffer.line_of(offset);
        let line_start = self.buffer.line_start(line);
        let text = self.buffer.line_text(line);
        let indent = &text[..text.len() - text.trim_start().len()];
        let indent = &indent[..indent.len().min(offset - line_start)];
        let before = self.buffer.slice(line_start..offset);
        let last = before.trim_end().chars().next_back();
        let opens = match last {
            Some(c) if closer_of(c).is_some() => true,
            Some(':') => self.language() == Some(Language::Python),
            _ => false,
        };
        if !opens {
            return (indent.to_owned(), None);
        }
        let deeper = format!("{indent}{}", self.indent.unit());
        let next = self.char_at(offset);
        let closes = last
            .and_then(closer_of)
            .is_some_and(|close| next == Some(close));
        (deeper, closes.then(|| indent.to_owned()))
    }

    /// Enter: a line break at every selection, auto-indented.
    pub(super) fn newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        let ending = self.buffer.line_ending();
        let edits = self
            .selections
            .all()
            .iter()
            .map(|s| {
                let (indent, closer_line) = self.newline_indent(s.start());
                let first = format!("{ending}{indent}");
                let cursor = first.len();
                let text = match closer_line {
                    Some(outer) => format!("{first}{ending}{outer}"),
                    None => first,
                };
                SelectionEdit {
                    range: s.range(),
                    text,
                    anchor: cursor,
                    head: cursor,
                }
            })
            .collect();
        self.edit_selections(edits, cx);
    }

    pub(super) fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// The non-empty selections' text, one per line.
    fn selected_text(&self) -> Option<String> {
        let parts: Vec<String> = self
            .selections
            .all()
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| self.buffer.slice(s.range()))
            .collect();
        (!parts.is_empty()).then(|| parts.join(self.buffer.line_ending()))
    }

    pub(super) fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            let edits = self
                .selections
                .all()
                .iter()
                .map(|s| SelectionEdit::insert(s.range(), String::new()))
                .collect();
            self.edit_selections(edits, cx);
        }
    }

    /// Pastes at every selection; N lines into N cursors go one per cursor.
    pub(super) fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.paste_text(&text, cx);
        }
    }

    pub(super) fn paste_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.single_line {
            return self.insert_at_selections(&text.replace(['\r', '\n'], " "), cx);
        }
        let text = self.normalize_line_endings(text);
        let count = self.selections.len();
        let lines: Vec<&str> = text.lines().collect();
        if count > 1 && lines.len() == count {
            let edits = self
                .selections
                .all()
                .iter()
                .zip(lines)
                .map(|(s, line)| SelectionEdit::insert(s.range(), line.to_owned()))
                .collect();
            return self.edit_selections(edits, cx);
        }
        self.insert_at_selections(&text, cx);
    }

    pub(super) fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        let was_dirty = self.buffer.is_dirty();
        let transaction = self.buffer.transaction_id();
        if let Some(range) = self.buffer.undo() {
            let entry = self.history_entry(transaction);
            self.selections = match entry {
                Some(entry) => entry.before.clamped(&self.buffer),
                None => SelectionSet::single(Selection::cursor(range.end)),
            };
            self.marked = None;
            self.autoscroll = true;
            self.finish_change(was_dirty, cx);
        }
    }

    pub(super) fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        let was_dirty = self.buffer.is_dirty();
        if let Some(range) = self.buffer.redo() {
            let entry = self.history_entry(self.buffer.transaction_id());
            self.selections = match entry {
                Some(entry) => entry.after.clamped(&self.buffer),
                None => SelectionSet::single(Selection::cursor(range.end)),
            };
            self.marked = None;
            self.autoscroll = true;
            self.finish_change(was_dirty, cx);
        }
    }

    fn history_entry(&self, transaction: Option<u64>) -> Option<HistoryEntry> {
        let id = transaction?;
        self.history
            .iter()
            .rev()
            .find(|entry| entry.transaction == id)
            .cloned()
    }
}
