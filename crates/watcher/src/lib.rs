//! Recursive project file watching with debouncing and `.gitignore` filtering.
//!
//! Raw OS notifications are coalesced per path over a short window and
//! delivered as batches on an async channel, so consumers (file tree, finder,
//! language servers) see one event per path per burst, e.g. a save that the
//! OS reports as create + several modifies arrives as a single `Created`.
//!
//! Ignored paths are dropped: every `.gitignore` on the way down from the
//! root counts (read when first needed, forgotten when it changes), as do
//! the repository's `info/exclude` and the user's global excludes, so a
//! build in a sub-crate's own ignored `target/` does not flood the batches.
//! The OS callback thread only forwards events; looking at the disk (to
//! tell a dir from a file where a `dir/` pattern could apply, or whether a
//! renamed path still exists) happens on the debounce thread, once per path
//! and batch.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use futures::channel::mpsc as async_mpsc;
use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder, Glob},
};
use notify::{
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{CreateKind, ModifyKind, RemoveKind, RenameMode},
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
    /// The git repository at the root changed: `index || refs`; its status
    /// must be re-read. Other `.git` changes are not reported.
    pub repository: bool,
    /// `.git/index` changed: staging, unstaging, a commit.
    pub index: bool,
    /// `HEAD`, `refs/`, `packed-refs` or `MERGE_HEAD` changed: a commit,
    /// checkout, fetch or merge; branches and the log may differ. Staging
    /// alone leaves this `false`.
    pub refs: bool,
}

/// Watches a directory tree until dropped.
pub struct ProjectWatcher {
    _watcher: RecommendedWatcher,
}

#[derive(Debug, PartialEq, Eq)]
enum Raw {
    Change {
        path: PathBuf,
        kind: RawKind,
        /// Whether the path is a directory, when the OS said (inotify and
        /// FSEvents do, Windows does not).
        is_dir: Option<bool>,
    },
    Rescan,
}

/// What an OS event means, before anyone looks at the disk.
#[derive(Debug, PartialEq, Eq)]
enum RawKind {
    Known(ChangeKind),
    /// A rename half without a direction, or an unspecific event:
    /// `present` if the path exists now, else `Removed`; decided on the
    /// debounce thread.
    IfPresent {
        present: ChangeKind,
    },
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

/// The changes an OS event stands for. Runs on the OS callback thread, so
/// it never touches the disk.
fn translate(event: Event) -> Vec<Raw> {
    use ChangeKind::*;
    let Event { kind, paths, .. } = event;
    let known = |kind: ChangeKind, is_dir: Option<bool>| -> Vec<Raw> {
        paths
            .iter()
            .map(|path| Raw::Change {
                path: path.clone(),
                kind: RawKind::Known(kind),
                is_dir,
            })
            .collect()
    };
    let if_present = |present: ChangeKind| -> Vec<Raw> {
        paths
            .iter()
            .map(|path| Raw::Change {
                path: path.clone(),
                kind: RawKind::IfPresent { present },
                is_dir: None,
            })
            .collect()
    };
    match kind {
        EventKind::Access(_) => Vec::new(),
        EventKind::Create(kind) => known(
            Created,
            match kind {
                CreateKind::File => Some(false),
                CreateKind::Folder => Some(true),
                _ => None,
            },
        ),
        EventKind::Remove(kind) => known(
            Removed,
            match kind {
                RemoveKind::File => Some(false),
                RemoveKind::Folder => Some(true),
                _ => None,
            },
        ),
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => known(Removed, None),
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => known(Created, None),
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            let mut paths = paths.iter();
            let from = paths.next().map(|path| Raw::Change {
                path: path.clone(),
                kind: RawKind::Known(Removed),
                is_dir: None,
            });
            let to = paths.next().map(|path| Raw::Change {
                path: path.clone(),
                kind: RawKind::Known(Created),
                is_dir: None,
            });
            from.into_iter().chain(to).collect()
        }
        // Rename halves without a direction: decided by what is on disk.
        EventKind::Modify(ModifyKind::Name(_)) => if_present(Created),
        EventKind::Modify(_) => known(Modified, None),
        EventKind::Any | EventKind::Other => if_present(Modified),
    }
}

/// One path's state within a debounce window.
struct Pending {
    /// `None` once its changes cancelled out.
    kind: Option<ChangeKind>,
    is_dir: Option<bool>,
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
    // NextCode's sticky notes are git-excluded but must reach the app, which
    // shows notes agents add.
    let notes_path = root.join(".next_code").join("notes.json");
    let mut ignores = Ignores::new(root);
    loop {
        let Ok(first) = raw.recv() else {
            return;
        };
        let mut pending: HashMap<PathBuf, Pending> = HashMap::new();
        let mut rescan = false;
        let mut add = |raw: Raw| match raw {
            Raw::Rescan => rescan = true,
            Raw::Change { path, kind, is_dir } => {
                let kind = match kind {
                    RawKind::Known(kind) => kind,
                    RawKind::IfPresent { present } if path.exists() => present,
                    RawKind::IfPresent { .. } => ChangeKind::Removed,
                };
                let entry = pending.entry(path).or_insert(Pending {
                    kind: None,
                    is_dir: None,
                });
                entry.kind = match entry.kind {
                    Some(earlier) => coalesce(earlier, kind),
                    // A path whose changes cancelled out starts fresh.
                    None => Some(kind),
                };
                if is_dir.is_some() {
                    entry.is_dir = is_dir;
                }
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

        if rescan {
            ignores.forget_all();
        } else {
            ignores.changed(&pending);
        }
        let (mut index, mut refs) = (false, false);
        for path in pending.keys() {
            match repository_state(&git_dir, path) {
                Some(RepositoryState::Index) => index = true,
                Some(RepositoryState::Refs) => refs = true,
                None => {}
            }
        }
        let mut events: Vec<FileEvent> = pending
            .into_iter()
            .filter_map(|(path, pending)| Some((path, pending.kind?, pending.is_dir)))
            .filter(|(path, _, is_dir)| {
                *path == notes_path
                    || (!path.starts_with(&git_dir) && !ignores.is_ignored(path, *is_dir))
            })
            .map(|(path, kind, _)| FileEvent { path, kind })
            .collect();
        events.sort_by(|a, b| a.path.cmp(&b.path));
        let repository = index || refs;
        if (!events.is_empty() || rescan || repository)
            && batches
                .unbounded_send(Batch {
                    events,
                    rescan,
                    repository,
                    index,
                    refs,
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

/// The repository state a `.git` path holds, when `git status`, the
/// branches or the log depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepositoryState {
    /// The index: what is staged.
    Index,
    /// HEAD, refs (commits, branch switches) or an ongoing merge.
    Refs,
}

fn repository_state(git_dir: &Path, path: &Path) -> Option<RepositoryState> {
    let relative = path.strip_prefix(git_dir).ok()?;
    let mut components = relative.components();
    let first = components.next()?;
    match first.as_os_str().to_str() {
        Some("index") if components.next().is_none() => Some(RepositoryState::Index),
        Some("HEAD" | "packed-refs" | "MERGE_HEAD") if components.next().is_none() => {
            Some(RepositoryState::Refs)
        }
        // Windows also reports the `refs` directory itself when anything
        // below `.git` is touched; only entries inside it matter.
        Some("refs") if components.next().is_some() => Some(RepositoryState::Refs),
        _ => None,
    }
}

/// The ignore rules of the watched tree. Each directory's `.gitignore` is
/// read the first time a path under it is checked and dropped when it
/// changes; the root's `.git/info/exclude` and the user's global excludes
/// are read once.
struct Ignores {
    root: PathBuf,
    exclude_path: PathBuf,
    exclude: Gitignore,
    global: Gitignore,
    /// Each directory's `.gitignore` (empty when it has none).
    dirs: HashMap<PathBuf, Arc<Gitignore>>,
}

/// A matcher's answer for one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    None,
    Ignore,
    Whitelist,
}

impl From<Match<&Glob>> for Verdict {
    fn from(matched: Match<&Glob>) -> Self {
        match matched {
            Match::None => Self::None,
            Match::Ignore(_) => Self::Ignore,
            Match::Whitelist(_) => Self::Whitelist,
        }
    }
}

impl Ignores {
    fn new(root: &Path) -> Self {
        let exclude_path = root.join(".git").join("info").join("exclude");
        Self {
            root: root.to_path_buf(),
            exclude: read_exclude(root, &exclude_path),
            exclude_path,
            global: Gitignore::global().0,
            dirs: HashMap::new(),
        }
    }

    /// Drops the rules the batch's paths may have changed: a `.gitignore`
    /// or `info/exclude` that changed, and what was read under a removed
    /// (or possibly removed) directory.
    fn changed(&mut self, pending: &HashMap<PathBuf, Pending>) {
        let mut removed_dirs = false;
        for (path, pending) in pending {
            if path.file_name().is_some_and(|name| name == ".gitignore")
                && let Some(dir) = path.parent()
            {
                self.dirs.remove(dir);
            }
            if *path == self.exclude_path {
                self.exclude = read_exclude(&self.root, &self.exclude_path);
            }
            removed_dirs |=
                pending.kind == Some(ChangeKind::Removed) && pending.is_dir != Some(false);
        }
        if removed_dirs {
            self.dirs.retain(|dir, _| {
                !dir.ancestors().any(|ancestor| {
                    pending
                        .get(ancestor)
                        .is_some_and(|pending| pending.kind == Some(ChangeKind::Removed))
                })
            });
        }
    }

    /// After dropped events: nothing read before is trusted.
    fn forget_all(&mut self) {
        self.dirs.clear();
        self.exclude = read_exclude(&self.root, &self.exclude_path);
    }

    /// `dir`'s `.gitignore`, read on first use.
    fn rules(&mut self, dir: &Path) -> Arc<Gitignore> {
        if let Some(rules) = self.dirs.get(dir) {
            return rules.clone();
        }
        let rules = Arc::new(Gitignore::new(dir.join(".gitignore")).0);
        self.dirs.insert(dir.to_path_buf(), rules.clone());
        rules
    }

    /// Whether `path` is ignored: it or a folder on the way from the root
    /// matches an ignore rule, where the nearest `.gitignore` wins (and a
    /// `!pattern` whitelist ends the search), then `info/exclude`, then
    /// the global excludes. `is_dir` is what the OS said about the path;
    /// unknown, the disk is asked only when a `dir/` pattern could apply.
    fn is_ignored(&mut self, path: &Path, is_dir: Option<bool>) -> bool {
        if !path.starts_with(&self.root) || path == self.root {
            return false;
        }
        // The directories from the path's parent up to the root, and their
        // rules (`rules[i]` belong to the parent of `prefixes[i]`).
        let root = self.root.clone();
        let rules: Vec<Arc<Gitignore>> = path
            .ancestors()
            .skip(1)
            .take_while(|dir| dir.starts_with(&root))
            .map(|dir| self.rules(dir))
            .collect();
        let prefixes: Vec<&Path> = path
            .ancestors()
            .take_while(|prefix| *prefix != root)
            .collect();
        // From the root's child down: an ignored folder settles everything
        // below it, as git never re-includes what an ignored folder holds.
        for (index, prefix) in prefixes.iter().enumerate().rev() {
            let kind = if index == 0 { is_dir } else { Some(true) };
            if self.verdict(prefix, kind, &rules[index..]) == Verdict::Ignore {
                return true;
            }
        }
        false
    }

    /// The first rule for `prefix` among `chain` (nearest `.gitignore`
    /// first), `info/exclude` and the global excludes.
    fn verdict(&self, prefix: &Path, kind: Option<bool>, chain: &[Arc<Gitignore>]) -> Verdict {
        let mut kind = kind;
        let files = chain
            .iter()
            .map(|rules| &**rules)
            .chain([&self.exclude, &self.global]);
        for file in files {
            if file.is_empty() {
                continue;
            }
            let as_file = Verdict::from(file.matched(prefix, false));
            let as_dir = Verdict::from(file.matched(prefix, true));
            let verdict = if as_file == as_dir {
                as_file
            } else {
                // A `dir/` pattern decides: now the kind matters. A path
                // that is gone is matched as a file, as before.
                let is_dir = *kind.get_or_insert_with(|| prefix.is_dir());
                if is_dir { as_dir } else { as_file }
            };
            if verdict != Verdict::None {
                return verdict;
            }
        }
        Verdict::None
    }
}

/// The repository's `.git/info/exclude`, with patterns relative to the root.
fn read_exclude(root: &Path, exclude_path: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(root);
    builder.add(exclude_path);
    builder.build().unwrap_or_else(|_| Gitignore::empty())
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

    /// The next batch flagging the repository.
    fn repository_batch(rx: &mut async_mpsc::UnboundedReceiver<Batch>) -> Batch {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match rx.try_recv() {
                Ok(batch) if batch.repository => return batch,
                Ok(_) => {}
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                Err(_) => panic!("no repository change reported"),
            }
        }
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
        // Staging touches the index only.
        fs::write(git.join("index"), "x").unwrap();
        let batch = repository_batch(&mut rx);
        assert!(batch.index && !batch.refs, "{batch:?}");
        assert!(batch.events.is_empty(), "{batch:?}");
        thread::sleep(DEBOUNCE * 2);
        while rx.try_recv().is_ok() {}
        // A commit or checkout moves refs.
        fs::write(git.join("refs").join("heads").join("main"), "abc").unwrap();
        let batch = repository_batch(&mut rx);
        assert!(batch.refs && !batch.index, "{batch:?}");
        assert!(batch.events.is_empty(), "{batch:?}");
    }

    #[test]
    fn nested_gitignore_files_filter_events_in_their_folder() {
        let dir = TempDir::new("nested");
        let sub = dir.0.join("sub");
        fs::create_dir_all(sub.join("build")).unwrap();
        fs::create_dir_all(dir.0.join("build")).unwrap();
        fs::write(sub.join(".gitignore"), "build/\n").unwrap();
        let (_watcher, mut rx) = watch(&dir.0, DEBOUNCE).unwrap();
        fs::write(sub.join("build").join("x.o"), "x").unwrap();
        let top = dir.0.join("build").join("top.o");
        fs::write(&top, "x").unwrap();
        let keep = sub.join("keep.txt");
        fs::write(&keep, "x").unwrap();

        let events = collect(&mut rx, |events| {
            events.iter().any(|e| e.path == keep) && events.iter().any(|e| e.path == top)
        });
        // Files only: Windows also reports `sub` itself as its entries change.
        let mut paths: Vec<&Path> = events
            .iter()
            .map(|e| e.path.as_path())
            .filter(|path| !path.is_dir())
            .collect();
        paths.sort();
        // The nested rule hides `sub/build/x.o` only.
        assert_eq!(paths, [top.as_path(), keep.as_path()]);

        // The rule withdrawn: its folder's events show again.
        fs::write(sub.join(".gitignore"), "").unwrap();
        let shown = sub.join("build").join("y.o");
        fs::write(&shown, "x").unwrap();
        let events = collect(&mut rx, |events| events.iter().any(|e| e.path == shown));
        assert!(events.iter().any(|e| e.path == shown), "{events:?}");
    }

    #[test]
    fn ignore_rules_apply_nearest_first_and_dir_patterns_ask_the_disk() {
        let dir = TempDir::new("rules");
        let root = &dir.0;
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::create_dir_all(root.join("build")).unwrap();
        fs::create_dir_all(root.join(".git").join("info")).unwrap();
        fs::write(root.join(".gitignore"), "*.log\nbuild/\n").unwrap();
        fs::write(sub.join(".gitignore"), "gen/\n!keep.log\n").unwrap();
        fs::write(root.join(".git").join("info").join("exclude"), "secret*\n").unwrap();
        let mut ignores = Ignores::new(root);

        assert!(ignores.is_ignored(&root.join("a.log"), Some(false)));
        assert!(ignores.is_ignored(&root.join("a.log"), None));
        assert!(!ignores.is_ignored(&root.join("readme.md"), None));
        // `build/` is for the folder (on disk, when the OS did not say)
        // and what it holds, not for a file of that name.
        assert!(ignores.is_ignored(&root.join("build"), None));
        assert!(!ignores.is_ignored(&root.join("build"), Some(false)));
        assert!(ignores.is_ignored(&root.join("build").join("x.o"), Some(false)));
        // A path that is gone, of unknown kind, counts as a file.
        assert!(!ignores.is_ignored(&sub.join("gen"), None));
        assert!(ignores.is_ignored(&sub.join("gen"), Some(true)));
        assert!(ignores.is_ignored(&sub.join("gen").join("x.py"), Some(false)));
        // The nearest file wins, and its rules do not climb.
        assert!(!ignores.is_ignored(&sub.join("keep.log"), Some(false)));
        assert!(ignores.is_ignored(&sub.join("other.log"), Some(false)));
        assert!(!ignores.is_ignored(&root.join("gen"), Some(true)));
        assert!(ignores.is_ignored(&root.join("secret.txt"), Some(false)));

        // Rules stay cached until their file is reported changed.
        fs::write(sub.join(".gitignore"), "").unwrap();
        assert!(ignores.is_ignored(&sub.join("gen"), Some(true)));
        let mut pending = HashMap::new();
        pending.insert(
            sub.join(".gitignore"),
            Pending {
                kind: Some(ChangeKind::Modified),
                is_dir: Some(false),
            },
        );
        ignores.changed(&pending);
        assert!(!ignores.is_ignored(&sub.join("gen"), Some(true)));
        assert!(ignores.is_ignored(&sub.join("other.log"), Some(false)));
    }

    #[test]
    fn os_events_carry_the_kind_they_know_and_never_touch_the_disk() {
        use notify::event::DataChange;
        let path = PathBuf::from("/nowhere/x");
        let raw = |kind: EventKind| translate(Event::new(kind).add_path(path.clone()));
        let change = |kind: RawKind, is_dir: Option<bool>| {
            vec![Raw::Change {
                path: path.clone(),
                kind,
                is_dir,
            }]
        };
        assert_eq!(
            raw(EventKind::Create(CreateKind::Folder)),
            change(RawKind::Known(ChangeKind::Created), Some(true))
        );
        assert_eq!(
            raw(EventKind::Remove(RemoveKind::File)),
            change(RawKind::Known(ChangeKind::Removed), Some(false))
        );
        assert_eq!(
            raw(EventKind::Modify(ModifyKind::Data(DataChange::Any))),
            change(RawKind::Known(ChangeKind::Modified), None)
        );
        assert_eq!(
            raw(EventKind::Modify(ModifyKind::Name(RenameMode::Any))),
            change(
                RawKind::IfPresent {
                    present: ChangeKind::Created
                },
                None
            )
        );
        assert_eq!(
            raw(EventKind::Any),
            change(
                RawKind::IfPresent {
                    present: ChangeKind::Modified
                },
                None
            )
        );
        assert_eq!(raw(EventKind::Access(notify::event::AccessKind::Any)), []);
    }
}
