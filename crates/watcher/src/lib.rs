//! Recursive project file watching with debouncing and `.gitignore` filtering.
//!
//! Raw OS notifications are coalesced per path over a short window and
//! delivered as batches on an async channel, so consumers (file tree, finder,
//! language servers) see one event per path per burst, e.g. a save that the
//! OS reports as create + several modifies arrives as a single `Created`.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use futures::channel::mpsc as async_mpsc;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{ModifyKind, RenameMode},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Created,
    Modified,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEvent {
    pub path: PathBuf,
    pub kind: ChangeKind,
}

/// Changes from one debounce window.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Batch {
    /// Sorted by path; at most one event per path.
    pub events: Vec<FileEvent>,
    /// The OS dropped events (e.g. buffer overflow); consumers must rescan.
    pub rescan: bool,
    /// The git repository at the root changed (index, HEAD or refs), e.g.
    /// a commit or checkout; its status must be re-read. Other `.git`
    /// changes are not reported.
    pub repository: bool,
}

/// Watches a directory tree until dropped.
pub struct ProjectWatcher {
    _watcher: RecommendedWatcher,
}

enum Raw {
    Change(PathBuf, ChangeKind),
    Rescan,
}

/// Starts watching `root` recursively. Batches arrive on the returned
/// receiver `debounce` after the first event of a burst.
pub fn watch(
    root: &Path,
    debounce: Duration,
) -> notify::Result<(ProjectWatcher, async_mpsc::UnboundedReceiver<Batch>)> {
    let (raw_tx, raw_rx) = mpsc::channel::<Raw>();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
        let raw = match result {
            Ok(event) if event.need_rescan() => vec![Raw::Rescan],
            Ok(event) => translate(event),
            Err(_) => vec![Raw::Rescan],
        };
        for raw in raw {
            raw_tx.send(raw).ok();
        }
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;

    let (batch_tx, batch_rx) = async_mpsc::unbounded();
    let root = root.to_path_buf();
    thread::Builder::new()
        .name("project-watcher".into())
        .spawn(move || debounce_loop(&root, debounce, &raw_rx, &batch_tx))?;
    Ok((ProjectWatcher { _watcher: watcher }, batch_rx))
}

fn translate(event: Event) -> Vec<Raw> {
    let mut paths = event.paths.into_iter();
    let existence = |path: PathBuf| {
        let kind = if path.exists() {
            ChangeKind::Modified
        } else {
            ChangeKind::Removed
        };
        Raw::Change(path, kind)
    };
    match event.kind {
        EventKind::Access(_) => Vec::new(),
        EventKind::Create(_) => paths.map(|p| Raw::Change(p, ChangeKind::Created)).collect(),
        EventKind::Remove(_) => paths.map(|p| Raw::Change(p, ChangeKind::Removed)).collect(),
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            paths.map(|p| Raw::Change(p, ChangeKind::Removed)).collect()
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            paths.map(|p| Raw::Change(p, ChangeKind::Created)).collect()
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            let from = paths.next().map(|p| Raw::Change(p, ChangeKind::Removed));
            let to = paths.next().map(|p| Raw::Change(p, ChangeKind::Created));
            from.into_iter().chain(to).collect()
        }
        // Rename halves without a direction: decide by what is on disk now.
        EventKind::Modify(ModifyKind::Name(_)) => paths
            .map(|p| {
                let kind = if p.exists() {
                    ChangeKind::Created
                } else {
                    ChangeKind::Removed
                };
                Raw::Change(p, kind)
            })
            .collect(),
        EventKind::Modify(_) => paths
            .map(|p| Raw::Change(p, ChangeKind::Modified))
            .collect(),
        EventKind::Any | EventKind::Other => paths.map(existence).collect(),
    }
}

/// Combines two consecutive changes to one path; `None` means they cancel out.
pub fn coalesce(earlier: ChangeKind, later: ChangeKind) -> Option<ChangeKind> {
    use ChangeKind::*;
    match (earlier, later) {
        (Created, Removed) => None,
        (Created, _) => Some(Created),
        (Removed, Created | Modified) => Some(Modified),
        (_, later) => Some(later),
    }
}

fn debounce_loop(
    root: &Path,
    debounce: Duration,
    raw: &mpsc::Receiver<Raw>,
    batches: &async_mpsc::UnboundedSender<Batch>,
) {
    let git_dir = root.join(".git");
    let gitignore_path = root.join(".gitignore");
    // NextCode's sticky notes are git-excluded but must reach the app, which
    // shows notes agents add.
    let notes_path = root.join(".next_code").join("notes.json");
    let mut ignore = load_ignore(root);
    loop {
        let Ok(first) = raw.recv() else {
            return;
        };
        let mut pending: HashMap<PathBuf, Option<ChangeKind>> = HashMap::new();
        let mut rescan = false;
        let mut add = |raw: Raw| match raw {
            Raw::Rescan => rescan = true,
            Raw::Change(path, kind) => {
                let merged = match pending.get(&path) {
                    Some(Some(earlier)) => coalesce(*earlier, kind),
                    // A path whose changes cancelled out starts fresh.
                    Some(None) | None => Some(kind),
                };
                pending.insert(path, merged);
            }
        };
        add(first);
        let deadline = Instant::now() + debounce;
        let disconnected = loop {
            match raw.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(next) => add(next),
                Err(mpsc::RecvTimeoutError::Timeout) => break false,
                Err(mpsc::RecvTimeoutError::Disconnected) => break true,
            }
        };

        if pending.contains_key(&gitignore_path) {
            ignore = load_ignore(root);
        }
        let repository = pending
            .keys()
            .any(|path| is_repository_state(&git_dir, path));
        let mut events: Vec<FileEvent> = pending
            .into_iter()
            .filter_map(|(path, kind)| Some(FileEvent { path, kind: kind? }))
            .filter(|event| {
                event.path == notes_path
                    || (!event.path.starts_with(&git_dir) && !is_ignored(&ignore, root, &event.path))
            })
            .collect();
        events.sort_by(|a, b| a.path.cmp(&b.path));
        if (!events.is_empty() || rescan || repository)
            && batches
                .unbounded_send(Batch {
                    events,
                    rescan,
                    repository,
                })
                .is_err()
        {
            return;
        }
        if disconnected {
            return;
        }
    }
}

/// Whether `path` holds state that `git status` depends on: the index,
/// HEAD, refs (commits, branch switches) or an ongoing merge.
fn is_repository_state(git_dir: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(git_dir) else {
        return false;
    };
    let mut components = relative.components();
    let Some(first) = components.next() else {
        return false;
    };
    match first.as_os_str().to_str() {
        Some("index" | "HEAD" | "packed-refs" | "MERGE_HEAD") => components.next().is_none(),
        // Windows also reports the `refs` directory itself when anything
        // below `.git` is touched; only entries inside it matter.
        Some("refs") => components.next().is_some(),
        _ => false,
    }
}

/// The root `.gitignore` plus `.git/info/exclude`. Nested `.gitignore` files
/// are not consulted for watch events.
fn load_ignore(root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    builder.add(root.join(".gitignore"));
    builder.add(root.join(".git").join("info").join("exclude"));
    builder.build().unwrap_or_else(|_| Gitignore::empty())
}

fn is_ignored(ignore: &Gitignore, root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|relative| {
        ignore
            .matched_path_or_any_parents(relative, path.is_dir())
            .is_ignore()
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    const DEBOUNCE: Duration = Duration::from_millis(150);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("next_code_watch_{name}_{}", std::process::id()));
            fs::remove_dir_all(&path).ok();
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    /// Collects batches until `until` holds or a timeout passes.
    fn collect(
        rx: &mut async_mpsc::UnboundedReceiver<Batch>,
        until: impl Fn(&[FileEvent]) -> bool,
    ) -> Vec<FileEvent> {
        let mut events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !until(&events) {
            match rx.try_recv() {
                Ok(batch) => events.extend(batch.events),
                Err(_) => thread::sleep(Duration::from_millis(20)),
            }
        }
        // Let any trailing batch for the same burst arrive.
        thread::sleep(DEBOUNCE * 3);
        while let Ok(batch) = rx.try_recv() {
            events.extend(batch.events);
        }
        events
    }

    fn kinds_for<'a>(
        events: &'a [FileEvent],
        path: &'a Path,
    ) -> impl Iterator<Item = ChangeKind> + 'a {
        events
            .iter()
            .filter(move |e| e.path == path)
            .map(|e| e.kind)
    }

    #[test]
    fn coalescing_reduces_a_burst_to_its_net_effect() {
        use ChangeKind::*;
        assert_eq!(coalesce(Created, Modified), Some(Created));
        assert_eq!(coalesce(Created, Removed), None);
        assert_eq!(coalesce(Removed, Created), Some(Modified));
        assert_eq!(coalesce(Modified, Removed), Some(Removed));
        assert_eq!(coalesce(Modified, Modified), Some(Modified));
    }

    #[test]
    fn a_burst_of_writes_to_a_new_file_is_one_created_event() {
        let dir = TempDir::new("burst");
        let (_watcher, mut rx) = watch(&dir.0, DEBOUNCE).unwrap();
        let file = dir.0.join("a.txt");
        fs::write(&file, "one").unwrap();
        fs::write(&file, "two").unwrap();
        fs::write(&file, "three").unwrap();

        let events = collect(&mut rx, |events| events.iter().any(|e| e.path == file));
        assert_eq!(
            kinds_for(&events, &file).collect::<Vec<_>>(),
            [ChangeKind::Created]
        );
    }

    #[test]
    fn successive_modifications_of_an_existing_file_are_reported() {
        let dir = TempDir::new("successive");
        let file = dir.0.join("edited.py");
        fs::write(&file, "value = 0\n").unwrap();
        let (_watcher, mut rx) = watch(&dir.0, DEBOUNCE).unwrap();

        for value in [1, 2] {
            fs::write(&file, format!("value = {value}\n")).unwrap();
            let events = collect(&mut rx, |events| {
                events.iter().any(|event| event.path == file)
            });
            assert_eq!(
                kinds_for(&events, &file).last(),
                Some(ChangeKind::Modified),
                "modification {value} was not reported"
            );
        }
    }

    #[test]
    fn removals_and_modifications_of_existing_files_are_reported() {
        let dir = TempDir::new("existing");
        let kept = dir.0.join("kept.txt");
        let gone = dir.0.join("gone.txt");
        fs::write(&kept, "x").unwrap();
        fs::write(&gone, "x").unwrap();
        let (_watcher, mut rx) = watch(&dir.0, DEBOUNCE).unwrap();
        fs::write(&kept, "changed").unwrap();
        fs::remove_file(&gone).unwrap();

        let events = collect(&mut rx, |events| {
            events.iter().any(|e| e.path == kept) && events.iter().any(|e| e.path == gone)
        });
        assert_eq!(kinds_for(&events, &kept).last(), Some(ChangeKind::Modified));
        assert_eq!(kinds_for(&events, &gone).last(), Some(ChangeKind::Removed));
    }

    #[test]
    fn gitignored_paths_and_the_git_dir_are_filtered() {
        let dir = TempDir::new("ignored");
        fs::write(dir.0.join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::create_dir_all(dir.0.join("target")).unwrap();
        fs::create_dir_all(dir.0.join(".git")).unwrap();
        let (_watcher, mut rx) = watch(&dir.0, DEBOUNCE).unwrap();
        fs::write(dir.0.join("target").join("build.o"), "x").unwrap();
        fs::write(dir.0.join("debug.log"), "x").unwrap();
        fs::write(dir.0.join(".git").join("index"), "x").unwrap();
        let visible = dir.0.join("main.py");
        fs::write(&visible, "x").unwrap();

        let events = collect(&mut rx, |events| events.iter().any(|e| e.path == visible));
        let paths: Vec<&Path> = events.iter().map(|e| e.path.as_path()).collect();
        assert_eq!(paths, [visible.as_path()]);
    }

    #[test]
    fn the_notes_file_is_reported_even_when_excluded() {
        let dir = TempDir::new("notes");
        fs::write(dir.0.join(".gitignore"), "/.next_code/\n").unwrap();
        fs::create_dir_all(dir.0.join(".next_code")).unwrap();
        let (_watcher, mut rx) = watch(&dir.0, DEBOUNCE).unwrap();
        let notes = dir.0.join(".next_code").join("notes.json");
        fs::write(dir.0.join(".next_code").join("other.json"), "x").unwrap();
        fs::write(&notes, "{}").unwrap();

        let events = collect(&mut rx, |events| events.iter().any(|e| e.path == notes));
        let paths: Vec<&Path> = events.iter().map(|e| e.path.as_path()).collect();
        assert_eq!(paths, [notes.as_path()]);
    }

    #[test]
    fn index_and_ref_updates_flag_the_repository_without_file_events() {
        let dir = TempDir::new("repository");
        let git = dir.0.join(".git");
        fs::create_dir_all(git.join("objects")).unwrap();
        fs::create_dir_all(git.join("refs").join("heads")).unwrap();
        let (_watcher, mut rx) = watch(&dir.0, DEBOUNCE).unwrap();
        // Objects alone (e.g. `git hash-object`) do not change status.
        fs::write(git.join("objects").join("ab"), "x").unwrap();
        thread::sleep(DEBOUNCE * 4);
        while let Ok(batch) = rx.try_recv() {
            assert!(!batch.repository, "{batch:?}");
        }
        fs::write(git.join("refs").join("heads").join("main"), "abc").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let batch = loop {
            match rx.try_recv() {
                Ok(batch) if batch.repository => break batch,
                Ok(_) => {}
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                Err(_) => panic!("no repository change reported"),
            }
        };
        assert!(batch.events.is_empty(), "{batch:?}");
    }
}
