//! Editor panes (VS Code's editor groups): the editor area splits right or
//! down into panes, each with its own tabs and active tab. Panes are the
//! leaves of a tree of splits; `Workspace::panes` lists them in tree order
//! (left to right, top to bottom), the order `Ctrl+1/2/3` and next/previous
//! pane follow. A file open in several panes has one editor per pane over
//! one linked buffer (`text::Buffer::link`); the language server document
//! belongs to the view that last had focus.

use std::{cell::Cell, rc::Rc};

use gpui::{
    AnyElement, Bounds, CursorStyle, EntityId, MouseButton, Pixels, Point, canvas, relative,
};
use serde::{Deserialize, Serialize};

use super::{docks::DragPreview, *};

actions!(
    workspace,
    [
        SplitRight,
        SplitDown,
        FocusNextPane,
        FocusPreviousPane,
        FocusFirstPane,
        FocusSecondPane,
        FocusThirdPane,
        MoveTabToNextPane,
        MoveTabToPreviousPane,
        ClosePane,
    ]
);

pub fn bind_keys(cx: &mut App) {
    let ctx = Some("Workspace");
    // VS Code's editor group shortcuts.
    cx.bind_keys([
        KeyBinding::new("secondary-\\", SplitRight, ctx),
        KeyBinding::new("secondary-k secondary-\\", SplitDown, ctx),
        KeyBinding::new("secondary-1", FocusFirstPane, ctx),
        KeyBinding::new("secondary-2", FocusSecondPane, ctx),
        KeyBinding::new("secondary-3", FocusThirdPane, ctx),
        KeyBinding::new("secondary-k secondary-right", FocusNextPane, ctx),
        KeyBinding::new("secondary-k secondary-left", FocusPreviousPane, ctx),
        KeyBinding::new("ctrl-alt-right", MoveTabToNextPane, ctx),
        KeyBinding::new("ctrl-alt-left", MoveTabToPreviousPane, ctx),
        KeyBinding::new("secondary-k w", ClosePane, ctx),
    ]);
}

/// Smallest share of a split either side keeps.
const MIN_RATIO: f32 = 0.1;
const HANDLE_SIZE: f32 = 4.;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct PaneId(pub(super) usize);

pub(super) struct Pane {
    pub(super) id: PaneId,
    pub(super) tabs: Vec<Tab>,
    pub(super) active: usize,
}

impl Pane {
    pub(super) fn new(id: PaneId) -> Self {
        Self {
            id,
            tabs: Vec::new(),
            active: 0,
        }
    }

    pub(super) fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    /// The editor tab of `path` in this pane.
    pub(super) fn editor_index(&self, path: &Path) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.path == path && tab.editor().is_some())
    }

    /// The built-in extension's tab of `path` of kind `kind` in this pane.
    pub(super) fn file_tab_index(&self, path: &Path, kind: &str) -> Option<usize> {
        self.tabs.iter().position(|tab| {
            tab.path == path && tab.file_tab().is_some_and(|tab| tab.kind() == kind)
        })
    }

    fn index_of(&self, view: EntityId) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.entity_id() == view)
    }
}

/// How a split arranges its two sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SplitAxis {
    /// Side by side.
    Horizontal,
    /// One above the other.
    Vertical,
}

/// Where a new pane goes next to an existing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    fn axis(self) -> SplitAxis {
        match self {
            Self::Left | Self::Right => SplitAxis::Horizontal,
            Self::Top | Self::Bottom => SplitAxis::Vertical,
        }
    }

    fn before(self) -> bool {
        matches!(self, Self::Left | Self::Top)
    }
}

pub(super) enum PaneNode {
    Leaf(PaneId),
    Split(Split),
}

pub(super) struct Split {
    id: usize,
    pub(super) axis: SplitAxis,
    /// Share of the first side, in `MIN_RATIO..=1 - MIN_RATIO`.
    pub(super) ratio: f32,
    pub(super) first: Box<PaneNode>,
    pub(super) second: Box<PaneNode>,
    /// Where the split was laid out last frame, for resizing.
    bounds: Rc<Cell<Bounds<Pixels>>>,
}

impl PaneNode {
    pub(super) fn split(
        id: usize,
        axis: SplitAxis,
        ratio: f32,
        first: PaneNode,
        second: PaneNode,
    ) -> Self {
        Self::Split(Split {
            id,
            axis,
            ratio: clamp_ratio(ratio),
            first: Box::new(first),
            second: Box::new(second),
            bounds: Rc::default(),
        })
    }

    /// Panes in order: left to right, top to bottom.
    pub(super) fn leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            Self::Leaf(id) => out.push(*id),
            Self::Split(split) => {
                split.first.leaves(out);
                split.second.leaves(out);
            }
        }
    }

    /// Splits leaf `target` to place `new` on `side` of it.
    fn insert(&mut self, target: PaneId, new: PaneId, side: Side, id: usize) -> bool {
        match self {
            Self::Leaf(leaf) if *leaf == target => {
                let (first, second) = if side.before() {
                    (new, target)
                } else {
                    (target, new)
                };
                *self = Self::split(id, side.axis(), 0.5, Self::Leaf(first), Self::Leaf(second));
                true
            }
            Self::Leaf(_) => false,
            Self::Split(split) => {
                split.first.insert(target, new, side, id)
                    || split.second.insert(target, new, side, id)
            }
        }
    }

    /// Removes leaf `pane`; its sibling takes the place of their split.
    fn remove(&mut self, pane: PaneId) -> bool {
        let Self::Split(split) = self else {
            return false;
        };
        let is_pane = |node: &PaneNode| matches!(node, Self::Leaf(id) if *id == pane);
        let survivor = if is_pane(&split.first) {
            Some(std::mem::replace(&mut *split.second, Self::Leaf(pane)))
        } else if is_pane(&split.second) {
            Some(std::mem::replace(&mut *split.first, Self::Leaf(pane)))
        } else {
            None
        };
        match survivor {
            Some(survivor) => {
                *self = survivor;
                true
            }
            None => split.first.remove(pane) || split.second.remove(pane),
        }
    }

    fn split_mut(&mut self, id: usize) -> Option<&mut Split> {
        let Self::Split(split) = self else {
            return None;
        };
        if split.id == id {
            return Some(split);
        }
        let Split { first, second, .. } = split;
        first.split_mut(id).or_else(|| second.split_mut(id))
    }
}

fn clamp_ratio(ratio: f32) -> f32 {
    if ratio.is_finite() {
        ratio.clamp(MIN_RATIO, 1. - MIN_RATIO)
    } else {
        0.5
    }
}

/// Payload of an editor tab being dragged.
pub(super) struct DraggedTab {
    view: EntityId,
    title: SharedString,
}

/// Where a dragged tab was dropped.
#[derive(Clone, Copy)]
enum DropTarget {
    /// Into a pane, before tab `index` (at the end for `None`).
    Pane { pane: PaneId, index: Option<usize> },
    /// Into a new pane on `side` of `pane`.
    Split { pane: PaneId, side: Side },
}

impl Workspace {
    pub(super) fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.iter().find(|pane| pane.id == id)
    }

    pub(super) fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|pane| pane.id == id)
    }

    fn pane_index(&self, id: PaneId) -> usize {
        self.panes
            .iter()
            .position(|pane| pane.id == id)
            .unwrap_or_default()
    }

    /// The pane commands and opened files go to.
    pub(super) fn active_pane(&self) -> &Pane {
        let index = self.pane_index(self.active_pane);
        &self.panes[index]
    }

    pub(super) fn active_tab(&self) -> Option<&Tab> {
        self.active_pane().active_tab()
    }

    pub(super) fn all_tabs(&self) -> impl Iterator<Item = &Tab> {
        self.panes.iter().flat_map(|pane| &pane.tabs)
    }

    /// Where the tab showing `view` is.
    pub(super) fn find_view(&self, view: EntityId) -> Option<(PaneId, usize)> {
        self.panes
            .iter()
            .find_map(|pane| Some((pane.id, pane.index_of(view)?)))
    }

    pub(super) fn next_id(&mut self) -> usize {
        self.next_id += 1;
        self.next_id
    }

    /// Adds an empty pane on `side` of `target` and returns it.
    fn split_pane(&mut self, target: PaneId, side: Side) -> PaneId {
        let new = PaneId(self.next_id());
        let split = self.next_id();
        self.pane_tree.insert(target, new, side, split);
        self.panes.push(Pane::new(new));
        self.order_panes();
        new
    }

    /// Sorts `panes` into tree order.
    pub(super) fn order_panes(&mut self) {
        let mut order = Vec::with_capacity(self.panes.len());
        self.pane_tree.leaves(&mut order);
        self.panes.sort_by_key(|pane| {
            order
                .iter()
                .position(|id| *id == pane.id)
                .unwrap_or(usize::MAX)
        });
    }

    /// Removes empty panes, keeping at least one; the active pane moves to
    /// the one before a removed active pane.
    pub(super) fn prune_empty_panes(&mut self) {
        while self.panes.len() > 1 {
            let Some(index) = self.panes.iter().position(|pane| pane.tabs.is_empty()) else {
                break;
            };
            let id = self.panes.remove(index).id;
            self.pane_tree.remove(id);
            if self.active_pane == id {
                self.active_pane = self.panes[index.saturating_sub(1)].id;
            }
        }
    }

    /// Makes `pane` the active pane, e.g. when it is clicked.
    pub(super) fn set_active_pane(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if self.active_pane == pane || self.pane(pane).is_none() {
            return;
        }
        self.active_pane = pane;
        let active = self.active_pane().active;
        self.claim_server(pane, active, cx);
        self.schedule_session_save(cx);
        cx.notify();
    }

    /// Follows keyboard focus: the pane whose active tab has it is active.
    pub(crate) fn sync_active_pane(&mut self, window: &Window, cx: &mut Context<Self>) {
        let focused = self.panes.iter().find_map(|pane| {
            pane.active_tab()?
                .contains_focus(window, cx)
                .then_some(pane.id)
        });
        if let Some(pane) = focused {
            self.set_active_pane(pane, cx);
        }
    }

    /// Gives the language server document of the file shown by tab
    /// `index` of `pane` to that view, so hover, completion and semantic
    /// highlighting work where the user is.
    pub(super) fn claim_server(&mut self, pane: PaneId, index: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.pane(pane).and_then(|pane| pane.tabs.get(index)) else {
            return;
        };
        let Some(editor) = tab.editor().cloned().filter(|_| !tab.lsp_owner) else {
            return;
        };
        let path = tab.path.clone();
        let owner = self.panes.iter().find_map(|pane| {
            let index = pane
                .tabs
                .iter()
                .position(|tab| tab.path == path && tab.lsp_owner && tab.editor().is_some())?;
            Some((pane.id, index))
        });
        if let Some((owner_pane, owner_index)) = owner {
            let Some(owner_tab) = self
                .pane_mut(owner_pane)
                .and_then(|pane| pane.tabs.get_mut(owner_index))
            else {
                return;
            };
            owner_tab.lsp_owner = false;
            if let Some(document) = owner_tab
                .editor()
                .and_then(|owner| owner.update(cx, |owner, _| owner.take_server()))
            {
                editor.update(cx, |editor, cx| editor.adopt_server(document, cx));
            }
        }
        if let Some(tab) = self
            .pane_mut(pane)
            .and_then(|pane| pane.tabs.get_mut(index))
        {
            tab.lsp_owner = true;
        }
    }

    /// Takes tab `index` out of `pane`, keeping the pane's active tab
    /// where it was (or on a neighbor).
    pub(super) fn take_tab(&mut self, pane: PaneId, index: usize) -> Option<Tab> {
        let pane = self.pane_mut(pane)?;
        if index >= pane.tabs.len() {
            return None;
        }
        let tab = pane.tabs.remove(index);
        if pane.active > index || pane.active >= pane.tabs.len() {
            pane.active = pane.active.saturating_sub(1);
        }
        Some(tab)
    }

    /// Closes tab `index` of `pane` without asking. The language server
    /// document moves to another view of the file, or is closed with the
    /// last one; a built-in extension's file tab is released with its last
    /// view (a notebook's kernel stops). A pane left empty goes away
    /// (unless it is the only one).
    pub(super) fn remove_tab(
        &mut self,
        pane: PaneId,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.take_tab(pane, index) else {
            return;
        };
        if matches!(tab.content, TabContent::Extension(_)) {
            self.forget_extension_view(tab.entity_id());
        }
        if let Some(file) = tab.file_tab()
            && !self.has_other_view(&tab)
        {
            file.release(cx);
        }
        if let Some(editor) = tab.editor() {
            let heir = self.panes.iter().find_map(|pane| {
                let index = pane.editor_index(&tab.path)?;
                Some((pane.id, index))
            });
            match heir {
                Some((heir_pane, heir_index)) if tab.lsp_owner => {
                    let document = editor.update(cx, |editor, _| editor.take_server());
                    if let Some(heir) = self
                        .pane_mut(heir_pane)
                        .and_then(|pane| pane.tabs.get_mut(heir_index))
                    {
                        heir.lsp_owner = true;
                        if let (Some(document), Some(heir)) = (document, heir.editor()) {
                            heir.update(cx, |heir, cx| heir.adopt_server(document, cx));
                        }
                    }
                }
                Some(_) => {}
                None => {
                    editor.update(cx, |editor, _| editor.detach_server(true));
                    // Its unknown words leave the Problems panel with it.
                    self.problems
                        .update(cx, |problems, cx| problems.set_spelling(&tab.path, &[], cx));
                }
            }
        }
        let was_active = self.active_pane == pane;
        self.prune_empty_panes();
        if was_active {
            let active = self.active_pane().active;
            if self.active_pane().tabs.is_empty() {
                window.focus(&self.focus_handle);
            } else {
                return self.activate(self.active_pane, active, window, cx);
            }
        }
        self.schedule_session_save(cx);
        cx.notify();
    }

    /// Moves tab `index` of `from` into pane `to` (before `insert`, or at
    /// the end). A file already open there just gets activated.
    fn move_tab(
        &mut self,
        from: PaneId,
        index: usize,
        to: PaneId,
        insert: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.pane(from).and_then(|pane| pane.tabs.get(index)) else {
            return;
        };
        let existing = self.pane(to).and_then(|pane| match &tab.content {
            TabContent::Editor(_) => pane.editor_index(&tab.path),
            TabContent::Builtin(file) if file.is_file() => {
                pane.file_tab_index(&tab.path, file.kind())
            }
            _ => None,
        });
        if from != to
            && let Some(existing) = existing
        {
            self.activate(to, existing, window, cx);
            self.remove_tab(from, index, window, cx);
            return;
        }
        let Some(tab) = self.take_tab(from, index) else {
            return;
        };
        let insert = match insert {
            Some(at) if from == to && at > index => Some(at - 1),
            other => other,
        };
        let Some(target) = self.pane_mut(to) else {
            return;
        };
        let at = insert.unwrap_or(target.tabs.len()).min(target.tabs.len());
        target.tabs.insert(at, tab);
        self.active_pane = to;
        self.prune_empty_panes();
        self.activate(to, at, window, cx);
    }

    fn drop_tab(
        &mut self,
        dragged: &DraggedTab,
        target: DropTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((from, index)) = self.find_view(dragged.view) else {
            return;
        };
        match target {
            DropTarget::Pane { pane, index: at } => {
                self.move_tab(from, index, pane, at, window, cx)
            }
            DropTarget::Split { pane, side } => {
                // Splitting a pane off its own only tab changes nothing.
                if pane == from && self.pane(from).is_some_and(|pane| pane.tabs.len() == 1) {
                    return;
                }
                let new = self.split_pane(pane, side);
                self.move_tab(from, index, new, None, window, cx);
            }
        }
    }

    /// Opens the active file of `pane` in a new pane on `side` (a second
    /// view of the same buffer or notebook, at the same position).
    fn split(&mut self, pane: PaneId, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.pane(pane).and_then(Pane::active_tab) else {
            return;
        };
        let state = match &tab.content {
            TabContent::Editor(source) => Some(source.read(cx).state()),
            TabContent::Builtin(tab) if tab.is_file() => None,
            _ => {
                self.notice = Some("Only file tabs can be split".into());
                return cx.notify();
            }
        };
        let path = tab.path.clone();
        let new = self.split_pane(pane, side);
        self.active_pane = new;
        // A notebook's tab gets another view of its document.
        if let Some(editor) = self.open_editor(&path, window, cx)
            && let Some(state) = &state
        {
            editor.update(cx, |editor, cx| editor.restore_state(state, cx));
        }
        self.prune_empty_panes();
    }

    pub(super) fn split_right(
        &mut self,
        _: &SplitRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_active_pane(window, cx);
        self.split(self.active_pane, Side::Right, window, cx);
    }

    pub(super) fn split_down(
        &mut self,
        _: &SplitDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_active_pane(window, cx);
        self.split(self.active_pane, Side::Bottom, window, cx);
    }

    /// Focuses pane `index` (in order); one past the last splits the
    /// active file to the right, as VS Code does.
    fn focus_pane(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active_pane(window, cx);
        match self.panes.get(index) {
            Some(pane) if pane.tabs.is_empty() => window.focus(&self.focus_handle),
            Some(pane) => {
                let (pane, active) = (pane.id, pane.active);
                self.activate(pane, active, window, cx);
            }
            None if index == self.panes.len() => {
                let last = self.panes[index - 1].id;
                self.split(last, Side::Right, window, cx);
            }
            None => {}
        }
    }

    pub(super) fn focus_first_pane(
        &mut self,
        _: &FocusFirstPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_pane(0, window, cx);
    }

    pub(super) fn focus_second_pane(
        &mut self,
        _: &FocusSecondPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_pane(1, window, cx);
    }

    pub(super) fn focus_third_pane(
        &mut self,
        _: &FocusThirdPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_pane(2, window, cx);
    }

    fn focus_neighbor(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active_pane(window, cx);
        let count = self.panes.len();
        let current = self.pane_index(self.active_pane);
        let next = if forward {
            (current + 1) % count
        } else {
            (current + count - 1) % count
        };
        if next != current {
            self.focus_pane(next, window, cx);
        }
    }

    pub(super) fn focus_next_pane(
        &mut self,
        _: &FocusNextPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_neighbor(true, window, cx);
    }

    pub(super) fn focus_previous_pane(
        &mut self,
        _: &FocusPreviousPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_neighbor(false, window, cx);
    }

    /// Moves the active tab to the next (or previous) pane, splitting one
    /// off when there is none that way.
    fn move_active_tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_active_pane(window, cx);
        let from = self.active_pane;
        let pane = self.active_pane();
        if pane.tabs.is_empty() {
            return;
        }
        let (index, alone) = (pane.active, pane.tabs.len() == 1);
        let position = self.pane_index(from);
        let neighbor = if forward {
            self.panes.get(position + 1)
        } else {
            position.checked_sub(1).and_then(|at| self.panes.get(at))
        };
        let to = match neighbor {
            Some(pane) => pane.id,
            // Nothing would be left behind.
            None if alone => return,
            None => self.split_pane(from, if forward { Side::Right } else { Side::Left }),
        };
        self.move_tab(from, index, to, None, window, cx);
    }

    pub(super) fn move_tab_to_next_pane(
        &mut self,
        _: &MoveTabToNextPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_active_tab(true, window, cx);
    }

    pub(super) fn move_tab_to_previous_pane(
        &mut self,
        _: &MoveTabToPreviousPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_active_tab(false, window, cx);
    }

    pub(super) fn close_pane_action(
        &mut self,
        _: &ClosePane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_active_pane(window, cx);
        self.close_pane(self.active_pane, window, cx);
    }

    /// Closes every tab of `pane`, asking once about files whose unsaved
    /// edits would be lost (those not open in another pane).
    fn close_pane(&mut self, pane: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tabs) = self.pane(pane).map(|pane| &pane.tabs) else {
            return;
        };
        let losing: Vec<(TabContent, SharedString)> = tabs
            .iter()
            .filter(|tab| tab.is_dirty(cx) && !self.has_other_view(tab))
            .map(|tab| (tab.content.clone(), tab.title.clone()))
            .collect();
        if losing.is_empty() {
            return self.discard_pane(pane, window, cx);
        }
        let names: Vec<&str> = losing.iter().map(|(_, title)| title.as_ref()).collect();
        let message = format!("Save changes to {}?", names.join(", "));
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            None,
            &["Save", "Don't Save", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(answer) = answer.await else {
                return;
            };
            let close = match answer {
                // Closes once every write is done; a failed save keeps the pane.
                0 => {
                    let saves = this.update(cx, |_, cx| {
                        losing
                            .iter()
                            .map(|(content, _)| content.save(cx))
                            .collect::<Vec<_>>()
                    });
                    let Ok(saves) = saves else {
                        return;
                    };
                    let mut all = true;
                    for save in saves {
                        all &= save.await;
                    }
                    all
                }
                1 => true,
                _ => false,
            };
            if close {
                this.update_in(cx, |this, window, cx| this.discard_pane(pane, window, cx))
                    .ok();
            }
        })
        .detach();
    }

    fn discard_pane(&mut self, pane: PaneId, window: &mut Window, cx: &mut Context<Self>) {
        while let Some(last) = self
            .pane(pane)
            .and_then(|pane| pane.tabs.len().checked_sub(1))
        {
            self.remove_tab(pane, last, window, cx);
        }
    }

    /// Whether the file of a file tab is also shown by another tab of the
    /// same kind (views of one buffer, or of one notebook document).
    pub(super) fn has_other_view(&self, tab: &Tab) -> bool {
        let same = |other: &Tab| match (&tab.content, &other.content) {
            (TabContent::Editor(_), TabContent::Editor(_)) => other.path == tab.path,
            (TabContent::Builtin(file), TabContent::Builtin(other_file))
                if file.is_file() && other_file.is_file() =>
            {
                other.path == tab.path && file.kind() == other_file.kind()
            }
            _ => false,
        };
        let others = self.all_tabs().filter(|other| same(other)).count();
        // `tab` may already be out of the panes (`remove_tab`).
        let listed = self.find_view(tab.entity_id()).is_some();
        others > usize::from(listed)
    }

    /// Drags a split's handle to `position`.
    pub(super) fn resize_split(
        &mut self,
        id: usize,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(split) = self.pane_tree.split_mut(id) else {
            return;
        };
        let bounds = split.bounds.get();
        let (offset, size) = match split.axis {
            SplitAxis::Horizontal => (position.x - bounds.origin.x, bounds.size.width),
            SplitAxis::Vertical => (position.y - bounds.origin.y, bounds.size.height),
        };
        let size = f32::from(size);
        if size > 0. {
            split.ratio = clamp_ratio(f32::from(offset) / size);
            cx.notify();
        }
    }

    /// The editor area: the pane tree.
    pub(super) fn render_panes(&self, cx: &mut Context<Self>) -> AnyElement {
        // Panes are numbered in reading order (`pane:1` is top left): ids
        // are internal and change between sessions.
        let mut order = Vec::new();
        self.pane_tree.leaves(&mut order);
        self.render_node(&self.pane_tree, &order, cx)
            .pin("editor-area")
            .into_any_element()
    }

    fn render_node(&self, node: &PaneNode, order: &[PaneId], cx: &mut Context<Self>) -> AnyElement {
        let split = match node {
            PaneNode::Leaf(id) => {
                let number = order.iter().position(|leaf| leaf == id).unwrap_or(0) + 1;
                return match self.pane(*id) {
                    Some(pane) => self.render_pane(pane, number, cx),
                    None => div().into_any_element(),
                };
            }
            PaneNode::Split(split) => split,
        };
        let id = split.id;
        let horizontal = split.axis == SplitAxis::Horizontal;
        let bounds = split.bounds.clone();
        // Flex, not block: see `main` in `Workspace::render`.
        let first = div()
            .flex_none()
            .flex()
            .overflow_hidden()
            .child(self.render_node(&split.first, order, cx));
        let first = if horizontal {
            first.w(relative(split.ratio)).h_full()
        } else {
            first.h(relative(split.ratio)).w_full()
        };
        let second = div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .overflow_hidden()
            .child(self.render_node(&split.second, order, cx));
        let handle = div()
            .flex_none()
            .bg(if self.pane_resize == Some(id) {
                theme::accent()
            } else {
                theme::border()
            })
            .hover(|style| style.bg(theme::accent()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.pane_resize = Some(id);
                    cx.stop_propagation();
                    cx.notify();
                }),
            );
        let handle = if horizontal {
            handle
                .w(px(HANDLE_SIZE))
                .h_full()
                .cursor(CursorStyle::ResizeLeftRight)
        } else {
            handle
                .h(px(HANDLE_SIZE))
                .w_full()
                .cursor(CursorStyle::ResizeUpDown)
        };
        div()
            .relative()
            .size_full()
            .flex()
            .when(!horizontal, |split| split.flex_col())
            .child(
                canvas(move |laid_out, _, _| bounds.set(laid_out), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .child(first)
            .child({
                // Each gap between neighbouring panes belongs to one split:
                // named after the pane before it.
                let mut after = Vec::new();
                split.second.leaves(&mut after);
                let before = after
                    .first()
                    .and_then(|first| order.iter().position(|leaf| leaf == first))
                    .unwrap_or(0);
                handle
                    .pin(format!("split-handle:{before}"))
                    .label("Resize panes")
            })
            .child(second)
            .into_any_element()
    }

    fn render_pane(&self, pane: &Pane, number: usize, cx: &mut Context<Self>) -> AnyElement {
        let id = pane.id;
        let content = match pane.active_tab() {
            Some(tab) => {
                let key = tab.pin_key(&self.project, cx);
                div()
                    .size_full()
                    .flex()
                    .child(tab.view())
                    .pin(tab.content_pin(&key))
                    .label(tab.title.clone())
                    .interactive(false)
                    .into_any_element()
            }
            None => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::muted())
                .child("Open a file from the project tree, or press Ctrl+P")
                .into_any_element(),
        };
        div()
            .id(("pane", id.0))
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .capture_any_mouse_down(cx.listener(move |this, _, _, cx| this.set_active_pane(id, cx)))
            .when(!pane.tabs.is_empty(), |column| {
                column.child(self.render_tab_bar(pane, cx))
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(content)
                    .when(cx.has_active_drag(), |area| {
                        area.child(self.render_drop_zones(id, cx))
                    }),
            )
            .pin(format!("pane:{number}"))
            .interactive(false)
            .into_any_element()
    }

    fn render_tab_bar(&self, pane: &Pane, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let pane_id = pane.id;
        let focused = pane_id == self.active_pane;
        let mut keys = HashSet::new();
        div()
            .flex()
            .flex_none()
            .h(px(34.))
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border())
            .text_sm()
            .children(pane.tabs.iter().enumerate().map(|(index, tab)| {
                let active = index == pane.active;
                let dirty = tab.is_dirty(cx);
                let dragged = DraggedTab {
                    view: tab.entity_id(),
                    title: tab.title.clone(),
                };
                // A file open as text and as a notebook in one pane.
                let mut key = tab.pin_key(&self.project, cx);
                let mut copy = 1;
                while !keys.insert(key.clone()) {
                    copy += 1;
                    key = format!("{}#{copy}", tab.pin_key(&self.project, cx));
                }
                div()
                    .id(("tab", index))
                    .relative()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pl_3()
                    .pr_2()
                    .border_r_1()
                    .border_color(theme::border())
                    .cursor_pointer()
                    .text_color(if active {
                        theme::text()
                    } else {
                        theme::muted()
                    })
                    .when(!active, |tab| {
                        tab.hover(|style| style.bg(theme::hover_row()).text_color(theme::text()))
                    })
                    // The active tab joins the editor below; an accent line
                    // marks it in the focused pane.
                    .when(active, |tab| {
                        tab.bg(theme::background()).mb(px(-1.)).child(
                            div()
                                .absolute()
                                .left_0()
                                .right_0()
                                .bottom_0()
                                .h(px(2.))
                                .when(focused, |line| line.bg(theme::accent())),
                        )
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate(pane_id, index, window, cx)
                    }))
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |this, _, window, cx| {
                            this.close_tab(pane_id, index, window, cx)
                        }),
                    )
                    .on_drag(dragged, |dragged, _, _, cx| {
                        cx.new(|_| DragPreview(dragged.title.clone()))
                    })
                    // Dropping on a tab inserts before it.
                    .drag_over::<DraggedTab>(|style, _, _, _| {
                        style.border_l_2().border_color(theme::accent())
                    })
                    .on_drop(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
                        let target = DropTarget::Pane {
                            pane: pane_id,
                            index: Some(index),
                        };
                        this.drop_tab(dragged, target, window, cx);
                    }))
                    .child(tab.title.clone())
                    .when(
                        matches!(tab.content, TabContent::Extension(_)),
                        |tab_element| {
                            let id = tab.entity_id();
                            tab_element.child(
                                div()
                                    .id(("extension-to-dock", index))
                                    .px_1()
                                    .text_xs()
                                    .child("Move to Dock")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.move_extension_to_dock(id, window, cx);
                                    }))
                                    .pin("move-to-dock")
                                    .label("Move extension panel to dock"),
                            )
                        },
                    )
                    .child(
                        div()
                            .id(("close", index))
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(18.))
                            .rounded_sm()
                            .text_color(theme::muted())
                            .hover(|style| style.bg(theme::hover_row()).text_color(theme::text()))
                            .child(if dirty { "●" } else { "×" })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close_tab(pane_id, index, window, cx);
                            }))
                            .pin("close")
                            .label(if dirty {
                                "Close (unsaved changes)"
                            } else {
                                "Close"
                            }),
                    )
                    .pin(format!("tab:{key}"))
                    .label(tab.title.clone())
                    .interactive(true)
            }))
            // Empty space: drop to append.
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .drag_over::<DraggedTab>(|style, _, _, _| style.bg(theme::hover_row()))
                    .on_drop(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
                        let target = DropTarget::Pane {
                            pane: pane_id,
                            index: None,
                        };
                        this.drop_tab(dragged, target, window, cx);
                    })),
            )
            .pin("tab-bar")
    }

    /// While a tab is dragged: the pane's edges split it, its middle takes
    /// the tab.
    fn render_drop_zones(&self, pane: PaneId, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let edge = relative(0.25);
        let zone = |target: DropTarget| {
            div()
                .absolute()
                .drag_over::<DraggedTab>(|style, _, _, _| style.bg(theme::selection()))
                .on_drop(cx.listener(move |this, dragged: &DraggedTab, window, cx| {
                    this.drop_tab(dragged, target, window, cx);
                }))
        };
        let split = |side| DropTarget::Split { pane, side };
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(
                zone(DropTarget::Pane { pane, index: None })
                    .top(edge)
                    .bottom(edge)
                    .left(edge)
                    .right(edge),
            )
            .child(zone(split(Side::Left)).top_0().bottom_0().left_0().w(edge))
            .child(
                zone(split(Side::Right))
                    .top_0()
                    .bottom_0()
                    .right_0()
                    .w(edge),
            )
            .child(
                zone(split(Side::Top))
                    .top_0()
                    .left(edge)
                    .right(edge)
                    .h(edge),
            )
            .child(
                zone(split(Side::Bottom))
                    .bottom_0()
                    .left(edge)
                    .right(edge)
                    .h(edge),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitting_and_removing_keeps_panes_in_reading_order() {
        let (a, b, c) = (PaneId(1), PaneId(2), PaneId(3));
        let mut tree = PaneNode::Leaf(a);
        assert!(tree.insert(a, b, Side::Right, 10));
        assert!(tree.insert(a, c, Side::Bottom, 11));
        let leaves = |tree: &PaneNode| {
            let mut out = Vec::new();
            tree.leaves(&mut out);
            out
        };
        assert_eq!(leaves(&tree), [a, c, b]);
        assert!(tree.insert(b, PaneId(4), Side::Left, 12));
        assert_eq!(leaves(&tree), [a, c, PaneId(4), b]);
        assert!(tree.split_mut(12).is_some());

        assert!(tree.remove(a));
        assert_eq!(leaves(&tree), [c, PaneId(4), b]);
        assert!(tree.split_mut(11).is_none(), "a's split is gone");
        assert!(tree.remove(PaneId(4)));
        assert!(tree.remove(b));
        assert!(matches!(tree, PaneNode::Leaf(id) if id == c));
        assert!(!tree.remove(c), "the last pane stays");
    }
}
