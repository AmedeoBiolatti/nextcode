//! Selectable static text: labels, messages and error text that can be
//! selected with the mouse and copied, like text on a web page.
//!
//! [`selectable`] renders a string exactly as a plain string child would
//! (inherited text style, `\n` line breaks, wrapping, `truncate()`), plus:
//! drag to select (also past the element's edges), double-click selects a
//! word, triple-click all of the element; `Ctrl+C` / `Ctrl+Insert` (and
//! Edit > Copy) copy, `Ctrl+A` selects the whole element, `Escape` or a
//! press anywhere else clears; right-click opens Copy / Copy All. There is
//! one selection per application, inside one element: no selection across
//! elements.
//!
//! A plain click (press and release without moving) passes through, so a
//! parent's `on_click` / `on_mouse_down` still runs (rows that navigate,
//! banners that go to a location). A drag or a double/triple click is a
//! selection: its release does not reach the parent's `on_click`, and
//! pointer moves during any press on the text do not reach the parents.
//! Selecting moves keyboard focus to the selection (as a browser does), so
//! `Ctrl+C` copies it instead of an editor's selection; Escape, or the
//! text going away (a popover closing), returns focus where it was.
//!
//! # How to convert a site
//!
//! - A plain text label: `.child(text)` → `.child(selectable(id, text))`;
//!   `StyledText::new(text).with_highlights(h)` →
//!   `selectable(id, text).with_highlights(h)`. Styling (color, size,
//!   `truncate()`, `whitespace_nowrap()`) stays on the parent div.
//! - `id` must be unique among the element's siblings with ids under the
//!   same parent chain: use a fixed name, or `(name, index)` / a key in
//!   lists.
//! - A multi-line block (output lines, a message with details) is ONE
//!   element with `\n`-joined text, so a selection can span its lines.
//! - Not for: buttons, menu items, tab titles, tooltips, inputs, the
//!   editor or terminal (they have their own selection), drag sources (a
//!   drag on the text selects instead of dragging), pan canvases (same),
//!   and centered/right-aligned text (selection rectangles assume left
//!   alignment). Right-click on the text opens the text menu, not a
//!   parent's context menu, unless the site opts into
//!   [`SelectableText::right_click_through`].
//! - Rows whose double-click has an action (open a diff, expand, edit,
//!   run): opt into [`SelectableText::double_click_through`], so a double-
//!   or triple-click on the text reaches the row's handlers unchanged
//!   instead of selecting a word / all; drag-select and copy still work.
//!   Don't add separate double-click handlers around the text.
//! - Windows render the Copy / Copy All menu through [`context_menu`]
//!   (the [`crate::shell::Shell`] root does).

use std::{
    any::TypeId,
    hash::{DefaultHasher, Hash, Hasher},
    ops::Range,
};

use gpui::{
    AnyElement, App, Bounds, ClipboardItem, CursorStyle, DispatchPhase, ElementId, Entity,
    FocusHandle, Global, GlobalElementId, HighlightStyle, Hitbox, HitboxBehavior,
    InspectorElementId, KeyBinding, KeyContext, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, SharedString, StyledText, Subscription,
    TextLayout, WeakFocusHandle, Window, WindowId, actions, anchored, deferred, div, fill, point,
    prelude::*, px,
};

use crate::{
    editor::{Copy, SelectAll},
    theme,
};

actions!(selectable, [ClearSelection]);

const CONTEXT: &str = "SelectableText";
/// Pointer travel before a press on the text becomes a selection drag.
const DRAG_THRESHOLD: f32 = 3.;

pub fn bind_keys(cx: &mut App) {
    let ctx = Some(CONTEXT);
    cx.bind_keys([
        // The last binding is the one menus show.
        KeyBinding::new("ctrl-insert", Copy, ctx),
        KeyBinding::new("secondary-c", Copy, ctx),
        KeyBinding::new("secondary-a", SelectAll, ctx),
        KeyBinding::new("escape", ClearSelection, ctx),
    ]);
}

/// `text` as selectable, copyable static text; see the module docs.
pub fn selectable(id: impl Into<ElementId>, text: impl Into<SharedString>) -> SelectableText {
    let text = text.into();
    SelectableText {
        id: id.into(),
        styled: StyledText::new(text.clone()),
        text,
        right_click_through: false,
        double_click_through: false,
    }
}

pub struct SelectableText {
    id: ElementId,
    text: SharedString,
    styled: StyledText,
    right_click_through: bool,
    double_click_through: bool,
}

impl SelectableText {
    /// Styles byte ranges of the text, as [`StyledText::with_highlights`].
    pub fn with_highlights(
        mut self,
        highlights: impl IntoIterator<Item = (Range<usize>, HighlightStyle)>,
    ) -> Self {
        self.styled = self.styled.with_highlights(highlights);
        self
    }

    /// A right-click on the text opens the parent's context menu (it
    /// passes through) unless part of the text is selected, which opens
    /// Copy / Copy All. For rows with their own context menu.
    pub fn right_click_through(mut self) -> Self {
        self.right_click_through = true;
        self
    }

    /// A double- or triple-click on the text passes through to the parent
    /// as a plain press (the parent's `on_click` sees `click_count` 2, its
    /// mouse-down handlers the press), instead of selecting a word / all.
    /// Drag-select and copy still work. For rows whose double-click opens,
    /// expands or edits something.
    pub fn double_click_through(mut self) -> Self {
        self.double_click_through = true;
        self
    }
}

/// An element in a window, by its global element id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Owner {
    window: WindowId,
    element: u64,
}

impl Owner {
    fn new(window: &Window, id: Option<&GlobalElementId>) -> Self {
        let mut hasher = DefaultHasher::new();
        id.hash(&mut hasher);
        Self {
            window: window.window_handle().window_id(),
            element: hasher.finish(),
        }
    }
}

/// A left press on the text, until its release.
struct Press {
    owner: Owner,
    index: usize,
    origin: Point<Pixels>,
    /// Dragged past the threshold, or a double/triple click.
    selecting: bool,
}

/// Byte offsets into the owner's text; the head follows the pointer.
struct Selection {
    owner: Owner,
    anchor: usize,
    head: usize,
}

struct Menu {
    window: WindowId,
    position: Point<Pixels>,
    /// The selected text when the menu opened.
    selected: Option<String>,
    all: SharedString,
}

#[derive(Default)]
struct State {
    /// Focused while there is a selection, so its key bindings apply.
    focus: Option<FocusHandle>,
    /// What had focus before the selection took it; Escape returns there.
    restore: Option<WeakFocusHandle>,
    press: Option<Press>,
    selection: Option<Selection>,
    menu: Option<Menu>,
    /// Clears the selection when its text leaves the window (a popover
    /// closed, a row went away), so focus is not stranded on a handle no
    /// element renders; for the window the focus was last taken in.
    owner_gone: Option<(WindowId, Entity<FocusWatcher>, Subscription)>,
}

impl Global for State {}

/// Holds the focus-lost subscription (gpui ties it to an entity).
struct FocusWatcher;

fn focus_handle(cx: &mut App) -> FocusHandle {
    if let Some(focus) = cx
        .try_global::<State>()
        .and_then(|state| state.focus.clone())
    {
        return focus;
    }
    let focus = cx.focus_handle();
    cx.default_global::<State>().focus = Some(focus.clone());
    focus
}

fn take_focus(window: &mut Window, cx: &mut App) {
    let focus = focus_handle(cx);
    if !focus.is_focused(window) {
        let previous = window.focused(cx).map(|previous| previous.downgrade());
        cx.default_global::<State>().restore = previous;
        window.focus(&focus);
    }
    let window_id = window.window_handle().window_id();
    let watching = cx
        .default_global::<State>()
        .owner_gone
        .as_ref()
        .is_some_and(|(id, _, _)| *id == window_id);
    if !watching {
        let watcher = cx.new(|_| FocusWatcher);
        let subscription = watcher.update(cx, |_, cx| {
            cx.on_focus_lost(window, |_, window, cx| {
                let ours = cx
                    .try_global::<State>()
                    .and_then(|state| state.focus.as_ref())
                    .is_some_and(|focus| focus.is_focused(window));
                if ours {
                    clear(window, cx);
                }
            })
        });
        cx.default_global::<State>().owner_gone = Some((window_id, watcher, subscription));
    }
}

/// Escape: no selection or menu, and focus back where it was.
fn clear(window: &mut Window, cx: &mut App) {
    let state = cx.default_global::<State>();
    state.selection = None;
    state.menu = None;
    if let Some(previous) = state.restore.take().and_then(|weak| weak.upgrade()) {
        window.focus(&previous);
    }
    window.refresh();
}

pub struct Prepainted {
    hitbox: Hitbox,
    owner: Owner,
}

impl Element for SelectableText {
    type RequestLayoutState = ();
    type PrepaintState = Prepainted;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        self.styled.request_layout(None, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepainted {
        self.styled
            .prepaint(None, inspector_id, bounds, request_layout, window, cx);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let owner = Owner::new(window, global_id);
        // The selection's focus handle lives in this element's dispatch
        // node while it holds the selection, and already while it may start
        // one (pressed or under the pointer): focusing it then keeps the
        // ancestors' `contains_focused` true in the next render, so
        // focus-gated parents (the editor's hover popover) don't drop the
        // text being selected.
        let state = cx.try_global::<State>();
        let register = match state.and_then(|state| state.selection.as_ref()) {
            Some(selection) => selection.owner == owner,
            None => {
                state
                    .and_then(|state| state.press.as_ref())
                    .is_some_and(|press| press.owner == owner)
                    || bounds.contains(&window.mouse_position())
            }
        };
        if register {
            let focus = focus_handle(cx);
            window.set_focus_handle(&focus, cx);
        }
        Prepainted { hitbox, owner }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut (),
        prepainted: &mut Prepainted,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Prepainted { hitbox, owner } = prepainted;
        let owner = *owner;
        let layout = self.styled.layout().clone();
        window.set_cursor_style(CursorStyle::IBeam, hitbox);
        let (selected, pressed) = match cx.try_global::<State>() {
            Some(state) => (
                state
                    .selection
                    .as_ref()
                    .filter(|s| s.owner == owner)
                    .map(|s| clamp(&self.text, ordered(s.anchor, s.head))),
                state.press.as_ref().is_some_and(|p| p.owner == owner),
            ),
            None => (None, false),
        };

        if let Some(range) = selected {
            let mut context = KeyContext::default();
            context.add(CONTEXT);
            window.set_key_context(context);
            paint_selection(&layout, range, window);
            let text = self.text.clone();
            window.on_action(TypeId::of::<Copy>(), move |_, phase, _, cx| {
                if phase == DispatchPhase::Bubble {
                    copy_selection(owner, &text, cx);
                }
            });
            let len = self.text.len();
            window.on_action(TypeId::of::<SelectAll>(), move |_, phase, window, cx| {
                if phase == DispatchPhase::Bubble
                    && let Some(selection) = cx.default_global::<State>().selection.as_mut()
                {
                    selection.anchor = 0;
                    selection.head = len;
                    window.refresh();
                }
            });
            window.on_action(TypeId::of::<ClearSelection>(), |_, phase, window, cx| {
                if phase == DispatchPhase::Bubble {
                    clear(window, cx);
                }
            });
            // A press anywhere else (including the menu) ends the selection.
            let hitbox = hitbox.clone();
            window.on_mouse_event(move |_: &MouseDownEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture && !hitbox.is_hovered(window) {
                    let state = cx.default_global::<State>();
                    if state.selection.as_ref().is_some_and(|s| s.owner == owner) {
                        state.selection = None;
                        window.refresh();
                    }
                }
            });
        }

        self.styled.paint(
            None,
            inspector_id,
            bounds,
            request_layout,
            &mut (),
            window,
            cx,
        );

        {
            let hitbox = hitbox.clone();
            let layout = layout.clone();
            let text = self.text.clone();
            let right_click_through = self.right_click_through;
            let double_click_through = self.double_click_through;
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                    return;
                }
                match event.button {
                    MouseButton::Left => press(
                        owner,
                        &layout,
                        &text,
                        event,
                        double_click_through,
                        window,
                        cx,
                    ),
                    MouseButton::Right => {
                        if right_click_through && !has_selection(owner, cx) {
                            return;
                        }
                        open_menu(owner, &layout, &text, event, window, cx)
                    }
                    _ => {}
                }
            });
        }

        if pressed {
            let text = self.text.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble {
                    drag(owner, &layout, &text, event, window, cx);
                }
            });
            window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                    return;
                }
                let state = cx.default_global::<State>();
                if let Some(press) = state.press.take_if(|p| p.owner == owner) {
                    // A selection is not a click on what is under it.
                    if press.selecting {
                        cx.stop_propagation();
                    }
                    window.refresh();
                }
            });
        }
    }
}

impl IntoElement for SelectableText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

fn press(
    owner: Owner,
    layout: &TextLayout,
    text: &str,
    event: &MouseDownEvent,
    double_click_through: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let state = cx.default_global::<State>();
    if event.click_count <= 1 || double_click_through {
        if state.selection.as_ref().is_some_and(|s| s.owner == owner) {
            state.selection = None;
        }
        state.press = Some(Press {
            owner,
            index: index_at(layout, text, event.position, true),
            origin: event.position,
            selecting: false,
        });
    } else {
        let range = if event.click_count == 2 {
            word_at(text, index_at(layout, text, event.position, false))
        } else {
            0..text.len()
        };
        state.selection = Some(Selection {
            owner,
            anchor: range.start,
            head: range.end,
        });
        state.press = Some(Press {
            owner,
            index: range.start,
            origin: event.position,
            selecting: true,
        });
        // After the event: a parent's mouse-down handler may focus its own
        // handle meanwhile (a panel focusing itself on any press).
        window.defer(cx, take_focus);
        // Keeps a focusable parent from taking focus back.
        window.prevent_default();
    }
    window.refresh();
}

fn drag(
    owner: Owner,
    layout: &TextLayout,
    text: &str,
    event: &MouseMoveEvent,
    window: &mut Window,
    cx: &mut App,
) {
    let state = cx.default_global::<State>();
    let Some(press) = state.press.as_mut().filter(|p| p.owner == owner) else {
        return;
    };
    // Released outside the window.
    if event.pressed_button != Some(MouseButton::Left) {
        state.press = None;
        window.refresh();
        return;
    }
    let head = index_at(layout, text, event.position, true);
    if press.selecting {
        if let Some(selection) = state.selection.as_mut().filter(|s| s.owner == owner)
            && selection.head != head
        {
            selection.head = head;
            window.refresh();
        }
    } else if (event.position - press.origin).magnitude() >= f64::from(DRAG_THRESHOLD) {
        press.selecting = true;
        state.selection = Some(Selection {
            owner,
            anchor: press.index,
            head,
        });
        take_focus(window, cx);
        window.refresh();
    }
    // Parents must not start their own drags or pans meanwhile.
    cx.stop_propagation();
}

fn open_menu(
    owner: Owner,
    layout: &TextLayout,
    text: &SharedString,
    event: &MouseDownEvent,
    window: &mut Window,
    cx: &mut App,
) {
    let state = cx.default_global::<State>();
    let range = match state.selection.as_ref().filter(|s| s.owner == owner) {
        Some(selection) => clamp(text, ordered(selection.anchor, selection.head)),
        None => {
            // An empty selection here: the menu's keys (Escape) reach it.
            let index = index_at(layout, text, event.position, true);
            state.selection = Some(Selection {
                owner,
                anchor: index,
                head: index,
            });
            index..index
        }
    };
    state.menu = Some(Menu {
        window: owner.window,
        position: event.position,
        selected: (!range.is_empty()).then(|| text[range].to_owned()),
        all: text.clone(),
    });
    take_focus(window, cx);
    window.prevent_default();
    cx.stop_propagation();
    window.refresh();
}

/// Whether `owner` holds a non-empty selection.
fn has_selection(owner: Owner, cx: &App) -> bool {
    cx.try_global::<State>()
        .and_then(|state| state.selection.as_ref())
        .is_some_and(|s| s.owner == owner && s.anchor != s.head)
}

fn copy_selection(owner: Owner, text: &str, cx: &mut App) {
    let Some(selection) = cx
        .try_global::<State>()
        .and_then(|state| state.selection.as_ref())
        .filter(|s| s.owner == owner)
    else {
        return;
    };
    let selected = &text[clamp(text, ordered(selection.anchor, selection.head))];
    if !selected.is_empty() {
        cx.write_to_clipboard(ClipboardItem::new_string(selected.to_owned()));
    }
}

/// The Copy / Copy All menu opened by a right-click on selectable text in
/// `window`, drawn over everything; the window's root renders it.
pub fn context_menu(window: &Window, cx: &App) -> Option<AnyElement> {
    let menu = cx.try_global::<State>()?.menu.as_ref()?;
    if menu.window != window.window_handle().window_id() {
        return None;
    }
    let item = |id: &'static str, label: &'static str, text: Option<SharedString>| {
        div()
            .id(id)
            .debug_selector(|| id.to_owned())
            .px_3()
            .py_1()
            .rounded_sm()
            .child(label)
            .map(|item| match text {
                Some(text) => item
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::active_row()))
                    .on_click(move |_, window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
                        cx.default_global::<State>().menu = None;
                        window.refresh();
                    }),
                None => item.text_color(theme::muted()),
            })
    };
    Some(
        deferred(
            anchored().position(menu.position).child(
                div()
                    .id("selectable-menu")
                    .occlude()
                    .min_w(px(160.))
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
                    .on_mouse_down_out(|_, window, cx| {
                        cx.default_global::<State>().menu = None;
                        window.refresh();
                    })
                    .child(item(
                        "selectable-copy",
                        "Copy",
                        menu.selected.clone().map(SharedString::from),
                    ))
                    .child(item(
                        "selectable-copy-all",
                        "Copy All",
                        Some(menu.all.clone()),
                    )),
            ),
        )
        .with_priority(2)
        .into_any_element(),
    )
}

/// Paints the selection behind `range`'s text, row by wrapped row: from
/// the start to the right edge, whole rows, then up to the end.
fn paint_selection(layout: &TextLayout, range: Range<usize>, window: &mut Window) {
    let len = layout.len();
    let (start, end) = (range.start.min(len), range.end.min(len));
    if start >= end {
        return;
    }
    let (Some(from), Some(to)) = (
        layout.position_for_index(start),
        layout.position_for_index(end),
    ) else {
        return;
    };
    let bounds = layout.bounds();
    let line_height = layout.line_height();
    let color = theme::selection();
    let mut rect = |a: Point<Pixels>, b: Point<Pixels>| {
        if b.x > a.x && b.y > a.y {
            window.paint_quad(fill(Bounds::from_corners(a, b), color));
        }
    };
    if from.y == to.y {
        rect(from, point(to.x, to.y + line_height));
    } else {
        rect(from, point(bounds.right(), from.y + line_height));
        rect(
            point(bounds.left(), from.y + line_height),
            point(bounds.right(), to.y),
        );
        rect(point(bounds.left(), to.y), point(to.x, to.y + line_height));
    }
}

/// The text offset under `position`: the nearest character boundary when
/// `closest` (placing a selection end), else the start of the character
/// under the pointer (picking a word). Outside the text: the nearest end
/// of the row, or of the text above/below it.
fn index_at(layout: &TextLayout, text: &str, position: Point<Pixels>, closest: bool) -> usize {
    let index = match layout.index_for_position(position) {
        Err(index) => index,
        Ok(index) if !closest => index,
        Ok(index) => {
            let next = next_boundary(text, index);
            match (
                layout.position_for_index(index),
                layout.position_for_index(next),
            ) {
                (Some(here), Some(there))
                    if here.y == there.y && position.x > (here.x + there.x) / 2. =>
                {
                    next
                }
                _ => index,
            }
        }
    };
    floor_boundary(text, index)
}

fn ordered(anchor: usize, head: usize) -> Range<usize> {
    anchor.min(head)..anchor.max(head)
}

/// `range` within `text`, on character boundaries (the text may have
/// changed since the selection was made).
fn clamp(text: &str, range: Range<usize>) -> Range<usize> {
    floor_boundary(text, range.start)..floor_boundary(text, range.end)
}

fn floor_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn next_boundary(text: &str, index: usize) -> usize {
    let index = floor_boundary(text, index);
    text[index..]
        .chars()
        .next()
        .map_or(index, |ch| index + ch.len_utf8())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Word,
    Space,
    Newline,
    Other,
}

fn class(ch: char) -> CharClass {
    if ch == '\n' {
        CharClass::Newline
    } else if ch.is_whitespace() {
        CharClass::Space
    } else if ch.is_alphanumeric() || ch == '_' {
        CharClass::Word
    } else {
        CharClass::Other
    }
}

/// What a double-click at `index` selects: the run of word characters
/// (letters, digits, `_`), spaces or punctuation around the character
/// there (or before it, at the end of the text); a line break alone.
fn word_at(text: &str, index: usize) -> Range<usize> {
    let index = floor_boundary(text, index);
    let (at, ch) = match text[index..].chars().next() {
        Some(ch) => (index, ch),
        None => match text[..index].chars().next_back() {
            Some(ch) => (index - ch.len_utf8(), ch),
            None => return index..index,
        },
    };
    let kind = class(ch);
    if kind == CharClass::Newline {
        return at..at + 1;
    }
    let start = text[..at]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| class(c) == kind)
        .last()
        .map_or(at, |(i, _)| i);
    let end = text[at..]
        .char_indices()
        .find(|&(_, c)| class(c) != kind)
        .map_or(text.len(), |(i, _)| at + i);
    start..end
}

#[cfg(test)]
mod tests {
    use gpui::{Context, Entity, Modifiers, TestAppContext, VisualTestContext, size};

    use super::*;

    #[test]
    fn double_click_picks_words_spaces_and_punctuation() {
        let text = "error[E0308]: mismatched  types\nnext";
        assert_eq!(&text[word_at(text, 0)], "error");
        assert_eq!(&text[word_at(text, 3)], "error");
        assert_eq!(&text[word_at(text, 7)], "E0308");
        assert_eq!(&text[word_at(text, 5)], "[");
        assert_eq!(&text[word_at(text, 11)], "]:");
        assert_eq!(&text[word_at(text, 25)], "  ");
        assert_eq!(&text[word_at(text, 31)], "\n");
        assert_eq!(&text[word_at(text, 32)], "next");
        // At the end: the word before.
        assert_eq!(&text[word_at(text, text.len())], "next");
        assert_eq!(word_at("", 0), 0..0);
        // snake_case and digits are one word.
        assert_eq!(&"a foo_bar2 b"[word_at("a foo_bar2 b", 6)], "foo_bar2");
    }

    #[test]
    fn words_and_ranges_respect_multibyte_text() {
        let text = "naïve café — ünïcode";
        // Inside 'ï' (2 bytes): floors to its start.
        assert_eq!(&text[word_at(text, 3)], "naïve");
        assert_eq!(&text[word_at(text, 8)], "café");
        assert_eq!(&text[word_at(text, 13)], "—");
        assert_eq!(&text[word_at(text, text.len() - 1)], "ünïcode");
        // Clamped to boundaries and length, either order.
        let range = clamp(text, ordered(14, 3));
        assert_eq!(&text[range], "ïve café ");
        assert_eq!(clamp(text, 0..999), 0..text.len());
        assert_eq!(next_boundary(text, 2), 4);
        assert_eq!(next_boundary(text, text.len()), text.len());
        assert_eq!(floor_boundary("日本", 4), 3);
    }

    struct Row {
        clicks: usize,
        double_clicks: usize,
        double_click_through: bool,
    }

    impl Render for Row {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let text = selectable("message", "hello wörld\nsecond line");
            let text = if self.double_click_through {
                text.double_click_through()
            } else {
                text
            };
            div()
                .size_full()
                .text_size(px(10.))
                .line_height(px(16.))
                .child(
                    div()
                        .id("row")
                        .on_click(cx.listener(|this, event: &gpui::ClickEvent, _, _| {
                            this.clicks += 1;
                            if event.click_count() == 2 {
                                this.double_clicks += 1;
                            }
                        }))
                        .child(text),
                )
                .children(context_menu(window, cx))
        }
    }

    /// The row's view and a window context; the test text system draws
    /// every character 0.6 em (6 px) wide, rows are 16 px high.
    fn row(cx: &mut TestAppContext) -> (Entity<Row>, VisualTestContext) {
        row_with(cx, false)
    }

    fn row_with(
        cx: &mut TestAppContext,
        double_click_through: bool,
    ) -> (Entity<Row>, VisualTestContext) {
        cx.update(bind_keys);
        let window = cx.add_window(|_, _| Row {
            clicks: 0,
            double_clicks: 0,
            double_click_through,
        });
        let view = window.root(cx).unwrap();
        let cx = VisualTestContext::from_window(window.into(), cx);
        cx.simulate_resize(size(px(400.), px(200.)));
        cx.run_until_parked();
        (view, cx)
    }

    fn double_click(cx: &mut VisualTestContext, position: Point<Pixels>) {
        for click_count in [1, 2] {
            cx.simulate_event(MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::none(),
                click_count,
                first_mouse: false,
            });
            cx.simulate_event(MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::none(),
                click_count,
            });
        }
    }

    fn at(column: f32, row: f32) -> Point<Pixels> {
        point(px(column * 6.), px(row * 16. + 8.))
    }

    fn clipboard(cx: &mut VisualTestContext) -> Option<String> {
        cx.read_from_clipboard().and_then(|item| item.text())
    }

    #[gpui::test]
    fn a_drag_selects_and_copies_and_a_click_still_reaches_the_parent(cx: &mut TestAppContext) {
        let (view, mut cx) = row(cx);
        let cx = &mut cx;
        // A plain click passes through to the row.
        cx.simulate_click(at(2., 0.), Modifiers::none());
        assert_eq!(view.read_with(cx, |row, _| row.clicks), 1);

        // Drag from "wö|rld" on row 0 to "sec|ond" on row 1.
        cx.simulate_mouse_down(at(8., 0.), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(at(9.2, 0.), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(at(3.2, 1.), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(at(3.2, 1.), MouseButton::Left, Modifiers::none());
        assert_eq!(
            view.read_with(cx, |row, _| row.clicks),
            1,
            "a drag is no click"
        );
        cx.simulate_keystrokes("ctrl-c");
        assert_eq!(clipboard(cx).as_deref(), Some("rld\nsec"));

        // Dragging past the element's end selects to the end of the text.
        cx.simulate_mouse_down(at(0., 1.), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(at(50., 5.), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(at(50., 5.), MouseButton::Left, Modifiers::none());
        cx.simulate_keystrokes("ctrl-insert");
        assert_eq!(clipboard(cx).as_deref(), Some("second line"));

        // Escape clears: Ctrl+C copies nothing more.
        cx.simulate_keystrokes("escape");
        cx.write_to_clipboard(ClipboardItem::new_string("before".into()));
        cx.simulate_keystrokes("ctrl-c");
        assert_eq!(clipboard(cx).as_deref(), Some("before"));
    }

    #[gpui::test]
    fn double_click_selects_a_word_and_right_click_copies_all(cx: &mut TestAppContext) {
        let (view, mut cx) = row(cx);
        let cx = &mut cx;
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: at(7.5, 0.),
            modifiers: Modifiers::none(),
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position: at(7.5, 0.),
            modifiers: Modifiers::none(),
            click_count: 2,
        });
        cx.simulate_keystrokes("ctrl-c");
        assert_eq!(clipboard(cx).as_deref(), Some("wörld"));
        assert_eq!(view.read_with(cx, |row, _| row.clicks), 0);

        // Right-click, then Copy All from the menu.
        cx.simulate_mouse_down(at(1., 1.), MouseButton::Right, Modifiers::none());
        cx.simulate_mouse_up(at(1., 1.), MouseButton::Right, Modifiers::none());
        cx.run_until_parked();
        let menu = cx.debug_bounds("selectable-copy-all").expect("menu shown");
        cx.simulate_click(menu.center(), Modifiers::none());
        assert_eq!(clipboard(cx).as_deref(), Some("hello wörld\nsecond line"));
        assert_eq!(view.read_with(cx, |row, _| row.clicks), 0);
    }

    #[gpui::test]
    fn double_click_through_reaches_the_parent_and_a_drag_still_selects(cx: &mut TestAppContext) {
        let (view, mut cx) = row_with(cx, true);
        let cx = &mut cx;
        cx.write_to_clipboard(ClipboardItem::new_string("before".into()));
        double_click(cx, at(7.5, 0.));
        assert_eq!(
            view.read_with(cx, |row, _| (row.clicks, row.double_clicks)),
            (2, 1),
            "both clicks reach the row, the second as a double-click"
        );
        // No word was selected: Ctrl+C copies nothing.
        cx.simulate_keystrokes("ctrl-c");
        assert_eq!(clipboard(cx).as_deref(), Some("before"));
        let selected = cx.update(|_, cx| {
            cx.try_global::<State>()
                .is_some_and(|state| state.selection.is_some())
        });
        assert!(!selected, "no selection");

        // A drag still selects and copies, and is no click.
        cx.simulate_mouse_down(at(6., 0.), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(at(11., 0.), MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(at(11., 0.), MouseButton::Left, Modifiers::none());
        cx.simulate_keystrokes("ctrl-c");
        assert_eq!(clipboard(cx).as_deref(), Some("wörld"));
        assert_eq!(view.read_with(cx, |row, _| row.clicks), 2);
    }
}
