//! Project file tree: lazily read, gitignore-aware, virtualized.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use gpui::{Context, EventEmitter, SharedString, Window, div, prelude::*, px, uniform_list};
use worktree::FileKind;

use crate::{file_icons, project::Project, theme};

const INDENT: f32 = 14.;

/// Emitted when the user activates a file row.
pub struct OpenFile(pub PathBuf);

impl EventEmitter<OpenFile> for FileTree {}

struct Entry {
    path: PathBuf,
    name: SharedString,
    is_dir: bool,
    /// A folder holding an `__init__.py`.
    package: bool,
}

struct Row {
    path: PathBuf,
    name: SharedString,
    is_dir: bool,
    package: bool,
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
                let parent = path.parent();
                if let Some(parent) = parent {
                    self.children.remove(parent);
                }
                // An `__init__.py` makes its folder a package: re-read the
                // listing showing that folder.
                if path.file_name().is_some_and(|name| name == "__init__.py")
                    && let Some(grandparent) = parent.and_then(Path::parent)
                {
                    self.children.remove(grandparent);
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

/// Lists one directory; an unreadable one shows as empty. Folders that
/// could be Python packages are checked for an `__init__.py`.
fn read_dir(project: &Project, dir: &Path) -> Vec<Entry> {
    project
        .read_dir(dir)
        .unwrap_or_default()
        .into_iter()
        .map(|entry| {
            let path = project.join(dir, &entry.name);
            let package = entry.is_dir
                && file_icons::may_be_package(&entry.name)
                && matches!(
                    project.kind(&project.join(&path, "__init__.py")),
                    Ok(Some(FileKind::File))
                );
            Entry {
                path,
                name: entry.name.into(),
                is_dir: entry.is_dir,
                package,
            }
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
            package: entry.package,
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
            .bg(theme::panel())
            .text_sm()
            .text_color(theme::text())
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(theme::muted())
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
                                let open = row.is_dir && this.expanded.contains(&row.path);
                                let chevron = match (row.is_dir, open) {
                                    (true, true) => "▾",
                                    (true, false) => "▸",
                                    (false, _) => "",
                                };
                                let icon = if row.is_dir {
                                    file_icons::folder(&row.name, open, row.package)
                                } else {
                                    file_icons::file(&row.name)
                                };
                                div()
                                    .id(index)
                                    .w_full()
                                    .flex()
                                    .items_center()
                                    .gap(px(5.))
                                    .h(px(24.))
                                    .pl(px(6. + INDENT * row.depth as f32))
                                    .pr_2()
                                    .cursor_pointer()
                                    .whitespace_nowrap()
                                    .when(selected, |row| row.bg(theme::active_row()))
                                    .when(!selected, |row| {
                                        row.hover(|style| style.bg(theme::hover_row()))
                                    })
                                    .child(
                                        div()
                                            .flex_none()
                                            .w(px(10.))
                                            .text_xs()
                                            .text_color(theme::muted())
                                            .child(chevron),
                                    )
                                    .child(icon)
                                    .child(div().min_w_0().truncate().child(row.name.clone()))
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
