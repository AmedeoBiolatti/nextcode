//! The sticky notes built-in extension: the project's note store, given to
//! each file's editor and notebook (which then show and track the notes,
//! see `editor::notes`), the Notes panel with its count of notes waiting
//! for the person, and an offer when an agent writes to them. On by
//! default. Off, editors show no notes; the notes file and the
//! `next_code notes` CLI are unaffected.

use gpui::{App, Context, Div, Window, prelude::*};
use watcher::Batch;

use super::{Builtin, Entry, FileView, MenuItem, Panel, PanelView, notebook::view::NotebookView};
use crate::{
    layout::{DockPosition, PanelId},
    note_store::{NoteStore, NoteStoreEvent},
    workspace::{OfferKind, Workspace},
};

pub mod panel;

use panel::{NotesPanel, NotesPanelEvent};

gpui::actions!(
    notes,
    [
        /// Its name before notes became a built-in extension still works
        /// (in `menus.json`, for instance).
        #[action(deprecated_aliases = ["workspace::ToggleNotes"])]
        TogglePanel
    ]
);

pub const BUILTIN: Builtin = Builtin {
    id: "notes",
    name: "Sticky Notes",
    description: "Notes between you and agents on lines of code: cards in editors, the Notes panel.",
    default_enabled: true,
    labels: &[
        ("notes::TogglePanel", "Notes Panel"),
        // The editor's commands (`editor::notes`), there while notes are on.
        ("notes::AddNote", "Add Note"),
        ("notes::ToggleResolvedNotes", "Show/Hide Resolved Notes"),
    ],
    // Keys of a note being written.
    unlisted: &["notes::SubmitNote", "notes::CancelNote"],
    menu_items: &[
        MenuItem {
            menu: "Edit",
            after: "editor::CopyReference",
            items: &[Entry::Command("notes::AddNote")],
        },
        MenuItem {
            menu: "View",
            after: "workspace::ToggleProblems",
            items: &[Entry::Labeled("notes::TogglePanel", "Notes")],
        },
    ],
    workspace_actions,
    panels: &[Panel {
        key: "notes",
        title: "Notes",
        position: DockPosition::Bottom,
        after: "debug-console",
        build: build_panel,
        badge: Some(badge),
    }],
    attach,
    detach,
    files_changed,
    accept_offer,
    ..Builtin::NONE
};

const PANEL: PanelId = PanelId::Builtin("notes");

/// Longest note text quoted in the offer.
const QUOTE_CHARS: usize = 80;

fn workspace_actions(element: Div, cx: &mut Context<Workspace>) -> Div {
    element.on_action(
        cx.listener(|workspace, _: &TogglePanel, _, cx| workspace.toggle_panel(PANEL, cx)),
    )
}

fn store(workspace: &Workspace, cx: &App) -> Option<gpui::Entity<NoteStore>> {
    let panel = workspace.builtin_panel::<NotesPanel>(PANEL.key())?;
    Some(panel.read(cx).store().clone())
}

/// The offer for note `id` of folder `root` (ids are unique per folder);
/// withdrawn when the person has seen it.
fn message_offer(root: usize, id: &str) -> OfferKind {
    OfferKind::Builtin {
        extension: "notes",
        key: format!("note-{root}-{id}"),
        action: "Show Note",
    }
}

/// The project's store, held by the panel.
fn build_panel(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> PanelView {
    let store = cx.new(|cx| NoteStore::new(workspace.project().clone(), cx));
    let panel = cx.new(|cx| NotesPanel::new(store.clone(), cx));
    let store_subscription = cx.subscribe(&store, |workspace, _, event, cx| match event {
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
            workspace.offer(
                message_offer(incoming.root, &incoming.id),
                &format!(
                    "{} {what} on {}:{}: “{quote}”",
                    incoming.author, incoming.file, incoming.line
                ),
                cx,
            );
        }
        NoteStoreEvent::Error(message) => workspace.report(message.to_string(), cx),
        NoteStoreEvent::Seen { root, id } => {
            workspace.withdraw_offer(&message_offer(*root, id), cx)
        }
    });
    let panel_subscription =
        cx.subscribe_in(
            &panel,
            window,
            |workspace, _, event, window, cx| match event {
                NotesPanelEvent::Open { root, file, id } => {
                    open_note(workspace, *root, file, id, window, cx)
                }
            },
        );
    PanelView::without_focus(panel, vec![store_subscription, panel_subscription])
}

/// The Notes tab's count: open notes waiting for the person.
fn badge(workspace: &Workspace, cx: &App) -> usize {
    store(workspace, cx).map_or(0, |store| store.read(cx).awaiting_user())
}

/// Shows the file's notes in its editor or notebook.
fn attach(workspace: &mut Workspace, view: &FileView, cx: &mut Context<Workspace>) {
    let Some(store) = store(workspace, cx) else {
        return;
    };
    match view {
        FileView::Editor(editor) => {
            editor.update(cx, |editor, cx| editor.set_note_store(store, None, cx))
        }
        FileView::Builtin(tab) => {
            let Some(view) = tab.downcast::<NotebookView>() else {
                return;
            };
            let path = view.read(cx).path(cx);
            if let Some((root, file)) = workspace.project().relative(&path) {
                view.update(cx, |view, cx| view.set_note_store(store, root, file, cx));
            }
        }
    }
}

fn detach(_: &mut Workspace, view: &FileView, cx: &mut Context<Workspace>) {
    match view {
        FileView::Editor(editor) => editor.update(cx, |editor, cx| editor.remove_note_store(cx)),
        FileView::Builtin(tab) => {
            if let Some(view) = tab.downcast::<NotebookView>() {
                view.update(cx, |view, cx| view.remove_note_store(cx));
            }
        }
    }
}

/// Notes files changed on disk: the store re-reads them.
fn files_changed(
    workspace: &mut Workspace,
    batch: &Batch,
    _: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(store) = store(workspace, cx) {
        store.update(cx, |store, cx| {
            let paths = batch.events.iter().map(|event| event.path.as_path());
            store.files_changed(paths, batch.rescan, cx)
        });
    }
}

/// The folder and note of `message_offer`'s key.
fn offer_note(key: &str) -> Option<(usize, &str)> {
    let (root, id) = key.strip_prefix("note-")?.split_once('-')?;
    Some((root.parse().ok()?, id))
}

fn accept_offer(
    workspace: &mut Workspace,
    key: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some((root, id)) = offer_note(key) else {
        return;
    };
    let file = store(workspace, cx).and_then(|store| {
        let store = store.read(cx);
        let (_, note) = store
            .all()
            .find(|(other, note)| *other == root && note.id == id)?;
        Some(note.file.clone())
    });
    if let Some(file) = file {
        open_note(workspace, root, &file, id, window, cx);
    }
}

/// Opens `file` of folder `root` at note `id`, its thread open.
fn open_note(
    workspace: &mut Workspace,
    root: usize,
    file: &str,
    id: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let Some(root_path) = project.roots().get(root).map(|root| root.path.clone()) else {
        return;
    };
    let path = project.join(&root_path, file);
    let cell = store(workspace, cx).and_then(|store| {
        store
            .read(cx)
            .file_notes(root, file)
            .find(|note| note.id == id)
            .and_then(|note| note.cell)
    });
    if let Some(editor) = workspace.open_file_editor(&path, window, cx) {
        editor.update(cx, |editor, cx| editor.reveal_note(id, window, cx));
    } else if let Some(cell) = cell
        && let Some(view) = workspace
            .active_builtin_view::<NotebookView>()
            .filter(|view| view.read(cx).path(cx) == path)
    {
        view.update(cx, |view, cx| view.reveal_note(cell, id, window, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menus::{Item, MenuBar};

    #[test]
    fn menus_read_as_before_notes_were_an_extension() {
        let bar = MenuBar::default();
        let items = |label: &str| {
            bar.menus
                .iter()
                .find(|menu| menu.label == label)
                .unwrap()
                .items
                .clone()
        };
        let edit = items("Edit");
        let reference = edit
            .iter()
            .position(|item| *item == Item::Plain("editor::CopyReference".into()))
            .unwrap();
        assert_eq!(edit[reference + 1], Item::Plain("notes::AddNote".into()));
        let view = items("View");
        let problems = view
            .iter()
            .position(|item| *item == Item::Plain("workspace::ToggleProblems".into()))
            .unwrap();
        assert_eq!(
            view[problems + 1],
            Item::Command {
                command: "notes::TogglePanel".into(),
                label: "Notes".into(),
            }
        );
    }

    #[test]
    fn offers_name_the_folder_and_note() {
        let OfferKind::Builtin { key, .. } = message_offer(2, "n1a2b") else {
            unreachable!()
        };
        assert_eq!(offer_note(&key), Some((2, "n1a2b")));
        assert_eq!(offer_note("python-sync"), None);
    }

    #[gpui::test]
    fn the_panel_command_keeps_its_old_name(cx: &mut gpui::TestAppContext) {
        let action = cx
            .update(|cx| cx.build_action("workspace::ToggleNotes", None))
            .unwrap();
        assert_eq!(action.name(), "notes::TogglePanel");
    }
}
