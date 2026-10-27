//! Git in the editors: each open file's base version for the change
//! markers (HEAD, or the index with `git.staging_area`) and its blame,
//! refreshed whenever the Commit window re-reads status (saves, commits,
//! staging, checkouts, external changes); and the editors' requests:
//! Stage Hunk, Show Diff, show a blamed commit in the Git log.
//!
//! Git runs where the project lives (locally, in WSL, in a container).
//! Each file uses the repository of its folder (the Commit window keeps
//! one per folder of a multi-root project; files in a folder without one
//! have no markers). One refresh asks each repository for its open files'
//! base objects at once and reads only the blobs that changed.

use std::{collections::HashMap, io};

use git::{BaseObject, Repository};
use gpui::EntityId;

use super::*;
use crate::{commit_panel::repository_key, editor::GitBase};

#[derive(Default)]
pub(super) struct GitGutters {
    /// Per editor: the base object (and whether it is the index) it shows.
    shown: HashMap<EntityId, (BaseObject, bool)>,
    /// Per repository (by key), HEAD when bases were last read: blame runs
    /// again when it moves.
    heads: HashMap<String, Option<String>>,
    /// Drops results of a superseded refresh.
    generation: u64,
    refresh: Option<Task<()>>,
    blame: HashMap<EntityId, Task<()>>,
    operation: Option<Task<()>>,
}

/// One repository's part of a refresh: base object changes as
/// `(position in the refresh's files, object, base)`, and HEAD.
type RepositoryBases = (Vec<(usize, BaseObject, Option<GitBase>)>, Option<String>);

/// A file whose base a refresh reads: `(position in the refresh's files,
/// relative path, base object shown and whether it is the index)`.
type BaseRequest = (usize, String, Option<(BaseObject, bool)>);

/// Reads `files`' base objects in `repository`, and the blobs of those
/// that changed. Blocks.
fn read_bases(
    repository: &Repository,
    files: &[BaseRequest],
    index: bool,
) -> io::Result<RepositoryBases> {
    let mut unique: Vec<String> = files.iter().map(|(_, path, _)| path.clone()).collect();
    unique.sort();
    unique.dedup();
    let objects = repository.base_objects(&unique, index)?;
    let head = repository.resolve("HEAD").ok();
    let mut blobs: HashMap<String, Option<GitBase>> = HashMap::new();
    let mut updates = Vec::new();
    for (position, relative, known) in files {
        let found = unique
            .binary_search(relative)
            .map_or(BaseObject::None, |i| {
                objects.get(i).cloned().unwrap_or(BaseObject::None)
            });
        if known.as_ref() == Some(&(found.clone(), index)) {
            continue;
        }
        let base = match &found {
            BaseObject::Blob(id) => match blobs.get(id) {
                Some(base) => base.clone(),
                None => {
                    let base = repository
                        .blob(id)?
                        .filter(|bytes| !bytes.contains(&0))
                        .and_then(|bytes| String::from_utf8(bytes).ok())
                        .map(|text| GitBase {
                            text: text.into(),
                            new_file: false,
                            index,
                        });
                    blobs.insert(id.clone(), base.clone());
                    base
                }
            },
            BaseObject::NewFile => Some(GitBase {
                text: "".into(),
                new_file: true,
                index,
            }),
            BaseObject::None => None,
        };
        updates.push((*position, found, base));
    }
    Ok((updates, head))
}

impl Workspace {
    /// `path`'s repository (its folder's) and its path there; `None`
    /// outside every repository or before status is read.
    pub(super) fn file_in_repository(&self, path: &Path, cx: &App) -> Option<(Repository, String)> {
        self.commit.read(cx).file_in_repository(path)
    }

    /// Open editors with their files.
    fn file_editors(&self) -> Vec<(Entity<Editor>, PathBuf)> {
        let mut seen = HashSet::new();
        self.all_tabs()
            .filter_map(|tab| Some((tab.editor()?.clone(), tab.path.clone())))
            .filter(|(editor, _)| seen.insert(editor.entity_id()))
            .collect()
    }

    fn editor_path(&self, editor: &Entity<Editor>) -> Option<PathBuf> {
        self.all_tabs()
            .find(|tab| tab.editor() == Some(editor))
            .map(|tab| tab.path.clone())
    }

    /// Re-reads the open files' bases in every repository (only changed
    /// blobs) and, where HEAD moved, their blame.
    pub(super) fn refresh_git_gutters(&mut self, cx: &mut Context<Self>) {
        let index = crate::settings::current().git.staging_area;
        // The repositories with open files, and per file its repository.
        let mut repositories: Vec<Repository> = Vec::new();
        let mut keys: Vec<String> = Vec::new();
        let mut files: Vec<(Entity<Editor>, usize, String)> = Vec::new();
        let mut outside = Vec::new();
        for (editor, path) in self.file_editors() {
            let Some((repository, relative)) = self.file_in_repository(&path, cx) else {
                outside.push(editor);
                continue;
            };
            let key = repository_key(repository.root());
            let repo = match keys.iter().position(|known| *known == key) {
                Some(repo) => repo,
                None => {
                    keys.push(key);
                    repositories.push(repository);
                    keys.len() - 1
                }
            };
            files.push((editor, repo, relative));
        }
        for editor in outside {
            editor.update(cx, |editor, cx| {
                if editor.has_git_base() {
                    editor.set_git_base(None, cx)
                }
            });
        }
        let open: HashSet<EntityId> = files.iter().map(|(e, _, _)| e.entity_id()).collect();
        self.git_gutters.shown.retain(|id, _| open.contains(id));
        self.git_gutters.blame.retain(|id, _| open.contains(id));
        if files.is_empty() {
            return;
        }
        // Per repository, the files it holds.
        let mut requests: Vec<Vec<BaseRequest>> = vec![Vec::new(); repositories.len()];
        for (position, (editor, repo, relative)) in files.iter().enumerate() {
            let shown = self.git_gutters.shown.get(&editor.entity_id()).cloned();
            requests[*repo].push((position, relative.clone(), shown));
        }
        self.git_gutters.generation += 1;
        let generation = self.git_gutters.generation;
        self.git_gutters.refresh = Some(cx.spawn(async move |this, cx| {
            // A repository failing to answer keeps its files as they are.
            let loaded: Vec<Option<RepositoryBases>> = cx
                .background_spawn(async move {
                    repositories
                        .iter()
                        .zip(&requests)
                        .map(|(repository, files)| read_bases(repository, files, index).ok())
                        .collect()
                })
                .await;
            this.update(cx, |this, cx| {
                if this.git_gutters.generation != generation {
                    return;
                }
                let mut moved = vec![false; loaded.len()];
                for (repo, loaded) in loaded.into_iter().enumerate() {
                    let Some((updates, head)) = loaded else {
                        continue;
                    };
                    for (position, object, base) in updates {
                        let editor = &files[position].0;
                        this.git_gutters
                            .shown
                            .insert(editor.entity_id(), (object, index));
                        editor.update(cx, |editor, cx| editor.set_git_base(base, cx));
                    }
                    moved[repo] = this.git_gutters.heads.get(&keys[repo]) != Some(&head);
                    this.git_gutters.heads.insert(keys[repo].clone(), head);
                }
                for (editor, repo, _) in &files {
                    let (wanted, loaded) = {
                        let editor = editor.read(cx);
                        (editor.wants_blame(), editor.has_blame())
                    };
                    if wanted && (moved[*repo] || !loaded) {
                        this.load_blame(editor.clone(), cx);
                    }
                }
            })
            .ok();
        }));
    }

    /// Runs blame on the editor's text (in its file's repository) and
    /// hands it over.
    fn load_blame(&mut self, editor: Entity<Editor>, cx: &mut Context<Self>) {
        let failed = |editor: &Entity<Editor>, error: &str, cx: &mut Context<Self>| {
            editor.update(cx, |editor, cx| editor.blame_failed(error, cx));
        };
        if self.commit.read(cx).repos().is_empty() {
            return failed(
                &editor,
                "no git repository here (or its status is still loading)",
                cx,
            );
        }
        let Some((repository, relative)) = self
            .editor_path(&editor)
            .and_then(|path| self.file_in_repository(&path, cx))
        else {
            return failed(&editor, "the file is outside the repository", cx);
        };
        let text: Arc<str> = editor.read(cx).text().into();
        let target = editor.clone();
        let task = cx.spawn(async move |_, cx| {
            let contents = text.clone();
            let blamed = cx
                .background_spawn(
                    async move { repository.blame(&relative, Some(contents.as_bytes())) },
                )
                .await;
            target
                .update(cx, |editor, cx| match blamed {
                    Ok(blame) => editor.set_blame(blame, text, cx),
                    Err(err) => editor.blame_failed(&err.to_string(), cx),
                })
                .ok();
        });
        self.git_gutters.blame.insert(editor.entity_id(), task);
    }

    /// The editor events of change markers and blame.
    pub(super) fn on_editor_git_event(
        &mut self,
        editor: &Entity<Editor>,
        event: &EditorEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::LoadBlame => self.load_blame(editor.clone(), cx),
            EditorEvent::StageHunk { contents } => self.stage_hunk(editor, contents.clone(), cx),
            EditorEvent::ShowChangeDiff { line } => {
                self.show_change_diff(editor, *line, window, cx)
            }
            EditorEvent::ShowCommit(id) => {
                // The commit is in the editor's repository.
                if let Some((repository, _)) = self
                    .editor_path(editor)
                    .and_then(|path| self.file_in_repository(&path, cx))
                {
                    self.git_log
                        .update(cx, |log, cx| log.select_repository(&repository, cx));
                }
                let id = id.clone();
                self.git_log.update(cx, |log, cx| log.reveal_commit(id, cx));
                self.show_git_log(window, cx);
            }
            _ => {}
        }
    }

    /// The editor's file in its repository, or a notice saying there is
    /// none (yet).
    fn editor_file_or_notice(
        &mut self,
        editor: &Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> Option<(Repository, String)> {
        let found = self
            .editor_path(editor)
            .and_then(|path| self.file_in_repository(&path, cx));
        if found.is_none() {
            self.notice = Some(
                "This file is not in a git repository (or its status is still loading)".into(),
            );
            cx.notify();
        }
        found
    }

    fn stage_hunk(&mut self, editor: &Entity<Editor>, contents: String, cx: &mut Context<Self>) {
        let Some((repository, relative)) = self.editor_file_or_notice(editor, cx) else {
            return;
        };
        self.git_gutters.operation = Some(cx.spawn(async move |this, cx| {
            let staged = {
                let relative = relative.clone();
                cx.background_spawn(async move {
                    repository.stage_contents(&relative, contents.as_bytes())
                })
                .await
            };
            this.update(cx, |this, cx| match staged {
                Ok(()) => {
                    this.repository_changed(Some(format!("Staged a change in {relative}")), cx)
                }
                Err(err) => this
                    .commit
                    .update(cx, |commit, cx| commit.report_error(err.to_string(), cx)),
            })
            .ok();
        }));
    }

    /// Show Diff from the hunk popup: the file's working-tree diff against
    /// the markers' base, scrolled to `line`.
    fn show_change_diff(
        &mut self,
        editor: &Entity<Editor>,
        line: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((repository, relative)) = self.editor_file_or_notice(editor, cx) else {
            return;
        };
        let shown = self.git_gutters.shown.get(&editor.entity_id());
        let source = match shown {
            Some((_, true)) => {
                let commit = self.commit.read(cx);
                let entry = commit.repo_of(&repository).and_then(|repo| {
                    repo.status()
                        .entries
                        .iter()
                        .find(|entry| entry.path == relative && entry.unstaged.is_some())
                        .cloned()
                });
                let Some(entry) = entry else {
                    self.notice = Some(
                        format!("{relative} has no unstaged changes on disk (save it first)")
                            .into(),
                    );
                    cx.notify();
                    return;
                };
                DiffSource::Index {
                    entry,
                    staged: false,
                }
            }
            shown => DiffSource::Revisions {
                old: if matches!(shown, Some((BaseObject::NewFile, _))) {
                    git::Side::Empty
                } else {
                    git::Side::Revision("HEAD".into())
                },
                new: git::Side::Worktree,
                change: git::FileChange {
                    path: relative,
                    orig_path: None,
                    change: git::Change::Modified,
                },
            },
        };
        self.open_diff(repository, source.clone(), window, cx);
        let tab = self.find_tab(|tab| match &tab.content {
            TabContent::Diff(diff) => *diff.read(cx).source() == source,
            _ => false,
        });
        if let Some((pane, index)) = tab
            && let Some(TabContent::Diff(diff)) = self.tab_content(pane, index)
        {
            diff.update(cx, |diff, cx| diff.reveal_line(line, cx));
        }
    }
}
