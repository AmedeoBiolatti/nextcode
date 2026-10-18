//! Sticky notes in the editor (see `note_store`): a marker in the gutter
//! on anchored lines, a card per note in the free space right of the code
//! at its first line (stacked so cards never overlap, an icon at the right
//! edge where the code leaves no room), the thread with a reply box when a
//! card is clicked, and Add Note on the selection or cursor line.
//!
//! Anchors follow the person's edits: each note's lines are byte offsets
//! mapped through every change (shifted by edits above, kept by edits
//! inside, orphaned when its lines are deleted), saved to the notes file
//! shortly after typing stops. The file is re-anchored (`notes::reanchor`)
//! only when the notes change on disk, never per frame; only notes on
//! visible rows are laid out.

use std::{collections::HashSet, io, time::Duration};

use ::notes::{AnchorState, Author, AuthorKind, Note};
use gpui::{Div, Stateful, Subscription, anchored, deferred, rgb};

use super::*;
use crate::note_store::NoteStore;

actions!(
    notes,
    [
        /// A note on the selection or the cursor line.
        AddNote,
        /// Adds the note, sends the reply or saves the edit being typed.
        SubmitNote,
        /// Closes the note being typed or the open thread.
        CancelNote,
        /// Shows or hides resolved notes in editors.
        ToggleResolvedNotes,
    ]
);

/// Around a note's text input, for Ctrl+Enter and Escape.
const INPUT_CONTEXT: &str = "NoteInput";

pub(super) fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-k secondary-n", AddNote, Some(CONTEXT)),
        // After the editor's keys, so they win in the input.
        KeyBinding::new("secondary-enter", SubmitNote, Some("NoteInput > Editor")),
        KeyBinding::new("escape", CancelNote, Some(INPUT_CONTEXT)),
    ]);
}

/// Card geometry, in pixels.
const CARD_MAX_WIDTH: f32 = 280.;
const CARD_MIN_WIDTH: f32 = 170.;
/// Border, padding, an 18 px header, a 2 px gap and two 15 px text lines.
const CARD_HEIGHT: f32 = 62.;
const CARD_GAP: f32 = 4.;
/// Space kept between the code and a card.
const CODE_GAP: f32 = 24.;
const ICON_WIDTH: f32 = 26.;
const ICON_HEIGHT: f32 = 18.;
const THREAD_WIDTH: f32 = 400.;
/// Typing pauses this long before moved notes are published (written on save).
const SAVE_DELAY: Duration = Duration::from_millis(700);

/// Where one card goes, relative to the editor's top-left corner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Placement {
    pub top: f32,
    pub left: f32,
    pub width: f32,
    /// Shown as an icon at the right edge.
    pub collapsed: bool,
}

/// Places cards for notes whose first rows are at `slots` (tops, in
/// order, with whether the person collapsed that card): each at its row or
/// just below the card above it, right-aligned at `right`, as wide as the
/// space right of the code allows (`code_right(top, bottom)`: where the
/// code on those rows ends), or an icon at the edge when that is too
/// narrow.
pub(super) fn place_cards(
    slots: &[(f32, bool)],
    right: f32,
    code_right: impl Fn(f32, f32) -> f32,
) -> Vec<Placement> {
    let mut below = f32::NEG_INFINITY;
    slots
        .iter()
        .map(|&(anchor, collapsed)| {
            let top = anchor.max(below + CARD_GAP);
            if !collapsed {
                let bottom = top + CARD_HEIGHT;
                let width = (right - code_right(top, bottom) - CODE_GAP).min(CARD_MAX_WIDTH);
                if width >= CARD_MIN_WIDTH {
                    below = bottom;
                    return Placement {
                        top,
                        left: right - width,
                        width,
                        collapsed: false,
                    };
                }
            }
            below = top + ICON_HEIGHT;
            Placement {
                top,
                left: right - ICON_WIDTH,
                width: ICON_WIDTH,
                collapsed: true,
            }
        })
        .collect()
}

/// Maps a note's text (`start..end`) through `change`; `None` when the
/// change deleted all of it (and inserted nothing in its place).
fn map_span(start: usize, end: usize, change: &Change) -> Option<(usize, usize)> {
    let covered = change.start <= start && end <= change.old_end && change.start < change.old_end;
    if covered {
        if change.new_end == change.start {
            return None;
        }
        // Replaced as a whole (pasted over): the new text is the note's.
        return Some((change.start, change.new_end));
    }
    let start = change.map_offset(start, Bias::Right);
    let end = change.map_offset(end, Bias::Left).max(start);
    Some((start, end))
}

/// What the person is typing for a note.
enum InputKind {
    /// A new note on these offsets (line start, line end) and columns.
    New {
        start: usize,
        end: usize,
        columns: Option<(usize, usize)>,
        to: Option<String>,
    },
    Reply(String),
    /// Message `1` of note `0`.
    Edit(String, usize),
}

struct NoteInput {
    kind: InputKind,
    editor: Entity<Editor>,
}

/// A note of this file and where it is in the buffer now.
struct Tracked {
    note: Note,
    /// The first anchored line's start and the last one's end.
    start: usize,
    end: usize,
    state: AnchorState,
    /// Moved or edited since last saved.
    dirty: bool,
}

#[derive(Default)]
pub(super) struct NotesState {
    store: Option<Entity<NoteStore>>,
    /// Folder index and the file relative to it.
    root: usize,
    file: String,
    /// The notebook cell this editor shows (1-based).
    cell: Option<usize>,
    /// The store's generation last synced.
    generation: Option<u64>,
    tracked: Vec<Tracked>,
    /// The note whose thread shows.
    expanded: Option<String>,
    /// Cards the person collapsed to icons.
    collapsed: HashSet<String>,
    input: Option<NoteInput>,
    _save: Option<Task<()>>,
    _observe: Option<Subscription>,
}

/// A note as `next_code context` reports it: where it is now.
#[derive(Debug, Clone, PartialEq)]
pub struct NoteFact {
    pub id: String,
    /// 1-based, inclusive.
    pub start_line: usize,
    pub end_line: usize,
    pub state: AnchorState,
    pub open: bool,
    pub to: Option<String>,
    pub author: String,
    pub text: String,
    pub replies: usize,
}

impl NoteFact {
    /// `note` at 1-based lines `start_line..=end_line`.
    pub fn new(note: &Note, start_line: usize, end_line: usize, state: AnchorState) -> Self {
        Self {
            id: note.id.clone(),
            start_line,
            end_line,
            state,
            open: note.is_open(),
            to: note.to.clone(),
            author: note
                .thread
                .first()
                .map(|message| message.author.name.clone())
                .unwrap_or_default(),
            text: note.text().to_owned(),
            replies: note.thread.len().saturating_sub(1),
        }
    }
}

/// A note's card color: a name or `#rrggbb`.
fn note_color(color: Option<&str>) -> Hsla {
    let hex = match color.unwrap_or("yellow") {
        "yellow" => 0xf3e08a,
        "blue" => 0xa9d3f0,
        "green" => 0xb9e3a3,
        "pink" => 0xf5b8cf,
        "purple" => 0xd2c0f2,
        "orange" => 0xf7c487,
        other => other
            .strip_prefix('#')
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            .unwrap_or(0xf3e08a),
    };
    rgb(hex).into()
}

/// Dark text on the (light) card colors, in every theme.
fn card_text() -> Hsla {
    rgb(0x2a2618).into()
}

/// "now", "5m", "3h", "2d".
fn ago(time: &str) -> String {
    let Some(ms) = ::notes::parse_time(time) else {
        return String::new();
    };
    let seconds = ::notes::now_ms().saturating_sub(ms) / 1000;
    match seconds {
        0..60 => "now".into(),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

fn to_label(to: Option<&str>) -> String {
    match to {
        None => "anyone".into(),
        Some(to) => to.to_owned(),
    }
}

/// agent → user → anyone → agent.
fn next_to(to: Option<&str>) -> Option<String> {
    match to {
        Some("agent") => Some("user".into()),
        Some("user") => None,
        _ => Some("agent".into()),
    }
}

/// The first `max` visual lines of `text`; `wrap` gives the byte offsets where a line wraps.
/// The flag marks the last line when text was left out.
fn clamp_lines(
    text: &str,
    max: usize,
    mut wrap: impl FnMut(&str) -> Vec<usize>,
) -> Vec<(String, bool)> {
    let mut lines = Vec::new();
    for paragraph in text.trim_end().split('\n') {
        let paragraph = paragraph.trim_end_matches('\r');
        let mut start = 0;
        for end in wrap(paragraph).into_iter().chain([paragraph.len()]) {
            lines.push((paragraph[start..end].trim_end().to_owned(), false));
            start = end;
            if lines.len() > max {
                lines.truncate(max);
                if let Some(last) = lines.last_mut() {
                    last.1 = true;
                }
                return lines;
            }
        }
    }
    lines
}

impl Editor {
    /// Shows `store`'s notes on this editor's file (`cell`: a notebook
    /// cell's editor, 1-based) and lets the person add some.
    pub fn set_note_store(
        &mut self,
        store: Entity<NoteStore>,
        cell: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let Some((root, file)) = self
            .project
            .as_ref()
            .zip(self.buffer.path())
            .and_then(|(project, path)| project.relative(path))
        else {
            return;
        };
        self.set_note_target(store, root, file, cell, cx);
    }

    /// [`Editor::set_note_store`] for an editor without the file (a
    /// notebook cell): folder index and root-relative file given.
    pub fn set_note_target(
        &mut self,
        store: Entity<NoteStore>,
        root: usize,
        file: String,
        cell: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let observe = cx.observe(&store, |this, _, cx| this.sync_notes(cx));
        if self.notes.store.is_none() {
            // A closed editor's positions would otherwise hide the stored anchors.
            let editor = cx.entity_id();
            cx.on_release(move |this: &mut Self, cx| {
                if let Some(store) = this.notes.store.take() {
                    store.update(cx, |store, cx| store.clear_live_positions(editor, cx));
                }
            })
            .detach();
        }
        self.notes = NotesState {
            store: Some(store),
            root,
            file,
            cell,
            _observe: Some(observe),
            ..Default::default()
        };
        self.sync_notes(cx);
    }

    /// The notes of this file where they are now (unsaved edits included).
    pub fn note_facts(&self) -> Vec<NoteFact> {
        self.notes
            .tracked
            .iter()
            .map(|tracked| {
                let (start_line, end_line) = self.note_lines(tracked);
                NoteFact::new(&tracked.note, start_line + 1, end_line + 1, tracked.state)
            })
            .collect()
    }

    /// 0-based first and last line of a tracked note.
    fn note_lines(&self, tracked: &Tracked) -> (usize, usize) {
        let len = self.buffer.len();
        (
            self.buffer.line_of(tracked.start.min(len)),
            self.buffer.line_of(tracked.end.min(len)),
        )
    }

    /// Takes the store's notes for this file: known ones keep their
    /// followed position while their stored anchor is the one last seen;
    /// others are found in the text.
    fn sync_notes(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.notes.store.clone() else {
            return;
        };
        let store = store.read(cx);
        if self.notes.generation == Some(store.generation()) {
            return;
        }
        self.notes.generation = Some(store.generation());
        let mut text: Option<String> = None;
        let mut old = std::mem::take(&mut self.notes.tracked);
        let mut tracked = Vec::new();
        let notes = store
            .file_notes(self.notes.root, &self.notes.file)
            .filter(|note| note.cell == self.notes.cell);
        for note in notes {
            let known = old
                .iter()
                .position(|t| t.note.id == note.id && (t.dirty || t.note.anchor == note.anchor));
            if let Some(index) = known {
                let mut kept = old.swap_remove(index);
                let anchor = std::mem::take(&mut kept.note.anchor);
                kept.note = note.clone();
                if kept.dirty {
                    kept.note.anchor = anchor;
                }
                tracked.push(kept);
                continue;
            }
            let text = text.get_or_insert_with(|| self.text());
            tracked.push(self.anchor_note(note.clone(), text));
        }
        tracked.sort_by_key(|t| (t.start, t.note.created.clone()));
        self.notes.tracked = tracked;
        self.publish_note_positions(cx);
        let ids: HashSet<&str> = self
            .notes
            .tracked
            .iter()
            .map(|t| t.note.id.as_str())
            .collect();
        if let Some(expanded) = &self.notes.expanded
            && !ids.contains(expanded.as_str())
        {
            self.notes.expanded = None;
            if matches!(
                self.notes.input,
                Some(NoteInput {
                    kind: InputKind::Reply(_) | InputKind::Edit(..),
                    ..
                })
            ) {
                self.notes.input = None;
            }
        }
        cx.notify();
    }

    /// Finds `note` in `text` (the buffer's).
    fn anchor_note(&self, note: Note, text: &str) -> Tracked {
        let found = note.reanchor(text);
        let last = self.buffer.line_count().saturating_sub(1);
        let start_line = (found.start_line.max(1) - 1).min(last);
        let end_line = (found.end_line.max(1) - 1).clamp(start_line, last);
        Tracked {
            note,
            start: self.buffer.line_start(start_line),
            end: self.buffer.line_end(end_line),
            state: found.state,
            dirty: false,
        }
    }

    /// Moves the notes with `self.changes`; schedules saving moved ones.
    pub(super) fn notes_edited(&mut self, cx: &mut Context<Self>) {
        if self.changes.is_empty() {
            return;
        }
        let mut dirty = false;
        for tracked in &mut self.notes.tracked {
            if tracked.state == AnchorState::Orphaned {
                continue;
            }
            for change in &self.changes {
                let touched = change.start <= tracked.end && tracked.start <= change.old_end;
                match map_span(tracked.start, tracked.end, change) {
                    Some((start, end)) => {
                        tracked.dirty |= touched || start != tracked.start;
                        tracked.start = start;
                        tracked.end = end;
                    }
                    None => {
                        tracked.state = AnchorState::Orphaned;
                        tracked.dirty = false;
                        break;
                    }
                }
            }
            dirty |= tracked.dirty;
        }
        if let Some(NoteInput {
            kind: InputKind::New { start, end, .. },
            ..
        }) = &mut self.notes.input
        {
            for change in &self.changes {
                if let Some((s, e)) = map_span(*start, *end, change) {
                    (*start, *end) = (s, e);
                }
            }
        }
        let orphans = self
            .notes
            .tracked
            .iter()
            .any(|t| t.state == AnchorState::Orphaned);
        if (dirty || orphans) && self.notes.store.is_some() {
            self.notes._save = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SAVE_DELAY).await;
                this.update(cx, |this, cx| this.refresh_note_positions(cx))
                    .ok();
            }));
        }
    }

    /// Looks again for orphaned notes (an undo brings their lines back) and
    /// publishes where notes are now; nothing is written before a save.
    fn refresh_note_positions(&mut self, cx: &mut Context<Self>) {
        let text = self.text();
        for index in 0..self.notes.tracked.len() {
            let tracked = &self.notes.tracked[index];
            if tracked.state == AnchorState::Orphaned {
                let found = self.anchor_note(tracked.note.clone(), &text);
                if found.state != AnchorState::Orphaned {
                    self.notes.tracked[index] = found;
                }
            }
        }
        cx.notify();
        self.publish_note_positions(cx);
    }

    /// The buffer was saved (or its notebook): writes the anchors of moved or
    /// edited notes, so notes.json never points into unsaved text.
    pub(crate) fn write_note_anchors(&mut self, cx: &mut Context<Self>) {
        self.notes._save = None;
        self.refresh_note_positions(cx);
        let text = self.text();
        let mut anchors = Vec::new();
        for index in 0..self.notes.tracked.len() {
            let tracked = &self.notes.tracked[index];
            if tracked.state == AnchorState::Orphaned || !tracked.dirty {
                continue;
            }
            let (start_line, end_line) = self.note_lines(tracked);
            let old = &tracked.note.anchor;
            let columns = old.start_column.zip(old.end_column);
            let anchor = ::notes::anchor_from(&text, start_line + 1, end_line + 1, columns);
            let tracked = &mut self.notes.tracked[index];
            tracked.dirty = false;
            if anchor != tracked.note.anchor {
                tracked.note.anchor = anchor.clone();
                anchors.push((tracked.note.id.clone(), anchor));
            }
        }
        let Some(store) = self.notes.store.clone() else {
            return;
        };
        if anchors.is_empty() {
            return;
        }
        let root = self.notes.root;
        store
            .update(cx, |store, cx| {
                store.change(
                    root,
                    move |notes| {
                        for (id, anchor) in anchors {
                            if notes.get(&id).is_some() {
                                notes.update_anchor(&id, anchor)?;
                            }
                        }
                        Ok(())
                    },
                    cx,
                )
            })
            .detach();
    }

    /// Whether notes can be added here (a project file's editor).
    pub(super) fn notes_enabled(&self) -> bool {
        self.notes.store.is_some()
    }

    /// The buffer went back to the file (reload, discarded edits): notes
    /// follow their stored anchors again.
    pub(super) fn notes_reloaded(&mut self, cx: &mut Context<Self>) {
        if self.notes.store.is_none() {
            return;
        }
        self.notes._save = None;
        self.notes.tracked.clear();
        self.notes.generation = None;
        self.sync_notes(cx);
    }

    /// Tells the store where this editor shows its notes (the Notes panel
    /// lists open files' notes where they are now).
    fn publish_note_positions(&self, cx: &mut Context<Self>) {
        let Some(store) = self.notes.store.clone() else {
            return;
        };
        let positions = self
            .notes
            .tracked
            .iter()
            .map(|t| (t.note.id.clone(), self.note_lines(t).0 + 1, t.state))
            .collect();
        let (editor, root) = (cx.entity_id(), self.notes.root);
        store.update(cx, |store, cx| {
            store.set_live_positions(editor, root, positions, cx)
        });
    }

    fn note_author(&self, cx: &App) -> Author {
        self.notes.store.as_ref().map_or_else(
            || Author::user("user"),
            |store| store.read(cx).user().clone(),
        )
    }

    fn new_note_input(
        &self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<Editor> {
        let input = cx.new(|cx| {
            let mut input = Editor::text_area(cx);
            input.set_text(text, cx);
            input.select_all_text(cx);
            input
        });
        window.focus(&input.focus_handle(cx));
        input
    }

    /// Add Note: on the selected lines (or the cursor's), with the text box
    /// open in its card.
    pub(super) fn add_note(&mut self, _: &AddNote, window: &mut Window, cx: &mut Context<Self>) {
        // A note's own text box: the editor around it handles the action.
        if self.notes.store.is_none() {
            return cx.propagate();
        }
        let s = self.primary();
        let (from, to) = (s.start(), s.end());
        let start_line = self.buffer.line_of(from);
        let mut end_line = self.buffer.line_of(to);
        // A selection of whole lines ends at the next line's start.
        if end_line > start_line && self.buffer.line_start(end_line) == to {
            end_line -= 1;
        }
        let columns = (!s.is_empty() && start_line == self.buffer.line_of(to))
            .then(|| (self.buffer.column(from) + 1, self.buffer.column(to) + 1));
        self.open_note_composer(start_line, end_line, columns, window, cx);
    }

    /// The new-note box on lines `start_line..=end_line` (0-based).
    pub fn open_note_composer(
        &mut self,
        start_line: usize,
        end_line: usize,
        columns: Option<(usize, usize)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let last = self.buffer.line_count().saturating_sub(1);
        let (start_line, end_line) = (start_line.min(last), end_line.min(last));
        let editor = self.new_note_input("", window, cx);
        self.text_menu = None;
        self.gutter_menu = None;
        self.notes.expanded = None;
        self.notes.input = Some(NoteInput {
            kind: InputKind::New {
                start: self.buffer.line_start(start_line),
                end: self.buffer.line_end(end_line),
                columns,
                to: Some("agent".into()),
            },
            editor,
        });
        cx.notify();
    }

    pub(super) fn submit_note(
        &mut self,
        _: &SubmitNote,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.notes.store.clone() else {
            return cx.propagate();
        };
        let Some(input) = &self.notes.input else {
            return;
        };
        let text = input.editor.read(cx).text().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let author = self.note_author(cx);
        let root = self.notes.root;
        let task = match &input.kind {
            InputKind::New {
                start,
                end,
                columns,
                to,
            } => {
                let (start_line, end_line) =
                    (self.buffer.line_of(*start), self.buffer.line_of(*end));
                let anchor =
                    ::notes::anchor_from(&self.text(), start_line + 1, end_line + 1, *columns);
                let (file, cell, to) = (self.notes.file.clone(), self.notes.cell, to.clone());
                self.notes.input = None;
                window.focus(&self.focus_handle);
                store.update(cx, |store, cx| {
                    store.change(
                        root,
                        move |notes| {
                            notes.add(&file, cell, anchor, &text, author, to);
                            Ok(())
                        },
                        cx,
                    )
                })
            }
            InputKind::Reply(id) => {
                let id = id.clone();
                let editor = self.new_note_input("", window, cx);
                self.notes.input = Some(NoteInput {
                    kind: InputKind::Reply(id.clone()),
                    editor,
                });
                self.mark_note_seen(&id, cx);
                store.update(cx, |store, cx| {
                    store.change(
                        root,
                        move |notes| notes.reply(&id, &text, author).map(drop),
                        cx,
                    )
                })
            }
            InputKind::Edit(id, index) => {
                let (id, index) = (id.clone(), *index);
                let editor = self.new_note_input("", window, cx);
                self.notes.input = Some(NoteInput {
                    kind: InputKind::Reply(id.clone()),
                    editor,
                });
                store.update(cx, |store, cx| {
                    store.change(
                        root,
                        move |notes| notes.edit_message(&id, index, &text).map(drop),
                        cx,
                    )
                })
            }
        };
        task.detach();
        cx.notify();
    }

    pub(super) fn cancel_note(
        &mut self,
        _: &CancelNote,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.notes.store.is_none() {
            return cx.propagate();
        }
        self.close_note(window, cx);
    }

    fn close_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.notes.input = None;
        self.notes.expanded = None;
        window.focus(&self.focus_handle);
        cx.notify();
    }

    pub(super) fn toggle_resolved_notes(
        &mut self,
        _: &ToggleResolvedNotes,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &self.notes.store {
            Some(store) => store.update(cx, |store, cx| {
                let show = !store.show_resolved();
                store.set_show_resolved(show, cx);
            }),
            None => cx.propagate(),
        }
    }

    /// Opens note `id`'s thread (with a reply box) and scrolls to it.
    pub fn reveal_note(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_notes(cx);
        let Some(tracked) = self.notes.tracked.iter().find(|t| t.note.id == id) else {
            return;
        };
        self.pin_reveal = Some(tracked.start..tracked.end);
        self.expand_note(id.to_owned(), window, cx);
    }

    fn expand_note(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.new_note_input("", window, cx);
        self.notes.collapsed.remove(&id);
        self.notes.input = Some(NoteInput {
            kind: InputKind::Reply(id.clone()),
            editor,
        });
        self.mark_note_seen(&id, cx);
        self.notes.expanded = Some(id);
        cx.notify();
    }

    /// What card placement depends on besides the text: `None` without
    /// notes (no extra frames then).
    pub(super) fn note_geometry(&self) -> Option<(Bounds<Pixels>, Point<Pixels>)> {
        if self.notes.tracked.is_empty() && self.notes.input.is_none() {
            return None;
        }
        Some((self.layout.as_ref()?.text_bounds, self.scroll))
    }

    /// Opening or answering a note dismisses its announcement.
    fn mark_note_seen(&self, id: &str, cx: &mut Context<Self>) {
        if let Some(store) = self.notes.store.clone() {
            let root = self.notes.root;
            store.update(cx, |store, cx| store.note_seen(root, id, cx));
        }
    }

    fn toggle_note(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.notes.expanded.as_deref() == Some(id) {
            self.close_note(window, cx);
        } else {
            self.expand_note(id.to_owned(), window, cx);
        }
    }

    /// Runs `change` on the notes file (resolve, delete, …).
    fn change_note(
        &mut self,
        change: impl FnOnce(&mut ::notes::Notes) -> io::Result<()> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        if let Some(store) = self.notes.store.clone() {
            let root = self.notes.root;
            store
                .update(cx, |store, cx| store.change(root, change, cx))
                .detach();
        }
    }

    /// The notes drawn (open ones, resolved ones when shown), not orphaned.
    fn shown_notes<'a>(&'a self, cx: &App) -> impl Iterator<Item = &'a Tracked> + 'a {
        let show_resolved = self
            .notes
            .store
            .as_ref()
            .is_some_and(|store| store.read(cx).show_resolved());
        self.notes.tracked.iter().filter(move |t| {
            t.state != AnchorState::Orphaned && (show_resolved || t.note.is_open())
        })
    }

    /// The note gutter markers: a bar beside the anchored lines, a square
    /// at the first, in the lightbulb's column.
    pub(super) fn paint_note_marker(
        &self,
        layout: &EditorLayout,
        line: usize,
        top: Pixels,
        window: &mut Window,
        cx: &App,
    ) {
        if self.notes.tracked.is_empty() {
            return;
        }
        let (left, width) = Self::lightbulb_column(layout);
        for tracked in self.shown_notes(cx) {
            let (first, last) = self.note_lines(tracked);
            if line < first || line > last {
                continue;
            }
            let color = note_color(tracked.note.color.as_deref());
            let color = if tracked.note.is_open() {
                color
            } else {
                color.opacity(0.5)
            };
            let x = left + width - px(4.);
            window.paint_quad(fill(
                Bounds::new(point(x, top), size(px(2.), layout.line_height)),
                color,
            ));
            if line == first {
                let side = px(8.);
                let y = top + (layout.line_height - side) / 2.;
                window.paint_quad(
                    fill(
                        Bounds::new(point(x - side + px(2.), y), size(side, side)),
                        color,
                    )
                    .corner_radii(px(1.5)),
                );
            }
        }
    }

    /// A click on a note's gutter marker opens (or closes) its thread.
    pub(super) fn note_marker_click(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(layout) = &self.layout else {
            return false;
        };
        let (left, width) = Self::lightbulb_column(layout);
        if position.x < left || position.x >= left + width {
            return false;
        }
        let Some((_, row)) = layout.row_layout_at(position.y, self.scroll.y) else {
            return false;
        };
        let line = row.row.line;
        let id = self.shown_notes(cx).find_map(|t| {
            let (first, last) = self.note_lines(t);
            (first..=last).contains(&line).then(|| t.note.id.clone())
        });
        let Some(id) = id else {
            return false;
        };
        cx.stop_propagation();
        self.toggle_note(&id, window, cx);
        true
    }

    /// Cards beside the visible notes (children of the text area), and the
    /// open thread or new-note box.
    pub(super) fn render_notes(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(layout) = &self.layout else {
            return Vec::new();
        };
        if self.single_line || (self.text_area && !self.notebook_cell) {
            return Vec::new();
        }
        let origin = layout.bounds.origin;
        let row_top = |line: usize| -> Option<f32> {
            let index = layout
                .rows
                .iter()
                .position(|row| row.row.line == line && row.row.first)?;
            Some(f32::from(layout.row_top(index, self.scroll.y) - origin.y))
        };
        // Visible notes in order, with their rows' tops.
        let mut visible: Vec<(&Tracked, f32)> = self
            .shown_notes(cx)
            .filter_map(|t| Some((t, row_top(self.note_lines(t).0)?)))
            .collect();
        visible.sort_by(|a, b| a.1.total_cmp(&b.1));
        let mut elements = Vec::new();
        let expanded = self.notes.expanded.as_deref();
        // A notebook cell is only as tall as its text: icons fit its rows.
        let slots: Vec<(f32, bool)> = visible
            .iter()
            .map(|(t, top)| {
                let collapsed = self.notes.collapsed.contains(&t.note.id);
                (*top, collapsed || self.notebook_cell)
            })
            .collect();
        let line_height = f32::from(layout.line_height);
        let code_right = |top: f32, bottom: f32| -> f32 {
            layout
                .rows
                .iter()
                .enumerate()
                .filter(|(index, _)| {
                    let row = f32::from(layout.row_top(*index, self.scroll.y) - origin.y);
                    row + line_height > top && row < bottom
                })
                .map(|(_, row)| {
                    f32::from(layout.text_left(row, self.scroll.x) + row.shaped.width - origin.x)
                })
                .fold(f32::from(layout.text_bounds.left() - origin.x), f32::max)
        };
        let right = f32::from(layout.text_bounds.right() - origin.x - scrollbar::WIDTH) - 4.;
        let placements = place_cards(&slots, right, code_right);
        let mut expanded_at = None;
        for ((tracked, _), placement) in visible.iter().zip(&placements) {
            if expanded == Some(tracked.note.id.as_str()) {
                expanded_at = Some(*placement);
                continue;
            }
            elements.push(self.render_card(tracked, placement, cx));
        }
        if let Some(id) = expanded
            && let Some(tracked) = self.notes.tracked.iter().find(|t| t.note.id == id)
        {
            let placement = expanded_at.map_or_else(
                || {
                    let top = row_top(self.note_lines(tracked).0).unwrap_or(0.);
                    (top, right)
                },
                |p| (p.top, p.left + p.width),
            );
            let left =
                (placement.1 - THREAD_WIDTH).max(f32::from(layout.text_bounds.left() - origin.x));
            let position = point(origin.x + px(left), origin.y + px(placement.0));
            elements.push(self.render_thread(tracked, position, cx));
        }
        if let Some(NoteInput {
            kind: InputKind::New { start, .. },
            ..
        }) = &self.notes.input
        {
            let line = self.buffer.line_of(*start);
            let top = row_top(line).map_or(0., |top| top + line_height);
            let left = (right - THREAD_WIDTH).max(f32::from(layout.text_bounds.left() - origin.x));
            let position = point(origin.x + px(left), origin.y + px(top));
            elements.push(self.render_composer(position, cx));
        }
        elements
    }

    fn render_card(
        &self,
        tracked: &Tracked,
        placement: &Placement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let note = &tracked.note;
        let id = note.id.clone();
        let color = note_color(note.color.as_deref());
        let open = note.is_open();
        let first = note.thread.first();
        let author = first.map_or("", |message| message.author.name.as_str());
        let is_agent = first.is_some_and(|message| message.author.kind == AuthorKind::Agent);
        let replies = note.thread.len().saturating_sub(1);
        let label = format!("Note by {author}: {}", note.text());
        let card = div()
            .id(SharedString::from(format!("note-{id}")))
            .absolute()
            .top(px(placement.top))
            .left(px(placement.left))
            .w(px(placement.width))
            .block_mouse_except_scroll()
            .cursor_pointer()
            .font_family(theme::UI_FONT)
            .text_color(card_text())
            .bg(color)
            .when(!open, |card| card.opacity(0.55))
            .on_click(cx.listener({
                let id = id.clone();
                move |this, _, window, cx| this.toggle_note(&id, window, cx)
            }));
        if placement.collapsed {
            return card
                .h(px(ICON_HEIGHT))
                .rounded_sm()
                .border_1()
                .border_color(color.blend(card_text().opacity(0.3)))
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .child(if replies > 0 {
                    format!("✎{}", replies + 1)
                } else {
                    "✎".to_owned()
                })
                .pin(format!("note:{id}"))
                .label(label)
                .into_any_element();
        }
        let collapse = div()
            .id(SharedString::from(format!("note-collapse-{id}")))
            .px_1()
            .rounded_sm()
            .hover(|style| style.bg(card_text().opacity(0.12)))
            .child("–")
            .on_click(cx.listener({
                let id = id.clone();
                move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.notes.collapsed.insert(id.clone());
                    cx.notify();
                }
            }))
            .pin("collapse")
            .label("Collapse note");
        card.h(px(CARD_HEIGHT))
            .px_2()
            .py_1()
            .rounded_md()
            .shadow_sm()
            .border_1()
            .border_color(color.blend(card_text().opacity(0.25)))
            .flex()
            .flex_col()
            .gap(px(2.))
            .overflow_hidden()
            .child(
                div()
                    .flex_none()
                    .h(px(18.))
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_xs()
                    .line_height(px(16.))
                    .whitespace_nowrap()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .min_w_0()
                            .truncate()
                            .child(format!("{}{author}", if is_agent { "◆ " } else { "" })),
                    )
                    .child(
                        div()
                            .text_color(card_text().opacity(0.65))
                            .child(ago(note.last_time())),
                    )
                    .child(div().flex_1())
                    .when(replies > 0, |row| {
                        row.child(div().text_color(card_text().opacity(0.7)).child(format!(
                            "{replies} repl{}",
                            if replies == 1 { "y" } else { "ies" }
                        )))
                    })
                    .child(
                        div()
                            .px_1()
                            .rounded_sm()
                            .bg(card_text().opacity(0.12))
                            .child(if open {
                                format!("→ {}", to_label(note.to.as_deref()))
                            } else {
                                "✓ resolved".to_owned()
                            }),
                    )
                    .child(collapse),
            )
            .child({
                // gpui's line_clamp drops the ellipsis when wrapping wastes width; wrap here.
                let width = px((placement.width - 18.).max(1.));
                let mut wrapper = cx
                    .text_system()
                    .line_wrapper(gpui::font(theme::UI_FONT), px(12.));
                let lines = clamp_lines(note.text(), 2, |line| {
                    let fragments = [gpui::LineFragment::text(line)];
                    wrapper.wrap_line(&fragments, width).map(|b| b.ix).collect()
                });
                let last = lines.len().saturating_sub(1);
                div()
                    .flex_none()
                    .h(px(30.))
                    .overflow_hidden()
                    .text_xs()
                    .line_height(px(15.))
                    .children(lines.into_iter().enumerate().map(|(ix, (line, cut))| {
                        let line = if cut && ix == last {
                            wrapper
                                .truncate_line(
                                    format!("{line}…").into(),
                                    width,
                                    "…",
                                    &mut Vec::new(),
                                )
                                .to_string()
                        } else {
                            line
                        };
                        div().whitespace_nowrap().overflow_hidden().child(line)
                    }))
            })
            .pin(format!("note:{id}"))
            .label(label)
            .into_any_element()
    }

    fn note_button(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Stateful<Div> {
        div()
            .id(ElementId::Name(id.into()))
            .px_2()
            .py(px(1.))
            .rounded_sm()
            .cursor_pointer()
            .text_xs()
            .text_color(theme::accent_text())
            .border_1()
            .border_color(theme::border())
            .hover(|style| style.bg(theme::hover_row()))
            .child(label.into())
    }

    fn note_input_box(&self, input: &Entity<Editor>) -> impl IntoElement + use<> {
        // As the breakpoint editor's fields: inside `anchored` the input
        // needs a flex-grown width and an explicit height.
        div()
            .flex()
            .px_1()
            .border_1()
            .border_color(theme::accent())
            .rounded_sm()
            .bg(theme::background())
            .font_family(theme::MONO_FONT)
            .text_size(px(theme::EDITOR_FONT_SIZE))
            .line_height(px(theme::EDITOR_LINE_HEIGHT))
            .text_color(theme::text())
            .child(
                div()
                    .flex_1()
                    .h(px(theme::EDITOR_LINE_HEIGHT * 3.))
                    .child(input.clone()),
            )
            .pin("text")
            .label("Note text")
    }

    /// A popover over the text at window `position`, pinned as `pin`.
    fn note_popover(
        &self,
        position: Point<Pixels>,
        color: Hsla,
        pin: String,
        label: String,
        content: Div,
    ) -> AnyElement {
        deferred(
            anchored().position(position).snap_to_window().child(
                div()
                    .key_context(INPUT_CONTEXT)
                    .occlude()
                    .w(px(THREAD_WIDTH))
                    .flex()
                    .bg(theme::panel())
                    .border_1()
                    .border_color(theme::border())
                    .rounded_md()
                    .shadow_lg()
                    .overflow_hidden()
                    .font_family(theme::UI_FONT)
                    .text_sm()
                    .text_color(theme::text())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    .child(div().w(px(5.)).flex_none().bg(color))
                    .child(content.flex_1().min_w_0().p_2().flex().flex_col().gap_2())
                    .pin(pin)
                    .label(label),
            ),
        )
        .with_priority(1)
        .into_any_element()
    }

    fn render_thread(
        &self,
        tracked: &Tracked,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let note = &tracked.note;
        let id = note.id.clone();
        let open = note.is_open();
        let (first, last) = self.note_lines(tracked);
        let lines = if first == last {
            format!("line {}", first + 1)
        } else {
            format!("lines {}–{}", first + 1, last + 1)
        };
        let state = match tracked.state {
            AnchorState::Exact => None,
            AnchorState::Moved => Some("moved"),
            AnchorState::Fuzzy => Some("text changed"),
            AnchorState::Orphaned => Some("lines deleted"),
        };
        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .text_xs()
            .text_color(theme::muted())
            .child(div().child(format!("{} · {lines}", self.notes.file)))
            .children(state.map(|state| div().text_color(theme::warning()).child(state)))
            .child(div().flex_1())
            .child(
                Self::note_button(
                    format!("note-to-{id}"),
                    format!("→ {}", to_label(note.to.as_deref())),
                )
                .on_click(cx.listener({
                    let id = id.clone();
                    let to = next_to(note.to.as_deref());
                    move |this, _, _, cx| {
                        let (id, to) = (id.clone(), to.clone());
                        this.change_note(move |notes| notes.set_to(&id, to).map(drop), cx);
                    }
                }))
                .pin("to")
                .label("Addressed to"),
            )
            .child(
                Self::note_button(format!("note-close-{id}"), "✕")
                    .on_click(cx.listener(|this, _, window, cx| this.close_note(window, cx)))
                    .pin("close")
                    .label("Close"),
            );
        let editing = match &self.notes.input {
            Some(NoteInput {
                kind: InputKind::Edit(edit_id, index),
                ..
            }) if *edit_id == id => Some(*index),
            _ => None,
        };
        let messages = note.thread.iter().enumerate().map(|(index, message)| {
            let agent = message.author.kind == AuthorKind::Agent;
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(if agent {
                                    theme::accent_text()
                                } else {
                                    theme::text()
                                })
                                .child(format!(
                                    "{}{}",
                                    if agent { "◆ " } else { "" },
                                    message.author.name
                                )),
                        )
                        .child(div().text_color(theme::muted()).child(format!(
                            "{}{}",
                            ago(&message.time),
                            if message.edited.is_some() {
                                " (edited)"
                            } else {
                                ""
                            }
                        )))
                        .child(div().flex_1())
                        .when(!agent && editing != Some(index), |row| {
                            row.child(
                                Self::note_button(format!("note-edit-{id}-{index}"), "Edit")
                                    .on_click(cx.listener({
                                        let id = id.clone();
                                        let text = message.text.clone();
                                        move |this, _, window, cx| {
                                            let editor = this.new_note_input(&text, window, cx);
                                            this.notes.input = Some(NoteInput {
                                                kind: InputKind::Edit(id.clone(), index),
                                                editor,
                                            });
                                            cx.notify();
                                        }
                                    }))
                                    .pin("edit")
                                    .label("Edit message"),
                            )
                        }),
                )
                .child(
                    div()
                        .text_sm()
                        .whitespace_normal()
                        .child(selectable(("note-message", index), message.text.clone())),
                )
                .pin(format!("message:{}", index + 1))
        });
        let (input_label, submit) = match editing {
            Some(_) => ("Edit message", "Save"),
            None => ("Reply", "Reply"),
        };
        let input = self.notes.input.as_ref().map(|input| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_xs().text_color(theme::muted()).child(format!(
                    "{input_label} · Ctrl+Enter to send, Escape to close"
                )))
                .child(self.note_input_box(&input.editor))
                .pin("reply")
                .label(input_label)
        });
        let buttons =
            div()
                .flex()
                .gap_1()
                .child(
                    Self::note_button(format!("note-send-{id}"), submit)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.submit_note(&SubmitNote, window, cx)
                        }))
                        .pin("send")
                        .label(submit),
                )
                .child(div().flex_1())
                .child(if open {
                    Self::note_button(format!("note-resolve-{id}"), "Resolve")
                        .on_click(cx.listener({
                            let id = id.clone();
                            move |this, _, window, cx| {
                                let (id, by) = (id.clone(), this.note_author(cx));
                                this.change_note(move |notes| notes.resolve(&id, by).map(drop), cx);
                                this.close_note(window, cx);
                            }
                        }))
                        .pin("resolve")
                        .label("Resolve")
                } else {
                    Self::note_button(format!("note-reopen-{id}"), "Reopen")
                        .on_click(cx.listener({
                            let id = id.clone();
                            move |this, _, _, cx| {
                                let id = id.clone();
                                this.change_note(move |notes| notes.reopen(&id).map(drop), cx);
                            }
                        }))
                        .pin("reopen")
                        .label("Reopen")
                })
                .child(
                    Self::note_button(format!("note-delete-{id}"), "Delete")
                        .text_color(theme::error())
                        .on_click(cx.listener({
                            let id = id.clone();
                            move |this, _, window, cx| {
                                let id = id.clone();
                                this.change_note(move |notes| notes.delete(&id).map(drop), cx);
                                this.close_note(window, cx);
                            }
                        }))
                        .pin("delete")
                        .label("Delete"),
                );
        let content = div()
            .child(header)
            .child(
                div()
                    .id(SharedString::from(format!("note-thread-{id}")))
                    .max_h(px(320.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(messages),
            )
            .children(input)
            .child(buttons);
        self.note_popover(
            position,
            note_color(note.color.as_deref()),
            format!("note:{id}"),
            format!("Note thread: {}", note.text()),
            content,
        )
    }

    fn render_composer(&self, position: Point<Pixels>, cx: &mut Context<Self>) -> AnyElement {
        let Some(NoteInput {
            kind: InputKind::New { start, end, to, .. },
            editor,
        }) = &self.notes.input
        else {
            return div().into_any_element();
        };
        let (first, last) = (self.buffer.line_of(*start), self.buffer.line_of(*end));
        let lines = if first == last {
            format!("line {}", first + 1)
        } else {
            format!("lines {}–{}", first + 1, last + 1)
        };
        let content = div()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(format!("New note · {} · {lines}", self.notes.file))
                    .child(div().flex_1())
                    .child(
                        Self::note_button("note-new-to", format!("→ {}", to_label(to.as_deref())))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(NoteInput {
                                    kind: InputKind::New { to, .. },
                                    ..
                                }) = &mut this.notes.input
                                {
                                    *to = next_to(to.as_deref());
                                    cx.notify();
                                }
                            }))
                            .pin("to")
                            .label("Addressed to"),
                    ),
            )
            .child(self.note_input_box(editor))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::muted())
                            .child("Ctrl+Enter to add, Escape to cancel"),
                    )
                    .child(div().flex_1())
                    .child(
                        Self::note_button("note-new-add", "Add Note")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_note(&SubmitNote, window, cx)
                            }))
                            .pin("add")
                            .label("Add Note"),
                    )
                    .child(
                        Self::note_button("note-new-cancel", "Cancel")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_note(window, cx)),
                            )
                            .pin("cancel")
                            .label("Cancel"),
                    ),
            );
        self.note_popover(
            position,
            note_color(None),
            "new-note".into(),
            "New note".into(),
            content,
        )
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::{
        super::tests::{editor, type_text, update},
        *,
    };

    #[test]
    fn card_text_keeps_two_lines_and_marks_the_cut() {
        // Wraps after the last space within 10 bytes, like gpui's word wrap.
        let wrap = |line: &str| {
            let mut ends = Vec::new();
            let mut start = 0;
            while line.len() - start > 10 {
                let end = line[start..start + 10]
                    .rfind(' ')
                    .map_or(start + 10, |i| start + i + 1);
                ends.push(end);
                start = end;
            }
            ends
        };
        let lines = |text| clamp_lines(text, 2, wrap);
        assert_eq!(lines("short"), [("short".to_owned(), false)]);
        assert_eq!(
            lines("aaaa bbbb cccc dddd eeee"),
            [
                ("aaaa bbbb".to_owned(), false),
                ("cccc dddd".to_owned(), true)
            ]
        );
        // Exactly two lines: nothing cut.
        assert_eq!(
            lines("one\ntwo\n"),
            [("one".to_owned(), false), ("two".to_owned(), false)]
        );
        assert_eq!(
            lines("one\r\ntwo\nthree"),
            [("one".to_owned(), false), ("two".to_owned(), true)]
        );
    }

    #[test]
    fn cards_stack_downward_and_collapse_beside_long_code() {
        let short = |_: f32, _: f32| 100.;
        // Two notes on neighboring lines: the second goes under the first.
        let placed = place_cards(&[(0., false), (20., false)], 800., short);
        assert_eq!(placed[0].top, 0.);
        assert_eq!(placed[1].top, CARD_HEIGHT + CARD_GAP);
        assert!(
            placed
                .iter()
                .all(|p| !p.collapsed && p.width == CARD_MAX_WIDTH)
        );
        assert_eq!(placed[0].left, 800. - CARD_MAX_WIDTH);
        // Far apart: each at its own row.
        let placed = place_cards(&[(0., false), (400., false)], 800., short);
        assert_eq!(placed[1].top, 400.);
        // A long first line reaching the edge: the card on it becomes an
        // icon at the edge; the next card fits below it.
        let long_at_top = |top: f32, _: f32| if top < 20. { 780. } else { 100. };
        let placed = place_cards(&[(0., false), (20., false)], 800., long_at_top);
        assert!(placed[0].collapsed);
        assert_eq!(
            (placed[0].left, placed[0].width),
            (800. - ICON_WIDTH, ICON_WIDTH)
        );
        assert!(!placed[1].collapsed);
        assert_eq!(placed[1].top, 20f32.max(ICON_HEIGHT + CARD_GAP));
        // Narrower space shrinks cards down to the minimum width.
        let placed = place_cards(&[(0., false)], 800., |_, _| 800. - CODE_GAP - 200.);
        assert_eq!(placed[0].width, 200.);
        // Collapsed by the person: an icon even with room.
        let placed = place_cards(&[(0., true), (0., false)], 800., short);
        assert!(placed[0].collapsed);
        assert_eq!(placed[1].top, ICON_HEIGHT + CARD_GAP);
    }

    fn note_on(text: &str, start_line: usize, end_line: usize) -> Note {
        let mut notes = ::notes::Notes::default();
        notes
            .add(
                "a.rs",
                None,
                ::notes::anchor_from(text, start_line, end_line, None),
                "check this",
                Author::user("me"),
                Some("agent".into()),
            )
            .clone()
    }

    fn track(window: gpui::WindowHandle<Editor>, note: Note, cx: &mut TestAppContext) {
        update(window, cx, |editor, _, _| {
            let text = editor.text();
            let tracked = editor.anchor_note(note, &text);
            editor.notes.tracked.push(tracked);
        });
    }

    /// The note's lines (1-based) and state.
    fn where_is(
        window: gpui::WindowHandle<Editor>,
        cx: &mut TestAppContext,
    ) -> (usize, usize, AnchorState, bool) {
        window
            .read_with(cx, |editor, _| {
                let fact = &editor.note_facts()[0];
                (
                    fact.start_line,
                    fact.end_line,
                    fact.state,
                    editor.notes.tracked[0].dirty,
                )
            })
            .unwrap()
    }

    const TEXT: &str = "fn a() {}\nfn b() {\n    1\n}\nfn c() {}\n";

    #[gpui::test]
    fn a_note_moves_with_lines_inserted_above(cx: &mut TestAppContext) {
        let window = editor("a.rs", &format!("|{TEXT}"), cx);
        track(window, note_on(TEXT, 2, 4), cx);
        assert_eq!(where_is(window, cx), (2, 4, AnchorState::Exact, false));
        type_text(window, "// one\n// two\n", cx);
        assert_eq!(where_is(window, cx), (4, 6, AnchorState::Exact, true));
        // Deleting a line above moves it back up.
        update(window, cx, |editor, window, cx| {
            editor.select_text_range(0, "// one\n".len(), cx);
            editor.backspace(&Backspace, window, cx);
        });
        assert_eq!(where_is(window, cx).0, 3);
    }

    #[gpui::test]
    fn a_note_stays_on_lines_edited_inside(cx: &mut TestAppContext) {
        let marked = TEXT.replacen("    1", "    1|", 1);
        let window = editor("a.rs", &marked, cx);
        track(window, note_on(TEXT, 2, 4), cx);
        type_text(window, " + 2", cx);
        assert_eq!(where_is(window, cx), (2, 4, AnchorState::Exact, true));
        // A line break inside grows the anchored range.
        type_text(window, "\n", cx);
        assert_eq!(where_is(window, cx).0..where_is(window, cx).1, 2..5);
        // Typing below the note leaves it alone.
        let window = editor("a.rs", &TEXT.replacen("fn c() {}", "fn c() {}|", 1), cx);
        track(window, note_on(TEXT, 2, 4), cx);
        type_text(window, " // end", cx);
        assert_eq!(where_is(window, cx), (2, 4, AnchorState::Exact, false));
    }

    #[gpui::test]
    fn deleting_the_anchored_lines_orphans_the_note(cx: &mut TestAppContext) {
        let window = editor("a.rs", &format!("|{TEXT}"), cx);
        track(window, note_on(TEXT, 2, 4), cx);
        let start = TEXT.find("fn b").unwrap();
        let end = TEXT.find("fn c").unwrap();
        update(window, cx, |editor, window, cx| {
            editor.select_text_range(start, end, cx);
            editor.backspace(&Backspace, window, cx);
        });
        assert_eq!(where_is(window, cx).2, AnchorState::Orphaned);
        // Orphaned notes are not drawn, nor followed further.
        type_text(window, "x", cx);
        assert_eq!(where_is(window, cx).2, AnchorState::Orphaned);
        // Undo brings the lines back: the next refresh finds the note again.
        update(window, cx, |editor, window, cx| {
            editor.undo(&Undo, window, cx);
            editor.undo(&Undo, window, cx);
            editor.refresh_note_positions(cx);
        });
        assert_eq!(where_is(window, cx), (2, 4, AnchorState::Exact, false));
    }

    #[gpui::test]
    fn deleting_part_of_the_anchored_lines_keeps_the_rest(cx: &mut TestAppContext) {
        let window = editor("a.rs", &format!("|{TEXT}"), cx);
        track(window, note_on(TEXT, 2, 4), cx);
        let start = TEXT.find("fn b").unwrap();
        let end = TEXT.find("    1").unwrap();
        update(window, cx, |editor, window, cx| {
            editor.select_text_range(start, end, cx);
            editor.backspace(&Backspace, window, cx);
        });
        assert_eq!(where_is(window, cx), (2, 3, AnchorState::Exact, true));
    }

    /// Moved anchors reach notes.json only with a save; discarded edits and a
    /// closed editor leave the stored anchor in charge.
    #[gpui::test]
    fn anchors_are_written_on_save_and_dropped_on_discard(cx: &mut TestAppContext) {
        let root = std::env::temp_dir().join(format!("nc-notes-save-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("a.rs");
        std::fs::write(&path, TEXT).unwrap();
        let disk = ::notes::Store::open(root.clone());
        let id = disk
            .add(
                "a.rs",
                None,
                ::notes::anchor_from(TEXT, 2, 4, None),
                "check",
                Author::user("me"),
                None,
            )
            .unwrap()
            .id;
        let stored_line = || disk.load().unwrap().get(&id).unwrap().anchor.start_line;
        let project = crate::project::Project::local(root.clone());
        let store = cx.new(|cx| NoteStore::new(project.clone(), cx));
        cx.run_until_parked();
        let window = cx.add_window(|_, cx| {
            Editor::for_file(Buffer::with_path(TEXT, path.clone()), project, cx)
        });
        update(window, cx, |editor, _, cx| {
            editor.set_note_store(store.clone(), None, cx)
        });
        let live = |cx: &mut TestAppContext| store.read_with(cx, |s, _| s.live_position(0, &id));
        assert_eq!(live(cx), Some((2, AnchorState::Exact)));

        type_text(window, "// one\n", cx);
        cx.executor().advance_clock(SAVE_DELAY * 2);
        cx.run_until_parked();
        assert_eq!(live(cx), Some((3, AnchorState::Exact)));
        assert_eq!(stored_line(), 2, "unsaved edits are not written");

        update(window, cx, |editor, _, cx| assert!(editor.save(cx)));
        cx.run_until_parked();
        assert_eq!(stored_line(), 3);

        // Discarded: back to the file, and to its anchor.
        type_text(window, "// two\n", cx);
        cx.executor().advance_clock(SAVE_DELAY * 2);
        cx.run_until_parked();
        assert_eq!(live(cx), Some((4, AnchorState::Exact)));
        update(window, cx, |editor, _, cx| {
            editor.reload_from_disk(true, cx);
        });
        cx.run_until_parked();
        assert_eq!(where_is(window, cx), (3, 5, AnchorState::Exact, false));
        assert_eq!(live(cx), Some((3, AnchorState::Exact)));
        assert_eq!(stored_line(), 3);

        // Closing the editor forgets its positions.
        type_text(window, "// three\n", cx);
        cx.executor().advance_clock(SAVE_DELAY * 2);
        cx.run_until_parked();
        assert_eq!(live(cx), Some((4, AnchorState::Exact)));
        window
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        assert_eq!(live(cx), None);
        assert_eq!(stored_line(), 3);
        std::fs::remove_dir_all(&root).ok();
    }
}
