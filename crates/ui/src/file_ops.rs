//! File and folder operations of the Files tree and headless mode, for every
//! kind of project (local, WSL, dev container): create, rename/move, copy
//! (with ` copy` names), delete (to the Recycle Bin for local Windows
//! projects), name checks per platform, and the LSP file-rename requests
//! (`workspace/willRenameFiles`, `workspace/didRenameFiles`).
//!
//! Everything here blocks (remote projects go through the agent): run it
//! off the UI thread.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use lsp::types::{
    FileOperationFilter, FileOperationPatternKind, FileOperationRegistrationOptions, FileRename,
    RenameFilesParams, WorkspaceEdit, notification::DidRenameFiles, request::WillRenameFiles,
};
use worktree::FileKind;

use crate::{
    file_types::{self, Base, FileType},
    lsp_store::RunningServer,
    project::Project,
};

/// What New… creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewKind {
    /// A file, of a type when chosen (its name, place and starter text:
    /// see [`file_types`]); without one the name is used as typed.
    File(Option<&'static FileType>),
    Folder,
    /// A folder with an empty `__init__.py`.
    PythonPackage,
}

impl NewKind {
    pub fn is_dir(self) -> bool {
        matches!(self, Self::Folder | Self::PythonPackage)
    }
}

/// Whether names follow Windows rules: a local project on Windows. WSL
/// and container projects follow Linux rules.
pub fn windows_names(project: &Project) -> bool {
    cfg!(windows) && !project.is_remote()
}

const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Checks one file or folder name (no separators).
pub fn check_name(name: &str, windows: bool) -> Result<(), String> {
    if name.is_empty() {
        return Err("A file or folder name must be provided".into());
    }
    if name == "." || name == ".." {
        return Err(format!("`{name}` is not a valid file or folder name"));
    }
    if name.contains('/') || (windows && name.contains('\\')) {
        let separator = if name.contains('/') { '/' } else { '\\' };
        return Err(format!("A name cannot contain `{separator}`"));
    }
    if name.contains('\0') {
        return Err("A name cannot contain a NUL character".into());
    }
    if windows {
        if let Some(c) = name.chars().find(|c| "<>:\"|?*".contains(*c)) {
            return Err(format!("A name cannot contain `{c}` on Windows"));
        }
        if name.chars().any(|c| (c as u32) < 32) {
            return Err("A name cannot contain control characters on Windows".into());
        }
        if name.ends_with('.') || name.ends_with(' ') {
            return Err("A name cannot end with a dot or a space on Windows".into());
        }
        let stem = name.split('.').next().unwrap_or_default().trim_end();
        if let Some(reserved) = WINDOWS_RESERVED
            .iter()
            .find(|reserved| reserved.eq_ignore_ascii_case(stem))
        {
            return Err(format!("`{reserved}` is a reserved name on Windows"));
        }
    }
    if name.len() > 255 {
        return Err("A name can have at most 255 bytes".into());
    }
    Ok(())
}

/// The folders and name of a typed new entry: `pkg/sub/mod.py` also creates
/// `pkg` and `pkg/sub` (`\` separates too on Windows). Each part is checked.
pub fn new_entry_parts(input: &str, windows: bool) -> Result<Vec<&str>, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("A file or folder name must be provided".into());
    }
    let parts: Vec<&str> = input
        .split(|c| c == '/' || (windows && c == '\\'))
        .collect();
    if parts.iter().any(|part| part.is_empty()) {
        return Err("A path cannot have empty parts (a leading, trailing or double `/`)".into());
    }
    for part in &parts {
        check_name(part, windows)?;
    }
    Ok(parts)
}

/// Where a new entry typed as `input` in `dir` goes for `kind`, and the
/// `/`-separated path typed there (see [`file_types::new_path`]): typed
/// files of some types go to the folder's or the project's root.
pub fn new_entry_input(
    project: &Project,
    dir: &Path,
    input: &str,
    kind: NewKind,
) -> Result<(PathBuf, String), String> {
    let NewKind::File(file_type) = kind else {
        return Ok((dir.to_path_buf(), input.trim().to_owned()));
    };
    let (base, relative) = file_types::new_path(file_type, input, windows_names(project))?;
    let base = match base {
        Base::Dir => dir.to_path_buf(),
        Base::FolderRoot => project
            .root_index_of(dir)
            .map_or(project.root(), |index| &project.roots()[index].path)
            .to_path_buf(),
        Base::ProjectRoot => project.root().to_path_buf(),
    };
    Ok((base, relative))
}

/// Whether two names are the same entry: equal, or equal ignoring case
/// on Windows.
pub fn same_name(a: &str, b: &str, windows: bool) -> bool {
    a == b || (windows && a.to_lowercase() == b.to_lowercase())
}

/// Whether `path` is `ancestor` or inside it (ignoring case on Windows).
pub fn is_within(path: &Path, ancestor: &Path, windows: bool) -> bool {
    if windows {
        let lower = |path: &Path| PathBuf::from(path.to_string_lossy().to_lowercase());
        lower(path).starts_with(lower(ancestor))
    } else {
        path.starts_with(ancestor)
    }
}

/// `paths` without those inside another one of them (deleting or moving a
/// folder takes its contents along), in their order, without duplicates.
pub fn top_level(paths: &[PathBuf], windows: bool) -> Vec<PathBuf> {
    let mut kept: Vec<PathBuf> = Vec::new();
    for path in paths {
        let covered = paths.iter().any(|other| {
            other != path && is_within(path, other, windows) && !is_within(other, path, windows)
        });
        if !covered && !kept.contains(path) {
            kept.push(path.clone());
        }
    }
    kept
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn exists_error(path: &Path) -> io::Error {
    let place = path
        .parent()
        .map(|parent| format!(" in {}", parent.display()))
        .unwrap_or_default();
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("`{}` already exists{place}", name_of(path)),
    )
}

/// Copies OS file drops from this computer into a project, including a remote
/// project. Source paths must never be resolved through the project's backend.
/// Links and special files are refused rather than followed outside a dropped
/// folder. Existing entries get the same free names as tree copies.
pub fn import_local(
    project: &Project,
    sources: &[PathBuf],
    dir: &Path,
) -> io::Result<Vec<PathBuf>> {
    fn import(project: &Project, source: &Path, target: &Path) -> io::Result<()> {
        let metadata = std::fs::symlink_metadata(source)?;
        if metadata.is_dir() {
            project.create_dir_all(target)?;
            for entry in std::fs::read_dir(source)? {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| invalid("Dropped file names must be valid UTF-8".into()))?;
                check_name(&name, windows_names(project)).map_err(invalid)?;
                import(project, &entry.path(), &project.join(target, &name))?;
            }
            Ok(())
        } else if metadata.is_file() {
            // Leave room for protocol framing; reads and writes use one buffer.
            if project.is_remote() && metadata.len() > (proto::MAX_FRAME_LEN / 2) as u64 {
                return Err(invalid(
                    "Dropped files on remote projects must be at most 128 MiB".into(),
                ));
            }
            let data = std::fs::read(source)?;
            project.create_file(target)?;
            project.write(target, data)
        } else {
            Err(invalid(format!(
                "Cannot import a link or special file: {}",
                source.display()
            )))
        }
    }

    let mut imported = Vec::new();
    for source in top_level(sources, cfg!(windows)) {
        let metadata = std::fs::symlink_metadata(&source)?;
        let name = source
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid("Dropped file names must be valid UTF-8".into()))?;
        check_name(name, windows_names(project)).map_err(invalid)?;
        // On local projects, canonical paths catch aliases through symlinks too.
        if !project.is_remote()
            && metadata.is_dir()
            && std::fs::canonicalize(dir)?.starts_with(std::fs::canonicalize(&source)?)
        {
            return Err(invalid(format!("Cannot copy `{name}` into itself")));
        }
        let mut index = 0;
        let target = loop {
            let path = project.join(dir, &copy_name(name, metadata.is_dir(), index));
            if project.kind(&path)?.is_none() {
                break path;
            }
            index += 1;
        };
        import(project, &source, &target)?;
        imported.push(target);
    }
    Ok(imported)
}

/// Creates the entry typed as `input` in `dir`; see [`new_entry_input`]
/// and [`new_entry_parts`]. Returns the new file or folder.
pub fn create(project: &Project, dir: &Path, input: &str, kind: NewKind) -> io::Result<PathBuf> {
    let (base, relative) = new_entry_input(project, dir, input, kind).map_err(invalid)?;
    if let NewKind::File(Some(file_type)) = kind
        && file_types::picks_free_name(Some(file_type), input)
    {
        for n in 1.. {
            let path = project.join(&base, &file_types::numbered(file_type, n));
            if project.kind(&path)?.is_none() {
                return create_at(project, &path, kind);
            }
        }
    }
    let parts = new_entry_parts(&relative, windows_names(project)).map_err(invalid)?;
    create_at(project, &project.join(&base, &parts.join("/")), kind)
}

/// Creates `path` (and its missing parent folders) with the starter text
/// of its type; fails if something is there already.
pub fn create_at(project: &Project, path: &Path, kind: NewKind) -> io::Result<PathBuf> {
    if project.kind(path)?.is_some() {
        return Err(exists_error(path));
    }
    if let Some(parent) = path.parent()
        && project.kind(parent)?.is_none()
    {
        project.create_dir_all(parent)?;
    }
    match kind {
        NewKind::Folder => project.create_dir_all(path)?,
        NewKind::PythonPackage => {
            project.create_dir_all(path)?;
            project.create_file(&project.join(path, "__init__.py"))?;
        }
        NewKind::File(file_type) => {
            project.create_file(path)?;
            if let Some(file_type) = file_type {
                write_template(project, path, file_type)?;
            }
        }
    }
    Ok(path.to_path_buf())
}

/// Fills a new file of `file_type` with its starter text; a workflow's
/// folder also gets the schema its first line names.
fn write_template(project: &Project, path: &Path, file_type: &FileType) -> io::Result<()> {
    let root = project
        .root_index_of(path)
        .map_or(project.root(), |index| &project.roots()[index].path);
    if let Some(text) = file_types::template(file_type, &name_of(path), &name_of(root)) {
        project.write(path, text.into_bytes())?;
    }
    if file_types::is_workflow(file_type)
        && let Some(dir) = path.parent()
    {
        let schema = project.join(dir, workflow::SCHEMA_FILE);
        if project.read_text(&schema).ok().as_deref() != Some(workflow::SCHEMA) {
            project.write(&schema, workflow::SCHEMA.as_bytes().to_vec())?;
        }
    }
    Ok(())
}

/// `path` renamed to `name` in the same folder.
pub fn sibling(project: &Project, path: &Path, name: &str) -> PathBuf {
    match path.parent() {
        Some(parent) => project.join(parent, name),
        None => PathBuf::from(name),
    }
}

/// Refuses moving `from` to `to` when something is at `to` (a case-only
/// rename on Windows is fine), or a folder into itself.
pub fn check_move(project: &Project, from: &Path, to: &Path) -> io::Result<()> {
    if from == to {
        return Ok(());
    }
    let windows = windows_names(project);
    let case_only = windows && same_name(&from.to_string_lossy(), &to.to_string_lossy(), true);
    if !case_only && is_within(to, from, windows) {
        return Err(invalid(format!(
            "Cannot move `{}` into itself",
            name_of(from)
        )));
    }
    if project.kind(from)?.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} does not exist", from.display()),
        ));
    }
    if !case_only && project.kind(to)?.is_some() {
        return Err(exists_error(to));
    }
    Ok(())
}

/// Renames or moves a file or folder; see [`check_move`].
pub fn move_path(project: &Project, from: &Path, to: &Path) -> io::Result<()> {
    if from == to {
        return Ok(());
    }
    check_move(project, from, to)?;
    project.rename(from, to)
}

/// Where `sources` go when moved into `dir`: `(from, to)` pairs, without
/// those already there and those inside another source.
pub fn move_targets(project: &Project, sources: &[PathBuf], dir: &Path) -> Vec<(PathBuf, PathBuf)> {
    top_level(sources, windows_names(project))
        .into_iter()
        .filter(|source| source.parent() != Some(dir))
        .map(|source| {
            let to = project.join(dir, &name_of(&source));
            (source, to)
        })
        .collect()
}

/// The `index`th name for a copy of `name` (Finder and VS Code style):
/// `a copy.py`, then `a copy 2.py`, …; folders and dotfiles have no
/// extension to keep last.
pub fn copy_name(name: &str, is_dir: bool, index: usize) -> String {
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if !is_dir && dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    match index {
        0 => name.to_owned(),
        1 => format!("{stem} copy{extension}"),
        n => format!("{stem} copy {n}{extension}"),
    }
}

/// Copies `sources` into `dir`, keeping names that are free there and
/// giving the others ` copy` names (see [`copy_name`]). Returns the new
/// paths. Refuses copying a folder into itself.
pub fn copy_into(project: &Project, sources: &[PathBuf], dir: &Path) -> io::Result<Vec<PathBuf>> {
    let windows = windows_names(project);
    let sources = top_level(sources, windows);
    for source in &sources {
        if is_within(dir, source, windows) && project.kind(source)? == Some(FileKind::Dir) {
            return Err(invalid(format!(
                "Cannot copy `{}` into itself",
                name_of(source)
            )));
        }
    }
    let mut copies = Vec::new();
    for source in &sources {
        let is_dir = match project.kind(source)? {
            Some(kind) => kind == FileKind::Dir,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{} does not exist", source.display()),
                ));
            }
        };
        let name = name_of(source);
        let mut index = 0;
        let target = loop {
            let candidate = project.join(dir, &copy_name(&name, is_dir, index));
            if project.kind(&candidate)?.is_none() {
                break candidate;
            }
            index += 1;
        };
        project.copy(source, &target)?;
        copies.push(target);
    }
    Ok(copies)
}

/// Copies `path` next to itself (`a copy.py`).
pub fn duplicate(project: &Project, path: &Path) -> io::Result<PathBuf> {
    let dir = path.parent().unwrap_or(&project.root_for(path).path);
    let mut copies = copy_into(project, &[path.to_path_buf()], dir)?;
    copies
        .pop()
        .ok_or_else(|| io::Error::other("nothing was copied"))
}

/// Deletes files and folders (with their contents): to the Recycle Bin
/// when `trash` and the project has one ([`Project::can_trash`]), else for
/// good.
pub fn delete(project: &Project, paths: &[PathBuf], trash: bool) -> io::Result<()> {
    let paths = top_level(paths, windows_names(project));
    if trash && project.can_trash() {
        return project.trash(&paths);
    }
    for path in &paths {
        project.remove(path, true)?;
    }
    Ok(())
}

/// A rename for the language servers: `(from, to, is_dir)`.
pub type FileMove = (PathBuf, PathBuf, bool);

/// The rename requests (`will`) or notifications (`!will`) to send: each
/// server that registered for them, with the moves its filters match.
pub fn rename_requests(
    servers: &[Arc<RunningServer>],
    moves: &[FileMove],
    will: bool,
) -> Vec<(Arc<RunningServer>, RenameFilesParams)> {
    servers
        .iter()
        .filter_map(|server| {
            let operations = server
                .capabilities
                .workspace
                .as_ref()?
                .file_operations
                .as_ref()?;
            let options = if will {
                operations.will_rename.as_ref()?
            } else {
                operations.did_rename.as_ref()?
            };
            let files: Vec<FileRename> = moves
                .iter()
                .filter(|(from, _, is_dir)| registered(options, from, *is_dir))
                .filter_map(|(from, to, _)| {
                    Some(FileRename {
                        old_uri: lsp::path_to_uri(from)?.as_str().to_owned(),
                        new_uri: lsp::path_to_uri(to)?.as_str().to_owned(),
                    })
                })
                .collect();
            (!files.is_empty()).then(|| (server.clone(), RenameFilesParams { files }))
        })
        .collect()
}

fn registered(options: &FileOperationRegistrationOptions, path: &Path, is_dir: bool) -> bool {
    options
        .filters
        .iter()
        .any(|filter| filter_matches(filter, path, is_dir))
}

fn filter_matches(filter: &FileOperationFilter, path: &Path, is_dir: bool) -> bool {
    if filter
        .scheme
        .as_deref()
        .is_some_and(|scheme| scheme != "file")
    {
        return false;
    }
    let pattern = &filter.pattern;
    match pattern.matches {
        Some(FileOperationPatternKind::File) if is_dir => return false,
        Some(FileOperationPatternKind::Folder) if !is_dir => return false,
        _ => {}
    }
    let ignore_case = pattern
        .options
        .as_ref()
        .and_then(|options| options.ignore_case)
        .unwrap_or(false);
    glob_matches(
        &pattern.glob,
        &path.to_string_lossy().replace('\\', "/"),
        ignore_case,
    )
}

/// Matches an LSP glob (`*`, `?`, `**`, `{a,b}`, `[0-9]`, `[!a]`) against a
/// `/`-separated path. A glob without a leading `/` or `**` may match the
/// path's end (servers send `**/*.rs`, but also `*.py`).
pub fn glob_matches(glob: &str, path: &str, ignore_case: bool) -> bool {
    let mut regex = String::from("^");
    if !glob.starts_with('/') && !glob.starts_with("**") {
        regex.push_str("(?:.*/)?");
    }
    let chars: Vec<char> = glob.chars().collect();
    let mut index = 0;
    let mut braces = 0;
    while index < chars.len() {
        let c = chars[index];
        match c {
            '*' if chars.get(index + 1) == Some(&'*') => {
                index += 1;
                if chars.get(index + 1) == Some(&'/') {
                    index += 1;
                    regex.push_str("(?:.*/)?");
                } else {
                    regex.push_str(".*");
                }
            }
            '*' => regex.push_str("[^/]*"),
            '?' => regex.push_str("[^/]"),
            '{' => {
                braces += 1;
                regex.push_str("(?:");
            }
            '}' if braces > 0 => {
                braces -= 1;
                regex.push(')');
            }
            ',' if braces > 0 => regex.push('|'),
            '[' => match chars[index + 1..].iter().position(|&c| c == ']') {
                Some(length) => {
                    let class: String = chars[index + 1..index + 1 + length].iter().collect();
                    let class = match class.strip_prefix('!') {
                        Some(rest) => format!("^{rest}"),
                        None => class,
                    };
                    regex.push('[');
                    regex.push_str(&class.replace('\\', "\\\\"));
                    regex.push(']');
                    index += length + 1;
                }
                None => regex.push_str("\\["),
            },
            c => regex.push_str(&regex::escape(&c.to_string())),
        }
        index += 1;
    }
    regex.push('$');
    regex::RegexBuilder::new(&regex)
        .case_insensitive(ignore_case)
        .build()
        .is_ok_and(|regex| regex.is_match(path))
}

/// Asks a server for the edits a rename needs (e.g. `mod` lines and
/// imports), before the files move.
pub async fn will_rename(
    server: &RunningServer,
    params: RenameFilesParams,
) -> Result<Option<WorkspaceEdit>, String> {
    server
        .connection
        .request::<WillRenameFiles>(params)
        .await
        .map_err(|err| match err {
            lsp::Error::Response { message, .. } => message,
            err => err.to_string(),
        })
}

/// Tells the servers that registered for it which files moved.
pub fn did_rename(servers: &[Arc<RunningServer>], moves: &[FileMove]) {
    for (server, params) in rename_requests(servers, moves, false) {
        server.connection.notify::<DidRenameFiles>(params);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn temp_project(name: &str) -> (PathBuf, Project) {
        let root = std::env::temp_dir().join(format!("nc-file-ops-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        (root.clone(), Project::local(root))
    }

    #[test]
    fn external_files_import_binary_folders_and_pick_free_names() {
        let (root, project) = temp_project("import");
        check_imports(&root, &project);
        let err = import_local(&project, std::slice::from_ref(&root), &root).unwrap_err();
        assert!(err.to_string().contains("into itself"));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn external_files_upload_through_the_agent() {
        let (root, _) = temp_project("import-remote");
        let project = Project::test_ssh(root.clone());
        check_imports(&root, &project);
        // A local symlink must not walk an arbitrary tree on the local machine.
        let link = root.join("source-link");
        std::os::unix::fs::symlink(root.join("source"), &link).unwrap();
        let err = import_local(&project, &[link], &root.join("destination")).unwrap_err();
        assert!(err.to_string().contains("link or special file"));
        fs::remove_dir_all(root).unwrap();
    }

    fn check_imports(root: &Path, project: &Project) {
        let source = root.join("source");
        fs::create_dir_all(source.join("folder/empty")).unwrap();
        fs::write(source.join("folder/data.bin"), [0, 255, 128, 10]).unwrap();
        fs::write(source.join("notes.txt"), "original").unwrap();
        let destination = root.join("destination");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("notes.txt"), "keep me").unwrap();
        let imported = import_local(
            project,
            &[
                source.join("folder"),
                source.join("folder/data.bin"),
                source.join("notes.txt"),
            ],
            &destination,
        )
        .unwrap();
        assert_eq!(
            imported,
            [
                destination.join("folder"),
                destination.join("notes copy.txt")
            ]
        );
        assert_eq!(
            fs::read(destination.join("folder/data.bin")).unwrap(),
            [0, 255, 128, 10]
        );
        assert!(destination.join("folder/empty").is_dir());
        assert_eq!(
            fs::read_to_string(destination.join("notes.txt")).unwrap(),
            "keep me"
        );
        assert_eq!(
            fs::read_to_string(destination.join("notes copy.txt")).unwrap(),
            "original"
        );
        assert!(source.join("notes.txt").is_file());
    }

    #[test]
    fn names_are_checked_per_platform() {
        assert!(check_name("main.py", false).is_ok());
        assert!(check_name("a:b?.txt", false).is_ok());
        assert!(check_name("back\\slash", false).is_ok());
        assert_eq!(
            check_name("", false).unwrap_err(),
            "A file or folder name must be provided"
        );
        assert!(check_name("..", false).is_err());
        assert_eq!(
            check_name("a/b", false).unwrap_err(),
            "A name cannot contain `/`"
        );
        assert_eq!(
            check_name("a:b", true).unwrap_err(),
            "A name cannot contain `:` on Windows"
        );
        assert_eq!(
            check_name("a\\b", true).unwrap_err(),
            "A name cannot contain `\\`"
        );
        assert!(check_name("tab\there", true).is_err());
        assert!(check_name("trailing.", true).is_err());
        assert!(check_name("trailing ", true).is_err());
        assert_eq!(
            check_name("nul.txt", true).unwrap_err(),
            "`NUL` is a reserved name on Windows"
        );
        assert!(check_name("nul.txt", false).is_ok());
        assert!(check_name("console.txt", true).is_ok());
        assert!(check_name(&"x".repeat(256), false).is_err());
    }

    #[test]
    fn typed_new_entries_may_name_folders_on_the_way() {
        assert_eq!(
            new_entry_parts(" pkg/mod.py ", false).unwrap(),
            ["pkg", "mod.py"]
        );
        assert_eq!(
            new_entry_parts("pkg\\mod.py", true).unwrap(),
            ["pkg", "mod.py"]
        );
        assert_eq!(
            new_entry_parts("pkg\\mod.py", false).unwrap(),
            ["pkg\\mod.py"]
        );
        assert!(new_entry_parts("a//b", false).is_err());
        assert!(new_entry_parts("/abs", false).is_err());
        assert!(new_entry_parts("dir/", false).is_err());
        assert!(new_entry_parts("ok/../x", false).is_err());
    }

    #[test]
    fn copies_get_copy_names_before_the_extension() {
        assert_eq!(copy_name("a.py", false, 0), "a.py");
        assert_eq!(copy_name("a.py", false, 1), "a copy.py");
        assert_eq!(copy_name("a.tar.gz", false, 2), "a.tar copy 2.gz");
        assert_eq!(copy_name(".gitignore", false, 1), ".gitignore copy");
        assert_eq!(copy_name("pkg.v2", true, 1), "pkg.v2 copy");
        assert_eq!(copy_name("Makefile", false, 3), "Makefile copy 3");
    }

    #[test]
    fn top_level_drops_paths_inside_others() {
        let paths = [
            PathBuf::from("/p/src/a.py"),
            PathBuf::from("/p/src"),
            PathBuf::from("/p/srcx"),
            PathBuf::from("/p/src"),
        ];
        assert_eq!(
            top_level(&paths, false),
            [PathBuf::from("/p/src"), PathBuf::from("/p/srcx")]
        );
        assert!(is_within(Path::new("/P/Src/a"), Path::new("/p/src"), true));
        assert!(!is_within(
            Path::new("/P/Src/a"),
            Path::new("/p/src"),
            false
        ));
    }

    #[test]
    fn create_rename_copy_and_delete_on_disk() {
        let (root, project) = temp_project("ops");
        // Nested names create their folders; an existing name is refused.
        let file = create(&project, &root, "pkg/sub/mod.py", NewKind::File(None)).unwrap();
        assert_eq!(file, root.join("pkg").join("sub").join("mod.py"));
        assert!(file.is_file());
        let err = create(&project, &root, "pkg/sub/mod.py", NewKind::File(None)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(err.to_string().starts_with("`mod.py` already exists in "));
        let err = create(&project, &root, "bad//name", NewKind::File(None)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let package = create(&project, &root, "tools", NewKind::PythonPackage).unwrap();
        assert!(package.join("__init__.py").is_file());
        let script = create(&project, &root, "run", python()).unwrap();
        assert_eq!(script, root.join("run.py"));
        let folder = create(&project, &root, "empty", NewKind::Folder).unwrap();
        assert!(folder.is_dir());

        // Moves: refused onto an existing name and into itself.
        fs::write(root.join("run2.py"), "x").unwrap();
        let err = move_path(&project, &root.join("run2.py"), &script).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        let err = move_path(&project, &root.join("pkg"), &root.join("pkg/sub/pkg")).unwrap_err();
        assert_eq!(err.to_string(), "Cannot move `pkg` into itself");
        let err = move_path(&project, &root.join("missing"), &root.join("x")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        move_path(&project, &root.join("pkg"), &root.join("empty/pkg")).unwrap();
        assert!(root.join("empty/pkg/sub/mod.py").is_file());
        assert!(!root.join("pkg").exists());
        let targets = move_targets(
            &project,
            &[
                root.join("run.py"),
                root.join("empty/pkg"),
                root.join("empty/pkg/sub"),
            ],
            &root.join("empty"),
        );
        assert_eq!(
            targets,
            [(root.join("run.py"), root.join("empty").join("run.py"))]
        );

        // Copies take free names, then ` copy`, ` copy 2`.
        let copies = copy_into(&project, std::slice::from_ref(&script), &root).unwrap();
        assert_eq!(copies, [root.join("run copy.py")]);
        assert_eq!(
            duplicate(&project, &script).unwrap(),
            root.join("run copy 2.py")
        );
        let copies = copy_into(&project, &[root.join("empty")], &root.join("tools")).unwrap();
        assert_eq!(copies, [root.join("tools").join("empty")]);
        assert!(root.join("tools/empty/pkg/sub/mod.py").is_file());
        let err =
            copy_into(&project, &[root.join("tools")], &root.join("tools/empty")).unwrap_err();
        assert_eq!(err.to_string(), "Cannot copy `tools` into itself");
        // A file copied into its own folder is fine.
        let copies = copy_into(&project, std::slice::from_ref(&script), &root).unwrap();
        assert_eq!(copies, [root.join("run copy 3.py")]);

        // Deleting a folder and a file inside it deletes both once.
        delete(
            &project,
            &[root.join("tools/empty/pkg/sub/mod.py"), root.join("tools")],
            false,
        )
        .unwrap();
        assert!(!root.join("tools").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    fn python() -> NewKind {
        typed("python")
    }

    fn typed(id: &str) -> NewKind {
        NewKind::File(file_types::by_id(id).unwrap())
    }

    #[test]
    fn typed_files_get_their_names_places_and_starter_text() {
        let (root, project) = temp_project("typed");
        let sub = root.join("src");
        fs::create_dir_all(&sub).unwrap();
        let read = |path: &Path| fs::read_to_string(path).unwrap();

        // Extensions are added where New File was started.
        let notebook = create(&project, &sub, "analysis", typed("jupyter")).unwrap();
        assert_eq!(notebook, sub.join("analysis.ipynb"));
        let parsed = notebook::ipynb::parse(&read(&notebook)).unwrap();
        assert_eq!(parsed.cells.len(), 1);
        let app = create(&project, &sub, "app", typed("marimo")).unwrap();
        assert!(notebook::marimo::is_marimo(&read(&app)));
        let script = create(&project, &sub, "deploy", typed("shell")).unwrap();
        assert_eq!(script, sub.join("deploy.sh"));
        assert!(read(&script).starts_with("#!/usr/bin/env bash\n"));
        let plain = create(&project, &sub, "notes", typed("markdown")).unwrap();
        assert_eq!(read(&plain), "");

        // Fixed names, with or without a typed name.
        let docker = create(&project, &sub, "", typed("dockerfile")).unwrap();
        assert_eq!(docker, sub.join("Dockerfile"));
        let ignore = create(&project, &sub, "build", typed("gitignore")).unwrap();
        assert_eq!(ignore, sub.join("build").join(".gitignore"));

        // Dev containers and workflows go to the roots, wherever started.
        let config = create(&project, &sub, "", typed("devcontainer")).unwrap();
        assert_eq!(config, root.join(".devcontainer").join("devcontainer.json"));
        let parsed = devcontainer::Config::parse(&read(&config)).unwrap();
        assert_eq!(
            parsed.name.as_deref(),
            root.file_name().and_then(|name| name.to_str())
        );
        let workflows = root.join(".next_code").join("workflows");
        let first = create(&project, &sub, "", typed("workflow")).unwrap();
        assert_eq!(first, workflows.join("workflow.yaml"));
        let second = create(&project, &sub, " ", typed("workflow")).unwrap();
        assert_eq!(second, workflows.join("workflow-2.yaml"));
        let named = create(&project, &sub, "deploy", typed("workflow")).unwrap();
        assert_eq!(named, workflows.join("deploy.yaml"));
        workflow::file::check(&read(&named), "deploy").unwrap();
        assert_eq!(
            read(&workflows.join(workflow::SCHEMA_FILE)),
            workflow::SCHEMA
        );
        let err = create(&project, &sub, "deploy", typed("workflow")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn lsp_globs_match_like_vs_code() {
        assert!(glob_matches("**/*.rs", "/home/me/p/src/main.rs", false));
        assert!(!glob_matches("**/*.rs", "/home/me/p/src/main.py", false));
        assert!(glob_matches("**", "/home/me/p/src", false));
        assert!(glob_matches("**/*.{py,pyi}", "C:/p/a.pyi", false));
        assert!(glob_matches("*.py", "/p/sub/a.py", false));
        assert!(!glob_matches("*.py", "/p/sub/a.pyc", false));
        assert!(glob_matches("**/test_[0-9].py", "/p/test_1.py", false));
        assert!(!glob_matches("**/test_[!0-9].py", "/p/test_1.py", false));
        assert!(glob_matches("**/*.PY", "/p/a.py", true));
        assert!(glob_matches("**/a+b(c).py", "/p/a+b(c).py", false));
    }
}
