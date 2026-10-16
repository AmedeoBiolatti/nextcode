//! Git in the editors: each open file's base version for the change
//! markers (HEAD, or the index with `git.staging_area`) and its blame,
//! refreshed whenever the Commit window re-reads status (saves, commits,
//! staging, checkouts, external changes); and the editors' requests:
//! Stage Hunk, Show Diff, show a blamed commit in the Git log.
//!
//! Git runs where the project lives (locally, in WSL, in a container).
//! One refresh asks git for every open file's base object at once and
//! reads only the blobs that changed.

use std::{collections::HashMap, io};

use git::BaseObject;
use gpui::EntityId;

use super::*;
use crate::editor::GitBase;

#[derive(Default)]
pub(super) struct GitGutters {
    /// Per editor: the base object (and whether it is the index) it shows.
    shown: HashMap<EntityId, (BaseObject, bool)>,
    /// HEAD when bases were last read: blame runs again when it moves.
    head: Option<String>,
    /// Drops results of a superseded refresh.
    generation: u64,
    refresh: Option<Task<()>>,
    blame: HashMap<EntityId, Task<()>>,
    operation: Option<Task<()>>,
}

impl Workspace {
    /// `path`'s repository-relative path, if it is inside `repository`.
    pub(super) fn repository_relative(
        &self,
        repository: &git::Repository,
        path: &Path,
    ) -> Option<String> {
        let inside = path.strip_prefix(self.project.root()).ok()?;
        let parts: Vec<String> = inside
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect();
        (!parts.is_empty()).then(|| format!("{}{}", repository.prefix(), parts.join("/")))
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

    /// Re-reads the open files' bases (only changed blobs) and, when HEAD
    /// moved, their blame.
    pub(super) fn refresh_git_gutters(&mut self, cx: &mut Context<Self>) {
        let Some(repository) = self.commit.read(cx).repository() else {
            return;
        };
        let index = crate::settings::current().git.staging_area;
        let mut files: Vec<(Entity<Editor>, String)> = Vec::new();
        for (editor, path) in self.file_editors() {
            match self.repository_relative(&repository, &path) {
                Some(relative) => files.push((editor, relative)),
                None => editor.update(cx, |editor, cx| {
                    if editor.has_git_base() {
                        editor.set_git_base(None, cx)
                    }
                }),
            }
        }
        let open: HashSet<EntityId> = files.iter().map(|(e, _)| e.entity_id()).collect();
        self.git_gutters.shown.retain(|id, _| open.contains(id));
        self.git_gutters.blame.retain(|id, _| open.contains(id));
        let known: Vec<Option<(BaseObject, bool)>> = files
            .iter()
            .map(|(editor, _)| self.git_gutters.shown.get(&editor.entity_id()).cloned())
            .collect();
        let relative: Vec<String> = files.iter().map(|(_, relative)| relative.clone()).collect();
        self.git_gutters.generation += 1;
        let generation = self.git_gutters.generation;
        self.git_gutters.refresh = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    let mut unique = relative.clone();
                    unique.sort();
                    unique.dedup();
                    let objects = repository.base_objects(&unique, index)?;
                    let head = repository.resolve("HEAD").ok();
                    let mut blobs: HashMap<String, Option<GitBase>> = HashMap::new();
                    let mut updates = Vec::new();
                    for (position, relative) in relative.iter().enumerate() {
                        let found = unique
                            .binary_search(relative)
                            .map_or(BaseObject::None, |i| {
                                objects.get(i).cloned().unwrap_or(BaseObject::None)
                            });
                        if known[position].as_ref() == Some(&(found.clone(), index)) {
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
                        updates.push((position, found, base));
                    }
                    Ok::<_, io::Error>((updates, head))
                })
                .await;
            this.update(cx, |this, cx| {
                if this.git_gutters.generation != generation {
                    return;
                }
                let Ok((updates, head)) = loaded else {
                    return;
                };
                for (position, object, base) in updates {
                    let editor = &files[position].0;
                    this.git_gutters
                        .shown
                        .insert(editor.entity_id(), (object, index));
                    editor.update(cx, |editor, cx| editor.set_git_base(base, cx));
                }
                let moved = head != this.git_gutters.head;
                this.git_gutters.head = head;
                for (editor, _) in &files {
                    let (wanted, loaded) = {
                        let editor = editor.read(cx);
                        (editor.wants_blame(), editor.has_blame())
                    };
                    if wanted && (moved || !loaded) {
                        this.load_blame(editor.clone(), cx);
                    }
                }
            })
            .ok();
        }));
    }

    /// Runs blame on the editor's text and hands it over.
    fn load_blame(&mut self, editor: Entity<Editor>, cx: &mut Context<Self>) {
        let failed = |editor: &Entity<Editor>, error: &str, cx: &mut Context<Self>| {
            editor.update(cx, |editor, cx| editor.blame_failed(error, cx));
        };
        let Some(repository) = self.commit.read(cx).repository() else {
            return failed(
                &editor,
                "no git repository here (or its status is still loading)",
                cx,
            );
        };
        let Some(relative) = self
            .editor_path(&editor)
            .and_then(|path| self.repository_relative(&repository, &path))
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
                let id = id.clone();
                self.git_log.update(cx, |log, cx| log.reveal_commit(id, cx));
                self.show_git_log(window, cx);
            }
            _ => {}
        }
    }

    fn stage_hunk(&mut self, editor: &Entity<Editor>, contents: String, cx: &mut Context<Self>) {
        let Some(repository) = self.repository_or_notice(cx) else {
            return;
        };
        let Some(relative) = self
            .editor_path(editor)
            .and_then(|path| self.repository_relative(&repository, &path))
        else {
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
        let Some(repository) = self.repository_or_notice(cx) else {
            return;
        };
        let Some(relative) = self
            .editor_path(editor)
            .and_then(|path| self.repository_relative(&repository, &path))
        else {
            return;
        };
        let shown = self.git_gutters.shown.get(&editor.entity_id());
        let source = match shown {
            Some((_, true)) => {
                let entry = self.commit.read(cx).status().and_then(|status| {
                    status
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
