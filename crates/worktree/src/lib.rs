//! Project file operations that run where the files live: in the editor
//! process for local projects, and in `next_code_agent` for WSL projects.
//!
//! Both sides call these same functions, so listing, ignore rules and tool
//! lookup behave identically whichever side of the agent protocol runs them.

use std::{
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};

/// One child of a listed directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileKind {
    File,
    Dir,
}

/// Lists one directory, honoring .gitignore (inside git repos), dirs first,
/// then case-insensitively by name. `.git` itself is hidden.
pub fn read_dir(dir: &Path) -> io::Result<Vec<DirEntry>> {
    if !fs::metadata(dir)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} is not a directory", dir.display()),
        ));
    }
    let mut entries: Vec<DirEntry> = WalkBuilder::new(dir)
        .max_depth(Some(1))
        .hidden(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.depth() == 1)
        .map(|entry| DirEntry {
            is_dir: entry.file_type().is_some_and(|kind| kind.is_dir()),
            name: entry.file_name().to_string_lossy().into_owned(),
        })
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

/// All non-ignored files under `root`, as sorted root-relative paths using
/// the platform separator.
pub fn list_files(root: &Path) -> Vec<String> {
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

/// Text of a file for editing: UTF-8 without NUL bytes.
pub fn decode_text(bytes: Vec<u8>) -> io::Result<String> {
    let text = String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file is not valid UTF-8"))?;
    if text.contains('\0') {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "binary file"));
    }
    Ok(text)
}

/// Replaces a file's contents in place, keeping its permissions and links.
pub fn write_file(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(data)?;
    file.flush()
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
