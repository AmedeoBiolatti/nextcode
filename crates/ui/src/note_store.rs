//! The project's sticky notes as the app holds them: one
//! `.next_code/notes.json` per project folder, read and changed on
//! background threads through the project's file API (so WSL and container
//! projects work too), reloaded when the watcher reports the file. Editors
//! and the Notes panel read it; an agent's new note or reply for the person
//! raises [`NoteStoreEvent::Incoming`].

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use gpui::{AppContext, Context, EntityId, EventEmitter, SharedString, Task};
use notes::{AnchorState, Author, AuthorKind, NOTES_FILE, Note, Notes, NotesFs, Store};

use crate::project::Project;

pub struct NoteStore {
    project: Project,
    roots: Vec<RootNotes>,
    /// Bumped whenever any folder's notes change.
    generation: u64,
    /// The person, as the author of what the app writes.
    user: Author,
    /// Resolved notes show in editors.
    show_resolved: bool,
    error: Option<SharedString>,
    /// Where open editors show notes now (unsaved edits included), by
    /// (folder, id): the publishing editor, 1-based first line and state.
    live: HashMap<(usize, String), (EntityId, usize, AnchorState)>,
}

struct RootNotes {
    store: Store,
    /// The notes file as watcher events name it.
    path: PathBuf,
    notes: Notes,
    loaded: bool,
    loading: bool,
    /// The file changed again while it was being read.
    reload_again: bool,
}

pub enum NoteStoreEvent {
    /// An agent wrote to the person (a note for them, or a reply) while
    /// the app was open.
    Incoming(Incoming),
    /// Reading or writing the notes failed.
    Error(SharedString),
    /// The person opened or answered note `id` of folder `root`.
    Seen { root: usize, id: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Incoming {
    pub root: usize,
    pub id: String,
    pub file: String,
    /// 1-based, as stored.
    pub line: usize,
    pub author: String,
    pub text: String,
    pub reply: bool,
}

impl EventEmitter<NoteStoreEvent> for NoteStore {}

/// The person's name on notes they write.
fn user_name() -> String {
    ["NEXT_CODE_USER_NAME", "USERNAME", "USER"]
        .iter()
        .find_map(|key| {
            std::env::var(key)
                .ok()
                .filter(|name| !name.trim().is_empty())
        })
        .unwrap_or_else(|| "user".to_owned())
}

impl NoteStore {
    pub fn new(project: Project, cx: &mut Context<Self>) -> Self {
        let roots = (0..project.roots().len())
            .map(|index| {
                let root = project.roots()[index].path.clone();
                let store = if project.is_remote() {
                    Store::with_fs(Arc::new(ProjectFs {
                        project: project.clone(),
                        root: root.clone(),
                    }))
                } else {
                    Store::open(root.clone())
                };
                RootNotes {
                    store,
                    path: project.join(&root, NOTES_FILE),
                    notes: Notes::default(),
                    loaded: false,
                    loading: false,
                    reload_again: false,
                }
            })
            .collect();
        let mut this = Self {
            project,
            roots,
            generation: 0,
            user: Author::user(user_name()),
            show_resolved: false,
            error: None,
            live: HashMap::new(),
        };
        for root in 0..this.roots.len() {
            this.reload(root, cx);
        }
        this
    }

    pub fn project(&self) -> &Project {
        &self.project
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn user(&self) -> &Author {
        &self.user
    }

    /// Where an open editor shows note `id` of folder `root` now.
    pub fn live_position(&self, root: usize, id: &str) -> Option<(usize, AnchorState)> {
        let (_, line, state) = self.live.get(&(root, id.to_owned()))?;
        Some((*line, *state))
    }

    /// Editor `editor`'s current note positions (1-based first line, state).
    pub fn set_live_positions(
        &mut self,
        editor: EntityId,
        root: usize,
        positions: Vec<(String, usize, AnchorState)>,
        cx: &mut Context<Self>,
    ) {
        let mut changed = false;
        for (id, line, state) in positions {
            let value = (editor, line, state);
            changed |= self.live.insert((root, id), value) != Some(value);
        }
        if changed {
            cx.notify();
        }
    }

    /// Forgets what `editor` published (it closed): stored anchors apply again.
    pub fn clear_live_positions(&mut self, editor: EntityId, cx: &mut Context<Self>) {
        let before = self.live.len();
        self.live.retain(|_, (owner, ..)| *owner != editor);
        if self.live.len() != before {
            cx.notify();
        }
    }

    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    pub fn show_resolved(&self) -> bool {
        self.show_resolved
    }

    pub fn set_show_resolved(&mut self, show: bool, cx: &mut Context<Self>) {
        if self.show_resolved != show {
            self.show_resolved = show;
            self.generation += 1;
            cx.notify();
        }
    }

    /// Every note with its folder's index.
    pub fn all(&self) -> impl Iterator<Item = (usize, &Note)> {
        self.roots
            .iter()
            .enumerate()
            .flat_map(|(root, notes)| notes.notes.notes.iter().map(move |note| (root, note)))
    }

    /// The notes of `file` (relative to folder `root`).
    pub fn file_notes<'a>(&'a self, root: usize, file: &'a str) -> impl Iterator<Item = &'a Note> {
        self.roots
            .get(root)
            .into_iter()
            .flat_map(move |notes| notes.notes.for_file(file))
    }

    /// Open notes waiting for the person (the panel's badge).
    pub fn awaiting_user(&self) -> usize {
        self.all()
            .filter(|(_, note)| awaits(note, &self.user))
            .count()
    }

    /// Re-reads the folders whose notes file is among `paths` (all on a
    /// rescan).
    pub fn files_changed<'a>(
        &mut self,
        paths: impl IntoIterator<Item = &'a Path>,
        rescan: bool,
        cx: &mut Context<Self>,
    ) {
        let mut due: Vec<usize> = if rescan {
            (0..self.roots.len()).collect()
        } else {
            Vec::new()
        };
        for path in paths {
            if let Some(root) = self.roots.iter().position(|root| root.path == path) {
                due.push(root);
            }
        }
        due.sort_unstable();
        due.dedup();
        for root in due {
            self.reload(root, cx);
        }
    }

    fn reload(&mut self, root: usize, cx: &mut Context<Self>) {
        let Some(notes) = self.roots.get_mut(root) else {
            return;
        };
        if notes.loading {
            notes.reload_again = true;
            return;
        }
        notes.loading = true;
        let store = notes.store.clone();
        cx.spawn(async move |this, cx| {
            let loaded = cx.background_spawn(async move { store.load() }).await;
            this.update(cx, |this, cx| {
                let notes = &mut this.roots[root];
                notes.loading = false;
                let again = std::mem::take(&mut notes.reload_again);
                match loaded {
                    Ok(loaded) => this.apply(root, loaded, cx),
                    Err(err) => this.fail(format!("Notes: {err}"), cx),
                }
                if again {
                    this.reload(root, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Takes `notes` as folder `root`'s current notes, telling about an
    /// agent's new messages for the person (not on the first read).
    fn apply(&mut self, root: usize, notes: Notes, cx: &mut Context<Self>) {
        let current = &mut self.roots[root];
        if current.loaded && current.notes == notes {
            return;
        }
        let incoming = if current.loaded {
            incoming(&current.notes, &notes, &self.user, root)
        } else {
            Vec::new()
        };
        current.notes = notes;
        current.loaded = true;
        self.error = None;
        self.generation += 1;
        for incoming in incoming {
            cx.emit(NoteStoreEvent::Incoming(incoming));
        }
        cx.notify();
    }

    fn fail(&mut self, message: String, cx: &mut Context<Self>) {
        let message: SharedString = message.into();
        self.error = Some(message.clone());
        cx.emit(NoteStoreEvent::Error(message));
        cx.notify();
    }

    /// Applies `change` to folder `root`'s notes as they are on disk (under
    /// the lock, merged with other writers) and shows the result. Errors
    /// are reported as [`NoteStoreEvent::Error`] too.
    pub fn change<R: Send + 'static>(
        &mut self,
        root: usize,
        change: impl FnOnce(&mut Notes) -> io::Result<R> + Send + 'static,
        cx: &mut Context<Self>,
    ) -> Task<io::Result<R>> {
        let Some(store) = self.roots.get(root).map(|notes| notes.store.clone()) else {
            return Task::ready(Err(io::Error::other("no such project folder")));
        };
        cx.spawn(async move |this, cx| {
            let written = cx
                .background_spawn(async move {
                    store.update(|notes| {
                        let result = change(notes)?;
                        Ok((result, notes.clone()))
                    })
                })
                .await;
            this.update(cx, |this, cx| match written {
                Ok((result, notes)) => {
                    this.apply(root, notes, cx);
                    Ok(result)
                }
                Err(err) => {
                    this.fail(format!("Notes: {err}"), cx);
                    Err(err)
                }
            })
            .unwrap_or_else(|_| Err(io::Error::other("the project closed")))
        })
    }

    /// The person opened or answered a note: its announcement goes.
    pub fn note_seen(&mut self, root: usize, id: &str, cx: &mut Context<Self>) {
        cx.emit(NoteStoreEvent::Seen {
            root,
            id: id.to_owned(),
        });
    }
}

/// Whether `note` waits for the person: open, addressed to them (or to
/// anyone, by an agent), and its last message is not theirs.
pub fn awaits(note: &Note, user: &Author) -> bool {
    if !note.is_open() {
        return false;
    }
    let addressed = match note.to.as_deref() {
        Some(to) => user.is_addressed_by(Some(to)),
        None => note
            .thread
            .first()
            .is_some_and(|message| message.author.kind == AuthorKind::Agent),
    };
    addressed
        && note
            .thread
            .last()
            .is_some_and(|message| message.author.kind != AuthorKind::User)
}

/// Agents' messages in `new` that `old` did not have and that are for the
/// person: replies on any note, and new notes addressed to them (or to
/// anyone).
fn incoming(old: &Notes, new: &Notes, user: &Author, root: usize) -> Vec<Incoming> {
    let mut found = Vec::new();
    for note in &new.notes {
        let seen = old.get(&note.id).map_or(0, |old| old.thread.len());
        for (index, message) in note.thread.iter().enumerate().skip(seen) {
            if message.author.kind != AuthorKind::Agent {
                continue;
            }
            if index == 0 && !user.is_addressed_by(note.to.as_deref()) {
                continue;
            }
            found.push(Incoming {
                root,
                id: note.id.clone(),
                file: note.file.clone(),
                line: note.anchor.start_line,
                author: message.author.name.clone(),
                text: message.text.clone(),
                reply: index > 0,
            });
        }
    }
    found
}

/// [`NotesFs`] through the project's file API (a WSL or container project:
/// the agent does the file work).
struct ProjectFs {
    project: Project,
    root: PathBuf,
}

impl ProjectFs {
    fn path(&self, path: &str) -> PathBuf {
        self.project.join(&self.root, path)
    }

    fn missing(&self, path: &Path) -> bool {
        matches!(self.project.kind(path), Ok(None))
    }
}

impl NotesFs for ProjectFs {
    fn read(&self, path: &str) -> io::Result<Option<String>> {
        let path = self.path(path);
        match self.project.read_bytes(&path) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
            Err(_) if self.missing(&path) => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn write(&self, path: &str, data: &[u8]) -> io::Result<()> {
        self.project.write(&self.path(path), data.to_vec())
    }

    fn create_new(&self, path: &str, data: &[u8]) -> io::Result<bool> {
        let full = self.path(path);
        let target = full.to_string_lossy().into_owned();
        // `set -C`: the shell refuses to overwrite, atomically.
        let output = self.project.run_program(
            "/bin/sh",
            &[
                "-c".into(),
                "set -C; cat > \"$1\"".into(),
                "sh".into(),
                target,
            ],
            &[],
            Some(data),
            std::time::Duration::from_secs(20),
        )?;
        if output.code == Some(0) {
            return Ok(true);
        }
        if !self.missing(&full) {
            return Ok(false);
        }
        Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.project.rename(&self.path(from), &self.path(to))
    }

    fn remove(&self, path: &str) -> io::Result<()> {
        let path = self.path(path);
        if self.missing(&path) {
            return Ok(());
        }
        self.project.remove(&path, false)
    }

    fn create_dir_all(&self, path: &str) -> io::Result<()> {
        self.project.create_dir_all(&self.path(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notes_with(thread: &[(AuthorKind, &str)], to: Option<&str>) -> Notes {
        let mut notes = Notes::default();
        let author = |kind: AuthorKind| match kind {
            AuthorKind::User => Author::user("me"),
            AuthorKind::Agent => Author::agent("claude"),
        };
        let (kind, text) = thread[0];
        let id = notes
            .add(
                "a.rs",
                None,
                notes::anchor_from("x\n", 1, 1, None),
                text,
                author(kind),
                to.map(str::to_owned),
            )
            .id
            .clone();
        for &(kind, text) in &thread[1..] {
            notes.reply(&id, text, author(kind)).unwrap();
        }
        notes
    }

    #[test]
    fn agents_notes_and_replies_for_the_person_are_incoming() {
        let user = Author::user("me");
        let empty = Notes::default();
        let for_user = notes_with(&[(AuthorKind::Agent, "look")], Some("user"));
        let found = incoming(&empty, &for_user, &user, 0);
        assert_eq!(found.len(), 1);
        assert!(!found[0].reply);
        assert_eq!(found[0].text, "look");

        // Between agents: not for the person.
        let for_agent = notes_with(&[(AuthorKind::Agent, "todo")], Some("agent"));
        assert!(incoming(&empty, &for_agent, &user, 0).is_empty());

        // The person's own note, then an agent's reply.
        let asked = notes_with(&[(AuthorKind::User, "why?")], Some("agent"));
        assert!(incoming(&empty, &asked, &user, 0).is_empty());
        let mut answered = asked.clone();
        let id = answered.notes[0].id.clone();
        answered
            .reply(&id, "because", Author::agent("claude"))
            .unwrap();
        let found = incoming(&asked, &answered, &user, 0);
        assert_eq!(found.len(), 1);
        assert!(found[0].reply);
        assert_eq!(found[0].author, "claude");
    }

    #[test]
    fn notes_wait_for_the_person_until_they_answer() {
        let user = Author::user("me");
        let note = |thread: &[(AuthorKind, &str)], to| notes_with(thread, to).notes.remove(0);
        assert!(awaits(
            &note(&[(AuthorKind::Agent, "a")], Some("user")),
            &user
        ));
        assert!(awaits(&note(&[(AuthorKind::Agent, "a")], None), &user));
        assert!(awaits(
            &note(&[(AuthorKind::Agent, "a")], Some("me")),
            &user
        ));
        assert!(!awaits(&note(&[(AuthorKind::User, "a")], None), &user));
        assert!(!awaits(
            &note(&[(AuthorKind::User, "a")], Some("agent")),
            &user
        ));
        let mut resolved = note(&[(AuthorKind::Agent, "a")], Some("user"));
        resolved.status = notes::Status::Resolved;
        assert!(!awaits(&resolved, &user));
        // Answered by the person: no longer waiting; an agent's next reply
        // makes it wait again.
        let answered = &[(AuthorKind::Agent, "a"), (AuthorKind::User, "b")];
        assert!(!awaits(&note(answered, Some("user")), &user));
        assert!(!awaits(&note(answered, None), &user));
        let again = &[
            (AuthorKind::Agent, "a"),
            (AuthorKind::User, "b"),
            (AuthorKind::Agent, "c"),
        ];
        assert!(awaits(&note(again, Some("user")), &user));
    }
}
