//! LSP `WorkspaceEdit`s (rename, code actions, `workspace/applyEdit`):
//! text edits for open documents or files on disk, and file creation,
//! renames and deletions, applied in the server's order. Also project-wide
//! Replace (the search panel's and headless `replace`) through the same
//! documents, and the minimal edit list between two texts, so whole-text
//! results (formatters) keep cursors where they were.

use std::{
    io,
    ops::Range,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use lsp::types::{
    ApplyWorkspaceEditResponse, DocumentChangeOperation, DocumentChanges, OneOf, Position,
    ResourceOp, TextDocumentEdit, TextEdit, Uri, WorkspaceEdit,
};
use search::{Replacer, Target};
use similar::{Algorithm, DiffTag, TextDiff};
use text::{Buffer, Encoding};
use worktree::FileKind;

use crate::project::Project;

/// One step of a workspace edit, with paths in the project's terms.
#[derive(Debug, Clone, PartialEq)]
pub enum Operation {
    Edit {
        path: PathBuf,
        /// The document version the edits were computed for, if the
        /// server named one.
        version: Option<i32>,
        edits: Vec<TextEdit>,
    },
    Create {
        path: PathBuf,
        overwrite: bool,
        ignore_if_exists: bool,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
        overwrite: bool,
        ignore_if_exists: bool,
    },
    Delete {
        path: PathBuf,
        recursive: bool,
        ignore_if_not_exists: bool,
    },
}

impl Operation {
    pub fn path(&self) -> &Path {
        match self {
            Self::Edit { path, .. } | Self::Create { path, .. } | Self::Delete { path, .. } => path,
            Self::Rename { from, .. } => from,
        }
    }
}

fn path_of(uri: &Uri) -> Result<PathBuf, String> {
    lsp::uri_to_path(uri).ok_or_else(|| format!("unsupported location {}", uri.as_str()))
}

fn text_document_edit(edit: TextDocumentEdit) -> Result<Operation, String> {
    Ok(Operation::Edit {
        path: path_of(&edit.text_document.uri)?,
        version: edit.text_document.version,
        edits: edit
            .edits
            .into_iter()
            .map(|edit| match edit {
                OneOf::Left(edit) => edit,
                OneOf::Right(annotated) => annotated.text_edit,
            })
            .collect(),
    })
}

/// The steps of `edit` in order: `documentChanges` when present (they may
/// carry versions and file operations), else `changes` by path.
pub fn operations(edit: WorkspaceEdit) -> Result<Vec<Operation>, String> {
    match edit.document_changes {
        Some(DocumentChanges::Edits(edits)) => edits.into_iter().map(text_document_edit).collect(),
        Some(DocumentChanges::Operations(operations)) => operations
            .into_iter()
            .map(|operation| match operation {
                DocumentChangeOperation::Edit(edit) => text_document_edit(edit),
                DocumentChangeOperation::Op(ResourceOp::Create(create)) => {
                    let options = create.options.as_ref();
                    Ok(Operation::Create {
                        path: path_of(&create.uri)?,
                        overwrite: options.and_then(|o| o.overwrite) == Some(true),
                        ignore_if_exists: options.and_then(|o| o.ignore_if_exists) == Some(true),
                    })
                }
                DocumentChangeOperation::Op(ResourceOp::Rename(rename)) => {
                    let options = rename.options.as_ref();
                    Ok(Operation::Rename {
                        from: path_of(&rename.old_uri)?,
                        to: path_of(&rename.new_uri)?,
                        overwrite: options.and_then(|o| o.overwrite) == Some(true),
                        ignore_if_exists: options.and_then(|o| o.ignore_if_exists) == Some(true),
                    })
                }
                DocumentChangeOperation::Op(ResourceOp::Delete(delete)) => {
                    let options = delete.options.as_ref();
                    Ok(Operation::Delete {
                        path: path_of(&delete.uri)?,
                        recursive: options.and_then(|o| o.recursive) == Some(true),
                        ignore_if_not_exists: options.and_then(|o| o.ignore_if_not_exists)
                            == Some(true),
                    })
                }
            })
            .collect(),
        None => {
            let mut operations = edit
                .changes
                .unwrap_or_default()
                .into_iter()
                .map(|(uri, edits)| {
                    Ok(Operation::Edit {
                        path: path_of(&uri)?,
                        version: None,
                        edits,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            operations.sort_by(|a, b| a.path().cmp(b.path()));
            Ok(operations)
        }
    }
}

/// Server text edits (all relative to the current text) as sorted buffer
/// edits; inserts at one position keep the server's order. New text uses
/// the buffer's line ending.
pub fn resolve(
    buffer: &Buffer,
    edits: &[TextEdit],
    encoding: Encoding,
) -> Result<Vec<(Range<usize>, String)>, String> {
    let crlf = buffer.line_ending() == "\r\n";
    let mut resolved: Vec<(Range<usize>, String)> =
        language::resolve_edits(buffer, edits, encoding)
            .into_iter()
            .map(|(range, text)| {
                let text = if crlf {
                    text.replace("\r\n", "\n").replace('\n', "\r\n")
                } else {
                    text
                };
                (range, text)
            })
            .collect();
    // Stable: same-position inserts stay in the server's order, and come
    // before a replacement starting there.
    resolved.sort_by_key(|(range, _)| (range.start, range.end));
    if resolved
        .windows(2)
        .any(|pair| pair[0].0.end > pair[1].0.start)
    {
        return Err("the server sent overlapping edits".into());
    }
    Ok(resolved)
}

/// `text` with server `edits` applied.
pub fn edit_text(text: &str, edits: &[TextEdit], encoding: Encoding) -> Result<String, String> {
    let mut buffer = Buffer::new(text);
    let resolved = resolve(&buffer, edits, encoding)?;
    buffer
        .edit_batch(resolved)
        .map_err(|_| "the server sent overlapping edits".to_owned())?;
    Ok(buffer.slice(0..buffer.len()))
}

/// Whole-text changes beyond this many bytes per changed line run are not
/// refined character by character.
const REFINE_LIMIT: usize = 64 * 1024;

/// The edits turning `old` into `new`, sorted and non-overlapping: changed
/// line runs, refined to characters so unchanged text inside a changed line
/// (and the cursors in it) stays put.
pub fn minimal_edits(old: &str, new: &str) -> Vec<(Range<usize>, String)> {
    let deadline = Instant::now() + Duration::from_millis(500);
    let lines = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .deadline(deadline)
        .diff_lines(old, new);
    let old_starts = starts(lines.old_slices());
    let new_starts = starts(lines.new_slices());
    let mut edits = Vec::new();
    for op in lines.ops() {
        let (tag, old_lines, new_lines) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            continue;
        }
        let old_range = old_starts[old_lines.start]..old_starts[old_lines.end];
        let new_range = new_starts[new_lines.start]..new_starts[new_lines.end];
        let (old_part, new_part) = (&old[old_range.clone()], &new[new_range]);
        if tag != DiffTag::Replace || old_part.len() + new_part.len() > REFINE_LIMIT {
            edits.push((old_range, new_part.to_owned()));
            continue;
        }
        let chars = TextDiff::configure()
            .algorithm(Algorithm::Myers)
            .deadline(deadline)
            .diff_chars(old_part, new_part);
        let old_chars = starts(chars.old_slices());
        let new_chars = starts(chars.new_slices());
        for op in chars.ops() {
            let (tag, old_span, new_span) = op.as_tag_tuple();
            if tag == DiffTag::Equal {
                continue;
            }
            let start = old_range.start + old_chars[old_span.start];
            let end = old_range.start + old_chars[old_span.end];
            let text = &new_part[new_chars[new_span.start]..new_chars[new_span.end]];
            edits.push((start..end, text.to_owned()));
        }
    }
    edits
}

/// Byte offsets where each slice starts, plus the end.
fn starts(slices: &[&str]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(slices.len() + 1);
    let mut offset = 0;
    offsets.push(0);
    for slice in slices {
        offset += slice.len();
        offsets.push(offset);
    }
    offsets
}

/// Documents open in the editor (or the headless session), which take text
/// edits instead of the files on disk.
pub trait Documents {
    /// The version of the open document at `path`, or `None` if it is not
    /// open.
    fn version(&mut self, path: &Path) -> Option<i32>;
    /// The current text of the open document at `path`, or `None` if it is
    /// not open.
    fn text(&mut self, path: &Path) -> Option<String>;
    /// Applies `edits` to the open document at `path`; `None` when it is not
    /// open, and the file on disk is edited instead.
    fn edit(
        &mut self,
        path: &Path,
        edits: &[TextEdit],
        encoding: Encoding,
    ) -> Option<Result<(), String>>;
    /// `from` (a file or a directory) moved to `to` on disk.
    fn renamed(&mut self, from: &Path, to: &Path);
    /// `path` was deleted on disk.
    fn deleted(&mut self, path: &Path);
}

/// Why applying stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// Index of the operation that failed; later ones were not applied.
    pub index: usize,
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Files edited, created, renamed (new path) or deleted, in order, once.
    pub changed: Vec<PathBuf>,
    pub failure: Option<Failure>,
}

impl Report {
    fn touched(&mut self, path: &Path) {
        if !self.changed.iter().any(|changed| changed == path) {
            self.changed.push(path.to_path_buf());
        }
    }

    /// The answer to `workspace/applyEdit`.
    pub fn response(&self) -> ApplyWorkspaceEditResponse {
        ApplyWorkspaceEditResponse {
            applied: self.failure.is_none(),
            failure_reason: self
                .failure
                .as_ref()
                .map(|failure| format!("{}: {}", failure.path.display(), failure.message)),
            failed_change: self.failure.as_ref().map(|failure| failure.index as u32),
        }
    }

    /// One line for the status bar, e.g. "Rename: changed 3 files".
    pub fn summary(&self, label: &str, project: &Project) -> String {
        let show = |path: &Path| shown(project, path);
        match &self.failure {
            Some(failure) => format!(
                "{label} failed at {}: {}{}",
                show(&failure.path),
                failure.message,
                match self.changed.len() {
                    0 => String::new(),
                    n => format!(" ({n} files already changed)"),
                }
            ),
            None => match self.changed.as_slice() {
                [] => format!("{label}: nothing to change"),
                [path] => format!("{label}: changed {}", show(path)),
                paths => format!("{label}: changed {} files", paths.len()),
            },
        }
    }
}

/// Applies `operations` in order: text edits to open documents, or to the
/// files on disk through `project`, and file operations on disk. Versioned
/// edits for open documents are checked first, so a stale edit changes
/// nothing; otherwise the first failure stops the rest ("abort").
pub fn apply(
    project: &Project,
    operations: &[Operation],
    encoding: Encoding,
    documents: &mut dyn Documents,
) -> Report {
    let mut report = Report::default();
    for (index, operation) in operations.iter().enumerate() {
        if let Operation::Edit {
            path,
            version: Some(version),
            ..
        } = operation
            && let Some(current) = documents.version(path)
            && current != *version
        {
            report.failure = Some(Failure {
                index,
                path: path.clone(),
                message: format!(
                    "the document changed since the edit was computed (version {current}, \
                     expected {version})"
                ),
            });
            return report;
        }
    }
    for (index, operation) in operations.iter().enumerate() {
        let result = match operation {
            Operation::Edit { path, edits, .. } => match documents.edit(path, edits, encoding) {
                Some(result) => result,
                None => edit_on_disk(project, path, edits, encoding),
            },
            Operation::Create {
                path,
                overwrite,
                ignore_if_exists,
            } => create(project, path, *overwrite, *ignore_if_exists).map_err(|e| e.to_string()),
            Operation::Rename {
                from,
                to,
                overwrite,
                ignore_if_exists,
            } => match rename(project, from, to, *overwrite, *ignore_if_exists) {
                Ok(true) => {
                    documents.renamed(from, to);
                    Ok(())
                }
                Ok(false) => Ok(()),
                Err(err) => Err(err.to_string()),
            },
            Operation::Delete {
                path,
                recursive,
                ignore_if_not_exists,
            } => match delete(project, path, *recursive, *ignore_if_not_exists) {
                Ok(true) => {
                    documents.deleted(path);
                    Ok(())
                }
                Ok(false) => Ok(()),
                Err(err) => Err(err.to_string()),
            },
        };
        match result {
            Ok(()) => match operation {
                Operation::Rename { to, .. } => report.touched(to),
                operation => report.touched(operation.path()),
            },
            Err(message) => {
                report.failure = Some(Failure {
                    index,
                    path: operation.path().to_path_buf(),
                    message,
                });
                break;
            }
        }
    }
    report
}

fn edit_on_disk(
    project: &Project,
    path: &Path,
    edits: &[TextEdit],
    encoding: Encoding,
) -> Result<(), String> {
    let text = project.read_text(path).map_err(|err| err.to_string())?;
    let edited = edit_text(&text, edits, encoding)?;
    if edited != text {
        project
            .write(path, edited.into_bytes())
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}

fn create_parent(project: &Project, path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => project.create_dir_all(parent),
        _ => Ok(()),
    }
}

fn exists_error(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{} already exists", path.display()),
    )
}

fn create(
    project: &Project,
    path: &Path,
    overwrite: bool,
    ignore_if_exists: bool,
) -> io::Result<()> {
    match project.kind(path)? {
        Some(FileKind::File) if overwrite => project.write(path, Vec::new()),
        Some(_) if ignore_if_exists => Ok(()),
        Some(_) => Err(exists_error(path)),
        None => {
            create_parent(project, path)?;
            project.write(path, Vec::new())
        }
    }
}

/// One file's matches for a project-wide replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTargets {
    pub path: PathBuf,
    pub targets: Vec<Target>,
}

/// What a project-wide replace did, file by file.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReplaceReport {
    /// Files changed and how many matches each, in order.
    pub replaced: Vec<(PathBuf, usize)>,
    /// Files left alone because they changed since the search, and why.
    pub skipped: Vec<(PathBuf, String)>,
    /// Files that could not be read, edited or written.
    pub failed: Vec<(PathBuf, String)>,
}

impl ReplaceReport {
    /// Matches replaced over all files.
    pub fn matches(&self) -> usize {
        self.replaced.iter().map(|(_, count)| count).sum()
    }

    /// One line for a notice, e.g. "Replaced 3 matches in 2 files; skipped
    /// a.py (changed since the search)".
    pub fn summary(&self, project: &Project) -> String {
        let show = |path: &Path| shown(project, path);
        let plural =
            |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let mut summary = format!(
            "Replaced {} in {}",
            plural(self.matches(), "match", "matches"),
            plural(self.replaced.len(), "file", "files")
        );
        if !self.skipped.is_empty() {
            let names: Vec<String> = self.skipped.iter().map(|(path, _)| show(path)).collect();
            summary += &format!("; skipped {} (changed since the search)", names.join(", "));
        }
        for (path, message) in &self.failed {
            summary += &format!("; {} failed: {message}", show(path));
        }
        summary
    }
}

/// `path` as notices name it: relative to the root, or as
/// [`Project::display_path`] (`api › src/a.py`) when the project has
/// several folders. A notebook's virtual document is named as the
/// notebook.
fn shown(project: &Project, path: &Path) -> String {
    let notebook;
    let path = match path.file_name().and_then(|name| {
        name.to_str()?
            .strip_suffix(crate::builtins::notebook::lsp::VIRTUAL_SUFFIX)
    }) {
        Some(name) => {
            notebook = path.with_file_name(name);
            &notebook
        }
        None => path,
    };
    if project.is_multi_root() {
        project.display_path(path)
    } else {
        path.strip_prefix(project.root())
            .unwrap_or(path)
            .display()
            .to_string()
    }
}

/// Replaces each file's targets. A file's edits are planned against its
/// current text (the open document, else the file read through `project`),
/// and the file is skipped when any of its targets no longer matches with
/// the text the search saw. Open documents take the edits (as one undo
/// step in the editor, left unsaved); other files are written.
pub fn replace(
    project: &Project,
    replacer: &Replacer,
    files: &[FileTargets],
    documents: &mut dyn Documents,
) -> ReplaceReport {
    let mut report = ReplaceReport::default();
    for file in files.iter().filter(|file| !file.targets.is_empty()) {
        let path = &file.path;
        let open = documents.text(path);
        let is_open = open.is_some();
        let text = match open.map_or_else(|| project.read_text(path), Ok) {
            Ok(text) => text,
            Err(err) => {
                report.failed.push((path.clone(), err.to_string()));
                continue;
            }
        };
        let planned = match replacer.plan(&text, &file.targets) {
            Ok(planned) => planned,
            Err(stale) => {
                report.skipped.push((path.clone(), stale.to_string()));
                continue;
            }
        };
        let result = if is_open {
            let edits: Vec<TextEdit> = planned
                .iter()
                .map(|replacement| {
                    let line = replacement.line as u32;
                    TextEdit::new(
                        lsp::types::Range::new(
                            Position::new(line, replacement.columns.start as u32),
                            Position::new(line, replacement.columns.end as u32),
                        ),
                        replacement.text.clone(),
                    )
                })
                .collect();
            documents
                .edit(path, &edits, Encoding::Utf8)
                .unwrap_or_else(|| Err("the document was closed".into()))
        } else {
            let edited = search::apply_replacements(&text, &planned);
            project
                .write(path, edited.into_bytes())
                .map_err(|err| err.to_string())
        };
        match result {
            Ok(()) => report.replaced.push((path.clone(), planned.len())),
            Err(message) => report.failed.push((path.clone(), message)),
        }
    }
    report
}

/// Returns whether anything moved.
fn rename(
    project: &Project,
    from: &Path,
    to: &Path,
    overwrite: bool,
    ignore_if_exists: bool,
) -> io::Result<bool> {
    if project.kind(to)?.is_some() && !overwrite {
        return if ignore_if_exists {
            Ok(false)
        } else {
            Err(exists_error(to))
        };
    }
    create_parent(project, to)?;
    project.rename(from, to)?;
    Ok(true)
}

/// Returns whether anything was deleted.
fn delete(
    project: &Project,
    path: &Path,
    recursive: bool,
    ignore_if_not_exists: bool,
) -> io::Result<bool> {
    if project.kind(path)?.is_none() {
        return if ignore_if_not_exists {
            Ok(false)
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} does not exist", path.display()),
            ))
        };
    }
    project.remove(path, recursive)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fs};

    use lsp::types::{
        CreateFile, CreateFileOptions, OptionalVersionedTextDocumentIdentifier, Position,
        RenameFile,
    };

    use super::*;

    fn edit(line: u32, start: u32, end_line: u32, end: u32, text: &str) -> TextEdit {
        TextEdit::new(
            lsp::types::Range::new(Position::new(line, start), Position::new(end_line, end)),
            text.into(),
        )
    }

    fn uri(path: &Path) -> Uri {
        lsp::path_to_uri(path).unwrap()
    }

    /// Open documents as plain strings, with versions.
    #[derive(Default)]
    struct Open {
        docs: HashMap<PathBuf, (i32, String)>,
        renamed: Vec<(PathBuf, PathBuf)>,
    }

    impl Documents for Open {
        fn version(&mut self, path: &Path) -> Option<i32> {
            self.docs.get(path).map(|(version, _)| *version)
        }

        fn text(&mut self, path: &Path) -> Option<String> {
            self.docs.get(path).map(|(_, text)| text.clone())
        }

        fn edit(
            &mut self,
            path: &Path,
            edits: &[TextEdit],
            encoding: Encoding,
        ) -> Option<Result<(), String>> {
            let (version, text) = self.docs.get_mut(path)?;
            Some(edit_text(text, edits, encoding).map(|edited| {
                *text = edited;
                *version += 1;
            }))
        }

        fn renamed(&mut self, from: &Path, to: &Path) {
            self.renamed.push((from.to_path_buf(), to.to_path_buf()));
        }

        fn deleted(&mut self, _: &Path) {}
    }

    fn temp_project(name: &str) -> (PathBuf, Project) {
        let root =
            std::env::temp_dir().join(format!("next_code_wsedit_{name}_{}", std::process::id()));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(&root).unwrap();
        (root.clone(), Project::local(root))
    }

    #[test]
    fn edits_apply_to_the_original_text_and_inserts_keep_their_order() {
        // Given out of order, an insert at a replaced range's start, two
        // inserts at one position.
        let edits = [
            edit(1, 0, 1, 3, "four"),
            edit(0, 4, 0, 4, "b"),
            edit(0, 0, 0, 3, "one"),
            edit(0, 4, 0, 4, "c"),
            edit(1, 0, 1, 0, ">"),
        ];
        assert_eq!(
            edit_text("uno a\ntre\n", &edits, Encoding::Utf16).unwrap(),
            "one bca\n>four\n"
        );
        let overlapping = [edit(0, 0, 0, 3, "x"), edit(0, 2, 0, 4, "y")];
        assert!(edit_text("abcdef", &overlapping, Encoding::Utf16).is_err());
        // CRLF files get CRLF in new text; UTF-16 columns past an emoji.
        assert_eq!(
            edit_text("a😀b\r\nc", &[edit(0, 3, 0, 4, "x\ny")], Encoding::Utf16).unwrap(),
            "a😀x\r\ny\r\nc"
        );
    }

    #[test]
    fn versioned_document_changes_go_to_open_documents_or_disk() {
        let (root, project) = temp_project("versions");
        let open_path = root.join("open.rs");
        let disk_path = root.join("disk.rs");
        fs::write(&disk_path, "fn old() {}\n").unwrap();
        let mut open = Open::default();
        open.docs
            .insert(open_path.clone(), (3, "old();\nold();\n".into()));
        let document_edit =
            |path: &Path, version: Option<i32>, edits: Vec<TextEdit>| TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: uri(path),
                    version,
                },
                edits: edits.into_iter().map(OneOf::Left).collect(),
            };
        let workspace_edit = |version| WorkspaceEdit {
            document_changes: Some(DocumentChanges::Edits(vec![
                document_edit(
                    &open_path,
                    Some(version),
                    vec![edit(0, 0, 0, 3, "new"), edit(1, 0, 1, 3, "new")],
                ),
                document_edit(&disk_path, None, vec![edit(0, 3, 0, 6, "new")]),
            ])),
            ..WorkspaceEdit::default()
        };

        // A stale version changes nothing, not even the other file.
        let stale = operations(workspace_edit(2)).unwrap();
        let report = apply(&project, &stale, Encoding::Utf16, &mut open);
        assert_eq!(report.failure.as_ref().map(|f| f.index), Some(0));
        assert!(!report.response().applied);
        assert_eq!(open.docs[&open_path].1, "old();\nold();\n");
        assert_eq!(fs::read_to_string(&disk_path).unwrap(), "fn old() {}\n");

        let current = operations(workspace_edit(3)).unwrap();
        let report = apply(&project, &current, Encoding::Utf16, &mut open);
        assert_eq!(report.failure, None);
        assert_eq!(report.changed, [open_path.clone(), disk_path.clone()]);
        assert_eq!(open.docs[&open_path], (4, "new();\nnew();\n".into()));
        assert_eq!(fs::read_to_string(&disk_path).unwrap(), "fn new() {}\n");
        assert_eq!(
            report.summary("Rename", &project),
            "Rename: changed 2 files"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn files_are_created_renamed_and_failures_stop_the_rest() {
        let (root, project) = temp_project("files");
        let created = root.join("sub").join("new.rs");
        let renamed = root.join("moved").join("renamed.rs");
        let operation = |op| DocumentChangeOperation::Op(op);
        let edit_ops = WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(vec![
                operation(ResourceOp::Create(CreateFile {
                    uri: uri(&created),
                    options: None,
                    annotation_id: None,
                })),
                DocumentChangeOperation::Edit(TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        uri: uri(&created),
                        version: None,
                    },
                    edits: vec![OneOf::Left(edit(0, 0, 0, 0, "pub fn f() {}\n"))],
                }),
                operation(ResourceOp::Rename(RenameFile {
                    old_uri: uri(&created),
                    new_uri: uri(&renamed),
                    options: None,
                    annotation_id: None,
                })),
                // Exists now: fails without `overwrite`/`ignoreIfExists`.
                operation(ResourceOp::Create(CreateFile {
                    uri: uri(&renamed),
                    options: Some(CreateFileOptions {
                        overwrite: None,
                        ignore_if_exists: None,
                    }),
                    annotation_id: None,
                })),
                DocumentChangeOperation::Edit(TextDocumentEdit {
                    text_document: OptionalVersionedTextDocumentIdentifier {
                        uri: uri(&renamed),
                        version: None,
                    },
                    edits: vec![OneOf::Left(edit(0, 0, 0, 0, "never"))],
                }),
            ])),
            ..WorkspaceEdit::default()
        };
        let mut open = Open::default();
        let ops = operations(edit_ops).unwrap();
        let report = apply(&project, &ops, Encoding::Utf16, &mut open);
        assert_eq!(fs::read_to_string(&renamed).unwrap(), "pub fn f() {}\n");
        assert!(!created.exists());
        assert_eq!(open.renamed, [(created.clone(), renamed.clone())]);
        assert_eq!(report.changed, [created.clone(), renamed.clone()]);
        let failure = report.failure.clone().expect("the second create fails");
        assert_eq!((failure.index, failure.path), (3, renamed.clone()));
        assert_eq!(report.response().failed_change, Some(3));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    #[allow(clippy::mutable_key_type)] // lsp_types' `Uri` keys, as servers send them
    fn plain_changes_are_applied_by_path() {
        let (root, project) = temp_project("changes");
        let (a, b) = (root.join("a.py"), root.join("b.py"));
        fs::write(&a, "x = 1\n").unwrap();
        fs::write(&b, "print(x)\n").unwrap();
        let changes = HashMap::from([
            (uri(&b), vec![edit(0, 6, 0, 7, "y")]),
            (uri(&a), vec![edit(0, 0, 0, 1, "y")]),
        ]);
        let ops = operations(WorkspaceEdit::new(changes)).unwrap();
        let report = apply(&project, &ops, Encoding::Utf16, &mut Open::default());
        assert_eq!(report.changed, [a.clone(), b.clone()]);
        assert_eq!(fs::read_to_string(&a).unwrap(), "y = 1\n");
        assert_eq!(fs::read_to_string(&b).unwrap(), "print(y)\n");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn minimal_edits_touch_only_what_changed() {
        let old = "fn main() {\nlet x = 1;\n  call(x);\n}\n";
        let new = "fn main() {\n    let x = 1;\n    call(x);\n}\n";
        let edits = minimal_edits(old, new);
        // Pure indentation inserts: no character of the old text is replaced.
        assert_eq!(edits.len(), 2, "{edits:?}");
        assert!(
            edits
                .iter()
                .all(|(range, text)| range.is_empty() && text.chars().all(|c| c == ' '))
        );
        assert_eq!(edits[0], (12..12, "    ".to_owned()));
        let mut buffer = Buffer::new(old);
        buffer.edit_batch(edits).unwrap();
        assert_eq!(buffer.slice(0..buffer.len()), new);
        assert!(minimal_edits(new, new).is_empty());
        let edits = minimal_edits("a\nb\n", "a\nb\nc\n");
        assert_eq!(edits, [(4..4, "c\n".to_owned())]);
    }

    /// Targets of `query` in `text`, as a search of the file reports them.
    fn targets(query: &search::SearchQuery, text: &str) -> Vec<Target> {
        search::search_text(query, text)
            .unwrap()
            .iter()
            .flat_map(Target::of_line)
            .map(|(_, target)| target)
            .collect()
    }

    #[test]
    fn replace_edits_open_documents_writes_closed_files_and_skips_changed_ones() {
        let (root, project) = temp_project("replace");
        let query = search::SearchQuery {
            pattern: r"old(\d)".into(),
            regex: true,
            case_sensitive: true,
            ..Default::default()
        };
        let replacer = Replacer::new(&query, "new$1").unwrap();
        let (open_path, disk, stale, missing) = (
            root.join("open.rs"),
            root.join("disk.py"),
            root.join("stale.py"),
            root.join("missing.py"),
        );
        // The open document differs from its file: its own text is searched.
        fs::write(&open_path, "nothing\n").unwrap();
        let open_text = "x = old1;\r\nold2();\r\n";
        let mut open = Open::default();
        open.docs.insert(open_path.clone(), (1, open_text.into()));
        fs::write(&disk, "old1 old2 old3\n").unwrap();
        fs::write(&stale, "a = old1\n").unwrap();
        let mut disk_targets = targets(&query, "old1 old2 old3\n");
        // The user excluded the middle match.
        disk_targets.remove(1);
        let files = [
            FileTargets {
                path: open_path.clone(),
                targets: targets(&query, open_text),
            },
            FileTargets {
                path: disk.clone(),
                targets: disk_targets,
            },
            FileTargets {
                path: stale.clone(),
                targets: targets(&query, "a = old1\n"),
            },
            FileTargets {
                path: missing.clone(),
                targets: targets(&query, "old1\n"),
            },
        ];
        // Changed after the search: the match moved.
        fs::write(&stale, "ab = old1\n").unwrap();

        let report = replace(&project, &replacer, &files, &mut open);
        assert_eq!(report.replaced, [(open_path.clone(), 2), (disk.clone(), 2)]);
        assert_eq!(open.docs[&open_path].1, "x = new1;\r\nnew2();\r\n");
        assert_eq!(fs::read_to_string(&open_path).unwrap(), "nothing\n");
        assert_eq!(fs::read_to_string(&disk).unwrap(), "new1 old2 new3\n");
        assert_eq!(fs::read_to_string(&stale).unwrap(), "ab = old1\n");
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].0, stale);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, missing);
        let summary = report.summary(&project);
        assert!(
            summary.starts_with(
                "Replaced 4 matches in 2 files; skipped stale.py (changed since the search); \
                 missing.py failed: "
            ),
            "{summary}"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn summaries_name_files_with_their_folder_in_a_multi_root_project() {
        let (api, web) = (PathBuf::from("/ws/api"), PathBuf::from("/ws/web"));
        let project = Project::local_folders(&[&api, &web]);
        let report = ReplaceReport {
            replaced: vec![(api.join("a.py"), 2)],
            skipped: vec![(api.join("src").join("b.py"), "changed".into())],
            failed: vec![(web.join("c.py"), "denied".into())],
        };
        assert_eq!(
            report.summary(&project),
            "Replaced 2 matches in 1 file; skipped api › src/b.py (changed since the search); \
             web › c.py failed: denied"
        );
        let edits = Report {
            changed: vec![web.join("lib").join("x.ts")],
            ..Report::default()
        };
        assert_eq!(
            edits.summary("Rename", &project),
            "Rename: changed web › lib/x.ts"
        );
    }
}
