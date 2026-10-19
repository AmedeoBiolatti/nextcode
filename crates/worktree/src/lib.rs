//! Project file operations that run where the files live: in the editor
//! process for local projects, and in `next_code_agent` for WSL projects.
//!
//! Both sides call these same functions, so listing, ignore rules and tool
//! lookup behave identically whichever side of the agent protocol runs them.

use std::{
    collections::HashMap,
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, PoisonError, mpsc},
    time::SystemTime,
};

use ignore::{
    Match, WalkBuilder, WalkState,
    gitignore::{Gitignore, GitignoreBuilder},
};
use serde::{Deserialize, Serialize};

/// One child of a listed directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    /// A folder holding an `__init__.py` file: a Python package. Checked
    /// for folders whose name is a Python identifier (the others cannot be
    /// imported), inside the listing, so the caller needs no stat per
    /// folder; always `false` for files.
    pub package: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileKind {
    File,
    Dir,
}

/// Lists one directory, honoring .gitignore (inside git repos), dirs first,
/// then case-insensitively by name. `.git` itself is hidden.
///
/// The ignore rules are the ones [`list_files`]'s walk applies (`.ignore`
/// and `.gitignore` files up the tree, the repository's `info/exclude`, the
/// global excludes file), but they are cached per directory and re-read only
/// when those files change, so a listing costs one directory read, a few
/// stats per ancestor, and one stat per folder that may be a package.
pub fn read_dir(dir: &Path) -> io::Result<Vec<DirEntry>> {
    if !fs::metadata(dir)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} is not a directory", dir.display()),
        ));
    }
    let rules = IgnoreChain::for_dir(dir);
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)?.filter_map(Result::ok) {
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        // Like the walker (which does not follow links): a link is not a dir.
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        let path = entry.path();
        if rules.is_ignored(&path, is_dir) {
            continue;
        }
        let name = name.to_string_lossy().into_owned();
        let package = is_dir
            && is_identifier(&name)
            && fs::metadata(path.join("__init__.py")).is_ok_and(|meta| meta.is_file());
        entries.push(DirEntry {
            name,
            is_dir,
            package,
        });
    }
    entries.sort_by_cached_key(|entry| {
        (
            !entry.is_dir,
            entry.name.to_lowercase(),
            entry.name.clone(),
        )
    });
    Ok(entries)
}

/// Whether `name` is a Python identifier, which a package's folder must be.
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c == '_' || c.is_alphabetic())
        && chars.all(|c| c == '_' || c.is_alphanumeric())
}

/// All non-ignored files under `root`, as sorted root-relative paths using
/// the platform separator. The walk runs on all cores: on large trees, and
/// on Windows with endpoint security, it is many times faster than one
/// thread.
pub fn list_files(root: &Path) -> Vec<String> {
    let (tx, rx) = mpsc::channel::<String>();
    WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build_parallel()
        .run(|| {
            let tx = tx.clone();
            Box::new(move |entry| {
                if let Ok(entry) = entry
                    && entry.file_type().is_some_and(|kind| kind.is_file())
                    && let Ok(relative) = entry.path().strip_prefix(root)
                {
                    tx.send(relative.to_string_lossy().into_owned()).ok();
                }
                WalkState::Continue
            })
        });
    drop(tx);
    let mut files: Vec<String> = rx.into_iter().collect();
    files.sort_unstable();
    files
}

/// [`list_files`] on one thread, the reference the parallel walk is checked
/// against.
#[cfg(test)]
fn list_files_serial(root: &Path) -> Vec<String> {
    let mut files: Vec<String> = WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let relative = entry.path().strip_prefix(root).ok()?;
            Some(relative.to_string_lossy().into_owned())
        })
        .collect();
    files.sort_unstable();
    files
}

/// The ignore rules that apply to the entries of one directory: the
/// directory's own and its ancestors', nearest first, read the way
/// `ignore::WalkBuilder` reads them (without `.jj` support).
struct IgnoreChain {
    rules: Vec<Arc<DirRules>>,
    /// How many of `rules`, from the nearest, lie inside the git repository
    /// (up to and including its root, the first with `.git`); zero outside
    /// a repository, where `.gitignore` files, `info/exclude` and the global
    /// excludes do not apply (the walker's `require_git`).
    repository: usize,
    global: Arc<Gitignore>,
}

impl IgnoreChain {
    fn for_dir(dir: &Path) -> Self {
        let rules: Vec<Arc<DirRules>> = dir.ancestors().map(dir_rules).collect();
        let repository = rules
            .iter()
            .position(|rules| rules.has_git)
            .map_or(0, |index| index + 1);
        Self {
            rules,
            repository,
            global: global_excludes(),
        }
    }

    /// Whether `path` (a child of the chain's directory) is ignored. As in
    /// the walker, `.ignore` files win over `.gitignore` files, which win
    /// over `info/exclude` and the global excludes; within a kind, the
    /// nearest file wins and a whitelist (`!pattern`) ends the search.
    fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let repository = &self.rules[..self.repository];
        first_verdict(path, is_dir, self.rules.iter().map(|rules| &rules.ignore))
            .or_else(|| {
                first_verdict(path, is_dir, repository.iter().map(|rules| &rules.gitignore))
            })
            .or_else(|| first_verdict(path, is_dir, repository.iter().map(|rules| &rules.exclude)))
            .or_else(|| {
                first_verdict(
                    path,
                    is_dir,
                    (!repository.is_empty()).then_some(&*self.global),
                )
            })
            .unwrap_or(false)
    }
}

/// Whether the first of `files` with a rule for `path` ignores it;
/// `None` when none has one.
fn first_verdict<'a>(
    path: &Path,
    is_dir: bool,
    files: impl IntoIterator<Item = &'a Gitignore>,
) -> Option<bool> {
    files
        .into_iter()
        .find_map(|file| match file.matched(path, is_dir) {
            Match::None => None,
            Match::Ignore(_) => Some(true),
            Match::Whitelist(_) => Some(false),
        })
}

/// One directory's ignore files.
struct DirRules {
    /// `.ignore`: applies anywhere.
    ignore: Gitignore,
    /// `.gitignore`: applies inside a repository.
    gitignore: Gitignore,
    /// The repository's `info/exclude` (repository roots only).
    exclude: Gitignore,
    /// Holds `.git` (a directory, or a worktree's or submodule's file).
    has_git: bool,
    stamp: Stamp,
}

/// When a directory's ignore files last changed: the cache's validity key,
/// a few stats to compute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    ignore: Option<(SystemTime, u64)>,
    gitignore: Option<(SystemTime, u64)>,
    /// `Some` when `.git` exists: the change time of `info/exclude` under
    /// it, or of the `.git` file naming the git dir (worktrees, submodules).
    git: Option<Option<(SystemTime, u64)>>,
}

impl Stamp {
    fn of(dir: &Path) -> Self {
        let git = match fs::metadata(dir.join(".git")) {
            Ok(meta) if meta.is_dir() => Some(change_stamp(
                &dir.join(".git").join("info").join("exclude"),
            )),
            Ok(meta) => Some(meta.modified().ok().map(|time| (time, meta.len()))),
            Err(_) => None,
        };
        Self {
            ignore: change_stamp(&dir.join(".ignore")),
            gitignore: change_stamp(&dir.join(".gitignore")),
            git,
        }
    }
}

fn change_stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

impl DirRules {
    fn read(dir: &Path, stamp: Stamp) -> Self {
        let file = |name: &str, present: bool| {
            if present {
                Gitignore::new(dir.join(name)).0
            } else {
                Gitignore::empty()
            }
        };
        let exclude = match stamp.git.and_then(|_| git_common_dir(dir)) {
            Some(git_dir) => {
                let mut builder = GitignoreBuilder::new(dir);
                builder.add(git_dir.join("info").join("exclude"));
                builder.build().unwrap_or_else(|_| Gitignore::empty())
            }
            None => Gitignore::empty(),
        };
        Self {
            ignore: file(".ignore", stamp.ignore.is_some()),
            gitignore: file(".gitignore", stamp.gitignore.is_some()),
            exclude,
            has_git: stamp.git.is_some(),
            stamp,
        }
    }
}

/// The git directory shared by `dir`'s repository (where `info/exclude`
/// lives): `.git` itself, or the one a `.git` file names, following its
/// `commondir` for worktrees.
fn git_common_dir(dir: &Path) -> Option<PathBuf> {
    let git = dir.join(".git");
    let git_dir = if fs::metadata(&git).ok()?.is_dir() {
        git
    } else {
        let text = fs::read_to_string(&git).ok()?;
        let target = Path::new(text.strip_prefix("gitdir:")?.trim());
        if target.is_absolute() {
            target.to_path_buf()
        } else {
            dir.join(target)
        }
    };
    match fs::read_to_string(git_dir.join("commondir")) {
        Ok(common) => {
            let common = Path::new(common.trim());
            Some(if common.is_absolute() {
                common.to_path_buf()
            } else {
                git_dir.join(common)
            })
        }
        Err(_) => Some(git_dir),
    }
}

/// `dir`'s rules, from the cache while its ignore files are unchanged.
fn dir_rules(dir: &Path) -> Arc<DirRules> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<DirRules>>>> = OnceLock::new();
    let stamp = Stamp::of(dir);
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(rules) = cache.get(dir)
        && rules.stamp == stamp
    {
        return rules.clone();
    }
    let rules = Arc::new(DirRules::read(dir, stamp));
    cache.insert(dir.to_path_buf(), rules.clone());
    rules
}

/// The user's global excludes file (`core.excludesFile`, else
/// `~/.config/git/ignore`), read once per process.
fn global_excludes() -> Arc<Gitignore> {
    static GLOBAL: OnceLock<Arc<Gitignore>> = OnceLock::new();
    GLOBAL
        .get_or_init(|| Arc::new(Gitignore::global().0))
        .clone()
}

/// Text of a file for editing: UTF-8 without NUL bytes.
pub fn decode_text(bytes: Vec<u8>) -> io::Result<String> {
    let text = String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file is not valid UTF-8"))?;
    if text.contains('\0') {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "binary file"));
    }
    Ok(text)
}

/// Replaces a file's contents without ever leaving it truncated: the data
/// goes to a temporary file beside it that takes the file's place once it
/// is complete (see [`write_atomic`]). A symlink, and a file with other
/// hard links, is written in place instead, as a replacement would break
/// the link; permissions are kept either way.
pub fn write_file(path: &Path, data: &[u8]) -> io::Result<()> {
    if must_write_in_place(path)? {
        return write_in_place(path, data);
    }
    write_atomic(path, data)
}

/// The plain write: truncates, then writes.
fn write_in_place(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(data)?;
    file.flush()
}

/// Whether only an in-place write reaches the right file and keeps its
/// links: `path` is a symlink (on Windows, any reparse point) or has other
/// hard links.
fn must_write_in_place(path: &Path) -> io::Result<bool> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    if meta.file_type().is_symlink() {
        return Ok(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() > 1 {
            return Ok(true);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Writes `data` to `path` through a temporary file (`.<name>.<pid>.<n>.tmp`)
/// in the same directory, which replaces `path` once its contents are on
/// disk: a crash, a full disk or a lost connection midway leaves the old
/// file, never a truncated one. The temporary file is removed when the
/// write fails. An existing file's permissions carry over (Unix mode bits).
///
/// Where no temporary file can be made (a directory the user cannot write
/// to, holding a file they can) or it cannot take the file's place (on
/// Windows, a file another program holds open), the file is written in
/// place as before.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no file name", path.display()),
        )
    })?;
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(name);
    temp_name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let temp = dir.join(temp_name);
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temp)
    {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            return write_in_place(path, data);
        }
        Err(err) => return Err(err),
    };
    let written = fill(&mut file, data).and_then(|()| {
        #[cfg(unix)]
        if let Ok(meta) = fs::metadata(path) {
            fs::set_permissions(&temp, meta.permissions())?;
        }
        Ok(())
    });
    drop(file);
    if let Err(err) = written {
        fs::remove_file(&temp).ok();
        return Err(err);
    }
    // Replaces an existing file on every platform.
    match fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            fs::remove_file(&temp).ok();
            if err.kind() == io::ErrorKind::PermissionDenied {
                write_in_place(path, data)
            } else {
                Err(err)
            }
        }
    }
}

/// Writes `data` and gets it onto the disk before the rename, so a crash
/// cannot leave the name on empty contents. File systems that cannot sync
/// (some network mounts) are taken as they are.
fn fill(file: &mut fs::File, data: &[u8]) -> io::Result<()> {
    file.write_all(data)?;
    match file.sync_all() {
        Err(err)
            if !matches!(
                err.kind(),
                io::ErrorKind::Unsupported | io::ErrorKind::InvalidInput
            ) =>
        {
            Err(err)
        }
        _ => Ok(()),
    }
}

/// What `path` is (following symlinks), or `None` if nothing is there.
pub fn kind(path: &Path) -> io::Result<Option<FileKind>> {
    match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(Some(FileKind::Dir)),
        Ok(_) => Ok(Some(FileKind::File)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Finds an executable: explicit paths as given, bare names in the project's
/// virtualenv, then `PATH`, then common per-user tool directories. Inside WSL,
/// `PATH` entries on Windows drives are skipped: those are Windows programs
/// that cannot work with the project's Linux paths.
pub fn find_executable(command: &str, project_root: &Path) -> Option<PathBuf> {
    let command_path = Path::new(command);
    if command_path.components().count() > 1 {
        return with_extensions(command_path).find(|path| path.is_file());
    }
    let windows_mounts = windows_mounts();
    let home = dirs::home_dir();
    let path_var = env::var_os("PATH");
    let dirs = venv_dirs(project_root)
        .into_iter()
        .chain(
            path_var
                .iter()
                .flat_map(env::split_paths)
                .filter(|dir| !windows_mounts.iter().any(|mount| dir.starts_with(mount))),
        )
        .chain(
            home.iter()
                .flat_map(|home| [home.join(".cargo/bin"), home.join(".local/bin")]),
        );
    for dir in dirs {
        let candidate = dir.join(command);
        if let Some(found) = with_extensions(&candidate).find(|path| path.is_file()) {
            return Some(found);
        }
    }
    None
}

fn with_extensions(path: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    let extensions: &[&str] = if cfg!(windows) && path.extension().is_none() {
        &["exe", "cmd", "bat"]
    } else {
        &[]
    };
    std::iter::once(path.to_path_buf()).chain(extensions.iter().map(|ext| path.with_extension(ext)))
}

/// Mount points of Windows drives under WSL (`drvfs`, or 9p serving drvfs).
#[cfg(target_os = "linux")]
fn windows_mounts() -> Vec<PathBuf> {
    let Ok(mounts) = fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };
    mounts.lines().filter_map(parse_windows_mount).collect()
}

#[cfg(not(target_os = "linux"))]
fn windows_mounts() -> Vec<PathBuf> {
    Vec::new()
}

/// The mount point of a `/proc/mounts` line if it is a Windows drive.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_windows_mount(line: &str) -> Option<PathBuf> {
    let mut fields = line.split(' ');
    let (_source, mount, fstype, options) = (
        fields.next()?,
        fields.next()?,
        fields.next()?,
        fields.next()?,
    );
    let drvfs = fstype == "drvfs"
        || (fstype == "9p"
            && options
                .split(',')
                .any(|option| option.starts_with("aname=drvfs")));
    // /proc/mounts escapes spaces in mount points.
    drvfs.then(|| PathBuf::from(mount.replace("\\040", " ")))
}

/// Executable directories of the project's virtualenv, if any.
fn venv_dirs(project_root: &Path) -> Vec<PathBuf> {
    let bin = if cfg!(windows) { "Scripts" } else { "bin" };
    [".venv", "venv"]
        .iter()
        .map(|name| project_root.join(name).join(bin))
        .filter(|dir| dir.is_dir())
        .collect()
}

/// The project's Python interpreter: an active virtualenv/conda env, else
/// `.venv`/`venv` in the project root.
pub fn python_interpreter(project_root: &Path) -> Option<PathBuf> {
    let python = if cfg!(windows) {
        "python.exe"
    } else {
        "python"
    };
    let from_env = ["VIRTUAL_ENV", "CONDA_PREFIX"]
        .iter()
        .filter_map(env::var_os)
        .map(PathBuf::from)
        .map(|prefix| {
            if cfg!(windows) && prefix.join(python).is_file() {
                prefix.join(python) // conda on Windows keeps python.exe at the root
            } else {
                prefix
                    .join(if cfg!(windows) { "Scripts" } else { "bin" })
                    .join(python)
            }
        });
    from_env
        .chain(
            venv_dirs(project_root)
                .into_iter()
                .map(|dir| dir.join(python)),
        )
        .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("next_code_worktree_{name}_{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn listings_put_dirs_first_and_skip_ignored_files_and_git() {
        let root = temp_dir("listing");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::write(root.join("b.txt"), "").unwrap();
        fs::write(root.join("A.txt"), "").unwrap();
        fs::write(root.join("debug.log"), "").unwrap();
        fs::write(root.join("src").join("main.rs"), "").unwrap();
        fs::write(root.join("target").join("out"), "").unwrap();

        let names: Vec<(String, bool)> = read_dir(&root)
            .unwrap()
            .into_iter()
            .map(|entry| (entry.name, entry.is_dir))
            .collect();
        let expected = [
            ("src", true),
            (".gitignore", false),
            ("A.txt", false),
            ("b.txt", false),
        ];
        assert_eq!(
            names,
            expected.map(|(name, is_dir)| (name.to_owned(), is_dir))
        );

        let main = Path::new("src")
            .join("main.rs")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            list_files(&root),
            [".gitignore", "A.txt", "b.txt", main.as_str()]
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// A repository with nested `.gitignore` files, an `.ignore` file, a
    /// whitelist, `info/exclude` and a package.
    fn nested_tree(name: &str) -> PathBuf {
        let root = temp_dir(name);
        fs::create_dir_all(root.join(".git").join("info")).unwrap();
        fs::write(root.join(".git").join("info").join("exclude"), "secret*\n").unwrap();
        fs::write(root.join(".gitignore"), "*.log\n/build/\ndist\n").unwrap();
        fs::write(root.join(".ignore"), "notes/\n").unwrap();
        for dir in [
            "build",
            "src/build",
            "src/generated",
            "src/pkg",
            "src/pkg/sub",
            "src/dist",
            "notes",
            "lib/dist",
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(root.join("src").join(".gitignore"), "generated/\n!keep.log\n").unwrap();
        for file in [
            "build/out.o",
            "src/build/nested.o",
            "src/generated/gen.py",
            "src/keep.log",
            "src/other.log",
            "src/main.py",
            "src/pkg/__init__.py",
            "src/pkg/sub/mod.py",
            "src/dist/bundle.js",
            "lib/dist/bundle.js",
            "notes/todo.md",
            "secret.txt",
            "readme.md",
        ] {
            fs::write(root.join(file), "").unwrap();
        }
        root
    }

    #[test]
    fn nested_ignore_files_apply_to_listings_and_match_the_serial_walk() {
        let root = nested_tree("nested");
        let serial = list_files_serial(&root);
        assert_eq!(list_files(&root), serial);
        let slash = |path: &str| path.replace('/', std::path::MAIN_SEPARATOR_STR);
        // `/build/` is anchored to the root (so `src/build` stays), `dist`
        // is not (so `lib/dist` goes too).
        assert_eq!(
            serial,
            [
                ".gitignore",
                ".ignore",
                "readme.md",
                "src/.gitignore",
                "src/build/nested.o",
                "src/keep.log",
                "src/main.py",
                "src/pkg/__init__.py",
                "src/pkg/sub/mod.py",
            ]
            .map(slash)
        );

        let listing = |dir: &Path| -> Vec<(String, bool, bool)> {
            read_dir(dir)
                .unwrap()
                .into_iter()
                .map(|entry| (entry.name, entry.is_dir, entry.package))
                .collect()
        };
        let expected = |entries: &[(&str, bool, bool)]| -> Vec<(String, bool, bool)> {
            entries
                .iter()
                .map(|(name, is_dir, package)| ((*name).to_owned(), *is_dir, *package))
                .collect()
        };
        // `.ignore` hides `notes/`; `info/exclude` hides `secret.txt`.
        assert_eq!(
            listing(&root),
            expected(&[
                ("lib", true, false),
                ("src", true, false),
                (".gitignore", false, false),
                (".ignore", false, false),
                ("readme.md", false, false),
            ])
        );
        assert_eq!(listing(&root.join("lib")), expected(&[]));
        // The nested file hides `generated/` and whitelists `keep.log`
        // against the root's `*.log`; `pkg` is a package.
        assert_eq!(
            listing(&root.join("src")),
            expected(&[
                ("build", true, false),
                ("pkg", true, true),
                (".gitignore", false, false),
                ("keep.log", false, false),
                ("main.py", false, false),
            ])
        );
        assert_eq!(
            listing(&root.join("src").join("pkg")),
            expected(&[("sub", true, false), ("__init__.py", false, false)])
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn edited_ignore_files_are_read_again_and_rules_need_a_repository() {
        let root = temp_dir("edited");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src").join("a.tmp"), "").unwrap();
        fs::write(root.join("src").join("b.rs"), "").unwrap();
        fs::write(root.join(".gitignore"), "*.tmp\n").unwrap();
        let names = |dir: &Path| -> Vec<String> {
            read_dir(dir)
                .unwrap()
                .into_iter()
                .map(|entry| entry.name)
                .collect()
        };
        // Outside a repository, `.gitignore` is not consulted.
        assert_eq!(names(&root.join("src")), ["a.tmp", "b.rs"]);
        assert_eq!(list_files(&root), list_files_serial(&root));
        fs::create_dir_all(root.join(".git")).unwrap();
        assert_eq!(names(&root.join("src")), ["b.rs"]);
        // A changed rule (a different size, so the stamp differs on file
        // systems with coarse timestamps) takes effect on the next listing.
        fs::write(root.join(".gitignore"), "*.rs\n*.other\n").unwrap();
        assert_eq!(names(&root.join("src")), ["a.tmp"]);
        fs::remove_file(root.join(".gitignore")).unwrap();
        assert_eq!(names(&root.join("src")), ["a.tmp", "b.rs"]);
        fs::remove_dir_all(&root).unwrap();
    }

    /// `NEXT_CODE_LIST_DIR=<a big tree> cargo test -p worktree --release --
    /// --ignored --nocapture list_files_timing` compares the parallel walk
    /// with the single-threaded one.
    #[test]
    #[ignore = "benchmark: needs NEXT_CODE_LIST_DIR"]
    fn list_files_timing() {
        let Some(dir) = env::var_os("NEXT_CODE_LIST_DIR") else {
            return;
        };
        let dir = PathBuf::from(dir);
        for _ in 0..2 {
            let started = std::time::Instant::now();
            let serial = list_files_serial(&dir);
            let serial_time = started.elapsed();
            let started = std::time::Instant::now();
            let parallel = list_files(&dir);
            let parallel_time = started.elapsed();
            assert_eq!(serial, parallel);
            println!(
                "{} files: serial {serial_time:?}, parallel {parallel_time:?}",
                serial.len()
            );
        }
    }

    #[test]
    fn missing_directories_fail_instead_of_listing_empty() {
        let missing = env::temp_dir().join("next_code_worktree_missing_dir");
        assert_eq!(
            read_dir(&missing).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(kind(&missing).unwrap(), None);
    }

    #[test]
    fn binary_and_non_utf8_files_are_not_text() {
        assert_eq!(decode_text(b"ok\n".to_vec()).unwrap(), "ok\n");
        assert_eq!(
            decode_text(vec![0xff, 0xfe]).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            decode_text(b"a\0b".to_vec()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn writes_round_trip_replace_existing_content_and_leave_no_temp_files() {
        let root = temp_dir("atomic");
        let path = root.join("file.txt");
        write_file(&path, b"first").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first");
        write_file(&path, b"second, longer").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second, longer");
        write_atomic(&path, b"3").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"3");
        let leftovers: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "file.txt")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        // A missing directory fails as a plain write would, leaving nothing.
        assert_eq!(
            write_file(&root.join("missing").join("x"), b"")
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn writes_keep_permissions_and_go_through_links() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_dir("links");
        let script = root.join("script.sh");
        fs::write(&script, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        write_file(&script, b"#!/bin/sh\necho hi\n").unwrap();
        assert_eq!(
            fs::metadata(&script).unwrap().permissions().mode() & 0o777,
            0o755
        );

        let target = root.join("target.txt");
        fs::write(&target, "old").unwrap();
        let link = root.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        write_file(&link, b"new").unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link stays a link"
        );
        assert_eq!(fs::read(&target).unwrap(), b"new");

        let hard = root.join("hard.txt");
        fs::hard_link(&target, &hard).unwrap();
        write_file(&hard, b"newer").unwrap();
        assert_eq!(
            fs::read(&target).unwrap(),
            b"newer",
            "hard links see the write"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_writable_file_in_a_read_only_directory_is_written_in_place() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_dir("read_only_dir");
        let path = root.join("note.txt");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o555)).unwrap();
        // Root can write anywhere: nothing to show then.
        let locked = fs::write(root.join("probe"), "").is_err();
        let result = write_file(&path, b"new");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        if locked {
            result.unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"new");
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn executables_are_found_in_the_project_venv_first() {
        let root = temp_dir("find_exe");
        let bin = root
            .join(".venv")
            .join(if cfg!(windows) { "Scripts" } else { "bin" });
        fs::create_dir_all(&bin).unwrap();
        let file = if cfg!(windows) { "tool.exe" } else { "tool" };
        fs::write(bin.join(file), "").unwrap();

        assert_eq!(find_executable("tool", &root), Some(bin.join(file)));
        assert_eq!(find_executable("next_code_missing_tool", &root), None);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn only_windows_drive_mounts_are_recognized() {
        let c = r"C:\ /mnt/c 9p rw,noatime,aname=drvfs;path=C:\;uid=1000 0 0";
        assert_eq!(parse_windows_mount(c), Some(PathBuf::from("/mnt/c")));
        let spaced = r"D:\ /mnt/my\040drive drvfs rw 0 0";
        assert_eq!(
            parse_windows_mount(spaced),
            Some(PathBuf::from("/mnt/my drive"))
        );
        let drivers = "drivers /usr/lib/wsl/drivers 9p ro,aname=drivers;fmask=222 0 0";
        assert_eq!(parse_windows_mount(drivers), None);
        assert_eq!(parse_windows_mount("/dev/sdc / ext4 rw 0 0"), None);
    }
}
