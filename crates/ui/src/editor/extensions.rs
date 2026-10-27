//! The extension boundary stays in UTF-8 bytes and targets the captured editor,
//! never whichever tab happens to have focus when a background command finishes.

use extension_api::{Document, Selection as ExtensionSelection, TextEdit};

use super::*;

impl Editor {
    pub(crate) fn extension_document(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<Document, String> {
        self.sync_buffer(cx);
        if self.buffer.len() > 8 * 1024 * 1024 {
            return Err("Extensions support documents up to 8 MiB".into());
        }
        let selection = self.primary();
        Ok(Document {
            version: self.version,
            path: self
                .buffer
                .path()
                .map(|path| path.to_string_lossy().into_owned()),
            text: self.text(),
            selection: ExtensionSelection {
                start: selection.start() as u32,
                end: selection.end() as u32,
            },
        })
    }

    pub(crate) fn apply_extension_edits(
        &mut self,
        version: u64,
        expected_path: Option<&str>,
        edits: Vec<TextEdit>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.sync_buffer(cx);
        let path = self
            .buffer
            .path()
            .map(|path| path.to_string_lossy().into_owned());
        if self.version != version || path.as_deref() != expected_path {
            return Err(
                "Document changed while the extension was running; run the command again".into(),
            );
        }
        // The runtime has already validated boundaries and overlap against the
        // snapshot. Version equality makes those coordinates valid here too.
        let mut edits: Vec<_> = edits
            .into_iter()
            .map(|edit| (edit.start as usize..edit.end as usize, edit.text))
            .collect();
        edits.sort_by_key(|(range, _)| (range.start, range.end));
        self.buffer.seal_undo();
        self.apply_edits_keeping_selections(edits, cx);
        self.buffer.seal_undo();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::tests::{editor, type_text, update};
    use gpui::TestAppContext;

    #[gpui::test]
    fn extension_batch_is_one_undo_step_separate_from_surrounding_typing(cx: &mut TestAppContext) {
        let window = editor("extension.txt", "a|", cx);
        type_text(window, "b", cx);
        update(window, cx, |editor, _, cx| {
            let document = editor.extension_document(cx).unwrap();
            editor
                .apply_extension_edits(
                    document.version,
                    document.path.as_deref(),
                    vec![
                        TextEdit {
                            start: 0,
                            end: 1,
                            text: "A".into(),
                        },
                        TextEdit {
                            start: 1,
                            end: 2,
                            text: "B".into(),
                        },
                    ],
                    cx,
                )
                .unwrap();
            assert_eq!(editor.text(), "AB");
        });
        type_text(window, "c", cx);
        update(window, cx, |editor, window, cx| {
            editor.undo(&Undo, window, cx);
            assert_eq!(editor.text(), "AB");
            editor.undo(&Undo, window, cx);
            assert_eq!(editor.text(), "ab");
            editor.undo(&Undo, window, cx);
            assert_eq!(editor.text(), "a");
            editor.redo(&Redo, window, cx);
            editor.redo(&Redo, window, cx);
            assert_eq!(editor.text(), "AB");
        });
    }

    #[gpui::test]
    fn extension_rejects_stale_results_after_linked_edits_and_rename(cx: &mut TestAppContext) {
        let window = editor("extension.txt", "«é»", cx);
        update(window, cx, |editor, _, cx| {
            let document = editor.extension_document(cx).unwrap();
            assert_eq!((document.selection.start, document.selection.end), (0, 2));
            let mut linked = editor.buffer.link();
            linked.edit(0..2, "x");
            let edit = || {
                vec![TextEdit {
                    start: 0,
                    end: 2,
                    text: "É".into(),
                }]
            };
            assert!(
                editor
                    .apply_extension_edits(document.version, document.path.as_deref(), edit(), cx)
                    .is_err()
            );
            assert_eq!(editor.text(), "x");
            let document = editor.extension_document(cx).unwrap();
            editor.set_file_path("renamed.txt".into(), cx);
            assert!(
                editor
                    .apply_extension_edits(
                        document.version,
                        document.path.as_deref(),
                        vec![TextEdit {
                            start: 0,
                            end: 1,
                            text: "Y".into()
                        }],
                        cx
                    )
                    .is_err()
            );
            assert_eq!(editor.text(), "x");
        });
    }
}
