use crate::Extension;
use anyhow::{Context, Result, bail, ensure};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use extension_api::{API_VERSION, Manifest, Permission};
use std::{
    collections::HashSet,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_MODULE_BYTES: usize = 16 * 1024 * 1024;
const STAGING_PREFIX: &str = ".nextcode-install-";
static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

// Walk every path component with no-follow opens. Holding directory handles also
// prevents a concurrent ancestor rename/symlink swap from redirecting file I/O.
pub(super) fn open_directory(path: &Path, create: bool) -> Result<Dir> {
    let absolute = std::path::absolute(path).context("cannot resolve package directory")?;
    let mut anchor = PathBuf::new();
    let mut components = absolute.components().peekable();
    while matches!(
        components.peek(),
        Some(Component::Prefix(_) | Component::RootDir)
    ) {
        anchor.push(components.next().unwrap().as_os_str());
    }
    ensure!(
        !anchor.as_os_str().is_empty(),
        "directory must have an absolute filesystem root"
    );
    let mut dir = Dir::open_ambient_dir(&anchor, ambient_authority())
        .with_context(|| format!("cannot open filesystem root {}", anchor.display()))?;
    for component in components {
        match component {
            Component::Normal(name) => {
                if create {
                    match dir.create_dir(name) {
                        Ok(()) => (),
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
                        Err(error) => {
                            return Err(error).context("cannot create extension directory");
                        }
                    }
                }
                dir = dir.open_dir_nofollow(name).with_context(|| {
                    format!(
                        "cannot open directory component {:?}; symlinks are forbidden",
                        name
                    )
                })?;
            }
            Component::CurDir => (),
            Component::ParentDir => dir = dir.open_parent_dir(ambient_authority())?,
            _ => bail!("unsupported directory path component"),
        }
    }
    Ok(dir)
}

fn read_file(dir: &Dir, name: &str, limit: usize) -> Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        // A package FIFO must not hang a background loader before metadata checks.
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = dir
        .open_with(name, &options)
        .with_context(|| format!("cannot open '{name}' (package files must not be symlinks)"))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file(),
        "package file '{name}' must be a regular file"
    );
    ensure!(
        metadata.len() <= limit as u64,
        "package file '{name}' exceeds {limit} bytes"
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "package file '{name}' exceeds {limit} bytes"
    );
    Ok(bytes)
}

pub(super) fn read_package(dir: &Dir) -> Result<(Manifest, Vec<u8>)> {
    let manifest: Manifest =
        serde_json::from_slice(&read_file(dir, "extension.json", MAX_MANIFEST_BYTES)?)
            .context("invalid extension.json")?;
    validate_manifest(&manifest)?;
    let wasm = read_file(dir, &manifest.module, MAX_MODULE_BYTES)?;
    Ok((manifest, wasm))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && id
            .bytes()
            .last()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && id.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.' || b == b'_'
        })
        && !id.contains("..")
        && !is_windows_device(id)
}

fn is_windows_device(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
        || (stem.len() == 4
            && (stem.starts_with("com") || stem.starts_with("lpt"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
}

fn valid_label(label: &str) -> bool {
    !label.trim().is_empty() && label.len() <= 256 && !label.chars().any(char::is_control)
}

pub(super) fn validate_manifest(manifest: &Manifest) -> Result<()> {
    ensure!(
        valid_id(&manifest.id),
        "invalid extension id: use 1–128 lowercase ASCII letters/digits with '.', '_' or '-' separators; start with a letter and end with a letter/digit"
    );
    ensure!(
        valid_label(&manifest.name),
        "extension name must be nonempty, at most 256 bytes and contain no control characters"
    );
    ensure!(
        manifest.version.len() <= 128 && semver::Version::parse(&manifest.version).is_ok(),
        "extension version must be a valid semantic version, e.g. 1.0.0"
    );
    ensure!(
        matches!(manifest.api_version, 2 | API_VERSION),
        "unsupported extension API version {}; host supports {}",
        manifest.api_version,
        API_VERSION
    );
    let module = &manifest.module;
    ensure!(
        module.len() > 5
            && module.len() <= 128
            && module.ends_with(".wasm")
            && module
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
            && !module.starts_with('.')
            && !module.contains("..")
            && !is_windows_device(module),
        "module must be one plain .wasm filename (no paths, traversal or device names)"
    );
    ensure!(
        (!manifest.commands.is_empty() || !manifest.panels.is_empty())
            && manifest.commands.len() <= 256
            && manifest.panels.len() <= 8,
        "manifest must define commands or panels (at most 256 commands and 8 panels)"
    );
    let mut commands = HashSet::new();
    for command in &manifest.commands {
        ensure!(valid_id(&command.id), "invalid command id '{}'", command.id);
        ensure!(
            commands.insert(&command.id),
            "duplicate command id '{}'",
            command.id
        );
        ensure!(
            valid_label(&command.title),
            "command '{}' has an invalid title",
            command.id
        );
    }
    let mut panels = HashSet::new();
    for panel in &manifest.panels {
        ensure!(valid_id(&panel.id), "invalid panel id");
        ensure!(panels.insert(&panel.id), "duplicate panel id");
        ensure!(valid_label(&panel.title), "invalid panel title");
    }
    let mut permissions = HashSet::new();
    for permission in &manifest.permissions {
        ensure!(
            permissions.insert(permission),
            "duplicate permission '{permission:?}'"
        );
    }
    ensure!(
        !permissions.contains(&Permission::EditDocument)
            || permissions.contains(&Permission::ReadDocument),
        "edit-document requires read-document permission"
    );
    ensure!(manifest.processes.len() <= 16, "too many declared programs");
    let mut programs = HashSet::new();
    for process in &manifest.processes {
        let name = &process.program;
        ensure!(
            !name.is_empty()
                && name.len() <= 128
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                && !name.starts_with('.')
                && !is_windows_device(name),
            "program must be a bare executable name"
        );
        ensure!(programs.insert(name), "duplicate declared program");
    }
    ensure!(
        manifest.processes.is_empty() == !permissions.contains(&Permission::RunProcess),
        "run-process requires declared programs, and declared programs require run-process"
    );
    ensure!(
        manifest.api_version >= 3 || manifest.processes.is_empty(),
        "processes require API 3"
    );
    Ok(())
}

struct Staging<'a> {
    root: &'a Dir,
    name: &'a str,
    module: &'a str,
    directory: Option<Dir>,
    identity: (u64, u64),
    armed: bool,
}

fn directory_identity(metadata: &cap_std::fs::Metadata) -> (u64, u64) {
    use cap_fs_ext::MetadataExt;
    (metadata.dev(), metadata.ino())
}

impl Drop for Staging<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let directory = self.directory.take().or_else(|| {
            let directory = self.root.open_dir_nofollow(self.name).ok()?;
            let metadata = directory.dir_metadata().ok()?;
            (directory_identity(&metadata) == self.identity).then_some(directory)
        });
        if let Some(directory) = directory {
            // Only these two files belong to this operation. Never recurse
            // through an installation-root entry that another writer replaced.
            let _ = directory.remove_file_or_symlink("extension.json");
            let _ = directory.remove_file_or_symlink(self.module);
            let _ = directory.remove_open_dir();
        }
    }
}

pub(super) fn install(source: &Path, root: &Path) -> Result<Extension> {
    let source_dir = open_directory(source, false)?;
    let (manifest, wasm) = read_package(&source_dir)?;
    // Compile the exact immutable bytes we will install; never re-read the source.
    let mut extension = Extension::compile(root.join(&manifest.id), manifest, &wasm)?;
    let root_dir = open_directory(root, true)?;
    match root_dir.symlink_metadata(&extension.manifest.id) {
        Ok(_) => bail!(
            "extension '{}' is already installed; existing targets are never replaced",
            extension.manifest.id
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error).context("cannot inspect installation target"),
    }
    let mut staging = None;
    for _ in 0..128 {
        let name = format!(
            "{STAGING_PREFIX}{}-{}",
            std::process::id(),
            NEXT_STAGE.fetch_add(1, Ordering::Relaxed)
        );
        match root_dir.create_dir(&name) {
            Ok(()) => {
                staging = Some(name);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
            Err(error) => {
                return Err(error).context("cannot create installation staging directory");
            }
        }
    }
    let name = staging.context("cannot reserve an installation staging directory")?;
    let stage_dir = match root_dir.open_dir_nofollow(&name) {
        Ok(dir) => dir,
        Err(error) => {
            let _ = root_dir.remove_dir(&name);
            return Err(error).context("cannot open installation staging directory");
        }
    };
    let identity = directory_identity(&stage_dir.dir_metadata()?);
    let mut stage = Staging {
        root: &root_dir,
        name: &name,
        module: &extension.manifest.module,
        directory: Some(stage_dir),
        identity,
        armed: true,
    };
    let dir = stage.directory.as_ref().unwrap();
    write_new(
        dir,
        "extension.json",
        &serde_json::to_vec(&extension.manifest)?,
    )?;
    write_new(dir, &extension.manifest.module, &wasm)?;
    // Windows path-based directory renames must not retain our read capability.
    // The rollback guard can reopen only this same directory through root.
    #[cfg(windows)]
    drop(stage.directory.take());
    let metadata = root_dir.symlink_metadata(&name)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && directory_identity(&metadata) == stage.identity,
        "installation staging directory changed before publication"
    );
    rename_exclusive(&root_dir, &name, &extension.manifest.id)
        .context("cannot atomically install extension; target may already exist or filesystem may not support exclusive directory rename")?;
    // The moved directory now belongs to the user; never clean it up on drop.
    stage.armed = false;
    drop(stage);
    extension.directory = root.join(&extension.manifest.id);
    Ok(extension)
}

fn write_new(dir: &Dir, name: &str, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let mut file = dir.open_with(name, &options)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_exclusive(dir: &Dir, from: &str, to: &str) -> io::Result<()> {
    rustix::fs::renameat_with(dir, from, dir, to, rustix::fs::RenameFlags::NOREPLACE)?;
    Ok(())
}

#[cfg(target_vendor = "apple")]
fn rename_exclusive(dir: &Dir, from: &str, to: &str) -> io::Result<()> {
    use std::{ffi::CString, os::fd::AsRawFd};
    let from = CString::new(from)?;
    let to = CString::new(to)?;
    // SAFETY: valid NUL-terminated names and a live directory descriptor.
    let result = unsafe {
        libc::renameatx_np(
            dir.as_raw_fd(),
            from.as_ptr(),
            dir.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn rename_exclusive(dir: &Dir, from: &str, to: &str) -> io::Result<()> {
    // Windows rename cannot replace an existing directory, even an empty one.
    dir.rename(from, dir, to)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    windows
)))]
fn rename_exclusive(_: &Dir, _: &str, _: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic exclusive directory rename is unsupported on this platform",
    ))
}

pub(super) fn discover(root: &Path) -> Vec<(PathBuf, Result<Extension>)> {
    let dir = match open_directory(root, false) {
        Ok(dir) => dir,
        Err(error) => {
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::NotFound)
            {
                return Vec::new();
            }
            return vec![(root.to_path_buf(), Err(error))];
        }
    };
    let entries = match dir.entries() {
        Ok(entries) => entries,
        Err(error) => return vec![(root.to_path_buf(), Err(error.into()))],
    };
    let mut results = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                results.push((root.to_path_buf(), Err(error.into())));
                continue;
            }
        };
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(STAGING_PREFIX) {
            continue;
        }
        let path = root.join(&name);
        let kind = match entry.file_type() {
            Ok(kind) => kind,
            Err(error) => {
                results.push((path, Err(error.into())));
                continue;
            }
        };
        if !kind.is_dir() && !kind.is_symlink() {
            continue;
        }
        let result = (|| {
            let package = dir
                .open_dir_nofollow(&name)
                .context("installed extension directory must not be a symlink")?;
            let (manifest, wasm) = read_package(&package)?;
            ensure!(
                name.to_str() == Some(manifest.id.as_str()),
                "installed directory name must match manifest id '{}'",
                manifest.id
            );
            Extension::compile(path.clone(), manifest, &wasm)
        })();
        results.push((path, result));
    }
    results.sort_by(|a, b| a.0.cmp(&b.0));
    results
}
