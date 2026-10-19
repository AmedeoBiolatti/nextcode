//! Incremental document sync: the `contentChanges` a language server gets
//! for a batch of buffer changes, one event per change instead of the
//! whole document.

use lsp::types::TextDocumentContentChangeEvent;
use text::{Bias, Buffer, Change, Encoding};

use super::lsp_position;

/// `contentChanges` for `changes` (in application order, each with the
/// positions of the text before it, as `text::Change` records them),
/// which the server applies one after the other. Each change's new text
/// is read from the current `buffer`, where the changes after it may
/// have shifted it. `None` when a later change touched an earlier one's
/// new text (typing and its undo drained together, say): only the full
/// text describes that.
pub(super) fn incremental_changes(
    changes: &[Change],
    buffer: &Buffer,
    encoding: Encoding,
) -> Option<Vec<TextDocumentContentChangeEvent>> {
    changes
        .iter()
        .enumerate()
        .map(|(i, change)| {
            let mut new = change.start..change.new_end;
            for later in &changes[i + 1..] {
                if later.start < new.end && new.start < later.old_end {
                    return None;
                }
                let start = later.map_offset(new.start, Bias::Right);
                let end = later.map_offset(new.end, Bias::Left).max(start);
                new = start..end;
            }
            let (start, end) = match encoding {
                Encoding::Utf8 => (change.start_point, change.old_end_point),
                Encoding::Utf16 => (change.start_utf16, change.old_end_utf16),
            };
            Some(TextDocumentContentChangeEvent {
                range: Some(lsp::types::Range::new(
                    lsp_position(start),
                    lsp_position(end),
                )),
                range_length: None,
                text: buffer.slice(new),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use lsp::types::Position;

    use super::*;

    /// Byte offset of `position` in `text` for `encoding`.
    fn offset(text: &str, position: Position, encoding: Encoding) -> usize {
        let line_start: usize = text
            .split_inclusive('\n')
            .take(position.line as usize)
            .map(str::len)
            .sum();
        let line = &text[line_start..];
        let mut units = 0;
        for (i, c) in line.char_indices() {
            if units >= position.character as usize {
                return line_start + i;
            }
            units += match encoding {
                Encoding::Utf8 => c.len_utf8(),
                Encoding::Utf16 => c.len_utf16(),
            };
        }
        line_start + line.len()
    }

    /// `text` with `events` applied in order, as a server does.
    fn apply(text: &str, events: &[TextDocumentContentChangeEvent], encoding: Encoding) -> String {
        let mut text = text.to_owned();
        for event in events {
            let range = event.range.expect("incremental");
            let start = offset(&text, range.start, encoding);
            let end = offset(&text, range.end, encoding);
            text.replace_range(start..end, &event.text);
        }
        text
    }

    /// The events for what `edit` does to `before` (after `setup`, whose
    /// changes are not synced) turn the text into the buffer's.
    fn check(
        before: &str,
        setup: impl FnOnce(&mut Buffer),
        edit: impl FnOnce(&mut Buffer),
        encoding: Encoding,
    ) {
        let mut buffer = Buffer::new(before);
        setup(&mut buffer);
        buffer.drain_changes();
        let start = buffer.slice(0..buffer.len());
        edit(&mut buffer);
        let changes: Vec<Change> = buffer.drain_changes().collect();
        let events = incremental_changes(&changes, &buffer, encoding)
            .unwrap_or_else(|| panic!("incremental sync of {changes:?}"));
        assert_eq!(events.len(), changes.len());
        assert_eq!(
            apply(&start, &events, encoding),
            buffer.slice(0..buffer.len()),
            "{changes:?}"
        );
    }

    #[test]
    fn batches_undo_and_redo_sync_one_event_per_change() {
        let batch = |b: &mut Buffer| {
            b.edit_batch(vec![
                (0..2, "".into()),
                (3..3, "é".into()),
                (8..8, "\nd".into()),
            ])
            .unwrap()
        };
        for encoding in [Encoding::Utf8, Encoding::Utf16] {
            // A batch applies back to front: the later changes shift the
            // earlier ones' new text.
            check(
                "aé\nbc\nd",
                |_| {},
                |b| {
                    b.edit_batch(vec![
                        (0..0, "xé".into()),
                        (1..3, "".into()),
                        (7..7, "y".into()),
                    ])
                    .unwrap()
                },
                encoding,
            );
            // Inserts at one offset keep their order.
            check(
                "x",
                |_| {},
                |b| {
                    b.edit_batch(vec![(0..0, "a".into()), (0..0, "b".into())])
                        .unwrap()
                },
                encoding,
            );
            // Separate edits drained together, front to back and back to
            // front.
            check(
                "one two three",
                |_| {},
                |b| {
                    b.edit(0..3, "1");
                    b.edit(6..11, "3");
                },
                encoding,
            );
            check(
                "one two three",
                |_| {},
                |b| {
                    b.edit(8..13, "3");
                    b.edit(0..3, "1");
                },
                encoding,
            );
            // The undo of a batch, and its redo.
            check(
                "aa\nbb\ncc",
                batch,
                |b| assert!(b.undo().is_some()),
                encoding,
            );
            check(
                "aa\nbb\ncc",
                |b| {
                    batch(b);
                    b.undo();
                },
                |b| assert!(b.redo().is_some()),
                encoding,
            );
        }
    }

    #[test]
    fn changes_touching_earlier_new_text_need_the_full_document() {
        let mut buffer = Buffer::new("ab");
        buffer.edit(1..1, "x");
        buffer.undo();
        let changes: Vec<Change> = buffer.drain_changes().collect();
        assert_eq!(changes.len(), 2);
        assert!(incremental_changes(&changes, &buffer, Encoding::Utf8).is_none());
        // Typing after inserted text is fine: it only touches its edge.
        let mut buffer = Buffer::new("ab");
        buffer.edit(1..1, "x");
        buffer.edit(2..2, "y");
        let changes: Vec<Change> = buffer.drain_changes().collect();
        let events = incremental_changes(&changes, &buffer, Encoding::Utf8).unwrap();
        assert_eq!(events[0].text, "x");
        assert_eq!(events[1].text, "y");
    }
}
