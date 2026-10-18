//! Call Hierarchy panel: incoming or outgoing calls of the symbol Show Call
//! Hierarchy was run on, as a tree whose levels are fetched lazily from the
//! language server (`callHierarchy/incomingCalls` / `outgoingCalls`).

use std::{sync::Arc, time::Duration};

use gpui::{
    AnyElement, App, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, KeyBinding,
    ScrollStrategy, SharedString, Task, UniformListScrollHandle, Window, actions, div, prelude::*,
    px, uniform_list,
};
use language::Location;
use lsp::types::{
    CallHierarchyIncomingCallsParams, CallHierarchyItem, CallHierarchyOutgoingCallsParams, Range,
    request::{CallHierarchyIncomingCalls, CallHierarchyOutgoingCalls},
};

use crate::{
    editor::symbol_kind_badge,
    lsp_store::{RunningServer, request_retrying},
    pinpoint::Pin,
    selectable::selectable,
    theme,
};

actions!(
    call_hierarchy,
    [SelectPrev, SelectNext, Expand, Collapse, Open]
);

const CONTEXT: &str = "CallHierarchy";
const ROW_HEIGHT: f32 = 22.;
const INDENT: f32 = 16.;

pub fn bind_keys(cx: &mut App) {
    let ctx = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", SelectPrev, ctx),
        KeyBinding::new("down", SelectNext, ctx),
        KeyBinding::new("right", Expand, ctx),
        KeyBinding::new("left", Collapse, ctx),
        KeyBinding::new("enter", Open, ctx),
    ]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Who calls the item (VS Code's default).
    Incoming,
    /// What the item calls.
    Outgoing,
}

/// One call: the caller (incoming) or callee (outgoing), and the call
/// sites. Incoming call sites are in the caller's file; outgoing ones are in
/// the file of the item whose calls were asked for.
#[derive(Clone, Debug)]
pub struct Call {
    pub item: CallHierarchyItem,
    pub from_ranges: Vec<Range>,
}

/// Asks the server for the calls of `item` in `direction`.
pub async fn calls(
    server: &RunningServer,
    direction: Direction,
    item: CallHierarchyItem,
    executor: &gpui::BackgroundExecutor,
    retries: u32,
    delay: Duration,
) -> Result<Vec<Call>, lsp::Error> {
    Ok(match direction {
        Direction::Incoming => {
            let params = CallHierarchyIncomingCallsParams {
                item,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            request_retrying::<CallHierarchyIncomingCalls>(server, params, executor, retries, delay)
                .await?
                .unwrap_or_default()
                .into_iter()
                .map(|call| Call {
                    item: call.from,
                    from_ranges: call.from_ranges,
                })
                .collect()
        }
        Direction::Outgoing => {
            let params = CallHierarchyOutgoingCallsParams {
                item,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            request_retrying::<CallHierarchyOutgoingCalls>(server, params, executor, retries, delay)
                .await?
                .unwrap_or_default()
                .into_iter()
                .map(|call| Call {
                    item: call.to,
                    from_ranges: call.from_ranges,
                })
                .collect()
        }
    })
}

#[derive(Debug, PartialEq)]
enum Children {
    /// Not asked for yet.
    Unloaded,
    Loading,
    Loaded(Vec<usize>),
    Failed(SharedString),
}

struct Node {
    item: CallHierarchyItem,
    from_ranges: Vec<Range>,
    parent: Option<usize>,
    depth: usize,
    expanded: bool,
    children: Children,
}

/// A visible row of the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    Node(usize),
    /// Under an expanded node whose calls are pending.
    Loading(usize),
    /// Under an expanded node whose request failed.
    Failed(usize),
}

/// The tree model: nodes in an arena (cycles are fine, each expansion makes
/// new nodes), the visible rows derived from expansion state.
struct Tree {
    direction: Direction,
    roots: Vec<CallHierarchyItem>,
    nodes: Vec<Node>,
    root_ids: Vec<usize>,
    selected: usize,
    rows: Vec<Row>,
}

impl Tree {
    fn new(roots: Vec<CallHierarchyItem>, direction: Direction) -> Self {
        let mut tree = Self {
            direction,
            roots,
            nodes: Vec::new(),
            root_ids: Vec::new(),
            selected: 0,
            rows: Vec::new(),
        };
        tree.reset(direction);
        tree
    }

    /// Back to the root items, collapsed, showing `direction`.
    fn reset(&mut self, direction: Direction) {
        self.direction = direction;
        self.nodes = self
            .roots
            .iter()
            .map(|item| Node {
                item: item.clone(),
                from_ranges: Vec::new(),
                parent: None,
                depth: 0,
                expanded: false,
                children: Children::Unloaded,
            })
            .collect();
        self.root_ids = (0..self.nodes.len()).collect();
        self.selected = 0;
        self.rebuild_rows();
    }

    fn rebuild_rows(&mut self) {
        fn visit(tree: &Tree, id: usize, rows: &mut Vec<Row>) {
            rows.push(Row::Node(id));
            let node = &tree.nodes[id];
            if !node.expanded {
                return;
            }
            match &node.children {
                Children::Unloaded => {}
                Children::Loading => rows.push(Row::Loading(id)),
                Children::Failed(_) => rows.push(Row::Failed(id)),
                Children::Loaded(children) => {
                    for &child in children {
                        visit(tree, child, rows);
                    }
                }
            }
        }
        let mut rows = Vec::new();
        for &root in &self.root_ids {
            visit(self, root, &mut rows);
        }
        self.rows = rows;
    }

    /// Whether `id` may have children (unknown until loaded).
    fn expandable(&self, id: usize) -> bool {
        match &self.nodes[id].children {
            Children::Loaded(children) => !children.is_empty(),
            _ => true,
        }
    }

    /// Expands `id`; returns its item when its calls must be fetched.
    fn expand(&mut self, id: usize) -> Option<CallHierarchyItem> {
        let node = &mut self.nodes[id];
        node.expanded = true;
        let fetch = if matches!(node.children, Children::Unloaded | Children::Failed(_)) {
            node.children = Children::Loading;
            Some(node.item.clone())
        } else {
            None
        };
        self.rebuild_rows();
        fetch
    }

    /// Collapses `id`; a selection inside it moves to `id`.
    fn collapse(&mut self, id: usize) {
        self.nodes[id].expanded = false;
        if self.is_descendant(self.selected, id) {
            self.selected = id;
        }
        self.rebuild_rows();
    }

    fn is_descendant(&self, mut id: usize, ancestor: usize) -> bool {
        while let Some(parent) = self.nodes.get(id).and_then(|node| node.parent) {
            if parent == ancestor {
                return true;
            }
            id = parent;
        }
        false
    }

    /// The answer to `id`'s request.
    fn set_children(&mut self, id: usize, result: Result<Vec<Call>, String>) {
        let depth = self.nodes[id].depth + 1;
        self.nodes[id].children = match result {
            Ok(calls) => {
                let first = self.nodes.len();
                self.nodes.extend(calls.into_iter().map(|call| Node {
                    item: call.item,
                    from_ranges: call.from_ranges,
                    parent: Some(id),
                    depth,
                    expanded: false,
                    children: Children::Unloaded,
                }));
                Children::Loaded((first..self.nodes.len()).collect())
            }
            Err(err) => Children::Failed(err.into()),
        };
        self.rebuild_rows();
    }

    fn selected_row(&self) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| *row == Row::Node(self.selected))
    }

    /// Moves the selection `delta` node rows (skipping loading/error rows).
    fn move_selection(&mut self, forward: bool) {
        let Some(current) = self.selected_row() else {
            return;
        };
        let next = if forward {
            self.rows[current + 1..].iter().find_map(|row| match row {
                Row::Node(id) => Some(*id),
                _ => None,
            })
        } else {
            self.rows[..current].iter().rev().find_map(|row| match row {
                Row::Node(id) => Some(*id),
                _ => None,
            })
        };
        if let Some(id) = next {
            self.selected = id;
        }
    }

    /// Right: expands a collapsed node, else steps into its first child.
    /// Returns the item to fetch, if any.
    fn expand_selected(&mut self) -> Option<CallHierarchyItem> {
        let id = self.selected;
        if !self.nodes[id].expanded {
            return self.expandable(id).then(|| self.expand(id)).flatten();
        }
        if let Children::Loaded(children) = &self.nodes[id].children
            && let Some(&first) = children.first()
        {
            self.selected = first;
        }
        None
    }

    /// Left: collapses an expanded node, else selects its parent.
    fn collapse_selected(&mut self) {
        let id = self.selected;
        if self.nodes[id].expanded && self.expandable(id) {
            self.collapse(id);
        } else if let Some(parent) = self.nodes[id].parent {
            self.selected = parent;
        }
    }

    /// Where opening `id` goes: an incoming caller's first call site (as
    /// VS Code), else the item's name.
    fn jump_target(&self, id: usize) -> Option<Location> {
        let node = &self.nodes[id];
        let range = match (self.direction, node.from_ranges.first()) {
            (Direction::Incoming, Some(site)) if node.parent.is_some() => *site,
            _ => node.item.selection_range,
        };
        Some(Location {
            path: lsp::uri_to_path(&node.item.uri)?,
            range,
        })
    }
}

pub enum CallHierarchyEvent {
    Jump(Location),
}

impl EventEmitter<CallHierarchyEvent> for CallHierarchyPanel {}

pub struct CallHierarchyPanel {
    tree: Option<Tree>,
    server: Option<Arc<RunningServer>>,
    /// Bumped on every re-root; answers for older trees are dropped.
    generation: u64,
    focus_handle: FocusHandle,
    scroll: UniformListScrollHandle,
    _requests: Vec<Task<()>>,
}

impl CallHierarchyPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            tree: None,
            server: None,
            generation: 0,
            focus_handle: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            _requests: Vec::new(),
        }
    }

    /// Shows the incoming calls of `items` (from `prepareCallHierarchy`),
    /// expanding the first one, and focuses the tree.
    pub fn show(
        &mut self,
        server: Arc<RunningServer>,
        items: Vec<CallHierarchyItem>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.server = Some(server);
        self.tree = Some(Tree::new(items, Direction::Incoming));
        self.reroot(cx);
        window.focus(&self.focus_handle);
    }

    fn set_direction(&mut self, direction: Direction, cx: &mut Context<Self>) {
        let Some(tree) = &mut self.tree else {
            return;
        };
        if tree.direction != direction {
            tree.reset(direction);
            self.reroot(cx);
        }
    }

    /// Drops pending answers and expands the first root.
    fn reroot(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self._requests.clear();
        if let Some(tree) = &mut self.tree
            && let Some(&root) = tree.root_ids.first()
            && let Some(item) = tree.expand(root)
        {
            self.fetch(root, item, cx);
        }
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn fetch(&mut self, id: usize, item: CallHierarchyItem, cx: &mut Context<Self>) {
        let (Some(server), Some(tree)) = (self.server.clone(), &self.tree) else {
            return;
        };
        let direction = tree.direction;
        let generation = self.generation;
        self._requests.push(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let result = if server.connection.has_exited() {
                Err("The language server stopped; run Show Call Hierarchy again".to_owned())
            } else {
                calls(
                    &server,
                    direction,
                    item,
                    &executor,
                    1,
                    Duration::from_millis(400),
                )
                .await
                .map_err(|err| err.to_string())
            };
            this.update(cx, |this, cx| {
                if this.generation == generation
                    && let Some(tree) = &mut this.tree
                {
                    tree.set_children(id, result);
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn scroll_to_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(row) = self.tree.as_ref().and_then(Tree::selected_row) {
            // Only scrolls when the row is out of view.
            self.scroll.scroll_to_item(row, ScrollStrategy::Center);
        }
        cx.notify();
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(tree) = &mut self.tree {
            tree.move_selection(false);
            self.scroll_to_selection(cx);
        }
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(tree) = &mut self.tree {
            tree.move_selection(true);
            self.scroll_to_selection(cx);
        }
    }

    fn expand(&mut self, _: &Expand, _: &mut Window, cx: &mut Context<Self>) {
        let Some(tree) = &mut self.tree else {
            return;
        };
        let id = tree.selected;
        if let Some(item) = tree.expand_selected() {
            self.fetch(id, item, cx);
        }
        self.scroll_to_selection(cx);
    }

    fn collapse(&mut self, _: &Collapse, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(tree) = &mut self.tree {
            tree.collapse_selected();
            self.scroll_to_selection(cx);
        }
    }

    fn open(&mut self, _: &Open, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(tree) = &self.tree {
            self.jump(tree.selected, cx);
        }
    }

    fn jump(&mut self, id: usize, cx: &mut Context<Self>) {
        if let Some(location) = self.tree.as_ref().and_then(|tree| tree.jump_target(id)) {
            cx.emit(CallHierarchyEvent::Jump(location));
        }
    }

    fn toggle(&mut self, id: usize, cx: &mut Context<Self>) {
        let Some(tree) = &mut self.tree else {
            return;
        };
        if tree.nodes[id].expanded {
            tree.collapse(id);
        } else if tree.expandable(id)
            && let Some(item) = tree.expand(id)
        {
            self.fetch(id, item, cx);
        }
        cx.notify();
    }

    fn click(&mut self, id: usize, event: &ClickEvent, cx: &mut Context<Self>) {
        if let Some(tree) = &mut self.tree {
            tree.selected = id;
        }
        if event.click_count() >= 2 {
            self.toggle(id, cx);
        } else {
            self.jump(id, cx);
        }
        cx.notify();
    }

    fn render_row(&self, tree: &Tree, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let row = tree.rows[index];
        let (Row::Node(id) | Row::Loading(id) | Row::Failed(id)) = row;
        let node = &tree.nodes[id];
        let base = div()
            .id(index)
            .h(px(ROW_HEIGHT))
            .w_full()
            .pr_2()
            .flex()
            .items_center()
            .gap_1()
            .overflow_hidden()
            .text_sm();
        match row {
            Row::Loading(_) | Row::Failed(_) => {
                let text: SharedString = match &node.children {
                    Children::Failed(err) => err.clone(),
                    _ => "loading…".into(),
                };
                return base
                    .pl(px(INDENT * (node.depth + 1) as f32 + 22.))
                    .text_color(theme::muted())
                    .child(div().truncate().child(selectable("message", text)))
                    .pin(format!("{}>message", Self::item_key(tree, id)))
                    .into_any_element();
            }
            Row::Node(_) => {}
        }
        let path = lsp::uri_to_path(&node.item.uri);
        let language = path.as_deref().and_then(syntax::Language::from_path);
        let (badge, color) = symbol_kind_badge(node.item.kind, language);
        let file = path
            .as_deref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| node.item.uri.as_str().to_owned());
        let place = format!("{file}:{}", node.item.selection_range.start.line + 1);
        let chevron = if !tree.expandable(id) {
            ""
        } else if node.expanded {
            "▾"
        } else {
            "▸"
        };
        let selected = tree.selected == id;
        base.pl(px(INDENT * node.depth as f32 + 4.))
            .cursor_pointer()
            .when(selected, |row| row.bg(theme::active_row()))
            .when(!selected, |row| {
                row.hover(|style| style.bg(theme::hover_row()))
            })
            .on_click(cx.listener(move |this, event, _, cx| this.click(id, event, cx)))
            .child(
                div()
                    .id(("chevron", index))
                    .w(px(14.))
                    .flex_none()
                    .text_color(theme::muted())
                    .child(chevron)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        if let Some(tree) = &mut this.tree {
                            tree.selected = id;
                        }
                        this.toggle(id, cx);
                    }))
                    .pin("toggle")
                    .label(if node.expanded { "Collapse" } else { "Expand" }),
            )
            .child(div().w(px(14.)).flex_none().text_color(color).child(badge))
            .child(
                div()
                    .flex_none()
                    .text_color(theme::text())
                    .child(selectable("name", node.item.name.clone()).double_click_through()),
            )
            .children(node.item.detail.as_ref().map(|detail| {
                div()
                    .flex_shrink()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(selectable("detail", detail.clone()).double_click_through())
            }))
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(selectable("place", place).double_click_through()),
            )
            .pin(Self::item_key(tree, id))
            .label(node.item.name.clone())
            .interactive(true)
            .into_any_element()
    }

    /// `item:` and the node's `name@file:line` after its ancestors' (one
    /// function can show up under several callers).
    fn item_key(tree: &Tree, id: usize) -> String {
        let mut parts = Vec::new();
        let mut at = Some(id);
        while let Some(id) = at {
            let item = &tree.nodes[id].item;
            let file = lsp::uri_to_path(&item.uri)
                .as_deref()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| item.uri.as_str().to_owned());
            parts.push(format!(
                "{}@{file}:{}",
                item.name,
                item.selection_range.start.line + 1
            ));
            at = tree.nodes[id].parent;
        }
        parts.reverse();
        format!("item:{}", parts.join(">"))
    }

    fn direction_button(
        &self,
        direction: Direction,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let active = self
            .tree
            .as_ref()
            .is_some_and(|tree| tree.direction == direction);
        div()
            .id(label)
            .px_2()
            .rounded_sm()
            .cursor_pointer()
            .text_xs()
            .when(active, |button| {
                button.bg(theme::active_row()).text_color(theme::text())
            })
            .when(!active, |button| {
                button
                    .text_color(theme::muted())
                    .hover(|style| style.bg(theme::hover_row()))
            })
            .on_click(cx.listener(move |this, _, _, cx| this.set_direction(direction, cx)))
            .child(label)
            .pin(label.to_lowercase())
    }
}

impl Focusable for CallHierarchyPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CallHierarchyPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = div()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col();
        let Some(tree) = &self.tree else {
            return panel
                .p_3()
                .text_sm()
                .text_color(theme::muted())
                .child(selectable(
                    "empty",
                    "Run Show Call Hierarchy (Shift+Alt+H) on a function in the editor.",
                ))
                .into_any_element();
        };
        let title: SharedString = tree
            .roots
            .first()
            .map(|item| item.name.clone())
            .unwrap_or_default()
            .into();
        let caption = match tree.direction {
            Direction::Incoming => "Callers of",
            Direction::Outgoing => "Calls from",
        };
        let stopped = self
            .server
            .as_ref()
            .is_some_and(|server| server.connection.has_exited());
        let rows = tree.rows.len();
        panel
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::expand))
            .on_action(cx.listener(Self::collapse))
            .on_action(cx.listener(Self::open))
            .child(
                div()
                    .flex_none()
                    .h(px(28.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme::border())
                    .text_sm()
                    .child(
                        div()
                            .text_color(theme::muted())
                            .child(selectable("caption", caption)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(theme::text())
                            .child(selectable("title", title)),
                    )
                    .child(self.direction_button(Direction::Incoming, "Incoming", cx))
                    .child(self.direction_button(Direction::Outgoing, "Outgoing", cx))
                    .pin("toolbar"),
            )
            .when(stopped, |panel| {
                panel.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(selectable(
                            "stopped",
                            "The language server stopped; run Show Call Hierarchy again.",
                        )),
                )
            })
            .child(
                uniform_list(
                    "call-hierarchy",
                    rows,
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        let Some(tree) = this.tree.take() else {
                            return Vec::new();
                        };
                        let rows = range
                            .map(|index| this.render_row(&tree, index, cx))
                            .collect();
                        this.tree = Some(tree);
                        rows
                    }),
                )
                .track_scroll(self.scroll.clone())
                .flex_1()
                .pin("calls"),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use lsp::types::{Position, SymbolKind, Uri};

    use super::*;

    fn range(line: u32) -> Range {
        Range::new(Position::new(line, 4), Position::new(line, 8))
    }

    fn item(name: &str, line: u32) -> CallHierarchyItem {
        let uri: Uri = format!("file:///project/src/{name}.rs").parse().unwrap();
        CallHierarchyItem {
            name: name.to_owned(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            detail: None,
            uri,
            range: Range::new(Position::new(line, 0), Position::new(line + 2, 1)),
            selection_range: range(line),
            data: None,
        }
    }

    fn call(name: &str, line: u32, site: u32) -> Call {
        Call {
            item: item(name, line),
            from_ranges: vec![range(site)],
        }
    }

    fn names(tree: &Tree) -> Vec<String> {
        tree.rows
            .iter()
            .map(|row| match row {
                Row::Node(id) => format!(
                    "{}{}",
                    "  ".repeat(tree.nodes[*id].depth),
                    tree.nodes[*id].item.name
                ),
                Row::Loading(_) => "loading".to_owned(),
                Row::Failed(_) => "failed".to_owned(),
            })
            .collect()
    }

    /// `leaf` ← `a`, `b`; `a` ← `main`.
    fn loaded_tree() -> Tree {
        let mut tree = Tree::new(vec![item("leaf", 0)], Direction::Incoming);
        assert!(tree.expand(0).is_some());
        tree.set_children(0, Ok(vec![call("a", 10, 11), call("b", 20, 21)]));
        assert!(tree.expand(1).is_some());
        tree.set_children(1, Ok(vec![call("main", 30, 31)]));
        tree
    }

    #[test]
    fn expanding_fetches_once_and_inserts_children_one_level_deeper() {
        let mut tree = Tree::new(vec![item("leaf", 0)], Direction::Incoming);
        let fetch = tree.expand(0).expect("first expansion fetches");
        assert_eq!(fetch.name, "leaf");
        assert_eq!(names(&tree), ["leaf", "loading"]);
        tree.set_children(0, Ok(vec![call("a", 10, 11), call("b", 20, 21)]));
        assert_eq!(names(&tree), ["leaf", "  a", "  b"]);
        assert!(tree.expand(1).is_some());
        tree.set_children(1, Ok(vec![call("main", 30, 31)]));
        assert_eq!(names(&tree), ["leaf", "  a", "    main", "  b"]);
        // Collapsing and expanding again reuses the cached children.
        tree.collapse(1);
        assert!(tree.expand(1).is_none());
        assert_eq!(names(&tree), ["leaf", "  a", "    main", "  b"]);
    }

    #[test]
    fn empty_results_drop_the_chevron_and_failures_can_be_retried() {
        let mut tree = Tree::new(vec![item("leaf", 0)], Direction::Incoming);
        tree.expand(0);
        tree.set_children(0, Err("boom".to_owned()));
        assert_eq!(names(&tree), ["leaf", "failed"]);
        tree.collapse(0);
        assert!(tree.expand(0).is_some(), "a failed level is fetched again");
        tree.set_children(0, Ok(Vec::new()));
        assert!(!tree.expandable(0));
        assert_eq!(names(&tree), ["leaf"]);
    }

    #[test]
    fn collapsing_hides_descendants_and_moves_selection_up() {
        let mut tree = loaded_tree();
        tree.selected = 3; // main (ids: leaf 0, a 1, b 2, main 3)
        tree.collapse(0);
        assert_eq!(names(&tree), ["leaf"]);
        assert_eq!(
            tree.selected, 0,
            "the hidden selection moves to the collapsed node"
        );
        tree.expand(0);
        assert_eq!(
            names(&tree),
            ["leaf", "  a", "    main", "  b"],
            "children keep their own expansion"
        );
    }

    #[test]
    fn selection_moves_over_visible_node_rows() {
        let mut tree = loaded_tree();
        let mut order = vec![tree.selected];
        for _ in 0..5 {
            tree.move_selection(true);
            order.push(tree.selected);
        }
        assert_eq!(order, [0, 1, 3, 2, 2, 2], "leaf, a, main, b, then stays");
        tree.move_selection(false);
        assert_eq!(tree.selected, 3);
        // Loading rows are skipped.
        tree.selected = 2;
        tree.expand(2);
        tree.selected = 1;
        tree.move_selection(true);
        tree.move_selection(true);
        assert_eq!(tree.selected, 2);
        tree.move_selection(true);
        assert_eq!(tree.selected, 2);
    }

    #[test]
    fn right_and_left_expand_step_in_collapse_and_step_out() {
        let mut tree = Tree::new(vec![item("leaf", 0)], Direction::Incoming);
        assert!(tree.expand_selected().is_some());
        tree.set_children(0, Ok(vec![call("a", 10, 11)]));
        assert!(tree.expand_selected().is_none());
        assert_eq!(
            tree.selected, 1,
            "Right on an expanded node selects its first child"
        );
        tree.collapse_selected();
        assert_eq!(
            tree.selected, 0,
            "Left on a collapsed node selects its parent"
        );
        tree.collapse_selected();
        assert_eq!(names(&tree), ["leaf"]);
    }

    #[test]
    fn switching_direction_resets_to_the_roots() {
        let mut tree = loaded_tree();
        tree.selected = 3;
        tree.reset(Direction::Outgoing);
        assert_eq!(tree.direction, Direction::Outgoing);
        assert_eq!(names(&tree), ["leaf"]);
        assert_eq!(tree.selected, 0);
        assert_eq!(tree.nodes[0].children, Children::Unloaded);
        assert_eq!(
            tree.expand(0).map(|item| item.name),
            Some("leaf".to_owned())
        );
    }

    #[test]
    fn incoming_calls_jump_to_the_call_site_outgoing_to_the_callee() {
        let tree = loaded_tree();
        let target = tree.jump_target(1).unwrap();
        assert!(target.path.ends_with("a.rs"));
        assert_eq!(target.range, range(11), "the call site inside the caller");
        assert_eq!(
            tree.jump_target(0).unwrap().range,
            range(0),
            "roots: their name"
        );

        let mut tree = Tree::new(vec![item("main", 30)], Direction::Outgoing);
        tree.expand(0);
        tree.set_children(0, Ok(vec![call("a", 10, 31)]));
        let target = tree.jump_target(1).unwrap();
        assert!(target.path.ends_with("a.rs"));
        assert_eq!(target.range, range(10), "the callee's name");
    }
}
