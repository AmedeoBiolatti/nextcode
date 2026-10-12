//! Project file tree: lazily read, gitignore-aware, virtualized.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use gpui::{Context, EventEmitter, SharedString, Window, div, prelude::*, px, rgb, uniform_list};
use worktree::FileKind;

use crate::{project::Project, theme};

const INDENT: f32 = 14.;

/// Emitted when the user activates a file row.
pub struct OpenFile(pub PathBuf);

impl EventEmitter<OpenFile> for FileTree {}

struct Entry {
    path: PathBuf,
    name: SharedString,
    is_dir: bool,
}

struct Row {
    path: PathBuf,
    name: SharedString,
    is_dir: bool,
    depth: usize,
}

pub struct FileTree {
    project: Project,
    root_name: SharedString,
    expanded: HashSet<PathBuf>,
    /// Directory listings, read once per directory on first expand.
    children: HashMap<PathBuf, Vec<Entry>>,
    rows: Vec<Row>,
    selected: Option<PathBuf>,
}

impl FileTree {
    pub fn new(project: Project) -> Self {
        let root = project.root();
        let root_name = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string())
            .into();
        let mut tree = Self {
            expanded: HashSet::from([root.to_path_buf()]),
            project,
            root_name,
            children: HashMap::new(),
            rows: Vec::new(),
            selected: None,
        };
        tree.rebuild_rows();
        tree
    }

    pub fn select(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.selected = Some(path.to_path_buf());
        cx.notify();
    }

    /// Re-reads the directories containing created or removed `paths`
    /// (everything when `rescan`), keeping expansion state.
    pub fn refresh<'a>(
        &mut self,
        paths: impl IntoIterator<Item = &'a Path>,
        rescan: bool,
        cx: &mut Context<Self>,
    ) {
        if rescan {
            self.children.clear();
        } else {
            for path in paths {
                if let Some(parent) = path.parent() {
                    self.children.remove(parent);
                }
            }
        }
        // Keep expansion when the answer is unknown (e.g. agent disconnected).
        let project = &self.project;
        self.expanded
            .retain(|dir| !matches!(project.kind(dir), Ok(None | Some(FileKind::File))));
        self.rebuild_rows();
        cx.notify();
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        let root = self.project.root().to_path_buf();
        push_rows(
            &self.project,
            &mut self.children,
            &self.expanded,
            &root,
            0,
            &mut self.rows,
        );
    }

    fn activate(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(index) else {
            return;
        };
        let path = row.path.clone();
        if row.is_dir {
            if !self.expanded.remove(&path) {
                self.expanded.insert(path);
            }
            self.rebuild_rows();
        } else {
            self.selected = Some(path.clone());
            cx.emit(OpenFile(path));
        }
        cx.notify();
    }
}

/// Lists one directory; an unreadable one shows as empty.
fn read_dir(project: &Project, dir: &Path) -> Vec<Entry> {
    project
        .read_dir(dir)
        .unwrap_or_default()
        .into_iter()
        .map(|entry| Entry {
            path: project.join(dir, &entry.name),
            name: entry.name.into(),
            is_dir: entry.is_dir,
        })
        .collect()
}

fn push_rows(
    project: &Project,
    children: &mut HashMap<PathBuf, Vec<Entry>>,
    expanded: &HashSet<PathBuf>,
    dir: &Path,
    depth: usize,
    rows: &mut Vec<Row>,
) {
    if !children.contains_key(dir) {
        children.insert(dir.to_path_buf(), read_dir(project, dir));
    }
    for index in 0..children[dir].len() {
        let entry = &children[dir][index];
        let recurse = entry.is_dir && expanded.contains(&entry.path);
        rows.push(Row {
            path: entry.path.clone(),
            name: entry.name.clone(),
            is_dir: entry.is_dir,
            depth,
        });
        if recurse {
            let path = entry.path.clone();
            push_rows(project, children, expanded, &path, depth + 1, rows);
        }
    }
}

impl Render for FileTree {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::PANEL))
            .text_sm()
            .text_color(rgb(theme::TEXT))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(rgb(theme::MUTED))
                    .child(self.root_name.to_uppercase()),
            )
            .child(
                uniform_list(
                    "file-tree",
                    self.rows.len(),
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|index| {
                                let row = &this.rows[index];
                                let selected = this.selected.as_ref() == Some(&row.path);
                                let icon = match (row.is_dir, this.expanded.contains(&row.path)) {
                                    (true, true) => "▾ ",
                                    (true, false) => "▸ ",
                                    (false, _) => "  ",
                                };
                                div()
                                    .id(index)
                                    .flex()
                                    .pl(px(12. + INDENT * row.depth as f32))
                                    .pr_2()
                                    .cursor_pointer()
                                    .when(selected, |row| row.bg(rgb(theme::ACTIVE_ROW)))
                                    .when(!selected, |row| {
                                        row.hover(|style| style.bg(rgb(theme::HOVER_ROW)))
                                    })
                                    .child(div().text_color(rgb(theme::MUTED)).child(icon))
                                    .child(row.name.clone())
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| this.activate(index, cx)),
                                    )
                            })
                            .collect()
                    }),
                )
                .flex_1(),
            )
    }
}
