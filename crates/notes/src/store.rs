//! `<root>/.next_code/notes.json`: read, and changed by read-modify-write
//! under a lock file. The app and every CLI (Windows, WSL via /mnt, dev
//! containers via bind mounts) share it, so each change re-reads the file
//! under the lock and writes a temporary file renamed over it: concurrent
//! writers never lose each other's changes and readers never see half a
//! file.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{Anchor, Author, Note, Notes, now_ms};

/// Root-relative paths, `/`-separated.
pub const NOTES_FILE: &str = ".next_code/notes.json";
pub const LOCK_FILE: &str = ".next_code/notes.json.lock";
/// Added to `.git/info/exclude` (never `.gitignore`): notes stay local.
pub const EXCLUDE_PATTERN: &str = "/.next_code/notes.json*";
const DIR: &str = ".next_code";

/// A writer holds the lock for milliseconds; one older than this crashed.
const STALE_LOCK: Duration = Duration::from_secs(10);
/// Gives up waiting for the lock after this.
const LOCK_WAIT: Duration = Duration::from_secs(30);

/// File access relative to a project root (`/`-separated paths), so the app
/// can reach remote projects through its own file API.
pub trait NotesFs: Send + Sync {
    /// `None`: no such file.
    fn read(&self, path: &str) -> io::Result<Option<String>>;
    fn write(&self, path: &str, data: &[u8]) -> io::Result<()>;
    /// Creates `path` only if it does not exist; `false` if it did.
    fn create_new(&self, path: &str, data: &[u8]) -> io::Result<bool>;
    /// Replaces `to` if it exists.
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;
    /// A missing file is not an error.
    fn remove(&self, path: &str) -> io::Result<()>;
    fn create_dir_all(&self, path: &str) -> io::Result<()>;
}

/// [`NotesFs`] on this machine's file system.
pub struct LocalFs {
    root: PathBuf,
}

impl LocalFs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, path: &str) -> PathBuf {
        let mut full = self.root.clone();
        full.extend(path.split('/').filter(|part| !part.is_empty()));
        full
    }
}

impl NotesFs for LocalFs {
    fn read(&self, path: &str) -> io::Result<Option<String>> {
        match fs::read_to_string(self.path(path)) {
            Ok(text) => Ok(Some(text)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn write(&self, path: &str, data: &[u8]) -> io::Result<()> {
        fs::write(self.path(path), data)
    }

    fn create_new(&self, path: &str, data: &[u8]) -> io::Result<bool> {
        use std::io::Write as _;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.path(path))
        {
            Ok(mut file) => match file.write_all(data) {
                Ok(()) => Ok(true),
                Err(err) => {
                    // Not ours to keep half-written.
                    drop(file);
                    fs::remove_file(self.path(path)).ok();
                    Err(err)
                }
            },
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(err) => Err(err),
        }
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        fs::rename(self.path(from), self.path(to))
    }

    fn remove(&self, path: &str) -> io::Result<()> {
        match fs::remove_file(self.path(path)) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        }
    }

    fn create_dir_all(&self, path: &str) -> io::Result<()> {
        fs::create_dir_all(self.path(path))
    }
}

/// One project root's notes.
#[derive(Clone)]
pub struct Store {
    fs: Arc<dyn NotesFs>,
}

impl Store {
    /// The notes of the project at `root` on this machine.
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self::with_fs(Arc::new(LocalFs::new(root)))
    }

    pub fn with_fs(fs: Arc<dyn NotesFs>) -> Self {
        Self { fs }
    }

    /// No file: no notes. A file that does not parse is an error (and is
    /// never overwritten).
    pub fn load(&self) -> io::Result<Notes> {
        self.read().map(|(_, notes)| notes)
    }

    /// The file's text and notes. Reading while another process renames a
    /// new version over it can fail or see an empty file on some file
    /// systems (WSL's /mnt drives: ENODATA), and a writer never leaves an
    /// empty or partial file: such reads are retried for a while.
    fn read(&self) -> io::Result<(Option<String>, Notes)> {
        let mut attempt = 0;
        loop {
            let result = self.fs.read(NOTES_FILE).and_then(|text| {
                if text.as_deref().is_some_and(|text| text.trim().is_empty()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{NOTES_FILE} is empty"),
                    ));
                }
                let notes = Notes::parse(text.as_deref().unwrap_or_default())?;
                Ok((text, notes))
            });
            match result {
                Err(_) if attempt < 40 => {
                    attempt += 1;
                    thread::sleep(Duration::from_millis(25));
                }
                // Created empty by hand: no notes.
                Err(err) if err.kind() == io::ErrorKind::InvalidData => {
                    return match self.fs.read(NOTES_FILE)? {
                        Some(text) if text.trim().is_empty() => Ok((Some(text), Notes::default())),
                        _ => Err(err),
                    };
                }
                result => return result,
            }
        }
    }

    /// Applies `change` to the notes as they are on disk now, under the
    /// lock, and writes them back if they changed. An error from `change`
    /// writes nothing.
    pub fn update<R>(&self, change: impl FnOnce(&mut Notes) -> io::Result<R>) -> io::Result<R> {
        self.fs.create_dir_all(DIR)?;
        let _lock = Lock::acquire(&*self.fs)?;
        let (old, mut notes) = self.read()?;
        let result = change(&mut notes)?;
        let json = notes.to_json();
        if old.as_deref() != Some(json.as_str()) {
            if old.is_none() {
                // First notes of this project: keep them out of git.
                self.ensure_excluded()?;
            }
            self.replace(&json)?;
        }
        Ok(result)
    }

    fn replace(&self, json: &str) -> io::Result<()> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let temp = format!(
            "{NOTES_FILE}.{}-{}.tmp",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        self.fs.write(&temp, json.as_bytes())?;
        // Windows refuses while a scanner or sync client has the file open
        // without delete sharing; that passes quickly.
        let mut attempt = 0;
        loop {
            match self.fs.rename(&temp, NOTES_FILE) {
                Ok(()) => return Ok(()),
                Err(_) if attempt < 20 => {
                    attempt += 1;
                    thread::sleep(Duration::from_millis(25));
                }
                Err(err) => {
                    self.fs.remove(&temp).ok();
                    return Err(err);
                }
            }
        }
    }

    /// Adds [`EXCLUDE_PATTERN`] to `.git/info/exclude` if the root is a git
    /// repository and it is not there yet.
    pub fn ensure_excluded(&self) -> io::Result<()> {
        if self.fs.read(".git/HEAD").ok().flatten().is_none() {
            return Ok(());
        }
        let exclude = ".git/info/exclude";
        let mut text = self.fs.read(exclude)?.unwrap_or_default();
        if text.lines().any(|line| line.trim() == EXCLUDE_PATTERN) {
            return Ok(());
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("# NextCode sticky notes (local, not committed)\n");
        text.push_str(EXCLUDE_PATTERN);
        text.push('\n');
        self.fs.create_dir_all(".git/info")?;
        self.fs.write(exclude, text.as_bytes())
    }

    pub fn add(
        &self,
        file: &str,
        cell: Option<usize>,
        anchor: Anchor,
        text: &str,
        author: Author,
        to: Option<String>,
    ) -> io::Result<Note> {
        self.update(|notes| Ok(notes.add(file, cell, anchor, text, author, to).clone()))
    }

    pub fn reply(&self, id: &str, text: &str, author: Author) -> io::Result<Note> {
        self.update(|notes| notes.reply(id, text, author).cloned())
    }

    pub fn resolve(&self, id: &str, by: Author) -> io::Result<Note> {
        self.update(|notes| notes.resolve(id, by).cloned())
    }

    pub fn reopen(&self, id: &str) -> io::Result<Note> {
        self.update(|notes| notes.reopen(id).cloned())
    }

    pub fn edit_message(&self, id: &str, index: usize, text: &str) -> io::Result<Note> {
        self.update(|notes| notes.edit_message(id, index, text).cloned())
    }

    pub fn delete(&self, id: &str) -> io::Result<Note> {
        self.update(|notes| notes.delete(id))
    }

    pub fn update_anchor(&self, id: &str, anchor: Anchor) -> io::Result<Note> {
        self.update(|notes| notes.update_anchor(id, anchor).cloned())
    }

    /// Several anchors in one write (the app after an edit); notes deleted
    /// meanwhile are skipped.
    pub fn update_anchors(&self, anchors: Vec<(String, Anchor)>) -> io::Result<()> {
        self.update(|notes| {
            for (id, anchor) in anchors {
                if notes.get(&id).is_some() {
                    notes.update_anchor(&id, anchor)?;
                }
            }
            Ok(())
        })
    }
}

/// `.next_code/notes.json.lock`, holding its owner's pid and creation time.
struct Lock<'a> {
    fs: &'a dyn NotesFs,
}

impl<'a> Lock<'a> {
    fn acquire(fs: &'a dyn NotesFs) -> io::Result<Self> {
        let started = Instant::now();
        // A lock whose content does not read as ours (empty while being
        // written, or garbage) counts as stale once seen that long.
        let mut unreadable_since: Option<(String, Instant)> = None;
        loop {
            let content = format!("{} {}\n", std::process::id(), now_ms());
            // Errors here are mostly transient (Windows refuses to create a
            // file whose deletion is pending; /mnt drives may report
            // ENODATA mid-rename): retried until the deadline.
            let created_error = match fs.create_new(LOCK_FILE, content.as_bytes()) {
                Ok(true) => return Ok(Self { fs }),
                Ok(false) => None,
                Err(err) => Some(err),
            };
            let read_lock = || fs.read(LOCK_FILE).ok().flatten().unwrap_or_default();
            let held = read_lock();
            let created = held
                .split_whitespace()
                .nth(1)
                .and_then(|ms| ms.parse::<u64>().ok());
            let stale = match created {
                Some(created) => {
                    unreadable_since = None;
                    now_ms().saturating_sub(created) > STALE_LOCK.as_millis() as u64
                }
                None => match &unreadable_since {
                    Some((seen, since)) if *seen == held => since.elapsed() > STALE_LOCK,
                    _ => {
                        unreadable_since = Some((held.clone(), Instant::now()));
                        false
                    }
                },
            };
            // Re-read right before removing: another writer may just have
            // replaced the stale lock with its own.
            if stale && read_lock() == held {
                fs.remove(LOCK_FILE).ok();
                continue;
            }
            if started.elapsed() > LOCK_WAIT {
                return Err(match created_error {
                    Some(err) => {
                        io::Error::new(err.kind(), format!("cannot create {LOCK_FILE}: {err}"))
                    }
                    None => io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "{LOCK_FILE} is held (pid and time: {:?}); delete it if no NextCode or notes command is running",
                            held.trim()
                        ),
                    ),
                });
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Lock<'_> {
    fn drop(&mut self) {
        self.fs.remove(LOCK_FILE).ok();
    }
}
