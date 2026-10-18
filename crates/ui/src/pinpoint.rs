//! Pinpoint: every part of the interface has a stable path (for example
//! `workspace/left-dock/panel:files/row:src/main.rs`) that a person and an
//! agent can both name, find and highlight.
//!
//! # Recording
//! Elements opt in with [`Pin::pin`] (call it last in a builder chain).
//! Paths nest: a pinned element's path is its pinned ancestors' ids joined
//! with `/` (keys may hold `/` themselves: nodes keep their own segment and
//! their parent's index). Paths are computed during layout, so popovers
//! drawn through `deferred` keep their place under their owner; a pinned
//! element also stands for its path during its prepaint, so rows that lazy
//! lists lay out while prepainting nest too. Content laid out lazily inside
//! a `deferred` popover needs a pin inside the popover (its prepaint runs
//! after the owner's). Bounds are recorded during prepaint into a
//! per-thread buffer; the window's [`layer`] runs last in every frame,
//! publishes the frame (one lock) and paints highlights and the picker.
//!
//! Parts painted without an element of their own use [`current_path`] (in
//! request_layout) and [`record`] (in prepaint).
//!
//! # Text
//! Text owners attach readable text to their node with [`set_text`] (or
//! [`Pinned::text`]) and, during prepaint, ask which ranges are wanted:
//! [`wanted_text`] (byte ranges into that text), [`wanted_code`],
//! [`wanted_terminal`]; they answer with [`report`] (window rectangles, one
//! per line segment). Nothing is computed when nothing is highlighted.
//! While the human picker runs, [`picking`] gives the pointer and owners
//! report the characters under it with [`report_pick`].
//!
//! Agents reach all this through [`control::dispatch`] (see
//! `docs/pinpoint-for-agents.md`).

pub mod control;
mod highlight;
mod overlay;
mod picker;
pub mod server;
pub mod target;

use std::{
    cell::RefCell,
    collections::HashMap,
    ops::Range,
    path::Path,
    sync::{Arc, LazyLock},
};

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Point, SharedString, Window, WindowId, deferred, div,
};
use parking_lot::Mutex;

pub use target::{
    CodeWant, TerminalRef, TerminalWant, code_reference, parse_code_reference, text_reference,
};

use crate::project::Project;

/// Identifies one highlight (unique for the process).
pub type HighlightId = u64;

/// One pinned element as drawn in the last frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// Ids from the outermost pinned ancestor down, joined with `/`.
    pub path: SharedString,
    /// This node's own segment (the end of `path`; may hold `/`).
    pub id: SharedString,
    /// Index of the enclosing node in the same frame.
    pub parent: Option<usize>,
    /// Number of pinned ancestors.
    pub depth: u16,
    /// What a person calls it ("Run All"), when it differs from the id.
    pub label: Option<SharedString>,
    /// The text it shows, when a text owner attached it.
    pub text: Option<SharedString>,
    /// Window coordinates.
    pub bounds: Bounds<Pixels>,
    /// The part not clipped away by scrolling containers; `None` when
    /// nothing of it shows.
    pub visible: Option<Bounds<Pixels>>,
    /// Something to click or type in. Explicit with
    /// [`Pinned::interactive`], else whether it has no pinned children.
    pub interactive: bool,
}

/// A window's last complete frame.
#[derive(Debug, Default)]
pub struct Frame {
    /// In drawing order: parents before children, popovers last.
    pub nodes: Vec<Node>,
}

impl Frame {
    /// The last-drawn node with `path` (popovers are drawn last).
    pub fn find(&self, path: &str) -> Option<usize> {
        self.nodes.iter().rposition(|node| node.path == path)
    }

    /// The nearest ancestor of `index` that shows.
    pub fn visible_ancestor(&self, index: usize) -> Option<usize> {
        let mut at = self.nodes[index].parent;
        while let Some(index) = at {
            if self.nodes[index].visible.is_some() {
                return Some(index);
            }
            at = self.nodes[index].parent;
        }
        None
    }

    /// Completes recorded nodes (ids and parents only): paths, depths,
    /// interactivity. Done when someone reads the frame, not every frame.
    fn build(mut nodes: Vec<Node>, explicit: &[Option<bool>]) -> Self {
        let mut has_children = vec![false; nodes.len()];
        for index in 0..nodes.len() {
            // Parents are recorded before their children.
            let (path, depth) = match nodes[index].parent {
                Some(parent) => {
                    has_children[parent] = true;
                    let parent = &nodes[parent];
                    (
                        format!("{}/{}", parent.path, nodes[index].id).into(),
                        parent.depth.saturating_add(1),
                    )
                }
                None => (nodes[index].id.clone(), 0),
            };
            nodes[index].path = path;
            nodes[index].depth = depth;
        }
        for (index, node) in nodes.iter_mut().enumerate() {
            node.interactive = explicit[index].unwrap_or(!has_children[index]);
        }
        Self { nodes }
    }
}

/// Characters under the pointer while picking, as their owner reports them.
#[derive(Clone, Debug)]
pub struct PickText {
    /// What the clipboard gets: a code reference (`src/a.rs:12:5-12:9`) or
    /// `nextcode://ui/<path>` with the text.
    pub reference: String,
    /// The pinned element holding the text.
    pub path: SharedString,
    /// Window coordinates, one per line segment.
    pub rects: Vec<Bounds<Pixels>>,
}

/// An owner's answer for one wanted range.
#[derive(Clone, Debug)]
struct Report {
    id: HighlightId,
    rects: Vec<Bounds<Pixels>>,
    clipped: bool,
}

/// What prepaint has recorded so far for one window. Nodes carry their id
/// and parent; paths are joined only when the frame is read.
#[derive(Default)]
struct Building {
    nodes: Vec<Node>,
    /// Per node: [`Pinned::interactive`] when set.
    explicit: Vec<Option<bool>>,
    reports: Vec<Report>,
    picks: Vec<PickText>,
}

const NONE: u32 = u32::MAX;

/// A pinned element laid out in the frame being drawn.
struct Laid {
    id: SharedString,
    /// The enclosing pinned element when it was laid out (popovers drawn
    /// later keep their owner).
    parent: u32,
    /// Its node, once prepainted.
    node: u32,
}

/// The pinned elements of the frame being drawn (cleared when it is
/// published), and those enclosing the element being laid out or
/// prepainted.
#[derive(Default)]
struct Tree {
    laid: Vec<Laid>,
    stack: Vec<u32>,
}

thread_local! {
    static TREE: RefCell<Tree> = RefCell::default();
    /// The frame being drawn, per window (UI thread only).
    static BUILDING: RefCell<Vec<(WindowId, Building)>> = const { RefCell::new(Vec::new()) };
    /// A published frame's buffers nobody read, reused for the next one.
    static SPARE: RefCell<Option<Building>> = const { RefCell::new(None) };
}

/// A published frame: recorded nodes, completed on first read.
enum Published {
    Recorded(Vec<Node>, Vec<Option<bool>>),
    Built(Arc<Frame>),
}

/// Published frames; readable from any thread.
static FRAMES: LazyLock<Mutex<HashMap<WindowId, Published>>> = LazyLock::new(Mutex::default);

fn with_building<R>(window: WindowId, f: impl FnOnce(&mut Building) -> R) -> R {
    BUILDING.with_borrow_mut(|building| {
        let index = match building.iter().position(|(id, _)| *id == window) {
            Some(index) => index,
            None => {
                let spare = SPARE.take().unwrap_or_default();
                building.push((window, spare));
                building.len() - 1
            }
        };
        f(&mut building[index].1)
    })
}

/// `window`'s last complete frame.
pub fn frame(window: WindowId) -> Arc<Frame> {
    let mut frames = FRAMES.lock();
    let Some(published) = frames.get_mut(&window) else {
        return Arc::default();
    };
    if let Published::Recorded(nodes, explicit) = published {
        let frame = Frame::build(std::mem::take(nodes), explicit);
        *published = Published::Built(Arc::new(frame));
    }
    match published {
        Published::Built(frame) => frame.clone(),
        Published::Recorded(..) => unreachable!(),
    }
}

/// Makes `building` the window's last frame; keeps the previous frame's
/// buffers for the next one when nobody read it.
fn publish(window: WindowId, building: &mut Building) {
    let nodes = std::mem::take(&mut building.nodes);
    let explicit = std::mem::take(&mut building.explicit);
    let old = FRAMES
        .lock()
        .insert(window, Published::Recorded(nodes, explicit));
    if let Some(Published::Recorded(mut nodes, mut explicit)) = old {
        // Dropped outside the lock.
        nodes.clear();
        explicit.clear();
        SPARE.set(Some(Building {
            nodes,
            explicit,
            ..Building::default()
        }));
    }
}

/// Forgets a closed window (its frame, highlights and picker).
pub fn forget(window: WindowId) {
    FRAMES.lock().remove(&window);
    BUILDING.with_borrow_mut(|building| building.retain(|(id, _)| *id != window));
    highlight::forget(window);
    picker::forget(window);
}

/// Sets up the commands, the Escape handling, the request dispatcher and
/// the control server (once, at startup).
pub fn init(cx: &mut App) {
    register_actions(cx);
    picker::watch_keys(cx);
    cx.on_window_closed(|cx| {
        let open: Vec<WindowId> = cx.windows().iter().map(|w| w.window_id()).collect();
        let closed: Vec<WindowId> = FRAMES
            .lock()
            .keys()
            .filter(|id| !open.contains(id))
            .copied()
            .collect();
        for window in closed {
            forget(window);
        }
    })
    .detach();
    control::init(cx);
    server::start(cx);
}

/// "Pinpoint: Pick Element" (with its shortcut) and "Pinpoint: Clear
/// Highlights".
fn register_actions(cx: &mut App) {
    picker::bind_keys(cx);
    // Global actions run while their window is being updated: touch it once
    // that update is over.
    cx.on_action(|_: &picker::PickElement, cx| {
        if let Some(handle) = action_window(cx) {
            cx.defer(move |cx| {
                handle.update(cx, |_, window, _| picker::start(window)).ok();
            });
        }
    });
    cx.on_action(|_: &picker::ClearHighlights, cx| {
        if let Some(handle) = action_window(cx) {
            highlight::clear(Some(handle.window_id()), None);
            cx.defer(move |cx| {
                handle.update(cx, |_, window, _| window.refresh()).ok();
            });
        }
    });
}

/// The window a global action is for: the active one, else (no window
/// manager focus, as on a bare X display) the first.
fn action_window(cx: &App) -> Option<gpui::AnyWindowHandle> {
    cx.active_window()
        .or_else(|| cx.windows().into_iter().next())
}

/// The innermost enclosing pinned path, in request_layout and prepaint.
pub fn current_path() -> Option<SharedString> {
    TREE.with_borrow(|tree| {
        let mut at = *tree.stack.last()?;
        let mut ids = Vec::new();
        while let Some(laid) = tree.laid.get(at as usize) {
            ids.push(laid.id.as_ref());
            at = laid.parent;
        }
        ids.reverse();
        Some(ids.join("/").into())
    })
}

/// Records a node for a part painted without an element of its own (call
/// in prepaint). `path` is the full path, usually [`current_path`] plus
/// segments.
pub fn record(
    window: &Window,
    path: SharedString,
    label: Option<SharedString>,
    bounds: Bounds<Pixels>,
) {
    with_building(window.window_handle().window_id(), |building| {
        // Under the innermost enclosing node whose path starts `path`.
        let parent = enclosing_nodes().into_iter().find_map(|node| {
            let len = path_len(&building.nodes, node, &path)?;
            (path.as_bytes().get(len) == Some(&b'/')).then_some((node, len + 1))
        });
        let id = match parent {
            Some((_, start)) => path[start..].to_owned().into(),
            None => path,
        };
        push(
            building,
            window,
            id,
            parent.map(|(node, _)| node),
            label,
            None,
            None,
            bounds,
        );
    });
}

/// The nodes of the pinned elements being prepainted, innermost first.
fn enclosing_nodes() -> Vec<usize> {
    TREE.with_borrow(|tree| {
        tree.stack
            .iter()
            .rev()
            .filter_map(|&at| tree.laid.get(at as usize))
            .filter(|laid| laid.node != NONE)
            .map(|laid| laid.node as usize)
            .collect()
    })
}

/// The length of node `index`'s path when `path` starts with it.
fn path_len(nodes: &[Node], index: usize, path: &str) -> Option<usize> {
    let node = nodes.get(index)?;
    let start = match node.parent {
        Some(parent) => {
            let len = path_len(nodes, parent, path)?;
            (path.as_bytes().get(len) == Some(&b'/')).then_some(len + 1)?
        }
        None => 0,
    };
    path.get(start..)?
        .starts_with(node.id.as_ref())
        .then(|| start + node.id.len())
}

/// The node recorded this frame with `path`: usually one being prepainted,
/// else the last drawn.
fn find_building(nodes: &[Node], path: &str) -> Option<usize> {
    let is = |index: usize| path_len(nodes, index, path) == Some(path.len());
    enclosing_nodes()
        .into_iter()
        .find(|&index| is(index))
        .or_else(|| {
            (0..nodes.len())
                .rev()
                .find(|&index| path.ends_with(nodes[index].id.as_ref()) && is(index))
        })
}

/// Attaches readable text to the node `path` recorded in this frame (call in
/// prepaint, inside the node's element).
pub fn set_text(window: &Window, path: &str, text: SharedString) {
    with_building(window.window_handle().window_id(), |building| {
        if let Some(index) = find_building(&building.nodes, path) {
            building.nodes[index].text = Some(text);
        }
    });
}

/// Appends `text` to the node `path`'s text (after a line break when it has
/// some), for nodes showing several pieces of text; returns the byte offset
/// where `text` starts (`None`: no such node this frame). Prepaint only.
pub fn append_text(window: &Window, path: &str, text: &str) -> Option<usize> {
    with_building(window.window_handle().window_id(), |building| {
        let index = find_building(&building.nodes, path)?;
        let node = &mut building.nodes[index];
        let (joined, offset) = match &node.text {
            Some(old) if !old.is_empty() => (format!("{old}\n{text}"), old.len() + 1),
            _ => (text.to_owned(), 0),
        };
        node.text = Some(joined.into());
        Some(offset)
    })
}

/// Ranges of `path`'s text (see [`set_text`]) that highlights want, as byte
/// ranges; empty unless something is highlighted there.
pub fn wanted_text(window: &Window, path: &str) -> Vec<(HighlightId, Range<usize>)> {
    let id = window.window_handle().window_id();
    if !highlight::any(id) {
        return Vec::new();
    }
    let text = with_building(id, |building| {
        let index = find_building(&building.nodes, path)?;
        building.nodes[index].text.clone()
    });
    match text {
        Some(text) => highlight::wanted_text(id, path, &text),
        None => Vec::new(),
    }
}

/// Code ranges that highlights want in `window`.
pub fn wanted_code(window: &Window) -> Vec<(HighlightId, CodeWant)> {
    highlight::wanted_code(window.window_handle().window_id())
}

/// Terminal text that highlights want in `window`.
pub fn wanted_terminal(window: &Window) -> Vec<(HighlightId, TerminalWant)> {
    highlight::wanted_terminal(window.window_handle().window_id())
}

/// Where a wanted range is drawn (window coordinates, one rectangle per
/// line segment); `clipped` when part of it is scrolled away. Call in
/// prepaint.
pub fn report(window: &Window, id: HighlightId, rects: Vec<Bounds<Pixels>>, clipped: bool) {
    with_building(window.window_handle().window_id(), |building| {
        building.reports.push(Report { id, rects, clipped });
    });
}

/// The pointer, while the human picker runs in `window`.
pub fn picking(window: &Window) -> Option<Point<Pixels>> {
    picker::active(window.window_handle().window_id()).then(|| window.mouse_position())
}

/// The characters under the pointer while picking (call in prepaint); a
/// click picks them instead of the element.
pub fn report_pick(window: &Window, pick: PickText) {
    with_building(window.window_handle().window_id(), |building| {
        building.picks.push(pick);
    });
}

/// `label` as a path key: lowercase words joined by `-` ("Open Recent…" →
/// "open-recent").
pub fn kebab(label: &str) -> String {
    let mut key = String::with_capacity(label.len());
    for word in label
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        if !key.is_empty() {
            key.push('-');
        }
        key.extend(word.chars().flat_map(char::to_lowercase));
    }
    key
}

/// The key of a file or folder in paths: relative to its folder (after the
/// folder's name in multi-root projects), `/`-separated; a folder itself is
/// its name (multi-root) or `.`; outside every folder, the full path.
pub fn file_key(project: &Project, path: &Path) -> String {
    let roots = project.roots();
    match project.relative(path) {
        Some((index, relative)) if roots.len() > 1 => {
            format!("{}/{relative}", roots[index].name)
        }
        Some((_, relative)) => relative,
        None => match project.root_index_of(path) {
            Some(index) if roots.len() > 1 => roots[index].name.clone(),
            Some(_) => ".".to_owned(),
            None => path.to_string_lossy().replace('\\', "/"),
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn push(
    building: &mut Building,
    window: &Window,
    id: SharedString,
    parent: Option<usize>,
    label: Option<SharedString>,
    text: Option<SharedString>,
    interactive: Option<bool>,
    bounds: Bounds<Pixels>,
) -> usize {
    let visible = bounds.intersect(&window.content_mask().bounds);
    let visible = (visible.size.width > Pixels::ZERO && visible.size.height > Pixels::ZERO)
        .then_some(visible);
    building.nodes.push(Node {
        path: SharedString::default(),
        id,
        parent,
        depth: 0,
        label,
        text,
        bounds,
        visible,
        interactive: false,
    });
    building.explicit.push(interactive);
    building.nodes.len() - 1
}

pub trait Pin: IntoElement + Sized {
    /// Gives this element the path segment `id` (`name` or `kind:key`).
    fn pin(self, id: impl Into<SharedString>) -> Pinned {
        Pinned {
            id: id.into(),
            label: None,
            text: None,
            interactive: None,
            layout: NONE,
            child: self.into_any_element(),
        }
    }
}

impl<E: IntoElement> Pin for E {}

pub struct Pinned {
    id: SharedString,
    label: Option<SharedString>,
    text: Option<SharedString>,
    interactive: Option<bool>,
    /// Its entry in the frame's [`Tree`].
    layout: u32,
    child: AnyElement,
}

impl Pinned {
    /// The name a person sees, shown with the path when picking.
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// The text it shows (its caption), when known while building.
    pub fn text(mut self, text: impl Into<SharedString>) -> Self {
        self.text = Some(text.into());
        self
    }

    /// Whether it is something to click or type in (default: whether it has
    /// no pinned children).
    pub fn interactive(mut self, interactive: bool) -> Self {
        self.interactive = Some(interactive);
        self
    }
}

impl IntoElement for Pinned {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for Pinned {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        self.layout = TREE.with_borrow_mut(|tree| {
            let at = tree.laid.len() as u32;
            let parent = tree.stack.last().copied().unwrap_or(NONE);
            tree.laid.push(Laid {
                id: self.id.clone(),
                parent,
                node: NONE,
            });
            tree.stack.push(at);
            at
        });
        let layout = self.child.request_layout(window, cx);
        TREE.with_borrow_mut(|tree| tree.stack.pop());
        (layout, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let parent = TREE.with_borrow(|tree| {
            let laid = tree.laid.get(self.layout as usize)?;
            let node = tree.laid.get(laid.parent as usize)?.node;
            (node != NONE).then_some(node as usize)
        });
        // Painted once per frame: the node takes what it needs.
        let node = with_building(window.window_handle().window_id(), |building| {
            push(
                building,
                window,
                std::mem::take(&mut self.id),
                parent,
                self.label.take(),
                self.text.take(),
                self.interactive,
                bounds,
            )
        });
        let pushed = TREE.with_borrow_mut(|tree| {
            let laid = tree.laid.get_mut(self.layout as usize)?;
            laid.node = node as u32;
            tree.stack.push(self.layout);
            Some(())
        });
        self.child.prepaint(window, cx);
        if pushed.is_some() {
            TREE.with_borrow_mut(|tree| tree.stack.pop());
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

/// Drawn after everything else in the window (popovers included): publishes
/// the frame's nodes and paints highlights and the picker. Put it once at
/// the window's root.
pub fn layer() -> impl IntoElement {
    deferred(Publish).with_priority(LAYER_PRIORITY)
}

/// Above every other deferred draw in the app.
const LAYER_PRIORITY: usize = 1_000_000;

struct Publish;

impl IntoElement for Publish {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for Publish {
    type RequestLayoutState = ();
    type PrepaintState = Option<overlay::Overlay>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (div().into_any_element().request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Option<overlay::Overlay> {
        let id = window.window_handle().window_id();
        let mut building = BUILDING.with_borrow_mut(|building| {
            let index = building.iter().position(|(window, _)| *window == id);
            let ours = index.map(|index| building.swap_remove(index).1);
            // Windows without this layer never publish: drop their nodes.
            building.clear();
            ours.unwrap_or_default()
        });
        // Every pinned element of the frame is drawn.
        TREE.with_borrow_mut(|tree| tree.laid.clear());
        publish(id, &mut building);
        overlay::prepaint(id, building.reports, building.picks, window, cx)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        overlay: &mut Option<overlay::Overlay>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(overlay) = overlay {
            overlay.paint(window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kebab_keys() {
        assert_eq!(kebab("Open Recent…"), "open-recent");
        assert_eq!(kebab("Commit and Push…"), "commit-and-push");
        assert_eq!(kebab("  Run  All! "), "run-all");
        assert_eq!(kebab("Go to Line/Column"), "go-to-line-column");
        assert_eq!(kebab("…"), "");
    }

    use std::{cell::Cell, rc::Rc};

    use gpui::{
        Context, InteractiveElement, Modifiers, ParentElement, Render, StatefulInteractiveElement,
        Styled, TestAppContext, VisualTestContext, point, px, uniform_list,
    };

    /// A menu with a button, a popover drawn through `deferred` and a lazy
    /// list, all pinned.
    struct View {
        clicks: Rc<Cell<usize>>,
    }

    impl Render for View {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let clicks = self.clicks.clone();
            div()
                .size_full()
                .child(
                    div()
                        .w(px(300.))
                        .h(px(300.))
                        .child(
                            div()
                                .id("button")
                                .debug_selector(|| "button".into())
                                .w(px(60.))
                                .h(px(20.))
                                .on_click(move |_, _, _| clicks.set(clicks.get() + 1))
                                .pin("button")
                                .label("Press"),
                        )
                        .child(
                            uniform_list("rows", 50, |range, _, _| {
                                range
                                    .map(|i| div().h(px(20.)).pin(format!("row:dir/{i}")))
                                    .collect()
                            })
                            .h(px(100.))
                            .pin("list"),
                        )
                        .child(deferred(
                            div()
                                .absolute()
                                .top(px(200.))
                                .w(px(80.))
                                .h(px(40.))
                                .child(div().size_full().pin("item:open-recent"))
                                .pin("popup"),
                        ))
                        .pin("menu"),
                )
                .child(layer())
        }
    }

    fn open(cx: &mut TestAppContext) -> (Rc<Cell<usize>>, &mut VisualTestContext, WindowId) {
        let clicks = Rc::new(Cell::new(0));
        let view = View {
            clicks: clicks.clone(),
        };
        cx.update(|cx| {
            register_actions(cx);
            picker::watch_keys(cx);
        });
        let (_, cx) = cx.add_window_view(|_, _| view);
        cx.run_until_parked();
        let id = cx.update(|window, _| window.window_handle().window_id());
        (clicks, cx, id)
    }

    fn redraw(cx: &mut VisualTestContext) {
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn paths_nest_through_popovers_and_lazy_lists(cx: &mut TestAppContext) {
        let (_, _, id) = open(cx);
        let frame = frame(id);
        let paths: Vec<&str> = frame.nodes.iter().map(|n| n.path.as_ref()).collect();
        assert_eq!(
            paths,
            [
                "menu",
                "menu/button",
                "menu/list",
                "menu/list/row:dir/0",
                "menu/list/row:dir/1",
                "menu/list/row:dir/2",
                "menu/list/row:dir/3",
                "menu/list/row:dir/4",
                // Drawn last, still under its owner.
                "menu/popup",
                "menu/popup/item:open-recent",
            ]
        );
        let node = |path: &str| &frame.nodes[frame.find(path).unwrap()];
        let parent = |path: &str| node(path).parent.map(|i| frame.nodes[i].path.as_ref());
        assert_eq!(parent("menu/list/row:dir/3"), Some("menu/list"));
        assert_eq!(node("menu/list/row:dir/3").id.as_ref(), "row:dir/3");
        assert_eq!(parent("menu/popup"), Some("menu"));
        assert_eq!(parent("menu/popup/item:open-recent"), Some("menu/popup"));
        assert_eq!(node("menu/popup/item:open-recent").depth, 2);
        assert_eq!(
            node("menu/button").label.as_ref().map(|l| l.as_ref()),
            Some("Press")
        );
        assert!(node("menu/button").interactive);
        assert!(!node("menu").interactive);
        assert_eq!(
            node("menu/list/row:dir/1").bounds.origin,
            point(px(0.), px(40.))
        );
    }

    #[gpui::test]
    fn clicks_reach_pinned_elements(cx: &mut TestAppContext) {
        let (clicks, cx, _) = open(cx);
        let button = cx.debug_bounds("button").expect("the button renders");
        cx.simulate_click(button.center(), Modifiers::none());
        assert_eq!(clicks.get(), 1);
        redraw(cx);
        cx.simulate_click(button.center(), Modifiers::none());
        assert_eq!(clicks.get(), 2);
    }

    #[gpui::test]
    fn the_picker_pins_the_innermost_element_instead_of_clicking(cx: &mut TestAppContext) {
        let (clicks, cx, _) = open(cx);
        let button = cx.debug_bounds("button").expect("the button renders");
        let since = picker::last_seq();
        // The command (as the palette runs it).
        cx.dispatch_action(picker::PickElement);
        redraw(cx);
        cx.simulate_mouse_move(button.center(), None, Modifiers::none());
        cx.simulate_click(button.center(), Modifiers::none());
        assert_eq!(clicks.get(), 0, "the click picks");
        let pins = picker::pins(false);
        let pin = pins.last().unwrap();
        assert!(pin["seq"].as_u64().unwrap() > since);
        assert_eq!(pin["path"], "menu/button");
        assert_eq!(pin["reference"], "nextcode://ui/menu/button");
        assert_eq!(pin["label"], "Press");
        // One pick, then normal clicks again.
        let id = cx.update(|window, _| window.window_handle().window_id());
        assert!(!picker::active(id));
        redraw(cx);
        cx.simulate_click(button.center(), Modifiers::none());
        assert_eq!(clicks.get(), 1);
        // The shortcut; Escape leaves pick mode without picking.
        cx.simulate_keystrokes("ctrl-alt-shift-p");
        redraw(cx);
        assert!(picker::active(id));
        cx.simulate_keystrokes("escape");
        assert!(!picker::active(id));
    }

    #[gpui::test]
    fn escape_dismisses_highlights(cx: &mut TestAppContext) {
        let (_, cx, id) = open(cx);
        highlight::add(highlight::Highlight {
            id: highlight::next_id(),
            window: id,
            target: target::Target::Element {
                path: "menu/button".into(),
            },
            style: highlight::Style::Outline,
            color: None,
            badge: Some(1),
            note: Some("Press here".into()),
            created: std::time::Instant::now(),
            pulse: false,
            until: None,
        });
        redraw(cx);
        assert!(highlight::any(id));
        cx.simulate_keystrokes("escape");
        assert!(!highlight::any(id));
    }
}
