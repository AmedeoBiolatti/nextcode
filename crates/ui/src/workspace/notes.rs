//! Sticky notes in the workspace: the project's [`NoteStore`] (reloaded
//! when the watcher reports a notes file), the Notes panel, editors shown
//! their file's notes, and an offer when an agent writes to the person.

use super::*;
use crate::{
    note_store::{NoteStore, NoteStoreEvent},
    notes_panel::{NotesPanel, NotesPanelEvent},
};

/// Longest note text quoted in the offer.
const QUOTE_CHARS: usize = 80;

impl Workspace {
    /// The note store and panel, with their subscriptions.
    pub(super) fn notes_setup(
        project: &Project,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<NoteStore>, Entity<NotesPanel>, Vec<Subscription>) {
        let store = cx.new(|cx| NoteStore::new(project.clone(), cx));
        let panel = cx.new(|cx| NotesPanel::new(store.clone(), cx));
        let store_subscription = cx.subscribe(&store, |this, _, event, cx| match event {
            NoteStoreEvent::Incoming(incoming) => {
                let what = if incoming.reply {
                    "replied"
                } else {
                    "left you a note"
                };
                let mut quote: String = incoming.text.chars().take(QUOTE_CHARS).collect();
                if incoming.text.chars().count() > QUOTE_CHARS {
                    quote.push('…');
                }
                let quote = quote.replace('\n', " ");
                this.offer(
                    OfferKind::NoteMessage {
                        root: incoming.root,
                        file: incoming.file.clone(),
                        id: incoming.id.clone(),
                    },
                    &format!(
                        "{} {what} on {}:{}: “{quote}”",
                        incoming.author, incoming.file, incoming.line
                    ),
                    cx,
                );
            }
            NoteStoreEvent::Error(message) => {
                this.notice = Some(message.clone());
                cx.notify();
            }
            NoteStoreEvent::Seen { root, id } => {
                let before = this.offers.len();
                this.offers.retain(|offer| {
                    !matches!(&offer.kind, OfferKind::NoteMessage { root: r, id: i, .. }
                        if r == root && i == id)
                });
                if this.offers.len() != before {
                    cx.notify();
                }
            }
        });
        let panel_subscription =
            cx.subscribe_in(&panel, window, |this, _, event, window, cx| match event {
                NotesPanelEvent::Open { root, file, id } => {
                    this.open_note(*root, file, id, window, cx)
                }
            });
        (store, panel, vec![store_subscription, panel_subscription])
    }

    /// Shows a newly opened editor its file's notes.
    pub(super) fn show_notes_in(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let store = self.note_store.clone();
        editor.update(cx, |editor, cx| editor.set_note_store(store, None, cx));
    }

    /// Opens `file` of folder `root` at note `id`, its thread open.
    pub(super) fn open_note(
        &mut self,
        root: usize,
        file: &str,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root_path) = self.project.roots().get(root).map(|root| root.path.clone()) else {
            return;
        };
        let path = self.project.join(&root_path, file);
        self.record_position(cx);
        self.forward.clear();
        if let Some(editor) = self.open_editor(&path, window, cx) {
            editor.update(cx, |editor, cx| editor.reveal_note(id, window, cx));
        }
    }

    pub(super) fn toggle_notes(
        &mut self,
        _: &docks::ToggleNotes,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_panel(PanelId::Notes, cx);
    }

    /// The Notes tab's count: open notes waiting for the person.
    pub(super) fn notes_badge(&self, cx: &App) -> usize {
        self.note_store.read(cx).awaiting_user()
    }
}
