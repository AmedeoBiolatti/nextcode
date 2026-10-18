//! Sticky notes between a person and agents, anchored to lines of project
//! files: the model, `<root>/.next_code/notes.json` storage ([`Store`]),
//! anchoring that follows edits ([`reanchor`]) and the `notes` command line
//! shared by `next_code` and `next_code_agent` ([`cli`]). No GUI code.

mod anchor;
pub mod cli;
mod store;
mod time;

use std::{
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hasher},
    io,
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

pub use anchor::{Anchor, AnchorState, Anchored, anchor_from, reanchor};
pub use store::{EXCLUDE_PATTERN, LOCK_FILE, LocalFs, NOTES_FILE, NotesFs, Store};
pub use time::{format_time, now, now_ms, parse_time};

/// The file format's version; files from a newer NextCode are not written.
pub const VERSION: u32 = 1;

/// All notes of one project root: the whole `notes.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notes {
    pub version: u32,
    #[serde(default)]
    pub notes: Vec<Note>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    /// Short and unique within the file, e.g. `n7f3a`.
    pub id: String,
    /// Root-relative, `/`-separated.
    pub file: String,
    /// Notebook cell (1-based); the anchor's lines are then the cell's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell: Option<usize>,
    pub anchor: Anchor,
    #[serde(default)]
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<Author>,
    /// Who should act: `"agent"`, `"user"` or a name; `None`: anyone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// RFC 3339 (UTC), as all times here.
    pub created: String,
    /// Any change: a message, the status, the anchor.
    pub updated: String,
    /// The first message is the note itself.
    pub thread: Vec<Message>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Open,
    Resolved,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub author: Author,
    /// Markdown.
    pub text: String,
    pub time: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    pub kind: AuthorKind,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthorKind {
    User,
    Agent,
}

impl Author {
    pub fn user(name: impl Into<String>) -> Self {
        Self {
            kind: AuthorKind::User,
            name: name.into(),
        }
    }

    pub fn agent(name: impl Into<String>) -> Self {
        Self {
            kind: AuthorKind::Agent,
            name: name.into(),
        }
    }

    /// Whether a note's `to` addresses this author: `None` (anyone), the
    /// author's kind (`"user"`/`"agent"`) or name.
    pub fn is_addressed_by(&self, to: Option<&str>) -> bool {
        match to {
            None => true,
            Some(to) => {
                to == self.name
                    || to
                        == match self.kind {
                            AuthorKind::User => "user",
                            AuthorKind::Agent => "agent",
                        }
            }
        }
    }
}

impl Note {
    pub fn is_open(&self) -> bool {
        self.status == Status::Open
    }

    /// The newest message's time (for "new since" checks; anchor moves
    /// change `updated` but not this).
    pub fn last_time(&self) -> &str {
        self.thread
            .iter()
            .map(|message| message.time.as_str())
            .max_by_key(|time| parse_time(time).unwrap_or(0))
            .unwrap_or(&self.created)
    }

    /// The first message's text.
    pub fn text(&self) -> &str {
        self.thread.first().map_or("", |message| &message.text)
    }

    /// Where the note is in `text` (the file's, or the cell's source for a
    /// cell note) now.
    pub fn reanchor(&self, text: &str) -> Anchored {
        reanchor(&self.anchor, text)
    }
}

impl Default for Notes {
    fn default() -> Self {
        Self {
            version: VERSION,
            notes: Vec::new(),
        }
    }
}

fn not_found(id: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("no note {id:?}"))
}

impl Notes {
    /// Reads a `notes.json`; empty text is no notes.
    pub fn parse(text: &str) -> io::Result<Self> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let notes: Self = serde_json::from_str(text).map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{NOTES_FILE} is not valid notes JSON: {err}"),
            )
        })?;
        if notes.version > VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{NOTES_FILE} has version {}, newer than this build understands ({VERSION})",
                    notes.version
                ),
            ));
        }
        Ok(notes)
    }

    /// Pretty JSON with a final newline.
    pub fn to_json(&self) -> String {
        let mut json = serde_json::to_string_pretty(self).expect("notes serialize");
        json.push('\n');
        json
    }

    pub fn get(&self, id: &str) -> Option<&Note> {
        self.notes.iter().find(|note| note.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Note> {
        self.notes.iter_mut().find(|note| note.id == id)
    }

    /// The note with this id, or the only one whose id starts with it.
    pub fn find(&self, id: &str) -> io::Result<&Note> {
        if let Some(note) = self.get(id) {
            return Ok(note);
        }
        let mut matches = self.notes.iter().filter(|note| note.id.starts_with(id));
        match (matches.next(), matches.next()) {
            (Some(note), None) if !id.is_empty() => Ok(note),
            (Some(_), Some(_)) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{id:?} matches several notes"),
            )),
            _ => Err(not_found(id)),
        }
    }

    fn note_mut(&mut self, id: &str) -> io::Result<&mut Note> {
        self.get_mut(id).ok_or_else(|| not_found(id))
    }

    /// The notes on `file` (root-relative).
    pub fn for_file<'a>(&'a self, file: &'a str) -> impl Iterator<Item = &'a Note> + 'a {
        self.notes.iter().filter(move |note| note.file == file)
    }

    /// Adds an open note; its text is the thread's first message.
    pub fn add(
        &mut self,
        file: &str,
        cell: Option<usize>,
        anchor: Anchor,
        text: &str,
        author: Author,
        to: Option<String>,
    ) -> &Note {
        let time = now();
        let id = self.new_id();
        self.notes.push(Note {
            id,
            file: file.replace('\\', "/"),
            cell,
            anchor,
            status: Status::Open,
            resolved_by: None,
            to,
            color: None,
            created: time.clone(),
            updated: time.clone(),
            thread: vec![Message {
                author,
                text: text.to_owned(),
                time,
                edited: None,
            }],
        });
        self.notes.last().expect("just pushed")
    }

    pub fn reply(&mut self, id: &str, text: &str, author: Author) -> io::Result<&Note> {
        let note = self.note_mut(id)?;
        let time = now();
        note.thread.push(Message {
            author,
            text: text.to_owned(),
            time: time.clone(),
            edited: None,
        });
        note.updated = time;
        Ok(note)
    }

    pub fn resolve(&mut self, id: &str, by: Author) -> io::Result<&Note> {
        let note = self.note_mut(id)?;
        note.status = Status::Resolved;
        note.resolved_by = Some(by);
        note.updated = now();
        Ok(note)
    }

    pub fn reopen(&mut self, id: &str) -> io::Result<&Note> {
        let note = self.note_mut(id)?;
        note.status = Status::Open;
        note.resolved_by = None;
        note.updated = now();
        Ok(note)
    }

    /// Replaces message `index` (0: the note itself).
    pub fn edit_message(&mut self, id: &str, index: usize, text: &str) -> io::Result<&Note> {
        let note = self.note_mut(id)?;
        let time = now();
        let message = note.thread.get_mut(index).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("note {id:?} has no message {index}"),
            )
        })?;
        message.text = text.to_owned();
        message.edited = Some(time.clone());
        note.updated = time;
        Ok(note)
    }

    pub fn set_to(&mut self, id: &str, to: Option<String>) -> io::Result<&Note> {
        let note = self.note_mut(id)?;
        note.to = to;
        note.updated = now();
        Ok(note)
    }

    pub fn set_color(&mut self, id: &str, color: Option<String>) -> io::Result<&Note> {
        let note = self.note_mut(id)?;
        note.color = color;
        note.updated = now();
        Ok(note)
    }

    pub fn delete(&mut self, id: &str) -> io::Result<Note> {
        let index = self
            .notes
            .iter()
            .position(|note| note.id == id)
            .ok_or_else(|| not_found(id))?;
        Ok(self.notes.remove(index))
    }

    /// Moves the note's anchor (the app, as the person edits the file).
    /// Unchanged anchors are left alone, `updated` included.
    pub fn update_anchor(&mut self, id: &str, anchor: Anchor) -> io::Result<&Note> {
        let note = self.note_mut(id)?;
        if note.anchor != anchor {
            note.anchor = anchor;
            note.updated = now();
        }
        Ok(note)
    }

    fn new_id(&self) -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut digits = 4;
        loop {
            for _ in 0..16 {
                let mut hasher = RandomState::new().build_hasher();
                hasher.write_u64(now_ms());
                hasher.write_u32(std::process::id());
                hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
                let id = format!("n{:016x}", hasher.finish());
                let id = &id[..=digits];
                if self.get(id).is_none() {
                    return id.to_owned();
                }
            }
            digits += 2;
        }
    }
}

#[cfg(test)]
mod tests;
