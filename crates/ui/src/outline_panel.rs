//! Outline dock panel: the active editor's document symbols as a tree.
//! Rows expand/collapse, click (or Enter) jumps to the symbol, and the
//! symbol around the editor's cursor is highlighted as it moves.

use std::{collections::HashSet, ops::Range};

use gpui::{
    App, Context, Entity, FocusHandle, Focusable, Hsla, KeyBinding, MouseButton, ScrollStrategy,
    Subscription, UniformListScrollHandle, Window, actions, div, prelude::*, px, uniform_list,
};
use language::symbols::{self as model, Symbol};
use syntax::Language;

use crate::{
    editor::{Editor, SymbolsStatus, symbol_kind_badge},
    theme,
};

actions!(
    outline_panel,
    [SelectPrev, SelectNext, Confirm, Expand, Collapse]
);

const CONTEXT: &str = "OutlinePanel";
const ROW_HEIGHT: f32 = 24.;
const INDENT: f32 = 14.;

pub fn bind_keys(cx: &mut App) {
    let ctx = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("up", SelectPrev, ctx),
        KeyBinding::new("down", SelectNext, ctx),
        KeyBinding::new("enter", Confirm, ctx),
        KeyBinding::new("right", Expand, ctx),
        KeyBinding::new("left", Collapse, ctx),
    ]);
}

/// What the panel last drew for its editor, to redraw only on changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seen {
    generation: u64,
    status: SymbolsStatus,
    current: Option<usize>,
}

pub struct OutlinePanel {
    focus_handle: FocusHandle,
    editor: Option<Entity<Editor>>,
    /// Collapsed symbols by name path (survives refreshed symbol lists).
    collapsed: HashSet<String>,
    /// Keyboard selection (a symbol index).
    selected: Option<usize>,
    seen: Option<Seen>,
    scroll: UniformListScrollHandle,
    _subscription: Option<Subscription>,
}

impl OutlinePanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            editor: None,
            collapsed: HashSet::new(),
            selected: None,
            seen: None,
            scroll: UniformListScrollHandle::new(),
            _subscription: None,
        }
    }

    /// Follows the workspace's active editor. Called while the workspace
    /// renders, so the panel (drawn after it) needs no notify.
    pub fn set_editor(&mut self, editor: Option<Entity<Editor>>, cx: &mut Context<Self>) {
        if self.editor == editor {
            return;
        }
        self._subscription = editor.as_ref().map(|editor| {
            cx.observe(editor, |this, editor, cx| {
                let seen = seen(editor.read(cx));
                if this.seen == Some(seen) {
                    return;
                }
                if let Some(current) = seen.current
                    && this.seen.is_none_or(|old| old.current != seen.current)
                {
                    this.scroll_to_symbol(current, editor.read(cx).symbols());
                }
                if this
                    .seen
                    .is_some_and(|old| old.generation != seen.generation)
                {
                    this.selected = None;
                }
                this.seen = Some(seen);
                cx.notify();
            })
        });
        self.seen = None;
        self.selected = None;
        self.editor = editor;
    }

    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_none() {
            self.selected = self
                .editor
                .as_ref()
                .and_then(|editor| editor.read(cx).symbol_path().last().copied());
        }
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Scrolls the row showing `index` (or its collapsed ancestor) into
    /// view.
    fn scroll_to_symbol(&self, index: usize, symbols: &[Symbol<usize>]) {
        let rows = self.rows(symbols);
        let shown = shown_ancestor(symbols, &rows, index);
        if let Some(row) = shown.and_then(|shown| rows.iter().position(|&i| i == shown)) {
            self.scroll.scroll_to_item(row, ScrollStrategy::Top);
        }
    }

    fn rows(&self, symbols: &[Symbol<usize>]) -> Vec<usize> {
        visible_rows(symbols, &self.collapsed)
    }

    fn toggle(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        let key = name_path(editor.read(cx).symbols(), index);
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        cx.notify();
    }

    /// Jumps to symbol `index` in the editor and focuses it.
    fn open(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.clone() else {
            return;
        };
        self.selected = Some(index);
        editor.update(cx, |editor, cx| editor.reveal_symbol(index, cx));
        window.focus(&editor.focus_handle(cx));
    }

    /// Moves the keyboard selection by `delta` visible rows.
    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        let symbols = editor.read(cx).symbols();
        let rows = self.rows(symbols);
        let position = self
            .selected
            .and_then(|selected| rows.iter().position(|&i| i == selected));
        let next = match position {
            Some(position) => position
                .saturating_add_signed(delta)
                .min(rows.len().saturating_sub(1)),
            None => 0,
        };
        if let Some(&index) = rows.get(next) {
            self.selected = Some(index);
            self.scroll.scroll_to_item(next, ScrollStrategy::Top);
            cx.notify();
        }
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-1, cx);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.selected {
            self.open(index, window, cx);
        }
    }

    /// Right: expands the selected symbol, or steps into its children.
    fn expand(&mut self, _: &Expand, _: &mut Window, cx: &mut Context<Self>) {
        let (Some(editor), Some(index)) = (&self.editor, self.selected) else {
            return;
        };
        let symbols = editor.read(cx).symbols();
        if !model::has_children(symbols, index) {
            return;
        }
        if self.collapsed.remove(&name_path(symbols, index)) {
            cx.notify();
        } else {
            self.step(1, cx);
        }
    }

    /// Left: collapses the selected symbol, or selects its parent.
    fn collapse(&mut self, _: &Collapse, _: &mut Window, cx: &mut Context<Self>) {
        let (Some(editor), Some(index)) = (&self.editor, self.selected) else {
            return;
        };
        let symbols = editor.read(cx).symbols();
        let key = name_path(symbols, index);
        if model::has_children(symbols, index) && !self.collapsed.contains(&key) {
            self.collapsed.insert(key);
        } else if let Some(parent) = symbols.get(index).and_then(|symbol| symbol.parent) {
            self.selected = Some(parent);
        }
        cx.notify();
    }

    /// What row `index` shows, read before rendering (the editor is
    /// borrowed from the same context).
    fn row_data(
        &self,
        index: usize,
        symbols: &[Symbol<usize>],
        language: Option<Language>,
    ) -> RowData {
        let symbol = &symbols[index];
        let (badge, color) = symbol_kind_badge(symbol.kind, language);
        let chevron = model::has_children(symbols, index).then(|| {
            if self.collapsed.contains(&name_path(symbols, index)) {
                "▸"
            } else {
                "▾"
            }
        });
        RowData {
            index,
            depth: symbol.depth,
            name: symbol.name.clone(),
            detail: symbol
                .detail
                .as_deref()
                .and_then(|detail| detail.lines().next())
                .unwrap_or_default()
                .to_owned(),
            badge,
            color,
            chevron,
        }
    }

    fn render_row(
        &self,
        row: RowData,
        current: Option<usize>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let RowData {
            index,
            depth,
            name,
            detail,
            badge,
            color,
            chevron,
        } = row;
        div()
            .id(index)
            .h(px(ROW_HEIGHT))
            .w_full()
            .pl(px(6. + INDENT * depth as f32))
            .pr_2()
            .flex()
            .items_center()
            .gap_1()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_sm()
            .cursor_pointer()
            .when(current == Some(index), |row| row.bg(theme::active_row()))
            .when(current != Some(index), |row| {
                row.hover(|style| style.bg(theme::hover_row()))
            })
            .when(self.selected == Some(index), |row| {
                row.border_1().border_color(theme::accent())
            })
            .on_click(cx.listener(move |this, _, window, cx| this.open(index, window, cx)))
            .child(
                div()
                    .id(("chevron", index))
                    .w(px(12.))
                    .flex_none()
                    .text_xs()
                    .text_color(theme::muted())
                    .children(chevron)
                    .when(chevron.is_some(), |chevron| {
                        chevron.on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle(index, cx);
                        }))
                    }),
            )
            .child(
                div()
                    .w(px(14.))
                    .flex_none()
                    .text_xs()
                    .text_color(color)
                    .child(badge),
            )
            .child(div().flex_none().text_color(theme::text()).child(name))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(detail),
            )
    }
}

struct RowData {
    index: usize,
    depth: usize,
    name: String,
    detail: String,
    badge: &'static str,
    color: Hsla,
    chevron: Option<&'static str>,
}

fn seen(editor: &Editor) -> Seen {
    Seen {
        generation: editor.symbols_generation(),
        status: editor.symbols_status(),
        current: editor.symbol_path().last().copied(),
    }
}

/// `index` itself if it is a row, else its closest ancestor that is.
fn shown_ancestor(symbols: &[Symbol<usize>], rows: &[usize], index: usize) -> Option<usize> {
    let mut at = Some(index);
    while let Some(index) = at {
        if rows.binary_search(&index).is_ok() {
            return Some(index);
        }
        at = symbols.get(index).and_then(|symbol| symbol.parent);
    }
    None
}

/// Symbol indices of the visible rows: children of `collapsed` symbols
/// (by name path) are hidden.
fn visible_rows(symbols: &[Symbol<usize>], collapsed: &HashSet<String>) -> Vec<usize> {
    let mut rows = Vec::with_capacity(symbols.len());
    let mut hidden_below: Option<usize> = None;
    for (index, symbol) in symbols.iter().enumerate() {
        if let Some(depth) = hidden_below {
            if symbol.depth > depth {
                continue;
            }
            hidden_below = None;
        }
        rows.push(index);
        if model::has_children(symbols, index) && collapsed.contains(&name_path(symbols, index)) {
            hidden_below = Some(symbol.depth);
        }
    }
    rows
}

/// A symbol's names from the root, as a collapse key.
fn name_path(symbols: &[Symbol<usize>], index: usize) -> String {
    let mut names = Vec::new();
    let mut at = Some(index);
    while let Some(index) = at {
        let Some(symbol) = symbols.get(index) else {
            break;
        };
        names.push(symbol.name.as_str());
        at = symbol.parent;
    }
    names.reverse();
    names.join("\u{1f}")
}

impl Focusable for OutlinePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for OutlinePanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::expand))
            .on_action(cx.listener(Self::collapse))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, _| window.focus(&this.focus_handle)),
            )
            .size_full();
        let message =
            |text: &'static str| div().p_3().text_sm().text_color(theme::muted()).child(text);
        let Some(editor) = self.editor.clone() else {
            return root.child(message("Open a file to see its outline."));
        };
        let editor_ref = editor.read(cx);
        let symbols = editor_ref.symbols();
        if symbols.is_empty() {
            return root.child(message(match editor_ref.symbols_status() {
                SymbolsStatus::NoServer => "No language server for this file.",
                SymbolsStatus::Unsupported => "The language server does not provide symbols.",
                SymbolsStatus::Loading => "Loading symbols…",
                SymbolsStatus::Ready => "No symbols",
            }));
        }
        let rows = self.rows(symbols);
        let current = editor_ref
            .symbol_path()
            .last()
            .and_then(|&index| shown_ancestor(symbols, &rows, index));
        let row_count = rows.len();
        root.child(
            uniform_list(
                "outline",
                row_count,
                cx.processor(move |this, range: Range<usize>, _, cx| {
                    let Some(editor) = this.editor.clone() else {
                        return Vec::new();
                    };
                    let editor = editor.read(cx);
                    let symbols = editor.symbols();
                    let data: Vec<RowData> = range
                        .filter_map(|row| rows.get(row).copied())
                        .filter(|&index| index < symbols.len())
                        .map(|index| this.row_data(index, symbols, editor.language()))
                        .collect();
                    data.into_iter()
                        .map(|row| this.render_row(row, current, cx).into_any_element())
                        .collect()
                }),
            )
            .track_scroll(self.scroll.clone())
            .size_full(),
        )
    }
}

#[cfg(test)]
mod tests {
    use lsp::types::SymbolKind;

    use super::*;

    fn symbol(
        name: &str,
        depth: usize,
        parent: Option<usize>,
        range: Range<usize>,
    ) -> Symbol<usize> {
        Symbol {
            name: name.into(),
            detail: None,
            kind: SymbolKind::FUNCTION,
            selection_range: range.start..range.start,
            range,
            depth,
            parent,
        }
    }

    #[test]
    fn collapsed_symbols_hide_their_subtree() {
        let symbols = [
            symbol("A", 0, None, 0..50),
            symbol("a1", 1, Some(0), 5..20),
            symbol("x", 2, Some(1), 6..8),
            symbol("a2", 1, Some(0), 25..40),
            symbol("B", 0, None, 60..80),
        ];
        let mut collapsed = HashSet::new();
        collapsed.insert(name_path(&symbols, 1));
        assert_eq!(name_path(&symbols, 2), "A\u{1f}a1\u{1f}x");
        let rows = visible_rows(&symbols, &collapsed);
        assert_eq!(rows, [0, 1, 3, 4]);
        // The cursor inside a hidden symbol highlights its shown ancestor.
        assert_eq!(shown_ancestor(&symbols, &rows, 2), Some(1));
        collapsed.insert(name_path(&symbols, 0));
        let rows = visible_rows(&symbols, &collapsed);
        assert_eq!(rows, [0, 4]);
        assert_eq!(shown_ancestor(&symbols, &rows, 2), Some(0));
    }
}
