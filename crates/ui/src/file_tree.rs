//! Project file tree: lazily read, gitignore-aware, virtualized, with the
//! file operations of PyCharm's Project view and VS Code's Explorer:
//! Ctrl/Shift+click and keyboard selection, a context menu (rows and empty
//! space), New File / Folder / Python File / Python Package (Alt+Insert) and
//! Rename (F2, Shift+F6) in an inline name editor with live checks,
//! Cut/Copy/Paste (Ctrl+X/C/V), Duplicate (Ctrl+D), Delete, Copy Path /
//! Copy Relative Path, Reveal in the file manager (local projects), Open in
//! Terminal, and drag and drop onto folders (moves; Ctrl copies; Escape
//! cancels; more than [`CONFIRM_MOVE_OVER`] items ask first).
//!
//! A workspace (several folders, or a workspace file) shows one row per
//! folder, in workspace order, with Add Folder to Workspace… / Remove
//! Folder from Workspace in its menu; the folders themselves are never
//! renamed, moved or deleted from here. A plain folder lists its entries
//! at the top level under a header with its name.
//!
//! The tree creates and copies files itself (off the UI thread). Renames,
//! moves and deletes go to the workspace as [`FileTreeEvent`]s, so open
//! editors and language servers follow; it reports back through
//! [`FileTree::moved`] and [`FileTree::deleted`].

use std::{
    any::Any,
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use gpui::{
    App, ClipboardItem, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    FontWeight, KeyBinding, Modifiers, MouseButton, MouseDownEvent, Pixels, Point, PromptLevel,
    ScrollStrategy, SharedString, Subscription, UniformListScrollHandle, Window, actions, anchored,
    deferred, div, point, prelude::*, px, uniform_list,
};
use worktree::FileKind;

use crate::{
    commands,
    editor::{self, Editor, EditorEvent},
    file_icons,
    file_ops::{self, NewKind},
    pinpoint::{self, Pin},
    project::Project,
    selectable::selectable,
    theme,
    workspace::AddFolderToWorkspace,
};

const INDENT: f32 = 14.;
const ROW_HEIGHT: f32 = 24.;
/// Dropping more items than this asks before moving them.
pub const CONFIRM_MOVE_OVER: usize = 5;
const CONTEXT: &str = "FileTree";
/// How long watcher events for paths the tree changed itself are ignored.
const OWN_CHANGE_WINDOW: Duration = Duration::from_secs(3);

actions!(
    file_tree,
    [
        NewFile,
        NewFolder,
        NewPythonFile,
        NewPythonPackage,
        ShowNewMenu,
        Rename,
        Delete,
        Cut,
        Copy,
        Paste,
        Duplicate,
        CopyPath,
        CopyRelativePath,
        RevealInFileManager,
        OpenInTerminal,
        RemoveFolderFromWorkspace,
        SelectPrevious,
        SelectNext,
        ExtendSelectionUp,
        ExtendSelectionDown,
        SelectFirst,
        SelectLast,
        ExpandSelected,
        CollapseSelected,
        OpenSelected,
        SelectAllRows,
        Cancel,
        ConfirmName,
        CancelName
    ]
);

pub fn bind_keys(cx: &mut App) {
    let ctx = Some(CONTEXT);
    cx.bind_keys([
        // PyCharm's New… popup.
        KeyBinding::new("alt-insert", ShowNewMenu, ctx),
        // VS Code, then PyCharm.
        KeyBinding::new("f2", Rename, ctx),
        KeyBinding::new("shift-f6", Rename, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("secondary-x", Cut, ctx),
        KeyBinding::new("secondary-c", Copy, ctx),
        KeyBinding::new("secondary-v", Paste, ctx),
        KeyBinding::new("secondary-d", Duplicate, ctx),
        // PyCharm's Copy Path / Copy Reference keys.
        KeyBinding::new("secondary-shift-c", CopyPath, ctx),
        KeyBinding::new("secondary-alt-shift-c", CopyRelativePath, ctx),
        KeyBinding::new("up", SelectPrevious, ctx),
        KeyBinding::new("down", SelectNext, ctx),
        KeyBinding::new("shift-up", ExtendSelectionUp, ctx),
        KeyBinding::new("shift-down", ExtendSelectionDown, ctx),
        KeyBinding::new("home", SelectFirst, ctx),
        KeyBinding::new("end", SelectLast, ctx),
        KeyBinding::new("right", ExpandSelected, ctx),
        KeyBinding::new("left", CollapseSelected, ctx),
        KeyBinding::new("enter", OpenSelected, ctx),
        KeyBinding::new("secondary-a", SelectAllRows, ctx),
        KeyBinding::new("escape", Cancel, ctx),
        KeyBinding::new("enter", ConfirmName, Some("FileTreeInput > Editor")),
        KeyBinding::new("escape", CancelName, Some("FileTreeInput")),
    ]);
}

/// What the tree asks of the workspace.
#[derive(Debug, Clone, PartialEq)]
pub enum FileTreeEvent {
    /// The user activated a file row: open it; `focus_editor` for Enter and
    /// double-click, else focus stays in the tree (VS Code's single click).
    Open { path: PathBuf, focus_editor: bool },
    /// A file was created from the tree: open it.
    Created(PathBuf),
    /// Rename or move these `(from, to)` pairs (checked, with the language
    /// servers' edits and open editors following), then call
    /// [`FileTree::moved`].
    Move(Vec<(PathBuf, PathBuf)>),
    /// Delete these (asking first), then call [`FileTree::deleted`].
    Delete(Vec<PathBuf>),
    /// Start a terminal in this folder.
    OpenInTerminal(PathBuf),
    /// Remove the project's folder with this index from the workspace.
    RemoveFolder(usize),
    /// An operation failed: show this.
    Notice(String),
}

impl EventEmitter<FileTreeEvent> for FileTree {}

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
    /// The name editor of New…, not an entry.
    new_entry: bool,
    /// A workspace folder's own row.
    root: bool,
}

enum EditKind {
    New { dir: PathBuf, kind: NewKind },
    Rename { path: PathBuf, is_dir: bool },
}

/// The inline name editor.
struct Editing {
    kind: EditKind,
    input: Entity<Editor>,
    error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

/// The open context menu (or Alt+Insert's New popup).
struct TreeMenu {
    position: Point<Pixels>,
    /// The right-clicked row; `None` for empty space (the project root).
    target: Option<PathBuf>,
    new_only: bool,
    /// The item Up/Down moved to, which Enter runs.
    highlighted: Option<usize>,
}

/// A menu item's label, action and whether it applies; `None`: a separator.
type MenuItem = Option<(&'static str, Box<dyn gpui::Action>, bool)>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClipboardMode {
    Cut,
    Copy,
}

/// Files and folders being dragged from a tree.
#[derive(Clone)]
pub struct DraggedPaths {
    tree: EntityId,
    paths: Vec<PathBuf>,
}

struct DragPreview {
    label: SharedString,
}

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_0p5()
            .text_sm()
            .text_color(theme::text())
            .bg(theme::panel())
            .border_1()
            .border_color(theme::accent())
            .rounded_sm()
            .shadow_md()
            .child(self.label.clone())
    }
}

pub struct FileTree {
    project: Project,
    /// The header: the folder's name, or the workspace's.
    header: SharedString,
    /// One row per folder (workspaces), else the folder's entries at the
    /// top level.
    root_rows: bool,
    expanded: HashSet<PathBuf>,
    /// Directory listings, read once per directory on first expand.
    children: HashMap<PathBuf, Vec<Entry>>,
    rows: Vec<Row>,
    /// Selected entries, in selection order.
    selection: Vec<PathBuf>,
    /// The entry keys move from (the last one clicked or reached).
    lead: Option<PathBuf>,
    /// Where Shift+click / Shift+arrow ranges start.
    anchor: Option<PathBuf>,
    focus_handle: FocusHandle,
    scroll: UniformListScrollHandle,
    menu: Option<TreeMenu>,
    editing: Option<Editing>,
    clipboard: Option<(ClipboardMode, Vec<PathBuf>)>,
    /// Paths just created, moved or deleted from the tree, and when: the
    /// tree already shows them, so their watcher events are skipped.
    own_changes: Vec<(PathBuf, Instant)>,
}

impl FileTree {
    pub fn new(project: Project, cx: &mut Context<Self>) -> Self {
        let root_rows = project.is_multi_root() || project.is_workspace();
        let header = if root_rows {
            crate::shell::tab_title(&project.location())
        } else {
            let root = project.root();
            root.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.display().to_string())
        };
        let mut tree = Self {
            // Workspaces start with their first folder open.
            expanded: HashSet::from([project.root().to_path_buf()]),
            project,
            header: header.into(),
            root_rows,
            children: HashMap::new(),
            rows: Vec::new(),
            selection: Vec::new(),
            lead: None,
            anchor: None,
            focus_handle: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            menu: None,
            editing: None,
            clipboard: None,
            own_changes: Vec::new(),
        };
        tree.rebuild_rows();
        tree
    }

    /// Selects only `path` (the active editor's file).
    pub fn select(&mut self, path: &Path, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            return;
        }
        self.set_selection(vec![path.to_path_buf()]);
        cx.notify();
    }

    fn set_selection(&mut self, paths: Vec<PathBuf>) {
        self.lead = paths.last().cloned();
        self.anchor = self.lead.clone();
        self.selection = paths;
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
                if !self.is_own_change(path) {
                    self.invalidate(path);
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

    /// Forgets the listing showing `path` (so it is read again).
    fn invalidate(&mut self, path: &Path) {
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

    /// Expands the folders down to `path`, so its row shows.
    fn reveal(&mut self, path: &Path) {
        let Some(index) = self.project.root_index_of(path) else {
            return;
        };
        let root = self.project.roots()[index].path.clone();
        let mut dir = path.parent();
        while let Some(current) = dir {
            if !current.starts_with(&root) {
                break;
            }
            self.expanded.insert(current.to_path_buf());
            if current == root {
                break;
            }
            dir = current.parent();
        }
    }

    /// Paths about to change through the tree (renames, moves, deletes the
    /// workspace runs for it): their watcher events are not news.
    pub fn expect_changes(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        let now = Instant::now();
        self.own_changes
            .retain(|(_, at)| now.duration_since(*at) < OWN_CHANGE_WINDOW);
        self.own_changes
            .extend(paths.into_iter().map(|path| (path, now)));
    }

    /// Whether `path` (or a folder holding it) just changed through the tree.
    pub fn is_own_change(&self, path: &Path) -> bool {
        self.own_changes
            .iter()
            .any(|(own, at)| at.elapsed() < OWN_CHANGE_WINDOW && path.starts_with(own))
    }

    /// Entries the workspace renamed or moved: listings, expansion, the
    /// selection and the clipboard follow; the new paths are selected.
    pub fn moved(&mut self, moves: &[(PathBuf, PathBuf)], cx: &mut Context<Self>) {
        let follow = |path: &Path| -> PathBuf {
            for (from, to) in moves {
                if let Ok(rest) = path.strip_prefix(from) {
                    return if rest.as_os_str().is_empty() {
                        to.clone()
                    } else {
                        self.project.join(to, &rest.to_string_lossy())
                    };
                }
            }
            path.to_path_buf()
        };
        self.expanded = self.expanded.iter().map(|dir| follow(dir)).collect();
        if let Some((_, paths)) = &mut self.clipboard {
            *paths = paths.iter().map(|path| follow(path)).collect();
        }
        self.children
            .retain(|dir, _| !moves.iter().any(|(from, _)| dir.starts_with(from)));
        for (from, to) in moves {
            self.invalidate(from);
            self.invalidate(to);
            self.reveal(to);
        }
        let selection = moves.iter().map(|(_, to)| to.clone()).collect();
        self.set_selection(selection);
        self.rebuild_rows();
        self.scroll_to_lead();
        cx.notify();
    }

    /// Entries the workspace deleted.
    pub fn deleted(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        let gone = |path: &Path| paths.iter().any(|deleted| path.starts_with(deleted));
        self.expanded.retain(|dir| !gone(dir));
        self.selection.retain(|path| !gone(path));
        if self.lead.as_deref().is_some_and(gone) {
            self.lead = self.selection.last().cloned();
            self.anchor = self.lead.clone();
        }
        if let Some((_, clipboard)) = &mut self.clipboard {
            clipboard.retain(|path| !gone(path));
        }
        self.children.retain(|dir, _| !gone(dir));
        for path in paths {
            self.invalidate(path);
        }
        self.rebuild_rows();
        cx.notify();
    }

    /// Shows new entries (created or pasted here) and selects them.
    fn created(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        self.expect_changes(paths.iter().cloned());
        for path in paths {
            // Every new folder on the way (`a/b/c.py`) needs its listing read.
            let root = self.project.root_for(path).path.clone();
            let mut current = path.as_path();
            while let Some(parent) = current.parent() {
                self.children.remove(parent);
                if parent == root || !parent.starts_with(&root) {
                    break;
                }
                current = parent;
            }
            self.invalidate(path);
            self.reveal(path);
        }
        self.set_selection(paths.to_vec());
        self.rebuild_rows();
        self.scroll_to_lead();
        cx.notify();
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        if self.root_rows {
            for root in self.project.roots() {
                self.rows.push(Row {
                    path: root.path.clone(),
                    name: root.name.clone().into(),
                    is_dir: true,
                    package: false,
                    depth: 0,
                    new_entry: false,
                    root: true,
                });
                if self.expanded.contains(&root.path) {
                    push_rows(
                        &self.project,
                        &mut self.children,
                        &self.expanded,
                        &root.path,
                        1,
                        &mut self.rows,
                    );
                }
            }
        } else {
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
        if let Some(Editing {
            kind: EditKind::New { dir, kind },
            ..
        }) = &self.editing
        {
            let (index, depth) = match self.rows.iter().position(|row| row.path == *dir) {
                Some(index) => (index + 1, self.rows[index].depth + 1),
                None => (0, 0),
            };
            self.rows.insert(
                index,
                Row {
                    path: dir.clone(),
                    // Only picks the icon (see `render_row`).
                    name: if *kind == NewKind::PythonFile {
                        "new.py".into()
                    } else {
                        SharedString::default()
                    },
                    is_dir: kind.is_dir(),
                    package: *kind == NewKind::PythonPackage,
                    depth,
                    new_entry: true,
                    root: false,
                },
            );
        }
        // An entry being renamed that went away (deleted outside) ends
        // the editor.
        if let Some(Editing {
            kind: EditKind::Rename { path, .. },
            ..
        }) = &self.editing
            && !self.rows.iter().any(|row| row.path == *path)
        {
            self.editing = None;
        }
    }

    fn row_index(&self, path: &Path) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| !row.new_entry && row.path == path)
    }

    fn lead_index(&self) -> Option<usize> {
        self.lead.as_deref().and_then(|lead| self.row_index(lead))
    }

    fn scroll_to_lead(&self) {
        if let Some(index) = self.lead_index() {
            self.scroll.scroll_to_item(index, ScrollStrategy::Top);
        }
    }

    fn is_dir(&self, path: &Path) -> bool {
        match self.row_index(path) {
            Some(index) => self.rows[index].is_dir,
            None => self.is_root(path),
        }
    }

    /// Whether `path` is one of the project's folders: never renamed,
    /// moved or deleted from the tree.
    fn is_root(&self, path: &Path) -> bool {
        self.project.roots().iter().any(|root| root.path == path)
    }

    /// Where New…/Paste/Open in Terminal act: the folder of `target` (a
    /// folder itself, or a file's folder); the root without one.
    fn folder_of(&self, target: Option<&Path>) -> PathBuf {
        match target {
            Some(path) if self.is_dir(path) => path.to_path_buf(),
            Some(path) => path.parent().unwrap_or(self.project.root()).to_path_buf(),
            None => self.project.root().to_path_buf(),
        }
    }

    /// The row commands act on: the right-clicked one while the menu is
    /// open, else the lead.
    fn target(&self) -> Option<PathBuf> {
        match &self.menu {
            Some(menu) => menu.target.clone(),
            None => self.lead.clone(),
        }
    }

    /// The entries commands act on, in tree order; nothing for the empty
    /// space's menu.
    fn subjects(&self) -> Vec<PathBuf> {
        if self.menu.as_ref().is_some_and(|menu| menu.target.is_none()) {
            return Vec::new();
        }
        let mut subjects: Vec<PathBuf> = self
            .rows
            .iter()
            .filter(|row| !row.new_entry && self.selection.contains(&row.path))
            .map(|row| row.path.clone())
            .collect();
        if subjects.is_empty()
            && let Some(target) = self.target()
        {
            subjects.push(target);
        }
        subjects
    }

    /// The subjects for Cut, Rename, Duplicate and Delete: none when they
    /// include a project folder.
    fn movable_subjects(&self) -> Vec<PathBuf> {
        let subjects = self.subjects();
        if subjects.iter().any(|path| self.is_root(path)) {
            return Vec::new();
        }
        subjects
    }

    fn notice(&self, message: String, cx: &mut Context<Self>) {
        cx.emit(FileTreeEvent::Notice(message));
    }

    /// Runs a blocking file operation off the UI thread; `done` gets its
    /// result, failures become notices prefixed with `what`.
    fn run<R: Send + 'static>(
        &self,
        what: String,
        operation: impl FnOnce(&Project) -> io::Result<R> + Send + 'static,
        done: impl FnOnce(&mut Self, R, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { operation(&project) })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(value) => done(this, value, cx),
                Err(err) => this.notice(format!("{what}: {err}"), cx),
            })
            .ok();
        })
        .detach();
    }

    fn toggle_dir(&mut self, path: &Path) {
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_path_buf());
        }
        self.rebuild_rows();
    }

    fn activate(&mut self, index: usize, focus_editor: bool, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(index).filter(|row| !row.new_entry) else {
            return;
        };
        let path = row.path.clone();
        if row.is_dir {
            self.toggle_dir(&path);
        } else {
            cx.emit(FileTreeEvent::Open { path, focus_editor });
        }
        cx.notify();
    }

    /// A click on a row: Ctrl toggles it in the selection, Shift selects
    /// the range from the anchor, a plain click selects and activates it
    /// (a double-click on a file also focuses its editor).
    fn click_row(
        &mut self,
        index: usize,
        modifiers: Modifiers,
        click_count: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.rows.get(index).filter(|row| !row.new_entry) else {
            return;
        };
        let (path, is_dir) = (row.path.clone(), row.is_dir);
        self.menu = None;
        if modifiers.secondary() {
            match self.selection.iter().position(|selected| *selected == path) {
                Some(position) => {
                    self.selection.remove(position);
                }
                None => self.selection.push(path.clone()),
            }
            self.lead = Some(path.clone());
            self.anchor = Some(path);
        } else if modifiers.shift {
            self.select_range_to(index);
        } else {
            self.set_selection(vec![path]);
            // The first click of a double-click already opened the folder.
            if !(is_dir && click_count > 1) {
                self.activate(index, click_count > 1, cx);
            }
        }
        cx.notify();
    }

    /// Selects the rows from the anchor to `index`; `index` becomes the lead.
    fn select_range_to(&mut self, index: usize) {
        let anchor = self
            .anchor
            .as_deref()
            .and_then(|anchor| self.row_index(anchor))
            .unwrap_or(index);
        let (start, end) = (anchor.min(index), anchor.max(index));
        self.selection = self.rows[start..=end]
            .iter()
            .filter(|row| !row.new_entry)
            .map(|row| row.path.clone())
            .collect();
        self.lead = Some(self.rows[index].path.clone());
        if self.anchor.is_none() {
            self.anchor = self.lead.clone();
        }
    }

    fn move_lead(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        let entries: Vec<usize> = (0..self.rows.len())
            .filter(|&index| !self.rows[index].new_entry)
            .collect();
        if entries.is_empty() {
            return;
        }
        let last = entries.len() as isize - 1;
        let position = match self.lead_index() {
            Some(lead) => {
                entries.iter().position(|&index| index == lead).unwrap_or(0) as isize + delta
            }
            None if delta < 0 => last,
            None => 0,
        };
        let index = entries[position.clamp(0, last) as usize];
        if extend {
            self.select_range_to(index);
        } else {
            self.set_selection(vec![self.rows[index].path.clone()]);
        }
        self.scroll.scroll_to_item(index, ScrollStrategy::Top);
        cx.notify();
    }

    fn select_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu.is_some() {
            return self.move_menu_highlight(-1, cx);
        }
        self.move_lead(-1, false, cx);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu.is_some() {
            return self.move_menu_highlight(1, cx);
        }
        self.move_lead(1, false, cx);
    }

    /// Up/Down in the open menu: the previous/next enabled item.
    fn move_menu_highlight(&mut self, delta: isize, cx: &mut Context<Self>) {
        let enabled: Vec<usize> = self
            .menu_items()
            .iter()
            .enumerate()
            .filter(|(_, item)| item.as_ref().is_some_and(|(_, _, enabled)| *enabled))
            .map(|(index, _)| index)
            .collect();
        let Some(menu) = &mut self.menu else {
            return;
        };
        if enabled.is_empty() {
            return;
        }
        let last = enabled.len() as isize - 1;
        let position = match menu
            .highlighted
            .and_then(|current| enabled.iter().position(|&index| index == current))
        {
            Some(position) => (position as isize + delta).clamp(0, last),
            None if delta < 0 => last,
            None => 0,
        };
        menu.highlighted = Some(enabled[position as usize]);
        cx.notify();
    }

    fn extend_up(&mut self, _: &ExtendSelectionUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_lead(-1, true, cx);
    }

    fn extend_down(&mut self, _: &ExtendSelectionDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_lead(1, true, cx);
    }

    fn select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.move_lead(-(self.rows.len() as isize), false, cx);
    }

    fn select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.move_lead(self.rows.len() as isize, false, cx);
    }

    /// Right: opens a folder, or steps into an open one.
    fn expand_selected(&mut self, _: &ExpandSelected, _: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.lead_index() else {
            return;
        };
        let row = &self.rows[index];
        if !row.is_dir {
            return;
        }
        let path = row.path.clone();
        if self.expanded.contains(&path) {
            self.move_lead(1, false, cx);
        } else {
            self.toggle_dir(&path);
            cx.notify();
        }
    }

    /// Left: closes an open folder, else goes to the parent folder.
    fn collapse_selected(&mut self, _: &CollapseSelected, _: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.lead_index() else {
            return;
        };
        let row = &self.rows[index];
        let path = row.path.clone();
        if row.is_dir && self.expanded.contains(&path) {
            self.toggle_dir(&path);
        } else if let Some(parent) = path.parent()
            && self.row_index(parent).is_some()
        {
            self.set_selection(vec![parent.to_path_buf()]);
            self.scroll_to_lead();
        }
        cx.notify();
    }

    fn open_selected(&mut self, _: &OpenSelected, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(menu) = &self.menu {
            let action = menu.highlighted.and_then(|index| {
                let (_, action, enabled) = self.menu_items().into_iter().nth(index)??;
                enabled.then_some(action)
            });
            if let Some(action) = action {
                window.dispatch_action(action, cx);
            }
            return;
        }
        if let Some(index) = self.lead_index() {
            self.activate(index, true, cx);
        }
    }

    fn select_all_rows(&mut self, _: &SelectAllRows, _: &mut Window, cx: &mut Context<Self>) {
        let lead = self.lead.clone();
        self.selection = self
            .rows
            .iter()
            .filter(|row| !row.new_entry)
            .map(|row| row.path.clone())
            .collect();
        self.lead = lead.or_else(|| self.selection.first().cloned());
        cx.notify();
    }

    /// Escape: cancels a drag, closes the menu, forgets a Cut.
    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if cx.stop_active_drag(window) {
            return;
        }
        if self.menu.take().is_none() {
            self.clipboard = None;
        }
        cx.notify();
    }

    fn open_menu(
        &mut self,
        target: Option<PathBuf>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        if let Some(path) = &target
            && !self.selection.contains(path)
        {
            self.set_selection(vec![path.clone()]);
        }
        self.menu = Some(TreeMenu {
            position,
            target,
            new_only: false,
            highlighted: None,
        });
        cx.notify();
    }

    /// Alt+Insert: the New… items, under the lead row.
    fn show_new_menu(&mut self, _: &ShowNewMenu, _: &mut Window, cx: &mut Context<Self>) {
        let (bounds, offset) = {
            let state = self.scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        let (index, depth) = match self.lead_index() {
            Some(index) => (index + 1, self.rows[index].depth),
            None => (0, 0),
        };
        let position = point(
            bounds.origin.x + px(16. + INDENT * depth as f32),
            bounds.origin.y + offset.y + px(ROW_HEIGHT * index as f32),
        );
        self.menu = Some(TreeMenu {
            position,
            target: self.lead.clone(),
            new_only: true,
            highlighted: Some(0),
        });
        cx.notify();
    }

    fn start_new(&mut self, kind: NewKind, window: &mut Window, cx: &mut Context<Self>) {
        let dir = self.folder_of(self.target().as_deref());
        self.menu = None;
        self.reveal(&self.project.join(&dir, "x"));
        self.start_editing(EditKind::New { dir, kind }, "", window, cx);
    }

    fn new_file(&mut self, _: &NewFile, window: &mut Window, cx: &mut Context<Self>) {
        self.start_new(NewKind::File, window, cx);
    }

    fn new_folder(&mut self, _: &NewFolder, window: &mut Window, cx: &mut Context<Self>) {
        self.start_new(NewKind::Folder, window, cx);
    }

    fn new_python_file(&mut self, _: &NewPythonFile, window: &mut Window, cx: &mut Context<Self>) {
        self.start_new(NewKind::PythonFile, window, cx);
    }

    fn new_python_package(
        &mut self,
        _: &NewPythonPackage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_new(NewKind::PythonPackage, window, cx);
    }

    fn rename(&mut self, _: &Rename, window: &mut Window, cx: &mut Context<Self>) {
        let subjects = self.movable_subjects();
        self.menu = None;
        let [path] = subjects.as_slice() else {
            cx.notify();
            return;
        };
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let is_dir = self.is_dir(path);
        self.set_selection(vec![path.clone()]);
        self.start_editing(
            EditKind::Rename {
                path: path.clone(),
                is_dir,
            },
            &name,
            window,
            cx,
        );
    }

    fn start_editing(
        &mut self,
        kind: EditKind,
        initial: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The stem is selected, as in VS Code and PyCharm (`main|.py`).
        let stem_end = match (&kind, initial.rfind('.')) {
            (EditKind::Rename { is_dir: false, .. }, Some(dot)) if dot > 0 => dot,
            _ => initial.len(),
        };
        let input = cx.new(|cx| {
            let mut editor = Editor::single_line(cx);
            editor.set_text(initial, cx);
            editor.select_text_range(0, stem_end, cx);
            editor
        });
        let edited = cx.subscribe(&input, |this: &mut Self, _, event, cx| {
            if let EditorEvent::Edited = event {
                this.check_input(cx);
            }
        });
        let focus = input.focus_handle(cx);
        // Clicking elsewhere keeps a valid name (VS Code) and drops an
        // invalid one.
        let blurred = cx.on_blur(&focus, window, |this, window, cx| {
            if this.editing.is_some() {
                this.finish_editing(window, cx);
                if this.editing.is_some() {
                    this.editing = None;
                    this.rebuild_rows();
                    cx.notify();
                }
            }
        });
        self.editing = Some(Editing {
            kind,
            input,
            error: None,
            _subscriptions: vec![edited, blurred],
        });
        self.rebuild_rows();
        let index = match &self.editing {
            Some(Editing {
                kind: EditKind::Rename { path, .. },
                ..
            }) => self.row_index(path),
            _ => self.rows.iter().position(|row| row.new_entry),
        };
        if let Some(index) = index {
            self.scroll.scroll_to_item(index, ScrollStrategy::Top);
        }
        window.focus(&focus);
        cx.notify();
    }

    /// The problem with the typed name, if any: invalid for the platform,
    /// or taken in the folder (as listed; the operation checks again).
    fn input_error(&self, cx: &App) -> Option<String> {
        let editing = self.editing.as_ref()?;
        let text = editing.input.read(cx).text();
        let windows = file_ops::windows_names(&self.project);
        let (dir, name, own) = match &editing.kind {
            EditKind::New { dir, kind } => {
                let input = file_ops::new_entry_input(&text, *kind);
                let parts = match file_ops::new_entry_parts(&input, windows) {
                    Ok(parts) => parts,
                    Err(err) => return Some(err),
                };
                (dir.clone(), parts[0].to_owned(), None)
            }
            EditKind::Rename { path, .. } => {
                let name = text.trim();
                if let Err(err) = file_ops::check_name(name, windows) {
                    return Some(err);
                }
                let own = path
                    .file_name()
                    .map(|own| own.to_string_lossy().into_owned());
                let dir = path.parent().unwrap_or(self.project.root()).to_path_buf();
                (dir, name.to_owned(), own)
            }
        };
        if own
            .as_deref()
            .is_some_and(|own| file_ops::same_name(own, &name, windows))
        {
            return None;
        }
        let taken = self.children.get(&dir).is_some_and(|entries| {
            entries
                .iter()
                .any(|entry| file_ops::same_name(&entry.name, &name, windows))
        });
        // A typed path into an existing folder (`src/new.py`) is fine.
        let into_folder = matches!(editing.kind, EditKind::New { .. })
            && text.trim().len() > name.len()
            && self.children.get(&dir).is_some_and(|entries| {
                entries
                    .iter()
                    .any(|entry| entry.is_dir && file_ops::same_name(&entry.name, &name, windows))
            });
        (taken && !into_folder)
            .then(|| format!("A file or folder `{name}` already exists at this location"))
    }

    fn check_input(&mut self, cx: &mut Context<Self>) {
        let error = self.input_error(cx).map(SharedString::from);
        if let Some(editing) = &mut self.editing {
            editing.error = error;
        }
        cx.notify();
    }

    /// Enter: creates or renames when the name is fine (else shows why);
    /// an unchanged or empty-and-new name just closes the editor.
    fn finish_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = &self.editing else {
            return;
        };
        let text = editing.input.read(cx).text();
        let text = text.trim().to_owned();
        if text.is_empty() && matches!(editing.kind, EditKind::New { .. }) {
            return self.cancel_editing(window, cx);
        }
        if let Some(error) = self.input_error(cx) {
            if let Some(editing) = &mut self.editing {
                editing.error = Some(error.into());
            }
            return cx.notify();
        }
        let Some(editing) = self.editing.take() else {
            return;
        };
        let refocus = editing.input.focus_handle(cx).is_focused(window);
        self.rebuild_rows();
        if refocus {
            window.focus(&self.focus_handle);
        }
        cx.notify();
        match editing.kind {
            EditKind::New { dir, kind } => {
                let what = format!("Could not create `{text}`");
                self.run(
                    what,
                    move |project| file_ops::create(project, &dir, &text, kind),
                    move |this, path, cx| {
                        this.created(std::slice::from_ref(&path), cx);
                        if !kind.is_dir() {
                            cx.emit(FileTreeEvent::Created(path));
                        }
                    },
                    cx,
                );
            }
            EditKind::Rename { path, .. } => {
                let to = file_ops::sibling(&self.project, &path, &text);
                if to != path {
                    cx.emit(FileTreeEvent::Move(vec![(path, to)]));
                }
            }
        }
    }

    fn cancel_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        if editing.input.focus_handle(cx).is_focused(window) {
            window.focus(&self.focus_handle);
        }
        self.rebuild_rows();
        cx.notify();
    }

    fn confirm_name(&mut self, _: &ConfirmName, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_editing(window, cx);
    }

    /// Enter in the single-line input, where it is not bound to Confirm.
    fn input_newline(&mut self, _: &editor::Newline, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_editing(window, cx);
    }

    fn cancel_name(&mut self, _: &CancelName, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_editing(window, cx);
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        let subjects = self.movable_subjects();
        self.menu = None;
        if !subjects.is_empty() {
            cx.emit(FileTreeEvent::Delete(subjects));
        }
        cx.notify();
    }

    fn set_clipboard(&mut self, mode: ClipboardMode, cx: &mut Context<Self>) {
        let subjects = match mode {
            ClipboardMode::Cut => self.movable_subjects(),
            ClipboardMode::Copy => self.subjects(),
        };
        self.menu = None;
        if !subjects.is_empty() {
            // The paths as text too, for pasting into a terminal or a chat.
            let text = subjects
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.clipboard = Some((mode, subjects));
        }
        cx.notify();
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        self.set_clipboard(ClipboardMode::Cut, cx);
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        self.set_clipboard(ClipboardMode::Copy, cx);
    }

    /// Paste into the target's folder: copies get free names (` copy`),
    /// cut entries move (the workspace refuses taken names).
    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let dir = self.folder_of(self.target().as_deref());
        self.menu = None;
        cx.notify();
        let Some((mode, paths)) = self.clipboard.clone() else {
            return;
        };
        match mode {
            ClipboardMode::Copy => self.copy_into(paths, dir, cx),
            ClipboardMode::Cut => {
                self.clipboard = None;
                self.move_into(paths, dir, cx);
            }
        }
    }

    fn copy_into(&mut self, paths: Vec<PathBuf>, dir: PathBuf, cx: &mut Context<Self>) {
        self.run(
            "Could not copy".into(),
            move |project| file_ops::copy_into(project, &paths, &dir),
            |this, copies, cx| this.created(&copies, cx),
            cx,
        );
    }

    fn move_into(&mut self, paths: Vec<PathBuf>, dir: PathBuf, cx: &mut Context<Self>) {
        let moves = file_ops::move_targets(&self.project, &paths, &dir);
        if !moves.is_empty() {
            cx.emit(FileTreeEvent::Move(moves));
        }
    }

    fn duplicate(&mut self, _: &Duplicate, _: &mut Window, cx: &mut Context<Self>) {
        let subjects = self.movable_subjects();
        self.menu = None;
        cx.notify();
        if subjects.is_empty() {
            return;
        }
        self.run(
            "Could not duplicate".into(),
            move |project| {
                subjects
                    .iter()
                    .map(|path| file_ops::duplicate(project, path))
                    .collect::<io::Result<Vec<_>>>()
            },
            |this, copies, cx| this.created(&copies, cx),
            cx,
        );
    }

    fn copy_paths(&mut self, relative: bool, cx: &mut Context<Self>) {
        let subjects = self.subjects();
        self.menu = None;
        cx.notify();
        if subjects.is_empty() {
            return;
        }
        // Relative to the path's own folder (`.` for the folder itself).
        let text = subjects
            .iter()
            .map(
                |path| match path.strip_prefix(&self.project.root_for(path).path) {
                    Ok(rest) if relative && rest.as_os_str().is_empty() => ".".to_owned(),
                    Ok(rest) if relative => rest.display().to_string(),
                    _ => path.display().to_string(),
                },
            )
            .collect::<Vec<_>>()
            .join("\n");
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn copy_path(&mut self, _: &CopyPath, _: &mut Window, cx: &mut Context<Self>) {
        self.copy_paths(false, cx);
    }

    fn copy_relative_path(&mut self, _: &CopyRelativePath, _: &mut Window, cx: &mut Context<Self>) {
        self.copy_paths(true, cx);
    }

    fn reveal_in_file_manager(
        &mut self,
        _: &RevealInFileManager,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self
            .target()
            .unwrap_or_else(|| self.project.root().to_path_buf());
        self.menu = None;
        cx.notify();
        if self.project.is_remote() {
            return self.notice(
                "Reveal works for local projects only (this one lives in WSL or a container)"
                    .into(),
                cx,
            );
        }
        cx.reveal_path(&target);
    }

    fn open_in_terminal(&mut self, _: &OpenInTerminal, _: &mut Window, cx: &mut Context<Self>) {
        let dir = self.folder_of(self.target().as_deref());
        self.menu = None;
        cx.notify();
        cx.emit(FileTreeEvent::OpenInTerminal(dir));
    }

    /// A folder row's Remove Folder from Workspace (the workspace reopens
    /// without it).
    fn remove_folder_from_workspace(
        &mut self,
        _: &RemoveFolderFromWorkspace,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self.target();
        self.menu = None;
        cx.notify();
        let index = target.and_then(|path| {
            self.project
                .roots()
                .iter()
                .position(|root| root.path == path)
        });
        if self.root_rows
            && let Some(index) = index
        {
            cx.emit(FileTreeEvent::RemoveFolder(index));
        }
    }

    /// Dragged entries dropped on `dir`: moved there (Ctrl: copied). More
    /// than [`CONFIRM_MOVE_OVER`] items ask first; dropping a folder into
    /// itself does nothing.
    fn drop_paths(
        &mut self,
        paths: Vec<PathBuf>,
        dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let windows = file_ops::windows_names(&self.project);
        let paths = file_ops::top_level(&paths, windows);
        if paths
            .iter()
            .any(|path| file_ops::is_within(&dir, path, windows))
        {
            return;
        }
        if window.modifiers().secondary() {
            return self.copy_into(paths, dir, cx);
        }
        let moves = file_ops::move_targets(&self.project, &paths, &dir);
        if moves.len() <= CONFIRM_MOVE_OVER {
            if !moves.is_empty() {
                cx.emit(FileTreeEvent::Move(moves));
            }
            return;
        }
        let name = match self.project.roots().iter().find(|root| root.path == dir) {
            Some(root) => root.name.clone(),
            None => dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        let answer = window.prompt(
            PromptLevel::Info,
            &format!("Move {} items to `{name}`?", moves.len()),
            None,
            &["Move", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await == Ok(0) {
                this.update(cx, |_, cx| cx.emit(FileTreeEvent::Move(moves)))
                    .ok();
            }
        })
        .detach();
    }

    fn is_cut(&self, path: &Path) -> bool {
        matches!(&self.clipboard, Some((ClipboardMode::Cut, paths)) if paths.iter().any(|cut| path.starts_with(cut)))
    }

    fn render_input(&self, editing: &Editing) -> impl IntoElement + use<> {
        div()
            .key_context("FileTreeInput")
            .flex_1()
            .min_w_0()
            .relative()
            .h(px(20.))
            .px_1()
            .border_1()
            .border_color(if editing.error.is_some() {
                theme::error()
            } else {
                theme::accent()
            })
            .bg(theme::background())
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(editing.input.clone())
            .children(editing.error.clone().map(|error| {
                div().absolute().top(px(20.)).left(px(-1.)).child(deferred(
                    anchored().snap_to_window().child(
                        div()
                            .occlude()
                            .max_w(px(320.))
                            .px_2()
                            .py_1()
                            .text_xs()
                            .text_color(theme::text())
                            .bg(theme::panel())
                            .border_1()
                            .border_color(theme::error())
                            .child(selectable("file-tree-input-error", error))
                            .pin("error"),
                    ),
                ))
            }))
            .pin("name-input")
            .label("Name")
    }

    /// The menu's items (`None`: a separator) with their actions and
    /// whether they apply.
    fn menu_items(&self) -> Vec<MenuItem> {
        let Some(menu) = &self.menu else {
            return Vec::new();
        };
        let subjects = self.subjects();
        let single = subjects.len() == 1;
        let any = !subjects.is_empty();
        // Folders of the project are never moved, renamed or deleted here.
        let movable = any && !subjects.iter().any(|path| self.is_root(path));
        let paste = self.clipboard.is_some();
        let reveal_label = if cfg!(windows) {
            "Reveal in File Explorer"
        } else if cfg!(target_os = "macos") {
            "Reveal in Finder"
        } else {
            "Reveal in File Manager"
        };
        let mut items: Vec<MenuItem> = vec![
            Some(("New File…", Box::new(NewFile), true)),
            Some(("New Folder…", Box::new(NewFolder), true)),
            Some(("New Python File…", Box::new(NewPythonFile), true)),
            Some(("New Python Package…", Box::new(NewPythonPackage), true)),
        ];
        if !menu.new_only {
            items.extend([
                None,
                Some(("Cut", Box::new(Cut) as Box<dyn gpui::Action>, movable)),
                Some(("Copy", Box::new(Copy), any)),
                Some(("Paste", Box::new(Paste), paste)),
                Some(("Duplicate", Box::new(Duplicate), movable)),
                None,
                Some(("Copy Path", Box::new(CopyPath), any)),
                Some(("Copy Relative Path", Box::new(CopyRelativePath), any)),
                None,
                Some(("Rename…", Box::new(Rename), single && movable)),
                Some(("Delete…", Box::new(Delete), movable)),
                None,
            ]);
            if !self.project.is_remote() {
                items.push(Some((reveal_label, Box::new(RevealInFileManager), true)));
            }
            items.push(Some(("Open in Terminal", Box::new(OpenInTerminal), true)));
            if self.root_rows
                && menu
                    .target
                    .as_deref()
                    .is_some_and(|path| self.is_root(path))
            {
                items.extend([
                    None,
                    Some((
                        "Add Folder to Workspace…",
                        Box::new(AddFolderToWorkspace) as Box<dyn gpui::Action>,
                        true,
                    )),
                    // The workspace refuses to remove its last folder.
                    Some((
                        "Remove Folder from Workspace",
                        Box::new(RemoveFolderFromWorkspace),
                        self.project.roots().len() > 1,
                    )),
                ]);
            }
        }
        items
    }

    fn render_menu(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let menu = self.menu.as_ref()?;
        let highlighted = menu.highlighted;
        let items = self.menu_items();
        let focus = self.focus_handle.clone();
        let rows = items.into_iter().enumerate().map(|(index, item)| {
            let Some((label, action, enabled)) = item else {
                return div()
                    .my_1()
                    .h(px(1.))
                    .bg(theme::border())
                    .into_any_element();
            };
            let keys = commands::key_text(window, action.as_ref(), Some(&focus));
            let id = format!("item:{}", pinpoint::kebab(label));
            div()
                .id(("file-tree-menu-item", index))
                .px_3()
                .py_1()
                .rounded_sm()
                .flex()
                .gap_4()
                .justify_between()
                .when(!enabled, |item| item.text_color(theme::muted()))
                .when(highlighted == Some(index), |item| {
                    item.bg(theme::active_row())
                })
                .when(enabled, |item| {
                    item.cursor_pointer()
                        .hover(|style| style.bg(theme::active_row()))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            window.focus(&this.focus_handle);
                            window.dispatch_action(action.boxed_clone(), cx);
                        }))
                })
                .child(label)
                .children(keys.map(|keys| div().text_color(theme::muted()).child(keys)))
                .pin(id)
                .label(label)
                .into_any_element()
        });
        Some(deferred(
            anchored().position(menu.position).snap_to_window().child(
                div()
                    .id("file-tree-menu")
                    .occlude()
                    .min_w(px(240.))
                    .p_1()
                    .flex()
                    .flex_col()
                    .text_sm()
                    .text_color(theme::text())
                    .bg(theme::panel())
                    .border_1()
                    .border_color(theme::border())
                    .rounded_md()
                    .shadow_lg()
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.menu = None;
                        cx.notify();
                    }))
                    .children(rows)
                    .pin("menu"),
            ),
        ))
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
            new_entry: false,
            root: false,
        });
        if recurse {
            let path = entry.path.clone();
            push_rows(project, children, expanded, &path, depth + 1, rows);
        }
    }
}

impl Focusable for FileTree {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FileTree {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tree = cx.entity_id();
        let root = self.project.root().to_path_buf();
        // Workspaces take drops on folder rows only: empty space belongs
        // to no folder.
        let drops_on_root = !self.root_rows;
        let accepts = move |value: &dyn Any, _: &mut Window, _: &mut App| {
            drops_on_root
                && value
                    .downcast_ref::<DraggedPaths>()
                    .is_some_and(|dragged| dragged.tree == tree)
        };
        let root_drop = root.clone();
        let header_drop = root.clone();
        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::new_file))
            .on_action(cx.listener(Self::new_folder))
            .on_action(cx.listener(Self::new_python_file))
            .on_action(cx.listener(Self::new_python_package))
            .on_action(cx.listener(Self::show_new_menu))
            .on_action(cx.listener(Self::rename))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::duplicate))
            .on_action(cx.listener(Self::copy_path))
            .on_action(cx.listener(Self::copy_relative_path))
            .on_action(cx.listener(Self::reveal_in_file_manager))
            .on_action(cx.listener(Self::open_in_terminal))
            .on_action(cx.listener(Self::remove_folder_from_workspace))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::extend_up))
            .on_action(cx.listener(Self::extend_down))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::expand_selected))
            .on_action(cx.listener(Self::collapse_selected))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(Self::select_all_rows))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm_name))
            .on_action(cx.listener(Self::cancel_name))
            .on_action(cx.listener(Self::input_newline))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::panel())
            .text_sm()
            .text_color(theme::text())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.focus_handle);
                    if this.menu.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .child(
                div()
                    .id("file-tree-header")
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(self.header.to_uppercase())
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            this.open_menu(None, event.position, window, cx)
                        }),
                    )
                    .can_drop(accepts)
                    .drag_over::<DraggedPaths>(|style, _, _, _| style.bg(theme::hover_row()))
                    .on_drop(
                        cx.listener(move |this, dragged: &DraggedPaths, window, cx| {
                            this.drop_paths(dragged.paths.clone(), header_drop.clone(), window, cx)
                        }),
                    )
                    .pin("header")
                    .label(self.header.clone()),
            )
            .child(
                div()
                    .id("file-tree-body")
                    .flex_1()
                    .min_h_0()
                    .can_drop(accepts)
                    .on_drop(
                        cx.listener(move |this, dragged: &DraggedPaths, window, cx| {
                            this.drop_paths(dragged.paths.clone(), root_drop.clone(), window, cx)
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            this.open_menu(None, event.position, window, cx)
                        }),
                    )
                    // A click on empty space clears the selection.
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.selection.clear();
                        this.lead = None;
                        this.anchor = None;
                        cx.notify();
                    }))
                    .child(
                        uniform_list(
                            "file-tree",
                            self.rows.len(),
                            cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                                let focused = this.focus_handle.is_focused(window);
                                range
                                    .map(|index| this.render_row(index, focused, tree, cx))
                                    .collect()
                            }),
                        )
                        .track_scroll(self.scroll.clone())
                        .size_full(),
                    )
                    .pin("tree"),
            )
            .children(self.render_menu(window, cx))
    }
}

impl FileTree {
    fn render_row(
        &self,
        index: usize,
        focused: bool,
        tree: EntityId,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let row = &self.rows[index];
        let indent = px(6. + INDENT * row.depth as f32);
        if row.new_entry {
            let icon = if row.is_dir {
                file_icons::folder("", false, row.package)
            } else {
                // The typed name decides later; a Python file shows as one.
                file_icons::file(&row.name)
            };
            return div()
                .id(("file-tree-new", index))
                .w_full()
                .flex()
                .items_center()
                .gap(px(5.))
                .h(px(ROW_HEIGHT))
                .pl(indent)
                .pr_2()
                .child(div().flex_none().w(px(10.)))
                .child(icon)
                .children(
                    self.editing
                        .as_ref()
                        .map(|editing| self.render_input(editing)),
                )
                .pin("new-entry")
                .into_any_element();
        }
        let selected = self.selection.contains(&row.path);
        let lead = focused && self.lead.as_ref() == Some(&row.path);
        let open = row.is_dir && self.expanded.contains(&row.path);
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
        let renaming = self.editing.as_ref().filter(
            |editing| matches!(&editing.kind, EditKind::Rename { path, .. } if *path == row.path),
        );
        let drop_dir = if row.is_dir {
            row.path.clone()
        } else {
            row.path
                .parent()
                .unwrap_or(self.project.root())
                .to_path_buf()
        };
        // Folders of the project stay where they are.
        let dragged = DraggedPaths {
            tree,
            paths: if selected {
                let mut paths = self.subjects_in_order();
                paths.retain(|path| !self.is_root(path));
                paths
            } else {
                vec![row.path.clone()]
            },
        };
        let draggable = !row.root;
        let path = row.path.clone();
        let name = row.name.clone();
        div()
            .id(("file-tree-row", index))
            .debug_selector(|| format!("file-tree-row-{name}"))
            .w_full()
            .flex()
            .items_center()
            .gap(px(5.))
            .h(px(ROW_HEIGHT))
            .pl(indent)
            .pr_2()
            .cursor_pointer()
            .whitespace_nowrap()
            .when(self.is_cut(&row.path), |row| row.opacity(0.5))
            .when(selected, |row| row.bg(theme::active_row()))
            .when(!selected, |row| {
                row.hover(|style| style.bg(theme::hover_row()))
            })
            .border_1()
            .border_color(if lead {
                theme::accent()
            } else {
                gpui::transparent_black()
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
            .map(|element| match renaming {
                Some(editing) => element.child(self.render_input(editing)),
                None => element.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .when(row.root, |name| name.font_weight(FontWeight::SEMIBOLD))
                        .child(row.name.clone()),
                ),
            })
            .when(renaming.is_none(), |element| {
                element
                    .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.click_row(index, event.modifiers(), event.click_count(), cx);
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.open_menu(Some(path.clone()), event.position, window, cx);
                        }),
                    )
                    .when(draggable, |element| {
                        element.on_drag(dragged, |dragged: &DraggedPaths, _, _, cx| {
                            let label = match dragged.paths.as_slice() {
                                [path] => path
                                    .file_name()
                                    .map(|name| name.to_string_lossy().into_owned())
                                    .unwrap_or_default(),
                                paths => format!("{} items", paths.len()),
                            };
                            cx.new(|_| DragPreview {
                                label: label.into(),
                            })
                        })
                    })
            })
            .can_drop(move |value, _, _| {
                value
                    .downcast_ref::<DraggedPaths>()
                    .is_some_and(|dragged| dragged.tree == tree)
            })
            .when(row.is_dir, |row| {
                row.drag_over::<DraggedPaths>(|style, _, _, _| {
                    style
                        .bg(theme::hover_row())
                        .border_1()
                        .border_color(theme::accent())
                })
            })
            .on_drop(
                cx.listener(move |this, dragged: &DraggedPaths, window, cx| {
                    this.drop_paths(dragged.paths.clone(), drop_dir.clone(), window, cx)
                }),
            )
            .pin(format!(
                "row:{}",
                pinpoint::file_key(&self.project, &row.path)
            ))
            .label(row.name.clone())
            .interactive(true)
            .into_any_element()
    }

    /// The selection in tree order.
    fn subjects_in_order(&self) -> Vec<PathBuf> {
        self.rows
            .iter()
            .filter(|row| !row.new_entry && self.selection.contains(&row.path))
            .map(|row| row.path.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use gpui::{TestAppContext, VisualTestContext};

    use super::*;

    fn project_dir(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("nc-tree-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.py"), "print()\n").unwrap();
        fs::write(root.join("a.py"), "a\n").unwrap();
        fs::write(root.join("b.py"), "b\n").unwrap();
        root
    }

    /// A focused tree over a local folder in a window, and its events.
    fn open_tree<'a>(
        root: &Path,
        cx: &'a mut TestAppContext,
    ) -> (
        Entity<FileTree>,
        &'a mut VisualTestContext,
        std::rc::Rc<std::cell::RefCell<Vec<FileTreeEvent>>>,
    ) {
        open_project_tree(Project::local(root.to_path_buf()), cx)
    }

    /// A local workspace of `(folder, name)`s, without a file.
    fn workspace(folders: &[(&Path, &str)]) -> Project {
        let location = crate::project::Location::Workspace {
            distro: None,
            file: None,
            folders: folders
                .iter()
                .map(|(path, name)| crate::workspace_file::Folder {
                    path: path.to_string_lossy().into_owned(),
                    name: Some((*name).to_owned()),
                })
                .collect(),
        };
        Project::open(&location, &mut |_| {}).unwrap()
    }

    /// A focused tree in a window, and the events it emitted.
    fn open_project_tree(
        project: Project,
        cx: &mut TestAppContext,
    ) -> (
        Entity<FileTree>,
        &mut VisualTestContext,
        std::rc::Rc<std::cell::RefCell<Vec<FileTreeEvent>>>,
    ) {
        cx.update(|cx| {
            crate::editor::bind_keys(cx);
            bind_keys(cx);
        });
        let (tree, cx) = cx.add_window_view(|_, cx| FileTree::new(project, cx));
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = events.clone();
        cx.update(|window, cx| {
            cx.subscribe(&tree, move |_, event: &FileTreeEvent, _| {
                sink.borrow_mut().push(event.clone())
            })
            .detach();
            window.focus(&tree.focus_handle(cx));
        });
        cx.run_until_parked();
        (tree, cx, events)
    }

    fn names(tree: &Entity<FileTree>, cx: &mut VisualTestContext) -> Vec<String> {
        tree.read_with(cx, |tree, _| {
            tree.rows
                .iter()
                .map(|row| {
                    let name = if row.new_entry { "<new>" } else { &row.name };
                    format!("{}{name}", "  ".repeat(row.depth))
                })
                .collect()
        })
    }

    fn selection(tree: &Entity<FileTree>, cx: &mut VisualTestContext) -> Vec<String> {
        tree.read_with(cx, |tree, _| {
            tree.subjects_in_order()
                .iter()
                .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
                .collect()
        })
    }

    fn error(tree: &Entity<FileTree>, cx: &mut VisualTestContext) -> Option<String> {
        tree.read_with(cx, |tree, _| {
            tree.editing
                .as_ref()
                .and_then(|editing| editing.error.as_ref().map(|error| error.to_string()))
        })
    }

    #[gpui::test]
    fn new_file_is_typed_inline_checked_live_and_created(cx: &mut TestAppContext) {
        let root = project_dir("new");
        let (tree, cx, events) = open_tree(&root, cx);
        // Select a file inside src: New File goes into its folder.
        cx.simulate_keystrokes("down right down");
        assert_eq!(selection(&tree, cx), ["main.py"]);
        tree.update_in(cx, |tree, window, cx| tree.new_file(&NewFile, window, cx));
        cx.run_until_parked();
        assert_eq!(
            names(&tree, cx),
            ["src", "  <new>", "  main.py", "a.py", "b.py"]
        );

        // A taken name is reported while typing, and Enter keeps the editor.
        cx.simulate_input("main.py");
        cx.run_until_parked();
        assert_eq!(
            error(&tree, cx).as_deref(),
            Some("A file or folder `main.py` already exists at this location")
        );
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(tree.read_with(cx, |tree, _| tree.editing.is_some()));

        // A path creates its folders; Enter creates and selects the file.
        tree.update_in(cx, |tree, _, cx| {
            let input = tree.editing.as_ref().unwrap().input.clone();
            input.update(cx, |input, cx| input.set_text("pkg/util.py", cx));
        });
        cx.run_until_parked();
        assert_eq!(error(&tree, cx), None);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let created = root.join("src").join("pkg").join("util.py");
        assert!(created.is_file());
        assert_eq!(
            names(&tree, cx),
            ["src", "  pkg", "    util.py", "  main.py", "a.py", "b.py"]
        );
        assert_eq!(selection(&tree, cx), ["util.py"]);
        assert_eq!(*events.borrow(), [FileTreeEvent::Created(created)]);
        fs::remove_dir_all(&root).ok();
    }

    #[gpui::test]
    fn rename_selects_the_stem_and_escape_cancels(cx: &mut TestAppContext) {
        let root = project_dir("rename");
        let (tree, cx, events) = open_tree(&root, cx);
        cx.simulate_keystrokes("down down");
        assert_eq!(selection(&tree, cx), ["a.py"]);

        // Escape: nothing happens.
        cx.simulate_keystrokes("f2");
        cx.simulate_input("zzz");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(events.borrow().is_empty());
        assert_eq!(names(&tree, cx), ["src", "a.py", "b.py"]);

        // Typing replaces the stem only; a taken name is refused.
        cx.simulate_keystrokes("shift-f6");
        cx.simulate_input("b");
        cx.run_until_parked();
        assert_eq!(
            error(&tree, cx).as_deref(),
            Some("A file or folder `b.py` already exists at this location")
        );
        cx.simulate_keystrokes("backspace");
        cx.simulate_input("renamed");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            *events.borrow(),
            [FileTreeEvent::Move(vec![(
                root.join("a.py"),
                root.join("renamed.py")
            )])]
        );
        fs::remove_dir_all(&root).ok();
    }

    #[gpui::test]
    fn ctrl_and_shift_clicks_select_several_rows_for_delete(cx: &mut TestAppContext) {
        let root = project_dir("multi");
        let (tree, cx, events) = open_tree(&root, cx);
        tree.update(cx, |tree, cx| {
            tree.click_row(1, Modifiers::none(), 1, cx);
            tree.click_row(2, Modifiers::secondary_key(), 1, cx);
        });
        assert_eq!(selection(&tree, cx), ["a.py", "b.py"]);
        // Shift extends from the anchor (b.py) up to src.
        tree.update(cx, |tree, cx| tree.click_row(0, Modifiers::shift(), 1, cx));
        assert_eq!(selection(&tree, cx), ["src", "a.py", "b.py"]);
        cx.simulate_keystrokes("shift-down");
        assert_eq!(selection(&tree, cx), ["a.py", "b.py"]);
        events.borrow_mut().clear();
        cx.simulate_keystrokes("delete");
        assert_eq!(
            *events.borrow(),
            [FileTreeEvent::Delete(vec![
                root.join("a.py"),
                root.join("b.py")
            ])]
        );
        fs::remove_dir_all(&root).ok();
    }

    #[gpui::test]
    fn copy_paste_and_duplicate_make_copy_names_cut_paste_moves(cx: &mut TestAppContext) {
        let root = project_dir("clipboard");
        let (tree, cx, events) = open_tree(&root, cx);
        cx.simulate_keystrokes("down down");
        cx.simulate_keystrokes("ctrl-c ctrl-v");
        cx.run_until_parked();
        assert!(root.join("a copy.py").is_file());
        assert_eq!(selection(&tree, cx), ["a copy.py"]);
        cx.simulate_keystrokes("ctrl-d");
        cx.run_until_parked();
        assert!(root.join("a copy copy.py").is_file());

        // Cut b.py, paste into src (selected folder): a move request.
        tree.update(cx, |tree, cx| {
            let b = tree.row_index(&root.join("b.py")).unwrap();
            tree.click_row(b, Modifiers::none(), 1, cx);
        });
        events.borrow_mut().clear();
        cx.simulate_keystrokes("ctrl-x home ctrl-v");
        cx.run_until_parked();
        assert_eq!(
            *events.borrow(),
            [FileTreeEvent::Move(vec![(
                root.join("b.py"),
                root.join("src").join("b.py")
            )])]
        );
        fs::remove_dir_all(&root).ok();
    }

    #[gpui::test]
    fn drops_move_into_folders_and_never_into_themselves(cx: &mut TestAppContext) {
        let root = project_dir("drop");
        for name in ["c.py", "d.py", "e.py", "f.py"] {
            fs::write(root.join(name), "").unwrap();
        }
        let (tree, cx, events) = open_tree(&root, cx);
        let src = root.join("src");
        tree.update_in(cx, |tree, window, cx| {
            // A folder onto itself, or a file onto its own folder: nothing.
            tree.drop_paths(vec![src.clone()], src.clone(), window, cx);
            tree.drop_paths(vec![src.join("main.py")], src.clone(), window, cx);
            tree.drop_paths(vec![root.join("a.py")], src.clone(), window, cx);
        });
        assert_eq!(
            *events.borrow(),
            [FileTreeEvent::Move(vec![(
                root.join("a.py"),
                src.join("a.py")
            )])]
        );
        // Six files ask first; Cancel moves nothing.
        events.borrow_mut().clear();
        let many: Vec<PathBuf> = ["a.py", "b.py", "c.py", "d.py", "e.py", "f.py"]
            .iter()
            .map(|name| root.join(name))
            .collect();
        tree.update_in(cx, |tree, window, cx| {
            tree.drop_paths(many.clone(), src.clone(), window, cx)
        });
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(events.borrow().is_empty());
        tree.update_in(cx, |tree, window, cx| {
            tree.drop_paths(many.clone(), src.clone(), window, cx)
        });
        cx.simulate_prompt_answer("Move");
        cx.run_until_parked();
        assert!(
            matches!(events.borrow().as_slice(), [FileTreeEvent::Move(moves)] if moves.len() == 6)
        );
        fs::remove_dir_all(&root).ok();
    }

    #[gpui::test]
    fn moved_entries_stay_expanded_and_selected(cx: &mut TestAppContext) {
        let root = project_dir("moved");
        fs::create_dir_all(root.join("src/inner")).unwrap();
        let (tree, cx, _) = open_tree(&root, cx);
        cx.simulate_keystrokes("down right");
        assert_eq!(
            names(&tree, cx),
            ["src", "  inner", "  main.py", "a.py", "b.py"]
        );
        fs::rename(root.join("src"), root.join("lib")).unwrap();
        tree.update(cx, |tree, cx| {
            tree.moved(&[(root.join("src"), root.join("lib"))], cx)
        });
        assert_eq!(
            names(&tree, cx),
            ["lib", "  inner", "  main.py", "a.py", "b.py"]
        );
        assert_eq!(selection(&tree, cx), ["lib"]);
        fs::remove_dir_all(&root).ok();
    }

    #[gpui::test]
    fn alt_insert_offers_the_new_items_by_keyboard(cx: &mut TestAppContext) {
        let root = project_dir("new-menu");
        let (tree, cx, _) = open_tree(&root, cx);
        // Nothing selected: New Folder (second item) goes into the root.
        cx.simulate_keystrokes("alt-insert down enter");
        cx.run_until_parked();
        assert_eq!(names(&tree, cx), ["<new>", "src", "a.py", "b.py"]);
        cx.simulate_input("docs");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(root.join("docs").is_dir());
        assert_eq!(names(&tree, cx), ["docs", "src", "a.py", "b.py"]);
        assert_eq!(selection(&tree, cx), ["docs"]);
        fs::remove_dir_all(&root).ok();
    }

    fn clipboard(cx: &mut VisualTestContext) -> Option<String> {
        cx.read_from_clipboard().and_then(|item| item.text())
    }

    #[gpui::test]
    fn workspace_shows_a_row_per_folder_and_reveals_into_the_second(cx: &mut TestAppContext) {
        let api = project_dir("ws-api");
        let web = project_dir("ws-web");
        let (tree, cx, events) = open_project_tree(workspace(&[(&api, "api"), (&web, "web")]), cx);
        // The first folder starts open; same-named folders stay apart.
        assert_eq!(
            names(&tree, cx),
            ["api", "  src", "  a.py", "  b.py", "web"]
        );
        assert_eq!(
            tree.read_with(cx, |tree, _| tree.header.clone()),
            "api, web"
        );

        let main = web.join("src").join("main.py");
        tree.update(cx, |tree, cx| {
            tree.reveal(&main);
            tree.rebuild_rows();
            tree.select(&main, cx);
        });
        assert_eq!(
            names(&tree, cx),
            [
                "api",
                "  src",
                "  a.py",
                "  b.py",
                "web",
                "  src",
                "    main.py",
                "  a.py",
                "  b.py"
            ]
        );

        // Relative paths are relative to the file's own folder.
        tree.update(cx, |tree, cx| tree.copy_paths(true, cx));
        assert_eq!(
            clipboard(cx),
            Some(Path::new("src").join("main.py").display().to_string())
        );

        // Left climbs to the folder row, which is never renamed or deleted.
        cx.simulate_keystrokes("left left left");
        assert_eq!(
            tree.read_with(cx, |tree, _| tree.selection.clone()),
            std::slice::from_ref(&web)
        );
        assert_eq!(
            names(&tree, cx),
            [
                "api", "  src", "  a.py", "  b.py", "web", "  src", "  a.py", "  b.py"
            ]
        );
        cx.simulate_keystrokes("f2 delete ctrl-x ctrl-d");
        cx.run_until_parked();
        assert!(tree.read_with(cx, |tree, _| tree.editing.is_none()
            && tree.clipboard.is_none()));
        assert!(events.borrow().is_empty());
        tree.update(cx, |tree, cx| tree.copy_paths(true, cx));
        assert_eq!(clipboard(cx).as_deref(), Some("."));
        fs::remove_dir_all(&api).ok();
        fs::remove_dir_all(&web).ok();
    }

    #[gpui::test]
    fn new_entries_in_the_second_folder_land_there(cx: &mut TestAppContext) {
        let api = project_dir("ws-new-api");
        let web = project_dir("ws-new-web");
        let (tree, cx, events) = open_project_tree(workspace(&[(&api, "api"), (&web, "web")]), cx);
        // A file selected in the second folder: New File goes next to it.
        tree.update(cx, |tree, cx| {
            tree.click_row(4, Modifiers::none(), 1, cx);
            let a = tree.row_index(&web.join("a.py")).unwrap();
            tree.click_row(a, Modifiers::none(), 1, cx);
        });
        events.borrow_mut().clear();
        tree.update_in(cx, |tree, window, cx| tree.new_file(&NewFile, window, cx));
        cx.simulate_input("new.py");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(web.join("new.py").is_file());
        assert!(!api.join("new.py").exists());
        assert_eq!(
            *events.borrow(),
            [FileTreeEvent::Created(web.join("new.py"))]
        );

        // The folder row's menu: New Folder goes into that folder.
        tree.update_in(cx, |tree, window, cx| {
            tree.open_menu(Some(web.clone()), point(px(0.), px(0.)), window, cx);
            tree.new_folder(&NewFolder, window, cx);
        });
        assert_eq!(names(&tree, cx)[4..6], ["web", "  <new>"]);
        cx.simulate_input("docs");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(web.join("docs").is_dir());
        assert!(!api.join("docs").exists());
        fs::remove_dir_all(&api).ok();
        fs::remove_dir_all(&web).ok();
    }

    #[gpui::test]
    fn folder_rows_offer_workspace_items_and_take_drops_from_other_folders(
        cx: &mut TestAppContext,
    ) {
        let api = project_dir("ws-menu-api");
        let web = project_dir("ws-menu-web");
        fs::write(api.join("only.py"), "").unwrap();
        let (tree, cx, events) = open_project_tree(workspace(&[(&api, "api"), (&web, "web")]), cx);
        let items = |tree: &Entity<FileTree>, cx: &mut VisualTestContext| {
            tree.read_with(cx, |tree, _| {
                tree.menu_items()
                    .into_iter()
                    .flatten()
                    .map(|(label, _, enabled)| (label, enabled))
                    .collect::<Vec<_>>()
            })
        };
        tree.update_in(cx, |tree, window, cx| {
            tree.open_menu(Some(web.clone()), point(px(0.), px(0.)), window, cx)
        });
        let folder_items = items(&tree, cx);
        for item in [
            ("Cut", false),
            ("Copy", true),
            ("Rename…", false),
            ("Delete…", false),
            ("Add Folder to Workspace…", true),
            ("Remove Folder from Workspace", true),
        ] {
            assert!(folder_items.contains(&item), "{item:?} in {folder_items:?}");
        }
        tree.update_in(cx, |tree, window, cx| {
            tree.remove_folder_from_workspace(&RemoveFolderFromWorkspace, window, cx)
        });
        assert_eq!(*events.borrow(), [FileTreeEvent::RemoveFolder(1)]);

        // A file's menu has no workspace items.
        tree.update_in(cx, |tree, window, cx| {
            tree.open_menu(Some(api.join("a.py")), point(px(0.), px(0.)), window, cx)
        });
        assert!(
            !items(&tree, cx)
                .iter()
                .any(|(label, _)| label.contains("Workspace"))
        );

        // Dropped on another folder's row: moved there.
        events.borrow_mut().clear();
        tree.update_in(cx, |tree, window, cx| {
            tree.menu = None;
            tree.drop_paths(vec![api.join("only.py")], web.clone(), window, cx)
        });
        assert_eq!(
            *events.borrow(),
            [FileTreeEvent::Move(vec![(
                api.join("only.py"),
                web.join("only.py")
            )])]
        );
        fs::remove_dir_all(&api).ok();
        fs::remove_dir_all(&web).ok();
    }
}
