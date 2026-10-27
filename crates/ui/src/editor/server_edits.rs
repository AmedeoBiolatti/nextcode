//! Edits computed elsewhere (a server's workspace edit, a formatter's
//! output) applied to this buffer as one undo step, with the selections
//! moved through them; and following a file renamed on disk.

use std::path::PathBuf;

use lsp::types::TextEdit;

use super::*;
use crate::workspace_edit;

impl Editor {
    /// Applies sorted, non-overlapping `edits` as one undo step; selections
    /// move with the text around them (a cursor in a replaced range goes
    /// after the new text, a selection grows over edits at its ends).
    pub(super) fn apply_edits_keeping_selections(
        &mut self,
        edits: Vec<(Range<usize>, String)>,
        cx: &mut Context<Self>,
    ) {
        let edits: Vec<(Range<usize>, String)> = edits
            .into_iter()
            .filter(|(range, text)| self.buffer.slice(range.clone()) != *text)
            .collect();
        if edits.is_empty() {
            return;
        }
        let map = |offset: usize, bias: Bias| lines::map_through(&edits, offset, bias);
        let selections = self.selections.map(|s| {
            let (anchor_bias, head_bias) = match (s.is_empty(), s.reversed()) {
                (true, _) => (Bias::Right, Bias::Right),
                (false, false) => (Bias::Left, Bias::Right),
                (false, true) => (Bias::Right, Bias::Left),
            };
            Selection::new(map(s.anchor, anchor_bias), map(s.head, head_bias))
        });
        let primary = selections.primary_index();
        let items = selections.all().to_vec();
        self.commit(edits, |_| SelectionSet::new(items, primary), cx);
    }

    /// Applies a server's text edits (in `encoding`) to this buffer.
    pub fn apply_server_edits(
        &mut self,
        edits: &[TextEdit],
        encoding: Encoding,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let resolved = workspace_edit::resolve(&self.buffer, edits, encoding)?;
        self.apply_edits_keeping_selections(resolved, cx);
        Ok(())
    }

    /// The version of this view's server document, if it holds one.
    pub fn server_version(&self) -> Option<i32> {
        self.lsp.as_ref().map(|doc| doc.version)
    }

    /// The file moved to `path` on disk: closes the server document under
    /// the old name (the caller attaches the server again) and shows the new
    /// name.
    pub fn set_file_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.detach_server(true);
        self.buffer.set_path(path);
        cx.notify();
    }
}
