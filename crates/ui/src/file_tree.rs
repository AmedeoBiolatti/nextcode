//! Project file tree: lazily read, gitignore-aware, virtualized, with the
//! file operations of PyCharm's Project view and VS Code's Explorer:
//! Ctrl/Shift+click and keyboard selection, a context menu (rows and empty
//! space), New File (with an optional type: `Tab` opens the type list) /
//! Folder / Python File / Python Package (Alt+Insert) and
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
    App, ClipboardItem, Context, Corner, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    FontWeight, KeyBinding, Modifiers, MouseButton, MouseDownEvent, Pixels, Point, PromptLevel,
    ScrollHandle, ScrollStrategy, SharedString, Subscription, UniformListScrollHandle, Window,
    actions, anchored, deferred, div, point, prelude::*, px, uniform_list,
};

use crate::{
    commands,
    editor::{self, Editor, EditorEvent},
    file_icons,
    file_ops::{self, NewKind},
    file_types::{self, FileType},
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
        CancelName,
        ShowTypes,
        TypeUp,
        TypeDown,
        ChooseType,
        CloseTypes
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
        // New File's type list: Tab (or Alt+Down, a combo box's key) opens
        // it; typing filters; Shift+Tab or Escape go back to the name.
        KeyBinding::new("tab", ShowTypes, Some("FileTreeInput > Editor")),
        KeyBinding::new("alt-down", ShowTypes, Some("FileTreeInput > Editor")),
        KeyBinding::new("up", TypeUp, Some("FileTreeTypes > Editor")),
        KeyBinding::new("down", TypeDown, Some("FileTreeTypes > Editor")),
        KeyBinding::new("enter", ChooseType, Some("FileTreeTypes > Editor")),
        KeyBinding::new("tab", ChooseType, Some("FileTreeTypes > Editor")),
        KeyBinding::new("shift-tab", CloseTypes, Some("FileTreeTypes > Editor")),
        KeyBinding::new("escape", CloseTypes, Some("FileTreeTypes")),
    ]);
}

/// New Python File…'s preselected type.
pub fn python_type() -> Option<&'static FileType> {
    file_types::by_id("python").ok().flatten()
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
    /// New File's type list, while open.
    types: Option<TypeList>,
    _subscriptions: Vec<Subscription>,
}

impl Editing {
    /// New File's chosen type (`Some(None)`: Auto); `None` for other
    /// editors.
    fn file_type(&self) -> Option<Option<&'static FileType>> {
        match self.kind {
            EditKind::New {
                kind: NewKind::File(file_type),
                ..
            } => Some(file_type),
            _ => None,
        }
    }
}

/// New File's type list: a filter input over [`file_types::filter`].
struct TypeList {
    filter: Entity<Editor>,
    /// The entry Up/Down moved to, which Enter chooses.
    highlighted: usize,
    scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl TypeList {
    fn entries(&self, cx: &App) -> Vec<Option<&'static FileType>> {
        file_types::filter(&self.filter.read(cx).text())
    }
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

/// Files and folders being dragged from a tree: the row dragged, which takes
/// the selection along when it is selected (resolved by
/// [`FileTree::dragged_paths`] when needed, not per rendered row).
#[derive(Clone)]
pub struct DraggedPaths {
    tree: EntityId,
    path: PathBuf,
    selected: bool,
}

/// The selected entries: a set for the per-row lookups, and the order they
/// were selected in.
#[derive(Default)]
struct Selection {
    order: Vec<PathBuf>,
    set: HashSet<PathBuf>,
}

impl Selection {
    fn contains(&self, path: &Path) -> bool {
        self.set.contains(path)
    }

    /// In selection order.
    #[cfg(test)]
    fn paths(&self) -> &[PathBuf] {
        &self.order
    }

    fn first(&self) -> Option<&PathBuf> {
        self.order.first()
    }

    fn last(&self) -> Option<&PathBuf> {
        self.order.last()
    }

    fn set(&mut self, paths: Vec<PathBuf>) {
        self.set.clear();
        self.order = paths;
        let set = &mut self.set;
        self.order.retain(|path| set.insert(path.clone()));
    }

    fn insert(&mut self, path: PathBuf) {
        if self.set.insert(path.clone()) {
            self.order.push(path);
        }
    }

    /// Whether `path` was selected.
    fn remove(&mut self, path: &Path) -> bool {
        let removed = self.set.remove(path);
        if removed {
            self.order.retain(|selected| selected != path);
        }
        removed
    }

    fn retain(&mut self, keep: impl Fn(&Path) -> bool) {
        self.order.retain(|path| keep(path));
        self.set.retain(|path| keep(path));
    }

    fn clear(&mut self) {
        self.order.clear();
        self.set.clear();
    }
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
    /// Directory listings, read off the UI thread when a folder first shows
    /// (see [`FileTree::load_listings`]). A listing that changed on disk
    /// stays, and is shown, until its re-read arrives.
    children: HashMap<PathBuf, Vec<Entry>>,
    /// Listings to read again, with the generation of the change: a read
    /// started before it leaves the listing stale.
    stale: HashMap<PathBuf, u64>,
    /// Reads in flight, with their generation; an older result is dropped.
    loading: HashMap<PathBuf, u64>,
    /// Counts changes and reads, to order them.
    generation: u64,
    /// Folders the rows need listed, from the last [`FileTree::rebuild_rows`].
    wanted: Vec<PathBuf>,
    rows: Vec<Row>,
    /// Each entry row's index by path (`new_entry` rows left out).
    row_of: HashMap<PathBuf, usize>,
    selection: Selection,
    /// The entry keys move from (the last one clicked or reached).
    lead: Option<PathBuf>,
    /// Where Shift+click / Shift+arrow ranges start.
    anchor: Option<PathBuf>,
    /// Scroll to the lead once its row shows: it was just created or moved
    /// into a listing still being read.
    scroll_when_listed: bool,
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
            stale: HashMap::new(),
            loading: HashMap::new(),
            generation: 0,
            wanted: Vec::new(),
            rows: Vec::new(),
            row_of: HashMap::new(),
            selection: Selection::default(),
            lead: None,
            anchor: None,
            scroll_when_listed: false,
            focus_handle: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            menu: None,
            editing: None,
            clipboard: None,
            own_changes: Vec::new(),
        };
        tree.rebuild(cx);
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
        self.selection.set(paths);
        self.scroll_when_listed = false;
    }

    /// Re-reads the directories containing created or removed `paths`
    /// (everything when `rescan`), keeping expansion state. Nothing is
    /// read here: the listings arrive off the UI thread, and a folder
    /// that went away closes when its parent's listing comes back without
    /// it.
    pub fn refresh<'a>(
        &mut self,
        paths: impl IntoIterator<Item = &'a Path>,
        rescan: bool,
        cx: &mut Context<Self>,
    ) {
        if rescan {
            self.generation += 1;
            let generation = self.generation;
            self.stale.extend(
                self.children
                    .keys()
                    .chain(self.loading.keys())
                    .map(|dir| (dir.clone(), generation)),
            );
        } else {
            for path in paths {
                if !self.is_own_change(path) {
                    self.invalidate(path);
                }
            }
        }
        self.rebuild(cx);
    }

    /// Marks the listing showing `path` to be read again.
    fn invalidate(&mut self, path: &Path) {
        let parent = path.parent();
        if let Some(parent) = parent {
            self.mark_stale(parent);
        }
        // An `__init__.py` makes its folder a package: re-read the
        // listing showing that folder.
        if path.file_name().is_some_and(|name| name == "__init__.py")
            && let Some(grandparent) = parent.and_then(Path::parent)
        {
            self.mark_stale(grandparent);
        }
    }

    /// `dir`'s listing (one shown, or being read) changed on disk.
    fn mark_stale(&mut self, dir: &Path) {
        if self.children.contains_key(dir) || self.loading.contains_key(dir) {
            self.generation += 1;
            self.stale.insert(dir.to_path_buf(), self.generation);
        }
    }

    /// Rebuilds the rows and starts reading the listings they lack.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        self.rebuild_rows();
        self.load_listings(cx);
        cx.notify();
    }

    /// Reads the listings the rows want (folders shown open without one,
    /// or with one that changed) off the UI thread; [`FileTree::listed`]
    /// takes each result.
    fn load_listings(&mut self, cx: &mut Context<Self>) {
        for dir in std::mem::take(&mut self.wanted) {
            if self.loading.contains_key(&dir) {
                continue;
            }
            self.generation += 1;
            let generation = self.generation;
            self.loading.insert(dir.clone(), generation);
            let project = self.project.clone();
            let read = dir.clone();
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_spawn(async move { read_dir(&project, &read) })
                    .await;
                this.update(cx, |this, cx| this.listed(dir, generation, result, cx))
                    .ok();
            })
            .detach();
        }
    }

    /// A listing arrived. Open folders it no longer has (gone, or ignored
    /// now) close; an unreadable folder (e.g. the agent disconnected)
    /// keeps what it showed.
    fn listed(
        &mut self,
        dir: PathBuf,
        generation: u64,
        result: io::Result<Vec<Entry>>,
        cx: &mut Context<Self>,
    ) {
        if self.loading.get(&dir) != Some(&generation) {
            return;
        }
        self.loading.remove(&dir);
        if self
            .stale
            .get(&dir)
            .is_some_and(|&since| since < generation)
        {
            self.stale.remove(&dir);
        }
        match result {
            Ok(entries) => {
                let present: HashSet<&Path> = entries
                    .iter()
                    .filter(|entry| entry.is_dir)
                    .map(|entry| entry.path.as_path())
                    .collect();
                let vanished: Vec<PathBuf> = self
                    .expanded
                    .iter()
                    .filter(|open| {
                        open.parent() == Some(dir.as_path()) && !present.contains(open.as_path())
                    })
                    .cloned()
                    .collect();
                if !vanished.is_empty() {
                    let under = |path: &Path| vanished.iter().any(|gone| path.starts_with(gone));
                    self.expanded.retain(|open| !under(open));
                    self.children.retain(|listed, _| !under(listed));
                    self.stale.retain(|listed, _| !under(listed));
                    self.loading.retain(|listed, _| !under(listed));
                }
                self.children.insert(dir, entries);
            }
            Err(_) => {
                self.children.entry(dir).or_default();
            }
        }
        self.rebuild(cx);
        if self.scroll_when_listed
            && let Some(index) = self.lead_index()
        {
            self.scroll.scroll_to_item(index, ScrollStrategy::Top);
            self.scroll_when_listed = false;
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
        // Listings under a moved entry went with it; reads of them are moot.
        let moved = |dir: &Path| moves.iter().any(|(from, _)| dir.starts_with(from));
        self.children.retain(|dir, _| !moved(dir));
        self.stale.retain(|dir, _| !moved(dir));
        self.loading.retain(|dir, _| !moved(dir));
        for (from, to) in moves {
            self.invalidate(from);
            self.invalidate(to);
            self.reveal(to);
        }
        let selection = moves.iter().map(|(_, to)| to.clone()).collect();
        self.set_selection(selection);
        self.rebuild(cx);
        self.scroll_to_lead_when_shown();
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
        self.stale.retain(|dir, _| !gone(dir));
        self.loading.retain(|dir, _| !gone(dir));
        for path in paths {
            self.invalidate(path);
        }
        self.rebuild(cx);
    }

    /// Shows new entries (created or pasted here) and selects them.
    fn created(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        self.expect_changes(paths.iter().cloned());
        for path in paths {
            // Every new folder on the way (`a/b/c.py`) needs its listing
            // read: the listed folder nearest to the entry changed.
            let root = self.project.root_for(path).path.clone();
            let mut current = path.as_path();
            while let Some(parent) = current.parent() {
                self.mark_stale(parent);
                if parent == root || !parent.starts_with(&root) {
                    break;
                }
                current = parent;
            }
            self.invalidate(path);
            self.reveal(path);
        }
        self.set_selection(paths.to_vec());
        self.rebuild(cx);
        self.scroll_to_lead_when_shown();
    }

    /// Rebuilds the rows from the listings at hand (a folder shown open
    /// without one is noted in `wanted` for [`FileTree::load_listings`]).
    fn rebuild_rows(&mut self) {
        self.rows.clear();
        self.wanted.clear();
        let mut listing = Listing {
            children: &self.children,
            stale: &self.stale,
            expanded: &self.expanded,
            rows: &mut self.rows,
            wanted: &mut self.wanted,
        };
        if self.root_rows {
            for root in self.project.roots() {
                listing.rows.push(Row {
                    path: root.path.clone(),
                    name: root.name.clone().into(),
                    is_dir: true,
                    package: false,
                    depth: 0,
                    new_entry: false,
                    root: true,
                });
                if listing.expanded.contains(&root.path) {
                    listing.push_rows(&root.path, 1);
                }
            }
        } else {
            listing.push_rows(self.project.root(), 0);
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
                    // The icon follows the typed name (see `render_row`).
                    name: SharedString::default(),
                    is_dir: kind.is_dir(),
                    package: *kind == NewKind::PythonPackage,
                    depth,
                    new_entry: true,
                    root: false,
                },
            );
        }
        self.row_of = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| !row.new_entry)
            .map(|(index, row)| (row.path.clone(), index))
            .collect();
        // An entry being renamed that went away (deleted outside) ends
        // the editor.
        if let Some(Editing {
            kind: EditKind::Rename { path, .. },
            ..
        }) = &self.editing
            && !self.row_of.contains_key(path)
        {
            self.editing = None;
        }
    }

    fn row_index(&self, path: &Path) -> Option<usize> {
        self.row_of.get(path).copied()
    }

    fn lead_index(&self) -> Option<usize> {
        self.lead.as_deref().and_then(|lead| self.row_index(lead))
    }

    fn scroll_to_lead(&self) {
        if let Some(index) = self.lead_index() {
            self.scroll.scroll_to_item(index, ScrollStrategy::Top);
        }
    }

    /// Scrolls to the lead now, or once the listing holding it arrives.
    fn scroll_to_lead_when_shown(&mut self) {
        match self.lead_index() {
            Some(index) => self.scroll.scroll_to_item(index, ScrollStrategy::Top),
            None => self.scroll_when_listed = self.lead.is_some(),
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

    fn toggle_dir(&mut self, path: &Path, cx: &mut Context<Self>) {
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_path_buf());
        }
        self.rebuild(cx);
    }

    fn activate(&mut self, index: usize, focus_editor: bool, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(index).filter(|row| !row.new_entry) else {
            return;
        };
        let path = row.path.clone();
        if row.is_dir {
            self.toggle_dir(&path, cx);
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
            if !self.selection.remove(&path) {
                self.selection.insert(path.clone());
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
        self.selection.set(
            self.rows[start..=end]
                .iter()
                .filter(|row| !row.new_entry)
                .map(|row| row.path.clone())
                .collect(),
        );
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
            self.toggle_dir(&path, cx);
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
            self.toggle_dir(&path, cx);
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
        self.selection.set(
            self.rows
                .iter()
                .filter(|row| !row.new_entry)
                .map(|row| row.path.clone())
                .collect(),
        );
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
        self.start_new_near(self.target(), kind, window, cx);
    }

    /// New… in the folder of `near` (a folder, or a file's folder; the
    /// root without one).
    fn start_new_near(
        &mut self,
        near: Option<PathBuf>,
        kind: NewKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dir = self.folder_of(near.as_deref());
        self.menu = None;
        self.reveal(&self.project.join(&dir, "x"));
        self.start_editing(EditKind::New { dir, kind }, "", window, cx);
        self.load_listings(cx);
    }

    /// New File… (of `file_type`, when given) from outside the tree: in the
    /// folder of `near` (the active editor's file) when it is in the
    /// project, else of the tree's selection.
    pub fn start_new_file(
        &mut self,
        near: Option<PathBuf>,
        file_type: Option<&'static FileType>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let near = near
            .filter(|path| self.project.root_index_of(path).is_some())
            .or_else(|| self.target());
        self.start_new_near(near, NewKind::File(file_type), window, cx);
    }

    fn new_file(&mut self, _: &NewFile, window: &mut Window, cx: &mut Context<Self>) {
        self.start_new(NewKind::File(None), window, cx);
    }

    fn new_folder(&mut self, _: &NewFolder, window: &mut Window, cx: &mut Context<Self>) {
        self.start_new(NewKind::Folder, window, cx);
    }

    /// New Python File…: New File with Python chosen.
    fn new_python_file(&mut self, _: &NewPythonFile, window: &mut Window, cx: &mut Context<Self>) {
        self.start_new(NewKind::File(python_type()), window, cx);
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
        let blurred = cx.on_blur(&focus, window, Self::editor_blurred);
        self.editing = Some(Editing {
            kind,
            input,
            error: None,
            types: None,
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

    /// Focus left the name input or the type filter. Moving between the two
    /// keeps editing (back in the name closes the list); anywhere else
    /// keeps a valid name (VS Code) and drops an invalid one.
    fn editor_blurred(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = &mut self.editing else {
            return;
        };
        if editing.input.focus_handle(cx).is_focused(window) {
            if editing.types.take().is_some() {
                cx.notify();
            }
            return;
        }
        if editing
            .types
            .as_ref()
            .is_some_and(|types| types.filter.focus_handle(cx).is_focused(window))
        {
            return;
        }
        self.finish_editing(window, cx);
        if self.editing.is_some() {
            self.editing = None;
            self.rebuild(cx);
        }
    }

    /// The problem with the typed name, if any: invalid for the platform,
    /// or taken in the folder (as listed; the operation checks again).
    fn input_error(&self, cx: &App) -> Option<String> {
        let editing = self.editing.as_ref()?;
        let text = editing.input.read(cx).text();
        let windows = file_ops::windows_names(&self.project);
        let (dir, kind) = match &editing.kind {
            EditKind::New { dir, kind } => (dir, *kind),
            EditKind::Rename { path, .. } => {
                let name = text.trim();
                if let Err(err) = file_ops::check_name(name, windows) {
                    return Some(err);
                }
                let own = path
                    .file_name()
                    .map(|own| own.to_string_lossy().into_owned());
                if own
                    .as_deref()
                    .is_some_and(|own| file_ops::same_name(own, name, windows))
                {
                    return None;
                }
                let dir = path.parent().unwrap_or(self.project.root());
                let taken = self.children.get(dir).is_some_and(|entries| {
                    entries
                        .iter()
                        .any(|entry| file_ops::same_name(&entry.name, name, windows))
                });
                return taken
                    .then(|| format!("A file or folder `{name}` already exists at this location"));
            }
        };
        let (base, relative) = match file_ops::new_entry_input(&self.project, dir, &text, kind) {
            Ok(found) => found,
            Err(err) => return Some(err),
        };
        if let NewKind::File(file_type) = kind
            && file_types::picks_free_name(file_type, &text)
        {
            return None;
        }
        let parts = match file_ops::new_entry_parts(&relative, windows) {
            Ok(parts) => parts,
            Err(err) => return Some(err),
        };
        // Down the listed folders: a typed path into existing folders
        // (`src/new.py`) is fine, a taken name or a file on the way is not.
        let mut folder = base;
        for (index, part) in parts.iter().enumerate() {
            let entry = self
                .children
                .get(&folder)?
                .iter()
                .find(|entry| file_ops::same_name(&entry.name, part, windows))?;
            if index + 1 == parts.len() || !entry.is_dir {
                return Some(format!(
                    "A file or folder `{part}` already exists at this location"
                ));
            }
            folder = entry.path.clone();
        }
        None
    }

    fn check_input(&mut self, cx: &mut Context<Self>) {
        let error = self.input_error(cx).map(SharedString::from);
        if let Some(editing) = &mut self.editing {
            editing.error = error;
        }
        cx.notify();
    }

    /// Enter: creates or renames when the name is fine (else shows why);
    /// an unchanged name, or no name for a new entry that needs one, just
    /// closes the editor.
    fn finish_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = &self.editing else {
            return;
        };
        let text = editing.input.read(cx).text();
        let text = text.trim().to_owned();
        if text.is_empty()
            && matches!(editing.kind, EditKind::New { .. })
            && !editing.file_type().is_some_and(file_types::name_optional)
        {
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
        let refocus = editing.input.focus_handle(cx).is_focused(window)
            || editing
                .types
                .as_ref()
                .is_some_and(|types| types.filter.focus_handle(cx).is_focused(window));
        self.rebuild_rows();
        if refocus {
            window.focus(&self.focus_handle);
        }
        cx.notify();
        match editing.kind {
            EditKind::New { dir, kind } => {
                let what = match (text.as_str(), kind) {
                    ("", NewKind::File(Some(file_type))) => {
                        format!("Could not create a {}", file_type.label)
                    }
                    _ => format!("Could not create `{text}`"),
                };
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
        self.rebuild(cx);
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

    /// Tab / Alt+Down in New File's name (or a click on the selector):
    /// opens the type list with the chosen type highlighted and focuses
    /// its filter.
    fn show_types(&mut self, _: &ShowTypes, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = &mut self.editing else {
            return;
        };
        let Some(file_type) = editing.file_type() else {
            return;
        };
        if let Some(types) = &editing.types {
            window.focus(&types.filter.focus_handle(cx));
            return;
        }
        let filter = cx.new(Editor::single_line);
        let edited = cx.subscribe(&filter, |this: &mut Self, _, event, cx| {
            if let EditorEvent::Edited = event {
                this.filter_types(cx);
            }
        });
        let focus = filter.focus_handle(cx);
        let blurred = cx.on_blur(&focus, window, Self::editor_blurred);
        let highlighted = file_types::filter("")
            .iter()
            .position(|entry| *entry == file_type)
            .unwrap_or(0);
        let scroll = ScrollHandle::new();
        scroll.scroll_to_item(highlighted);
        editing.types = Some(TypeList {
            filter,
            highlighted,
            scroll,
            _subscriptions: vec![edited, blurred],
        });
        window.focus(&focus);
        cx.notify();
    }

    /// Typing in the filter: the first matching type is highlighted.
    fn filter_types(&mut self, cx: &mut Context<Self>) {
        let Some(types) = self
            .editing
            .as_mut()
            .and_then(|editing| editing.types.as_mut())
        else {
            return;
        };
        let query = types.filter.read(cx).text();
        let entries = file_types::filter(&query);
        // Auto matches most single letters; a typed query means a type.
        types.highlighted =
            usize::from(!query.trim().is_empty() && entries.len() > 1 && entries[0].is_none());
        types.scroll.scroll_to_item(types.highlighted);
        cx.notify();
    }

    fn move_type_highlight(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(types) = self
            .editing
            .as_mut()
            .and_then(|editing| editing.types.as_mut())
        else {
            return;
        };
        let count = types.entries(cx).len() as isize;
        if count == 0 {
            return;
        }
        types.highlighted = (types.highlighted as isize + delta).rem_euclid(count) as usize;
        types.scroll.scroll_to_item(types.highlighted);
        cx.notify();
    }

    fn type_up(&mut self, _: &TypeUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_type_highlight(-1, cx);
    }

    fn type_down(&mut self, _: &TypeDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_type_highlight(1, cx);
    }

    /// Enter / Tab in the type list: the highlighted type.
    fn choose_type(&mut self, _: &ChooseType, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self
            .editing
            .as_ref()
            .and_then(|editing| editing.types.as_ref())
            .map(|types| types.highlighted)
        else {
            return;
        };
        self.choose_type_at(index, window, cx);
    }

    /// Chooses the list's `index`th entry and goes back to the name.
    fn choose_type_at(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = &mut self.editing else {
            return;
        };
        let Some(entry) = editing
            .types
            .as_ref()
            .and_then(|types| types.entries(cx).get(index).copied())
        else {
            return;
        };
        if let EditKind::New { kind, .. } = &mut editing.kind {
            *kind = NewKind::File(entry);
        }
        self.close_types(window, cx);
        self.check_input(cx);
    }

    /// Escape / Shift+Tab in the type list: back to the name, unchanged.
    fn close_types_action(&mut self, _: &CloseTypes, window: &mut Window, cx: &mut Context<Self>) {
        self.close_types(window, cx);
    }

    fn close_types(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = &mut self.editing else {
            return;
        };
        window.focus(&editing.input.focus_handle(cx));
        editing.types = None;
        cx.notify();
    }

    /// A click on the type selector opens or closes the list.
    fn toggle_types(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .editing
            .as_ref()
            .is_some_and(|editing| editing.types.is_some())
        {
            self.close_types(window, cx);
        } else {
            self.show_types(&ShowTypes, window, cx);
        }
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
                "Reveal works for local projects only (this one lives in WSL, a container or another machine)"
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

/// Lists one directory (off the UI thread; for remote projects one agent
/// round trip, the package check included). Folders that could be Python
/// packages are those the listing found an `__init__.py` in.
fn read_dir(project: &Project, dir: &Path) -> io::Result<Vec<Entry>> {
    Ok(project
        .read_dir(dir)?
        .into_iter()
        .map(|entry| Entry {
            path: project.join(dir, &entry.name),
            package: entry.is_dir && entry.package && file_icons::may_be_package(&entry.name),
            name: entry.name.into(),
            is_dir: entry.is_dir,
        })
        .collect())
}

/// The rows being built from the listings at hand.
struct Listing<'a> {
    children: &'a HashMap<PathBuf, Vec<Entry>>,
    stale: &'a HashMap<PathBuf, u64>,
    expanded: &'a HashSet<PathBuf>,
    rows: &'a mut Vec<Row>,
    /// Open folders without a listing, or with one that changed.
    wanted: &'a mut Vec<PathBuf>,
}

impl Listing<'_> {
    /// The rows of open folder `dir`: its listing, if read (the one at
    /// hand while a re-read runs), with its open subfolders' rows after
    /// each.
    fn push_rows(&mut self, dir: &Path, depth: usize) {
        if !self.children.contains_key(dir) || self.stale.contains_key(dir) {
            self.wanted.push(dir.to_path_buf());
        }
        let Some(entries) = self.children.get(dir) else {
            return;
        };
        for entry in entries {
            self.rows.push(Row {
                path: entry.path.clone(),
                name: entry.name.clone(),
                is_dir: entry.is_dir,
                package: entry.package,
                depth,
                new_entry: false,
                root: false,
            });
            if entry.is_dir && self.expanded.contains(&entry.path) {
                self.push_rows(&entry.path, depth + 1);
            }
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
            .on_action(cx.listener(Self::show_types))
            .on_action(cx.listener(Self::type_up))
            .on_action(cx.listener(Self::type_down))
            .on_action(cx.listener(Self::choose_type))
            .on_action(cx.listener(Self::close_types_action))
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
                            let paths = this.dragged_paths(dragged);
                            this.drop_paths(paths, header_drop.clone(), window, cx)
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
                            let paths = this.dragged_paths(dragged);
                            this.drop_paths(paths, root_drop.clone(), window, cx)
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
                file_icons::file(&self.new_file_icon_name(cx))
            };
            let editing = self.editing.as_ref();
            return div()
                .id(("file-tree-new", index))
                .relative()
                .w_full()
                .flex()
                .items_center()
                .gap(px(5.))
                .h(px(ROW_HEIGHT))
                .pl(indent)
                .pr_2()
                .child(div().flex_none().w(px(10.)))
                .child(icon)
                .children(editing.map(|editing| self.render_input(editing)))
                .children(editing.and_then(|editing| self.render_type_selector(editing, cx)))
                .children(editing.and_then(|editing| self.render_type_list(editing, cx)))
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
        // What the drag takes along is resolved when it starts (or is
        // dropped), not for every row rendered.
        let dragged = DraggedPaths {
            tree,
            path: row.path.clone(),
            selected,
        };
        let drag_source = cx.weak_entity();
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
                        element.on_drag(dragged, move |dragged: &DraggedPaths, _, _, cx| {
                            let paths = drag_source
                                .upgrade()
                                .map(|tree| tree.read(cx).dragged_paths(dragged))
                                .unwrap_or_default();
                            let label = match paths.as_slice() {
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
                    let paths = this.dragged_paths(dragged);
                    this.drop_paths(paths, drop_dir.clone(), window, cx)
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

    /// The file name New File's icon goes by: the typed name as the chosen
    /// type completes it (the type's own icon while nothing is typed).
    fn new_file_icon_name(&self, cx: &App) -> String {
        let Some(editing) = &self.editing else {
            return String::new();
        };
        let text = editing.input.read(cx).text();
        let windows = file_ops::windows_names(&self.project);
        match editing.file_type() {
            Some(Some(file_type)) if text.trim().is_empty() => file_types::icon_name(file_type),
            Some(Some(file_type)) => file_types::new_path(Some(file_type), &text, windows)
                .map(|(_, path)| path.rsplit('/').next().unwrap_or_default().to_owned())
                .unwrap_or_else(|_| file_types::icon_name(file_type)),
            _ => text,
        }
    }

    /// New File's type selector beside the name: the chosen type, else
    /// Auto with the type the typed name implies, faint.
    fn render_type_selector(
        &self,
        editing: &Editing,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let file_type = editing.file_type()?;
        let open = editing.types.is_some();
        let detected = match file_type {
            Some(_) => None,
            None => file_types::detect(
                &editing.input.read(cx).text(),
                file_ops::windows_names(&self.project),
            ),
        };
        let label: SharedString = match (file_type, detected) {
            (Some(file_type), _) => file_type.label.into(),
            (None, Some(detected)) => format!("Auto · {}", detected.label).into(),
            (None, None) => "Auto".into(),
        };
        let icon = file_type.or(detected).map(|shown| {
            div()
                .flex_none()
                .when(file_type.is_none(), |icon| icon.opacity(0.6))
                .child(file_icons::file(&file_types::icon_name(shown)))
        });
        Some(
            div()
                .id("file-tree-new-type")
                .flex_none()
                .max_w(px(120.))
                .h(px(20.))
                .px_1()
                .flex()
                .items_center()
                .gap_1()
                .text_xs()
                .border_1()
                .rounded_sm()
                .border_color(if open {
                    theme::accent()
                } else {
                    theme::border()
                })
                .bg(theme::background())
                .cursor_pointer()
                .when(file_type.is_none(), |selector| {
                    selector.text_color(theme::muted())
                })
                .children(icon)
                .child(div().min_w_0().truncate().child(label.clone()))
                .child(div().flex_none().text_color(theme::muted()).child("▾"))
                // The name keeps focus until the list opens.
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .on_click(cx.listener(|this, _, window, cx| this.toggle_types(window, cx)))
                .pin("type")
                .label(format!("Type: {label}"))
                .interactive(true)
                .into_any_element(),
        )
    }

    /// New File's open type list under the selector: a filter input, then
    /// the matching types with their icons and extensions.
    fn render_type_list(
        &self,
        editing: &Editing,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let types = editing.types.as_ref()?;
        let chosen = editing.file_type()?;
        let entries = types.entries(cx);
        let empty = entries.is_empty();
        let items =
            entries.into_iter().enumerate().map(|(at, entry)| {
                let label = file_types::label(entry);
                div()
                    .id(("file-tree-type", at))
                    .h(px(22.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(at == types.highlighted, |item| item.bg(theme::active_row()))
                    .hover(|style| style.bg(theme::hover_row()))
                    .child(div().flex_none().w(px(file_icons::SIZE)).children(
                        entry.map(|entry| file_icons::file(&file_types::icon_name(entry))),
                    ))
                    // The label stays whole; the hint (where the file goes)
                    // gives way.
                    .child(div().flex_none().child(label))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_right()
                            .text_xs()
                            .text_color(theme::muted())
                            .children(entry.and_then(file_types::hint)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(12.))
                            .text_color(theme::accent())
                            .child(if entry == chosen { "✓" } else { "" }),
                    )
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.choose_type_at(at, window, cx)),
                    )
                    .pin(format!("item:{}", file_types::id(entry)))
                    .label(label)
                    .interactive(true)
            });
        Some(
            div()
                .absolute()
                .top(px(ROW_HEIGHT - 1.))
                .right(px(8.))
                .child(
                    deferred(
                        anchored().anchor(Corner::TopRight).snap_to_window().child(
                            div()
                                .id("file-tree-types")
                                .occlude()
                                .w(px(280.))
                                .p_1()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .text_sm()
                                .text_color(theme::text())
                                .bg(theme::panel())
                                .border_1()
                                .border_color(theme::border())
                                .rounded_md()
                                .shadow_lg()
                                .child(
                                    div()
                                        .key_context("FileTreeTypes")
                                        .h(px(22.))
                                        .px_1()
                                        .border_1()
                                        .border_color(theme::accent())
                                        .bg(theme::background())
                                        .child(types.filter.clone())
                                        .pin("filter")
                                        .label("Filter types"),
                                )
                                .child(
                                    div()
                                        .id("file-tree-type-items")
                                        .max_h(px(300.))
                                        .overflow_y_scroll()
                                        .track_scroll(&types.scroll)
                                        .flex()
                                        .flex_col()
                                        .children(items)
                                        .when(empty, |list| {
                                            list.child(
                                                div()
                                                    .px_2()
                                                    .py_1()
                                                    .text_color(theme::muted())
                                                    .child("No matching type"),
                                            )
                                        }),
                                )
                                .pin("type-list"),
                        ),
                    )
                    .with_priority(1),
                )
                .into_any_element(),
        )
    }

    /// The selection in tree order.
    fn subjects_in_order(&self) -> Vec<PathBuf> {
        self.rows
            .iter()
            .filter(|row| !row.new_entry && self.selection.contains(&row.path))
            .map(|row| row.path.clone())
            .collect()
    }

    /// What a drag moves: the selection (in tree order, without the
    /// project's folders, which stay where they are) when the dragged row
    /// is part of it, else that row alone.
    fn dragged_paths(&self, dragged: &DraggedPaths) -> Vec<PathBuf> {
        if dragged.selected && self.selection.contains(&dragged.path) {
            let mut paths = self.subjects_in_order();
            paths.retain(|path| !self.is_root(path));
            paths
        } else {
            vec![dragged.path.clone()]
        }
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
        // Select a file inside src (listed once opened): New File goes
        // into its folder.
        cx.simulate_keystrokes("down right");
        cx.simulate_keystrokes("down");
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
        // The root's listing is re-read off the UI thread; the moved
        // folder's own listing went with it.
        cx.run_until_parked();
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

    /// New File's chosen type id (`auto` without one).
    fn chosen(tree: &Entity<FileTree>, cx: &mut VisualTestContext) -> Option<&'static str> {
        tree.read_with(cx, |tree, _| {
            tree.editing
                .as_ref()
                .and_then(Editing::file_type)
                .map(file_types::id)
        })
    }

    /// The open type list's entries (ids) and the highlighted one.
    fn type_list(
        tree: &Entity<FileTree>,
        cx: &mut VisualTestContext,
    ) -> Option<(Vec<&'static str>, &'static str)> {
        tree.read_with(cx, |tree, cx| {
            let types = tree.editing.as_ref()?.types.as_ref()?;
            let entries: Vec<&str> = types.entries(cx).into_iter().map(file_types::id).collect();
            let highlighted = entries[types.highlighted];
            Some((entries, highlighted))
        })
    }

    fn name_focused(tree: &Entity<FileTree>, cx: &mut VisualTestContext) -> bool {
        cx.update(|window, cx| {
            tree.read(cx)
                .editing
                .as_ref()
                .is_some_and(|editing| editing.input.focus_handle(cx).is_focused(window))
        })
    }

    #[gpui::test]
    fn new_file_types_are_chosen_by_keyboard_and_written_with_their_template(
        cx: &mut TestAppContext,
    ) {
        let root = project_dir("new-typed");
        let (tree, cx, events) = open_tree(&root, cx);
        tree.update_in(cx, |tree, window, cx| tree.new_file(&NewFile, window, cx));
        cx.run_until_parked();
        assert_eq!(chosen(&tree, cx), Some("auto"));
        cx.simulate_input("analysis");

        // Tab opens the list on the chosen type (Auto); typing filters it
        // and highlights the first type; Up/Down wrap.
        cx.simulate_keystrokes("tab");
        cx.run_until_parked();
        let (entries, highlighted) = type_list(&tree, cx).unwrap();
        assert_eq!(entries.len(), file_types::TYPES.len() + 1);
        assert_eq!(highlighted, "auto");
        assert!(!name_focused(&tree, cx));
        cx.simulate_input("note");
        cx.run_until_parked();
        assert_eq!(
            type_list(&tree, cx),
            Some((vec!["jupyter", "marimo"], "jupyter"))
        );
        cx.simulate_keystrokes("down");
        assert_eq!(type_list(&tree, cx).unwrap().1, "marimo");
        cx.simulate_keystrokes("down");
        assert_eq!(type_list(&tree, cx).unwrap().1, "jupyter");

        // Enter chooses it and goes back to the name; the editing stays.
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(type_list(&tree, cx), None);
        assert_eq!(chosen(&tree, cx), Some("jupyter"));
        assert!(name_focused(&tree, cx));
        assert!(events.borrow().is_empty());

        // Escape closes the list only; Shift+Tab too.
        cx.simulate_keystrokes("alt-down");
        cx.run_until_parked();
        assert!(type_list(&tree, cx).is_some());
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(type_list(&tree, cx), None);
        assert!(name_focused(&tree, cx));
        cx.simulate_keystrokes("tab");
        cx.run_until_parked();
        cx.simulate_keystrokes("shift-tab");
        cx.run_until_parked();
        assert_eq!(type_list(&tree, cx), None);
        assert_eq!(chosen(&tree, cx), Some("jupyter"));
        assert!(name_focused(&tree, cx));

        // Enter creates `analysis.ipynb`: an empty notebook that opens.
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let created = root.join("analysis.ipynb");
        let text = fs::read_to_string(&created).unwrap();
        assert_eq!(notebook::ipynb::parse(&text).unwrap().cells.len(), 1);
        assert_eq!(*events.borrow(), [FileTreeEvent::Created(created)]);
        assert!(tree.read_with(cx, |tree, _| tree.editing.is_none()));

        // A second Escape (in the name) cancels New File.
        tree.update_in(cx, |tree, window, cx| tree.new_file(&NewFile, window, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("tab escape escape");
        cx.run_until_parked();
        assert!(tree.read_with(cx, |tree, _| tree.editing.is_none()));
        fs::remove_dir_all(&root).ok();
    }

    #[gpui::test]
    fn types_with_their_own_names_need_none_typed(cx: &mut TestAppContext) {
        let root = project_dir("new-workflow");
        let (tree, cx, events) = open_tree(&root, cx);
        // From inside src: the workflow still goes to the project root.
        cx.simulate_keystrokes("down right");
        cx.simulate_keystrokes("down");
        tree.update_in(cx, |tree, window, cx| tree.new_file(&NewFile, window, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("tab");
        cx.simulate_input("workflow");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(chosen(&tree, cx), Some("workflow"));
        assert_eq!(error(&tree, cx), None);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let created = root
            .join(".next_code")
            .join("workflows")
            .join("workflow.yaml");
        workflow::file::check(&fs::read_to_string(&created).unwrap(), "workflow").unwrap();
        assert_eq!(*events.borrow(), [FileTreeEvent::Created(created)]);
        fs::remove_dir_all(&root).ok();
    }

    #[gpui::test]
    fn new_python_file_is_new_file_with_python_chosen(cx: &mut TestAppContext) {
        let root = project_dir("new-python");
        let (tree, cx, events) = open_tree(&root, cx);
        tree.update_in(cx, |tree, window, cx| {
            tree.new_python_file(&NewPythonFile, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(chosen(&tree, cx), Some("python"));
        // The extension is added, so `a` is taken by `a.py`.
        cx.simulate_input("a");
        cx.run_until_parked();
        assert_eq!(
            error(&tree, cx).as_deref(),
            Some("A file or folder `a.py` already exists at this location")
        );
        cx.simulate_input("pp");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            *events.borrow(),
            [FileTreeEvent::Created(root.join("app.py"))]
        );
        // Without a name (and a type needing one), Enter just closes.
        tree.update_in(cx, |tree, window, cx| {
            tree.new_python_file(&NewPythonFile, window, cx)
        });
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(tree.read_with(cx, |tree, _| tree.editing.is_none()));
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
            tree.rebuild(cx);
            tree.select(&main, cx);
        });
        cx.run_until_parked();
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
            tree.read_with(cx, |tree, _| tree.selection.paths().to_vec()),
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
        // A file selected in the second folder (listed once opened): New
        // File goes next to it.
        tree.update(cx, |tree, cx| tree.click_row(4, Modifiers::none(), 1, cx));
        cx.run_until_parked();
        tree.update(cx, |tree, cx| {
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

    #[gpui::test]
    fn external_changes_are_read_off_the_ui_thread_and_gone_folders_close(cx: &mut TestAppContext) {
        let root = project_dir("refresh");
        let pkg = root.join("src").join("pkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(pkg.join("__init__.py"), "").unwrap();
        let (tree, cx, _) = open_tree(&root, cx);
        cx.simulate_keystrokes("down right");
        assert_eq!(
            names(&tree, cx),
            ["src", "  pkg", "  main.py", "a.py", "b.py"]
        );
        // The listing says which folders are packages: no stat per folder.
        assert!(tree.read_with(cx, |tree, _| tree.rows[1].package));
        cx.simulate_keystrokes("down right");
        let open = [
            "src",
            "  pkg",
            "    __init__.py",
            "  main.py",
            "a.py",
            "b.py",
        ];
        assert_eq!(names(&tree, cx), open);

        // Outside the tree, a file appears and the open package goes.
        fs::write(root.join("c.py"), "").unwrap();
        fs::remove_dir_all(&pkg).unwrap();
        tree.update(cx, |tree, cx| {
            tree.refresh([root.join("c.py").as_path(), pkg.as_path()], false, cx)
        });
        // The rows at hand stay until the re-read listings arrive.
        assert_eq!(names(&tree, cx), open);
        cx.run_until_parked();
        assert_eq!(
            names(&tree, cx),
            ["src", "  main.py", "a.py", "b.py", "c.py"]
        );
        assert!(tree.read_with(cx, |tree, _| !tree.expanded.contains(&pkg)));

        // A rescan re-reads every listing, keeping what is open.
        fs::write(root.join("src").join("new.py"), "").unwrap();
        tree.update(cx, |tree, cx| tree.refresh(std::iter::empty(), true, cx));
        cx.run_until_parked();
        assert_eq!(
            names(&tree, cx),
            ["src", "  main.py", "  new.py", "a.py", "b.py", "c.py"]
        );
        assert!(tree.read_with(cx, |tree, _| tree.loading.is_empty()
            && tree.stale.is_empty()));
        fs::remove_dir_all(&root).ok();
    }

    #[gpui::test]
    fn drags_take_the_selection_along_from_a_selected_row_only(cx: &mut TestAppContext) {
        let root = project_dir("drag-payload");
        let (tree, cx, _) = open_tree(&root, cx);
        tree.update(cx, |tree, cx| {
            tree.click_row(2, Modifiers::none(), 1, cx);
            tree.click_row(1, Modifiers::secondary_key(), 1, cx);
            let drag = |path: &Path, selected: bool| DraggedPaths {
                tree: cx.entity_id(),
                path: path.to_path_buf(),
                selected,
            };
            // In tree order, whichever was selected first.
            assert_eq!(
                tree.dragged_paths(&drag(&root.join("b.py"), true)),
                [root.join("a.py"), root.join("b.py")]
            );
            assert_eq!(
                tree.dragged_paths(&drag(&root.join("src"), false)),
                [root.join("src")]
            );
        });
        fs::remove_dir_all(&root).ok();
    }
}
