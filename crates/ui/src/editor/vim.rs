//! Vim emulation for file editors (`"vim_mode"`, Toggle Vim Mode): modes,
//! motions, operators with text objects, registers, dot repeat, marks,
//! macros and an ex command line in the status bar.
//!
//! Keys reach Vim before the IDE's bindings through a keystroke
//! interceptor ([`init`]); keys Vim does not use in the current mode (all
//! of insert mode but Escape, Ctrl chords it has no meaning for, Alt and
//! function keys) go on to the IDE. Vim acts on the primary selection;
//! insert-mode typing, `i`/`a` and visual-mode operators keep working at
//! every cursor.

use std::collections::HashMap;

use gpui::{KeystrokeEvent, WeakEntity, WeakFocusHandle};

use super::*;

mod ex;
mod exec;
mod motion;
mod parse;
#[cfg(test)]
mod tests;

use parse::{Command, Key, VisualKind};

actions!(editor, [ToggleVimMode]);

/// The app-wide choice of [`ToggleVimMode`], over `"vim_mode"`.
struct VimModeOverride(bool);

impl Global for VimModeOverride {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    Normal,
    Insert,
    Replace,
    Visual(VisualKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegisterKind {
    Chars,
    /// Whole lines; the text ends with a line break.
    Lines,
    /// Visual block: one line of text per row.
    Block,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Register {
    text: String,
    kind: RegisterKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LastSearch {
    pattern: String,
    forward: bool,
}

/// What Vim keeps across editors: registers, the last search and
/// character find, command-line histories, macros, and the editors with
/// Vim on (for the keystroke interceptor).
#[derive(Default)]
struct Shared {
    registers: HashMap<char, Register>,
    search: Option<LastSearch>,
    /// `hlsearch`: matches of the last search show until `:noh`.
    highlight: bool,
    find: Option<(char, bool, bool)>,
    ex_history: Vec<String>,
    search_history: Vec<String>,
    recording: Option<(char, Vec<Key>)>,
    macros: HashMap<char, Vec<Key>>,
    last_macro: Option<char>,
    last_ex: Option<String>,
    editors: Vec<(WeakFocusHandle, WeakEntity<Editor>)>,
}

impl Global for Shared {}

/// Text typed in insert or replace mode, for `.` and insert counts: the
/// inserted text, the cursor's place in it, and what Backspace/Delete
/// erased beyond it (in bytes).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Inserted {
    text: String,
    cursor: usize,
    erased_before: usize,
    erased_after: usize,
}

/// A visual selection's size, so `.` repeats a visual command on the same
/// amount of text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VisualExtent {
    kind: VisualKind,
    /// Lines below the first.
    lines: usize,
    /// Characters (one line) or the last line's end column.
    columns: usize,
}

/// The last change, for `.`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    command: Command,
    inserted: Option<Inserted>,
    visual: Option<VisualExtent>,
}

/// Insert or replace mode since the command that entered it.
#[derive(Debug, Clone)]
struct InsertSession {
    /// The command that entered insert mode (`None`: block insert).
    command: Option<Command>,
    visual: Option<VisualExtent>,
    /// `3ifoo<Esc>` inserts three times.
    count: usize,
    /// The inserted region, mapped through edits.
    start: usize,
    end: usize,
    erased_before: usize,
    erased_after: usize,
    /// Undo steps from here on become one when insert mode ends.
    undo_depth: usize,
    /// Replace mode: the characters typed over (`None`: appended), for
    /// Backspace, and the typed text for `.`.
    replaced: Vec<Option<char>>,
    typed: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExKind {
    Command,
    Search { forward: bool },
}

/// The command line in the status bar.
#[derive(Debug, Clone)]
struct ExLine {
    kind: ExKind,
    text: String,
    /// Index into the history while browsing it with Up/Down.
    history: Option<usize>,
    /// What was typed before browsing.
    typed: String,
}

/// Visible matches of the last search (`hlsearch`), for the lines on
/// screen only.
#[derive(Default)]
struct Highlights {
    key: Option<(u64, String, Range<usize>)>,
    regex: Option<(String, regex::Regex)>,
    matches: Vec<Range<usize>>,
}

/// Per-editor Vim state.
pub(super) struct VimState {
    /// Vim was on for this editor at the last frame.
    active: bool,
    registered: bool,
    /// The Commit tool window's message (Vim there only with
    /// `vim.in_commit_message`).
    commit_message: bool,
    mode: Mode,
    /// Keys of the command being typed.
    pending: Vec<Key>,
    /// Visual mode: where it started and the cursor (both on characters).
    anchor: usize,
    cursor: usize,
    /// Visual block after `$`: every line to its end.
    to_line_end: bool,
    /// The primary selection visual mode last showed (anchor, head), to
    /// notice when something else changed it.
    shown: Option<(usize, usize)>,
    last_visual: Option<(usize, usize, VisualKind)>,
    /// Column j/k keep (`usize::MAX` after `$`).
    goal: Option<usize>,
    insert: Option<InsertSession>,
    last_change: Option<Change>,
    marks: HashMap<char, usize>,
    ex: Option<ExLine>,
    highlights: Highlights,
    /// Nesting of `.` and `@` replays (they record nothing).
    replaying: usize,
}

impl Default for VimState {
    fn default() -> Self {
        Self {
            active: false,
            registered: false,
            commit_message: false,
            mode: Mode::Insert,
            pending: Vec::new(),
            anchor: 0,
            cursor: 0,
            to_line_end: false,
            shown: None,
            last_visual: None,
            goal: None,
            insert: None,
            last_change: None,
            marks: HashMap::new(),
            ex: None,
            highlights: Highlights::default(),
            replaying: 0,
        }
    }
}

/// Whether Vim mode is on: Toggle Vim Mode's choice, else `"vim_mode"`.
fn enabled(cx: &App) -> bool {
    cx.try_global::<VimModeOverride>()
        .map_or_else(|| crate::settings::current().vim_mode, |o| o.0)
}

/// `vim.ctrl_keys`.
fn ctrl_keys() -> bool {
    crate::settings::current().vim.ctrl_keys
}

/// Ctrl keys Vim uses in normal and visual mode (`Ctrl+P`, `Ctrl+N` and
/// `Ctrl+J` stay the IDE's: Go to File...).
fn vim_ctrl_key(c: char) -> bool {
    "dufbeyraxvq".contains(c)
}

/// Routes keystrokes to the focused editor's Vim before key bindings.
pub(super) fn init(cx: &mut App) {
    cx.intercept_keystrokes(|event: &KeystrokeEvent, window, cx| {
        let Some(editor) = focused_vim_editor(window, cx) else {
            return;
        };
        let keystroke = &event.keystroke;
        if keystroke.modifiers.control && keystroke.key == "[" && !ctrl_keys() {
            return;
        }
        let Some(key) = Key::from_keystroke(keystroke) else {
            return;
        };
        let consumed = editor.update(cx, |editor, cx| editor.vim_key(key, window, cx));
        if consumed {
            cx.stop_propagation();
        }
    })
    .detach();
}

fn focused_vim_editor(window: &Window, cx: &mut App) -> Option<Entity<Editor>> {
    let focused = window.focused(cx)?;
    let shared = cx.try_global::<Shared>()?;
    shared.editors.iter().find_map(|(handle, editor)| {
        (handle.upgrade().as_ref() == Some(&focused))
            .then(|| editor.upgrade())
            .flatten()
    })
}

impl Editor {
    /// Marks this text area as the Commit tool window's message, which
    /// gets Vim with `vim.in_commit_message`.
    pub fn set_commit_message(&mut self) {
        self.vim.commit_message = true;
    }

    fn vim_eligible(&self) -> bool {
        !self.single_line
            && (!self.text_area
                || (self.vim.commit_message && crate::settings::current().vim.in_commit_message))
    }

    /// Like Alt+Z: for every editor until the app exits.
    pub(super) fn toggle_vim_mode(
        &mut self,
        _: &ToggleVimMode,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let on = !enabled(cx);
        cx.set_global(VimModeOverride(on));
        cx.refresh_windows();
    }

    pub(super) fn vim_mode(&self) -> Option<Mode> {
        self.vim.active.then_some(self.vim.mode)
    }

    /// Before each frame: follows the setting and selections changed by
    /// the mouse or IDE commands (a selection in normal mode is visual
    /// mode, a normal-mode cursor sits on a character).
    pub(super) fn sync_vim(&mut self, cx: &mut Context<Self>) {
        let active = self.vim_eligible() && enabled(cx);
        if active != self.vim.active {
            self.vim.active = active;
            self.vim.pending.clear();
            self.vim.ex = None;
            self.vim.insert = None;
            self.vim.mode = if active { Mode::Normal } else { Mode::Insert };
        }
        if !active {
            return;
        }
        if !self.vim.registered {
            self.vim.registered = true;
            let entry = (self.focus_handle.downgrade(), cx.entity().downgrade());
            let shared = cx.default_global::<Shared>();
            shared.editors.retain(|(handle, editor)| {
                handle.upgrade().is_some() && editor.upgrade().is_some()
            });
            shared.editors.push(entry);
        }
        let primary = self.primary();
        match self.vim.mode {
            Mode::Normal if !primary.is_empty() => self.adopt_selection(primary),
            Mode::Normal => {
                let head = self.normal_cursor(primary.head);
                if head != primary.head && self.selections.len() == 1 {
                    self.selections = SelectionSet::single(Selection::cursor(head));
                }
            }
            Mode::Visual(_) if self.vim.shown != Some((primary.anchor, primary.head)) => {
                if primary.is_empty() {
                    self.vim.mode = Mode::Normal;
                    self.vim.shown = None;
                } else {
                    self.adopt_selection(primary);
                }
            }
            _ => {}
        }
    }

    /// Visual mode over a selection made outside Vim.
    fn adopt_selection(&mut self, s: Selection) {
        let (anchor, cursor) = if s.reversed() {
            (self.buffer.prev_grapheme(s.anchor), s.head)
        } else {
            (s.anchor, self.buffer.prev_grapheme(s.head))
        };
        self.vim.mode = Mode::Visual(VisualKind::Char);
        self.vim.anchor = anchor;
        self.vim.cursor = cursor;
        self.vim.to_line_end = false;
        self.vim.shown = Some((s.anchor, s.head));
    }

    /// `offset` moved onto the last character of its line if past it.
    fn normal_cursor(&self, offset: usize) -> usize {
        let line = self.buffer.line_of(offset);
        let line_start = self.buffer.line_start(line);
        if offset >= self.buffer.line_end(line) && offset > line_start {
            motion::last_char(&self.buffer, line)
        } else {
            offset
        }
    }

    /// Handles a key; `false` leaves it to the IDE.
    pub(super) fn vim_key(
        &mut self,
        key: Key,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.vim.active {
            return false;
        }
        self.record_macro_key(&key, cx);
        if self.vim.ex.is_some() {
            return self.ex_key(key, window, cx);
        }
        match self.vim.mode {
            Mode::Insert => self.insert_key(key, window, cx),
            Mode::Replace => self.replace_key(key, window, cx),
            Mode::Normal | Mode::Visual(_) => self.normal_key(key, window, cx),
        }
    }

    fn record_macro_key(&mut self, key: &Key, cx: &mut Context<Self>) {
        if self.vim.replaying > 0 {
            return;
        }
        if let Some((_, keys)) = &mut cx.default_global::<Shared>().recording {
            keys.push(key.clone());
        }
    }

    /// A popover Escape should close before it leaves insert mode.
    fn popover_open(&self) -> bool {
        self.completion.is_some()
            || self.signature.popover.is_some()
            || self.code_actions.menu.is_some()
            || self.hunk_popup_open()
    }

    fn insert_key(&mut self, key: Key, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match key {
            Key::Esc if !self.popover_open() => {
                self.leave_insert(window, cx);
                true
            }
            _ => false,
        }
    }

    fn normal_key(&mut self, key: Key, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if key == Key::Esc {
            if !self.vim.pending.is_empty() {
                self.vim.pending.clear();
                cx.notify();
                return true;
            }
            if let Mode::Visual(_) = self.vim.mode {
                self.exit_visual(cx);
                return true;
            }
            // Collapsing cursors, closing the find bar...
            return false;
        }
        if let Key::Ctrl(c) = key
            && (!ctrl_keys() || (self.vim.pending.is_empty() && !vim_ctrl_key(c)))
        {
            return false;
        }
        self.vim.pending.push(key);
        let visual = matches!(self.vim.mode, Mode::Visual(_));
        let recording = cx.default_global::<Shared>().recording.is_some();
        match parse::parse(&self.vim.pending, visual, recording) {
            parse::Parsed::Pending => cx.notify(),
            parse::Parsed::Invalid => {
                self.vim.pending.clear();
                cx.notify();
            }
            parse::Parsed::Search { forward } => {
                self.vim.pending.pop();
                self.open_ex(ExKind::Search { forward }, String::new(), cx);
            }
            parse::Parsed::Done(command) => {
                self.vim.pending.clear();
                self.run_command(command, window, cx);
            }
        }
        true
    }

    /// Edits made by any path: marks, the insert region and the visual
    /// anchors move with the text.
    pub(super) fn vim_edited(&mut self) {
        if !self.vim.active || self.changes.is_empty() {
            return;
        }
        for change in &self.changes {
            for offset in self.vim.marks.values_mut() {
                *offset = change.map_offset(*offset, Bias::Left);
            }
            if let Some((anchor, cursor, _)) = &mut self.vim.last_visual {
                *anchor = change.map_offset(*anchor, Bias::Left);
                *cursor = change.map_offset(*cursor, Bias::Left);
            }
            if let Some(session) = &mut self.vim.insert {
                let deleted = change.start..change.old_end;
                if deleted.start < session.start {
                    session.erased_before += session.start.min(deleted.end) - deleted.start;
                }
                if deleted.end > session.end {
                    session.erased_after += deleted.end - deleted.start.max(session.end);
                }
                session.start = change.map_offset(session.start, Bias::Left);
                session.end = change.map_offset(session.end, Bias::Right);
            }
            if let Mode::Visual(_) = self.vim.mode {
                self.vim.anchor = change.map_offset(self.vim.anchor, Bias::Left);
                self.vim.cursor = change.map_offset(self.vim.cursor, Bias::Left);
            }
        }
    }

    /// The offset the cursor of `s` is drawn at: visual mode draws it on
    /// the character under Vim's cursor, inside the selection.
    pub(super) fn cursor_head(&self, s: &Selection) -> usize {
        match self.vim_mode() {
            Some(Mode::Visual(_)) if *s == self.primary() => self.vim.cursor.min(self.buffer.len()),
            _ => s.head,
        }
    }

    /// The cursor's quad at `head` on `row`: a bar, or Vim's block
    /// (normal, visual), half block (operator pending) or underline
    /// (replace).
    pub(super) fn cursor_quad(
        &self,
        head: usize,
        row: &RowLayout,
        text_left: Pixels,
        top: Pixels,
        layout: &EditorLayout,
    ) -> (Bounds<Pixels>, Hsla) {
        let x = text_left + row.x_for(head);
        let line_height = layout.line_height;
        let mode = match self.vim_mode() {
            None | Some(Mode::Insert) => {
                return (
                    Bounds::from_corners(point(x, top), point(x + px(2.), top + line_height)),
                    theme::cursor(),
                );
            }
            Some(mode) => mode,
        };
        let next = self.buffer.next_grapheme(head);
        let width = if next > head && next <= row.row.range.end && self.char_at(head) != Some('\n')
        {
            row.x_for(next) - row.x_for(head)
        } else {
            layout.em_width
        };
        let width = width.max(px(2.));
        let height = if mode == Mode::Replace {
            px(3.)
        } else if !self.vim.pending.is_empty() {
            line_height / 2.
        } else {
            line_height
        };
        let bounds = Bounds::from_corners(
            point(x, top + line_height - height),
            point(x + width, top + line_height),
        );
        (bounds, theme::cursor().opacity(0.55))
    }

    /// Recomputes the search matches on screen when the text, the search
    /// or the visible lines changed (only while `hlsearch` is on).
    pub(super) fn update_vim_highlights(&mut self, cx: &App) {
        let search = cx
            .try_global::<Shared>()
            .filter(|shared| shared.highlight && self.vim.active)
            .and_then(|shared| shared.search.as_ref());
        let (Some(search), Some(layout)) = (search, &self.layout) else {
            self.vim.highlights.key = None;
            self.vim.highlights.matches.clear();
            return;
        };
        let (Some(first), Some(last)) = (layout.rows.first(), layout.rows.last()) else {
            return;
        };
        let range = self.buffer.line_start(first.row.line)..self.buffer.line_end(last.row.line);
        let highlights = &mut self.vim.highlights;
        if highlights
            .key
            .as_ref()
            .is_some_and(|(version, pattern, cached)| {
                *version == self.version && *pattern == search.pattern && *cached == range
            })
        {
            return;
        }
        highlights.matches.clear();
        if highlights
            .regex
            .as_ref()
            .is_none_or(|(pattern, _)| *pattern != search.pattern)
        {
            highlights.regex = ex::search_regex(&search.pattern)
                .ok()
                .map(|regex| (search.pattern.clone(), regex));
        }
        if let Some((_, regex)) = &highlights.regex {
            let text = self.buffer.slice(range.clone());
            highlights.matches.extend(
                regex
                    .find_iter(&text)
                    .filter(|m| !m.is_empty())
                    .map(|m| range.start + m.start()..range.start + m.end()),
            );
        }
        highlights.key = Some((self.version, search.pattern.clone(), range));
    }

    pub(super) fn vim_highlights(&self) -> &[Range<usize>] {
        &self.vim.highlights.matches
    }

    /// The command line, shown instead of the status message.
    pub(super) fn vim_command_line(&self) -> Option<SharedString> {
        let ex = self.vim.ex.as_ref().filter(|_| self.vim.active)?;
        let prefix = match ex.kind {
            ExKind::Command => ':',
            ExKind::Search { forward: true } => '/',
            ExKind::Search { forward: false } => '?',
        };
        Some(format!("{prefix}{}▏", ex.text).into())
    }

    /// The mode, pending keys and macro recording, at the status bar's left.
    pub(super) fn render_vim_status(&self, cx: &App) -> Option<AnyElement> {
        let mode = self.vim_mode()?;
        let label = match mode {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Replace => "REPLACE",
            Mode::Visual(VisualKind::Char) => "VISUAL",
            Mode::Visual(VisualKind::Line) => "V-LINE",
            Mode::Visual(VisualKind::Block) => "V-BLOCK",
        };
        let color = match mode {
            Mode::Normal => theme::accent_text(),
            Mode::Insert | Mode::Replace => theme::warning(),
            Mode::Visual(_) => theme::error(),
        };
        let pending: String = self.vim.pending.iter().map(key_label).collect();
        let recording = cx
            .try_global::<Shared>()
            .and_then(|shared| shared.recording.as_ref())
            .map(|(register, _)| format!("recording @{register}"));
        Some(
            div()
                .flex()
                .flex_none()
                .gap_2()
                .child(
                    div()
                        .font_weight(FontWeight::BOLD)
                        .text_color(color)
                        .child(label),
                )
                .when(!pending.is_empty(), |row| {
                    row.child(div().text_color(theme::text()).child(pending))
                })
                .children(recording.map(|text| div().text_color(theme::warning()).child(text)))
                .into_any_element(),
        )
    }
}

/// How a pending key reads in the status bar.
fn key_label(key: &Key) -> String {
    match key {
        Key::Char(c) => c.to_string(),
        Key::Ctrl(c) => format!("^{}", c.to_ascii_uppercase()),
        Key::Search { forward, pattern } => {
            format!("{}{pattern}", if *forward { '/' } else { '?' })
        }
        _ => String::new(),
    }
}
