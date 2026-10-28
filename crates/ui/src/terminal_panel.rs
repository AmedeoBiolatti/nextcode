//! Terminal panel: shell sessions in tabs, each rendered from a
//! `terminal::Snapshot`.

use std::{
    path::{Path, PathBuf},
    rc::Rc,
};

use futures::StreamExt;
use gpui::{
    App, BorderStyle, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, DispatchPhase,
    Element, ElementId, Entity, EventEmitter, FocusHandle, Focusable, Font, FontStyle, FontWeight,
    GlobalElementId, Hsla, InspectorElementId, KeyBinding, KeyDownEvent, LayoutId,
    ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, NoAction,
    Pixels, Point, ScrollWheelEvent, ShapedLine, SharedString, Size, StrikethroughStyle, Style,
    Subscription, Task, TextRun, UnderlineStyle, Window, div, fill, outline, point, prelude::*, px,
    relative, rgb, size,
};
use terminal::{
    Key, LinkTarget, Modifiers, OutputTap, Palette, Rgb, ScreenLink, Snapshot, Terminal,
    TerminalEvent, TerminalOptions,
};
use worktree::FileKind;

use crate::{
    hscrollbar::{self, HScrollbar},
    pinpoint::Pin,
    project::Project,
    selectable::selectable,
    theme,
};

mod pin_text;

const CONTEXT: &str = "Terminal";

pub fn bind_keys(cx: &mut App) {
    // Shells need these; keep workspace shortcuts from taking them.
    let ctx = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("ctrl-w", NoAction {}, ctx),
        KeyBinding::new("ctrl-j", NoAction {}, ctx),
        KeyBinding::new("alt-left", NoAction {}, ctx),
        KeyBinding::new("alt-right", NoAction {}, ctx),
        KeyBinding::new("alt-1", NoAction {}, ctx),
    ]);
}

/// A shell the user can start.
#[derive(Clone)]
pub struct Profile {
    pub name: &'static str,
    program: &'static str,
    args: &'static [&'static str],
    /// Runs the user's login shell where a remote project lives.
    remote: bool,
}

/// Shells available for `project`, default first.
pub fn profiles(project: &Project) -> Vec<Profile> {
    if project.is_remote() {
        let name = match project.location() {
            crate::project::Location::DevContainer { .. } => "Container Shell",
            crate::project::Location::Remote { .. } => "Remote Shell",
            _ => "WSL Shell",
        };
        return vec![Profile {
            name,
            program: "",
            args: &[],
            remote: true,
        }];
    }
    if cfg!(windows) {
        let mut profiles = vec![Profile {
            name: "PowerShell",
            program: "powershell.exe",
            args: &["-NoLogo"],
            remote: false,
        }];
        let system32 =
            std::env::var_os("SystemRoot").map(|root| PathBuf::from(root).join("System32"));
        if system32.is_some_and(|dir| dir.join("wsl.exe").is_file()) {
            profiles.push(Profile {
                name: "WSL",
                program: "wsl.exe",
                args: &[],
                remote: false,
            });
        }
        profiles.push(Profile {
            name: "Command Prompt",
            program: "cmd.exe",
            args: &[],
            remote: false,
        });
        profiles
    } else {
        vec![Profile {
            name: "Shell",
            program: "",
            args: &[],
            remote: false,
        }]
    }
}

/// Where New Terminal starts: the folder holding `near` (the active
/// editor's file); `None` for the first folder, the shell's default (also
/// when `near` is outside every folder).
fn shell_folder(project: &Project, near: Option<&Path>) -> Option<PathBuf> {
    match near.and_then(|path| project.root_index_of(path)) {
        Some(index) if index > 0 => Some(project.roots()[index].path.clone()),
        _ => None,
    }
}

/// Starts `program` in a pty where `project` lives (in its root), or on
/// this machine in the home folder without one, as a view named `title`
/// that stays when the program exits. `tap` follows its output.
pub fn spawn_program(
    project: Option<&Project>,
    title: &str,
    program: &str,
    args: &[String],
    tap: Option<OutputTap>,
    cx: &mut App,
) -> std::io::Result<Entity<TerminalView>> {
    let mut options = TerminalOptions::default_shell();
    let palette = theme::current().terminal;
    let (terminal, events) = match project {
        Some(project) if project.is_remote() => project.spawn_remote_terminal(
            Some((program, args)),
            None,
            (options.columns, options.rows, options.scrollback),
            palette,
            tap,
        )?,
        _ => {
            options.program = program.into();
            options.args = args.to_vec();
            options.cwd = match project {
                Some(project) => Some(project.root().to_path_buf()),
                None => dirs::home_dir(),
            };
            Terminal::spawn(options, palette, tap)?
        }
    };
    Ok(cx.new(|cx| {
        let mut view = TerminalView::new(terminal, events, "Command", palette, cx);
        view.run_name = Some(title.to_owned().into());
        view.keep_on_exit = true;
        view
    }))
}

pub(crate) fn to_hsla(color: Rgb) -> Hsla {
    rgb((u32::from(color.r) << 16) | (u32::from(color.g) << 8) | u32::from(color.b)).into()
}

pub enum TerminalViewEvent {
    TitleChanged,
    Exited,
    /// Ctrl+click on a file location (compiler output, a traceback, a
    /// pytest failure): the path as printed, 1-based line and column.
    OpenLocation {
        path: String,
        line: Option<u32>,
        column: Option<u32>,
    },
}

impl EventEmitter<TerminalViewEvent> for TerminalView {}

/// A screen cell position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CellPoint {
    row: usize,
    column: usize,
}

pub struct TerminalView {
    terminal: Terminal,
    profile: &'static str,
    title: SharedString,
    /// Set by `TerminalPanel::run_command`/`run_program`: the tab keeps
    /// this name (shell titles don't replace it) and running it again
    /// replaces the tab.
    run_name: Option<SharedString>,
    /// The folder the shell started in, shown after the title in a
    /// multi-root project.
    folder: Option<SharedString>,
    /// A program's tab (workflows) stays when the program exits, showing
    /// its exit code; a shell's tab closes.
    keep_on_exit: bool,
    /// Set once the process exited (with its code when known).
    exit: Option<Option<i32>>,
    focus_handle: FocusHandle,
    palette: Palette,
    /// Grid size last sent to the pty.
    grid: (u16, u16),
    /// Origin and cell size from the last paint, for mouse hit-testing.
    cell_origin: Point<Pixels>,
    cell_size: Size<Pixels>,
    /// Modes from the last snapshot, for encoding keys and pastes.
    app_cursor: bool,
    bracketed_paste: bool,
    alt_screen: bool,
    /// Anchor and head of the mouse selection.
    selection: Option<(CellPoint, CellPoint)>,
    selecting: bool,
    /// Fractional scroll-wheel lines not yet applied.
    scroll_remainder: f32,
    /// The cell under the mouse, and whether Ctrl is held.
    hover: Option<CellPoint>,
    ctrl: bool,
    /// The link under the mouse in the last frame (underlined; Ctrl+click
    /// opens it).
    link: Option<ScreenLink>,
    /// Columns scrolled right (lines wider than the view; the pty stays as
    /// wide as the view), the part of a column the wheel moved so far,
    /// and the snapshot's width in the last frame.
    scroll_x: usize,
    scroll_x_remainder: f32,
    content_columns: usize,
    /// The horizontal bar in the last frame, where its thumb is held while
    /// dragged, and whether the mouse is over it.
    hbar: Option<HScrollbar>,
    hbar_drag: Option<Pixels>,
    hbar_hovered: bool,
    /// Typing scrolls the cursor into view sideways, until the user
    /// scrolls sideways.
    reveal_cursor: bool,
    /// Columns to scroll into view in the next frame (Pinpoint reveal).
    reveal_columns: Option<(usize, usize)>,
    /// Bumped whenever the screen may show differently for a reason the
    /// other inputs of `layout` do not tell: output (a wakeup), scrolling
    /// the history, typing, a kill, a Pinpoint reveal.
    generation: u64,
    /// The screen as last shaped: a frame with the same inputs paints it
    /// again without taking a snapshot.
    shaped: Option<Shaped>,
    _events: Task<()>,
}

/// What a frame's `layout` depends on besides the mouse: equal keys shape
/// the same text.
#[derive(Clone, PartialEq)]
struct LayoutKey {
    generation: u64,
    bounds: Bounds<Pixels>,
    font: Font,
    font_size: Pixels,
    line_height: Pixels,
    scroll_x: usize,
    selection: Option<(CellPoint, CellPoint)>,
    pin_path: SharedString,
}

/// A shaped screen: for which inputs and mouse cell, from which snapshot
/// (a mouse move looks its link up there, and shapes again only when the
/// link changed).
struct Shaped {
    key: LayoutKey,
    hover: Option<CellPoint>,
    snapshot: Rc<Snapshot>,
    layout: Rc<TerminalLayout>,
}

impl TerminalView {
    fn new(
        terminal: Terminal,
        mut events: futures::channel::mpsc::UnboundedReceiver<TerminalEvent>,
        profile: &'static str,
        palette: Palette,
        cx: &mut Context<Self>,
    ) -> Self {
        let task = cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                let alive = this.update(cx, |this, cx| match event {
                    TerminalEvent::Wakeup => this.screen_changed(cx),
                    TerminalEvent::Title(title) => {
                        this.title = title.into();
                        cx.emit(TerminalViewEvent::TitleChanged);
                    }
                    TerminalEvent::Bell => {}
                    TerminalEvent::Exited(code) => {
                        if this.exit.is_none() {
                            this.exit = Some(code);
                            cx.emit(TerminalViewEvent::Exited);
                        }
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        Self {
            terminal,
            profile,
            title: SharedString::default(),
            run_name: None,
            folder: None,
            keep_on_exit: false,
            exit: None,
            focus_handle: cx.focus_handle(),
            palette,
            grid: (0, 0),
            cell_origin: Point::default(),
            cell_size: size(px(8.), px(16.)),
            app_cursor: false,
            bracketed_paste: false,
            alt_screen: false,
            selection: None,
            selecting: false,
            scroll_remainder: 0.,
            hover: None,
            ctrl: false,
            link: None,
            scroll_x: 0,
            scroll_x_remainder: 0.,
            content_columns: 0,
            hbar: None,
            hbar_drag: None,
            hbar_hovered: false,
            reveal_cursor: false,
            reveal_columns: None,
            generation: 0,
            shaped: None,
            _events: task,
        }
    }

    /// The screen may show differently: the next frame takes a snapshot
    /// and shapes it again.
    fn screen_changed(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        cx.notify();
    }

    /// Scrolls the history by `lines` (positive: up, into the past).
    fn scroll_history(&mut self, lines: i32, cx: &mut Context<Self>) {
        self.terminal.scroll(lines);
        self.screen_changed(cx);
    }

    fn tab_title(&self) -> SharedString {
        let title = match &self.run_name {
            Some(name) => name.trim(),
            None => self.title.trim(),
        };
        let title = if title.is_empty() {
            self.profile
        } else {
            title
        };
        // Long titles are usually paths; keep the end.
        let chars: Vec<char> = title.chars().collect();
        let mut title = if chars.len() > 28 {
            format!("…{}", chars[chars.len() - 27..].iter().collect::<String>())
        } else {
            title.to_owned()
        };
        // Run names already say which folder.
        if let (Some(folder), None) = (&self.folder, &self.run_name) {
            title = format!("{title} ({folder})");
        }
        match self.exit {
            Some(Some(code)) if self.keep_on_exit => format!("{title} (exit {code})").into(),
            Some(None) if self.keep_on_exit => format!("{title} (ended)").into(),
            _ => title.into(),
        }
    }

    /// The tab's run name (`run_command`/`run_program`).
    pub fn run_name(&self) -> Option<&str> {
        self.run_name.as_ref().map(|name| name.as_ref())
    }

    /// The tab as `next_code context` reports it (read on request only:
    /// the screen snapshot copies the grid).
    pub(crate) fn context_facts(&self) -> crate::pinpoint::context::TerminalFacts {
        let shell = self.terminal.shell_state();
        crate::pinpoint::context::TerminalFacts {
            // The shell's whole title (the tab cuts long ones); the run name
            // is reported apart.
            title: match self.title.trim() {
                "" => self.profile.to_owned(),
                title => title.to_owned(),
            },
            folder: self.folder.as_ref().map(ToString::to_string),
            run_name: self.run_name.as_ref().map(ToString::to_string),
            screen: self.terminal.snapshot().text(),
            exit: self.exit,
            cwd: shell.cwd,
            last_command_exit: shell.last_exit,
        }
    }

    /// Whether the process exited, and its exit code when known (tests).
    #[cfg(test)]
    pub fn exit(&self) -> Option<Option<i32>> {
        self.exit
    }

    /// Ctrl+C, as typed.
    pub fn interrupt(&mut self, cx: &mut Context<Self>) {
        if self.exit.is_none() {
            self.write(vec![0x03], cx);
        }
    }

    /// Ends the process, keeping its output on screen.
    pub fn kill(&mut self, cx: &mut Context<Self>) {
        if self.exit.is_none() {
            self.terminal.kill();
            self.exit = Some(None);
            self.screen_changed(cx);
        }
    }

    fn write(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        self.selection = None;
        self.reveal_cursor = true;
        self.terminal.scroll_to_bottom();
        self.terminal.write(bytes);
        self.screen_changed(cx);
    }

    fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection?;
        let (start, end) = (a.min(b), a.max(b));
        let snapshot = self.terminal.snapshot();
        let lines: Vec<String> = (start.row..=end.row.min(snapshot.rows.saturating_sub(1)))
            .map(|row| {
                let cells = snapshot.row(row);
                let from = if row == start.row { start.column } else { 0 };
                let to = if row == end.row {
                    end.column + 1
                } else {
                    cells.len()
                };
                let text: String = cells[from.min(cells.len())..to.min(cells.len())]
                    .iter()
                    .filter(|cell| !cell.spacer)
                    .map(|cell| cell.c)
                    .collect();
                text.trim_end().to_owned()
            })
            .collect();
        Some(lines.join("\n"))
    }

    fn copy(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.selected_text().filter(|text| !text.is_empty()) else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.selection = None;
        cx.notify();
        true
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let bytes = terminal::encode_paste(&text, self.bracketed_paste);
            self.write(bytes, cx);
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let m = keystroke.modifiers;
        let key = keystroke.key.as_str();
        // Windows Terminal conventions for clipboard and history scrolling.
        let handled = match (m.control, m.shift, key) {
            (true, true, "c") => {
                self.copy(cx);
                true
            }
            (true, _, "v") => {
                self.paste(cx);
                true
            }
            (true, false, "c") => self.copy(cx),
            (false, true, "pageup") => {
                self.scroll_history(self.grid.1 as i32, cx);
                true
            }
            (false, true, "pagedown") => {
                self.scroll_history(-(self.grid.1 as i32), cx);
                true
            }
            _ => false,
        };
        if handled {
            cx.stop_propagation();
            return;
        }

        if let Some(bytes) = Self::key_bytes(keystroke, self.app_cursor) {
            self.write(bytes, cx);
            cx.stop_propagation();
        }
    }

    /// What a key press sends to the shell, if anything. `app_cursor` is the
    /// terminal's DECCKM mode.
    fn key_bytes(keystroke: &gpui::Keystroke, app_cursor: bool) -> Option<Vec<u8>> {
        let m = keystroke.modifiers;
        let key = keystroke.key.as_str();
        let mods = Modifiers {
            shift: m.shift,
            alt: m.alt,
            ctrl: m.control,
        };
        let named = match key {
            "enter" => Some(Key::Enter),
            "tab" => Some(Key::Tab),
            "backspace" => Some(Key::Backspace),
            // gpui on Windows gives space no `key_char` (the character comes
            // as text input, which this view does not take): send it here.
            // Ctrl+Space is NUL, Alt+Space ESC + space.
            "space" => Some(Key::Char(' ')),
            "escape" => Some(Key::Escape),
            "up" => Some(Key::Up),
            "down" => Some(Key::Down),
            "left" => Some(Key::Left),
            "right" => Some(Key::Right),
            "home" => Some(Key::Home),
            "end" => Some(Key::End),
            "pageup" => Some(Key::PageUp),
            "pagedown" => Some(Key::PageDown),
            "insert" => Some(Key::Insert),
            "delete" => Some(Key::Delete),
            _ => key
                .strip_prefix('f')
                .and_then(|n| n.parse::<u8>().ok())
                .filter(|n| (1..=12).contains(n))
                .map(Key::F),
        };
        // AltGr arrives as ctrl+alt with the composed character; type that.
        let alt_gr = m.control && m.alt && keystroke.key_char.is_some();
        if let Some(named) = named {
            terminal::encode_key(named, mods, app_cursor)
        } else if (m.control || m.alt) && !alt_gr {
            let mut chars = key.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => terminal::encode_key(Key::Char(c), mods, app_cursor),
                _ => None,
            }
        } else {
            keystroke
                .key_char
                .as_ref()
                .map(|text| text.as_bytes().to_vec())
        }
    }

    /// The cell under `position`, past the columns scrolled out on the left.
    fn cell_at(&self, position: Point<Pixels>) -> CellPoint {
        let x = ((position.x - self.cell_origin.x) / self.cell_size.width).max(0.);
        let y = ((position.y - self.cell_origin.y) / self.cell_size.height).max(0.);
        let columns = self.content_columns.max(usize::from(self.grid.0));
        CellPoint {
            row: (y as usize).min(usize::from(self.grid.1).saturating_sub(1)),
            column: (x as usize + self.scroll_x).min(columns.saturating_sub(1)),
        }
    }

    /// How far the view can scroll right: the widest row past the view.
    fn max_scroll_x(&self) -> usize {
        self.content_columns
            .saturating_sub(usize::from(self.grid.0))
    }

    /// Scrolls `scroll` pixels from the left (whole columns), as the user
    /// put it.
    fn set_scroll_x(&mut self, scroll: Pixels) {
        let columns = (scroll / self.cell_size.width).round().max(0.) as usize;
        self.scroll_x = columns.min(self.max_scroll_x());
        self.reveal_cursor = false;
    }

    /// Scrolls sideways by a wheel's `dx` (gpui's sign: positive goes
    /// toward the start).
    fn scroll_sideways(&mut self, dx: Pixels, cx: &mut Context<Self>) {
        self.scroll_x_remainder -= dx / self.cell_size.width;
        let columns = self.scroll_x_remainder.trunc();
        if columns == 0. {
            return;
        }
        self.scroll_x_remainder -= columns;
        self.scroll_x = self
            .scroll_x
            .saturating_add_signed(columns as isize)
            .min(self.max_scroll_x());
        self.reveal_cursor = false;
        cx.notify();
    }

    /// Dragging the horizontal bar's thumb to window `x`.
    fn drag_hbar(&mut self, x: Pixels, cx: &mut Context<Self>) {
        if let (Some(grab), Some(bar)) = (self.hbar_drag, self.hbar) {
            self.set_scroll_x(bar.scroll_for(x, grab));
            cx.notify();
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        if let Some((grab, jump)) = self.hbar.and_then(|bar| bar.press(event.position)) {
            if let Some(scroll) = jump {
                self.set_scroll_x(scroll);
            }
            self.hbar_drag = Some(grab);
            cx.notify();
            return;
        }
        let cell = self.cell_at(event.position);
        if event.modifiers.control
            && let Some(link) = self.terminal.snapshot().link_at(cell.row, cell.column)
        {
            self.open_link(link.target, cx);
            return;
        }
        self.selection = Some((cell, cell));
        self.selecting = true;
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        // A thumb drag follows the mouse anywhere (see `TerminalElement`).
        if self.hbar_drag.is_some() {
            return;
        }
        let over_bar = self
            .hbar
            .is_some_and(|bar| bar.track.contains(&event.position));
        if over_bar != self.hbar_hovered {
            self.hbar_hovered = over_bar;
            cx.notify();
        }
        let cell = self.cell_at(event.position);
        if self.selecting
            && let Some((anchor, _)) = self.selection
        {
            self.selection = Some((anchor, cell));
            cx.notify();
        }
        if self.hover != Some(cell) || self.ctrl != event.modifiers.control {
            self.hover = Some(cell);
            self.ctrl = event.modifiers.control;
            cx.notify();
        }
    }

    fn on_hover(&mut self, hovered: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if !hovered && (self.hover.take().is_some() || self.hbar_hovered) {
            self.hbar_hovered = false;
            cx.notify();
        }
    }

    fn on_modifiers_changed(
        &mut self,
        event: &ModifiersChangedEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.ctrl != event.modifiers.control {
            self.ctrl = event.modifiers.control;
            cx.notify();
        }
    }

    /// Opens what Ctrl+click hit: URLs in the browser, file locations in
    /// the editor.
    fn open_link(&mut self, target: LinkTarget, cx: &mut Context<Self>) {
        match target {
            LinkTarget::Url(url) => cx.open_url(&url),
            LinkTarget::File { path, line, column } => {
                cx.emit(TerminalViewEvent::OpenLocation { path, line, column })
            }
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.selecting = false;
        if self.hbar_drag.take().is_some() {
            cx.notify();
        }
        if self.selection.is_some_and(|(a, b)| a == b) {
            self.selection = None;
            cx.notify();
        }
    }

    /// The wheel scrolls the history; sideways (Shift, trackpads, or over
    /// the horizontal bar) it scrolls wide lines.
    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let line_height = self.cell_size.height;
        let over_bar = self
            .hbar
            .is_some_and(|bar| bar.track.contains(&event.position));
        let dx = if over_bar {
            hscrollbar::wheel_over_bar(event, line_height)
        } else {
            hscrollbar::wheel_dx(event, line_height)
        };
        if dx != px(0.) {
            if self.hbar.is_some() {
                self.scroll_sideways(dx, cx);
            }
            return;
        }
        let delta = event.delta.pixel_delta(line_height).y / line_height;
        self.scroll_remainder += delta;
        let lines = self.scroll_remainder.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.scroll_remainder -= lines as f32;
        if self.alt_screen {
            // Full-screen apps (less, vim) scroll with arrow keys.
            let key = if lines > 0 { Key::Up } else { Key::Down };
            for _ in 0..lines.unsigned_abs() {
                if let Some(bytes) =
                    terminal::encode_key(key, Modifiers::default(), self.app_cursor)
                {
                    self.terminal.write(bytes);
                }
            }
        } else {
            self.scroll_history(lines, cx);
        }
    }

    /// Resizes the pty to fit `bounds` and shapes the visible screen; a
    /// frame whose inputs are those of the last one (nothing printed, the
    /// mouse at rest) paints the screen shaped then.
    fn layout(
        &mut self,
        bounds: Bounds<Pixels>,
        pin_path: &SharedString,
        window: &mut Window,
    ) -> Rc<TerminalLayout> {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let base = style.font();
        let mut key = LayoutKey {
            generation: self.generation,
            bounds,
            font: base.clone(),
            font_size,
            line_height,
            scroll_x: self.scroll_x,
            selection: self.selection,
            pin_path: pin_path.clone(),
        };
        // The last snapshot, when the text is the same but the mouse moved
        // onto another link: shaped again without taking a new one.
        let mut reuse: Option<Rc<Snapshot>> = None;
        if let Some(shaped) = &mut self.shaped
            && shaped.key == key
        {
            if shaped.hover != self.hover {
                let link = self
                    .hover
                    .and_then(|cell| shaped.snapshot.link_at(cell.row, cell.column));
                shaped.hover = self.hover;
                if link != self.link {
                    self.link = link;
                    reuse = Some(shaped.snapshot.clone());
                }
            }
            if reuse.is_none() {
                let layout = shaped.layout.clone();
                self.record_pins(&layout, bounds, pin_path, window);
                return layout;
            }
        }
        let text_system = window.text_system().clone();
        let cell_width = text_system
            .shape_line(
                "M".into(),
                font_size,
                &[TextRun {
                    len: 1,
                    font: base.clone(),
                    color: Hsla::default(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            )
            .width;
        let columns = ((bounds.size.width / cell_width).floor() as u16).max(2);
        let rows = ((bounds.size.height / line_height).floor() as u16).max(1);
        if (columns, rows) != self.grid {
            self.grid = (columns, rows);
            self.terminal.resize(columns, rows);
        }
        self.cell_origin = bounds.origin;
        self.cell_size = size(cell_width, line_height);

        let fresh = reuse.is_none();
        let snapshot = match reuse {
            Some(snapshot) => snapshot,
            None => Rc::new(self.terminal.snapshot()),
        };
        self.app_cursor = snapshot.app_cursor;
        self.bracketed_paste = snapshot.bracketed_paste;
        self.alt_screen = snapshot.alt_screen;
        // Rows wider than the view scroll sideways; nothing wider (a
        // `clear`, a full-screen program) brings the view back to the start.
        let visible = usize::from(columns);
        self.content_columns = snapshot.columns;
        if let Some((start, end)) = self.reveal_columns.take() {
            self.scroll_x = reveal(self.scroll_x, visible, start, end);
        }
        if self.reveal_cursor
            && let Some((_, column)) = snapshot.cursor
        {
            self.scroll_x = reveal(self.scroll_x, visible, column, column + 1);
        }
        self.scroll_x = self.scroll_x.min(self.max_scroll_x());
        let hbar = HScrollbar::new(
            bounds,
            cell_width * visible as f32,
            cell_width * snapshot.columns as f32,
            cell_width * self.scroll_x as f32,
        );
        if hbar.is_none() {
            self.hbar_drag = None;
            self.hbar_hovered = false;
        }
        self.hbar = hbar;
        // Where column 0 would be.
        let origin = point(
            bounds.left() - cell_width * self.scroll_x as f32,
            bounds.top(),
        );
        // Text moves under a resting mouse too: find the link whenever
        // the screen changed.
        if fresh {
            self.link = self
                .hover
                .and_then(|cell| snapshot.link_at(cell.row, cell.column));
        }
        let link = self.link.as_ref();
        let background = self.palette.background;

        // The columns in view (and the one cut by the right edge).
        let shown = self.scroll_x..snapshot.columns.min(self.scroll_x + visible + 1);
        let mut lines = Vec::with_capacity(snapshot.rows);
        let mut backgrounds = Vec::new();
        for row in 0..snapshot.rows {
            let cells = &snapshot.row(row)[shown.clone()];
            let mut text = String::with_capacity(cells.len());
            let mut runs: Vec<TextRun> = Vec::new();
            // What the last run was made of (bold, italic, colour,
            // underline, strikeout): a cell with the same joins it.
            let mut last: Option<(bool, bool, Rgb, bool, bool)> = None;
            for (column, cell) in (shown.start..).zip(cells) {
                if cell.bg != background {
                    let width = if cell.wide { 2. } else { 1. };
                    backgrounds.push((
                        Bounds::new(
                            point(
                                origin.x + cell_width * column as f32,
                                origin.y + line_height * row as f32,
                            ),
                            size(cell_width * width, line_height),
                        ),
                        to_hsla(cell.bg),
                    ));
                }
                // The second half of a wide character shows nothing, or a
                // blank when its first half is scrolled out.
                if cell.spacer && column > shown.start {
                    continue;
                }
                let c = match cell.c {
                    _ if cell.spacer => ' ',
                    '\0' => ' ',
                    c => c,
                };
                text.push(c);
                let underline =
                    cell.underline || link.is_some_and(|link| link.contains(row, column));
                let attributes = (cell.bold, cell.italic, cell.fg, underline, cell.strikeout);
                if last == Some(attributes)
                    && let Some(run) = runs.last_mut()
                {
                    run.len += c.len_utf8();
                    continue;
                }
                last = Some(attributes);
                let mut font = base.clone();
                if cell.bold {
                    font.weight = FontWeight::BOLD;
                }
                if cell.italic {
                    font.style = FontStyle::Italic;
                }
                let color = to_hsla(cell.fg);
                runs.push(TextRun {
                    len: c.len_utf8(),
                    font,
                    color,
                    background_color: None,
                    underline: underline.then_some(UnderlineStyle {
                        color: Some(color),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    strikethrough: cell.strikeout.then_some(StrikethroughStyle {
                        color: Some(color),
                        thickness: px(1.),
                    }),
                });
            }
            lines.push(text_system.shape_line(text.into(), font_size, &runs, None));
        }

        let cursor = snapshot.cursor.map(|(row, column)| {
            Bounds::new(
                point(
                    origin.x + cell_width * column as f32,
                    origin.y + line_height * row as f32,
                ),
                size(cell_width, line_height),
            )
        });
        let selection = self.selection_bounds(&snapshot, origin, cell_width, line_height);
        let layout = Rc::new(TerminalLayout {
            lines,
            backgrounds,
            cursor,
            selection,
            origin,
            line_height,
            background: to_hsla(background),
            cursor_color: to_hsla(self.palette.cursor),
            hbar: self.hbar,
            screen: pin_text::Screen::new(&snapshot),
        });
        // The next frame starts from where this one scrolled to.
        key.scroll_x = self.scroll_x;
        self.shaped = Some(Shaped {
            key,
            hover: self.hover,
            snapshot,
            layout: layout.clone(),
        });
        self.record_pins(&layout, bounds, pin_path, window);
        layout
    }

    /// Pins and Pinpoint text see what shows (text scrolled out is
    /// clipped); what agents want highlighted or picked changes between
    /// frames, so this runs every frame.
    fn record_pins(
        &self,
        layout: &TerminalLayout,
        bounds: Bounds<Pixels>,
        pin_path: &SharedString,
        window: &mut Window,
    ) {
        if pin_path.is_empty() {
            return;
        }
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            pin_text::record(
                &layout.screen,
                pin_path,
                layout.origin,
                self.cell_size,
                window,
            );
            if let Some(bar) = &self.hbar {
                bar.record_pins(pin_path, window);
            }
        });
    }

    /// Cells selected, per row, for a grid whose column 0 is at `origin`.
    fn selection_bounds(
        &self,
        snapshot: &Snapshot,
        origin: Point<Pixels>,
        cell_width: Pixels,
        line_height: Pixels,
    ) -> Vec<Bounds<Pixels>> {
        let Some((a, b)) = self.selection.filter(|(a, b)| a != b) else {
            return Vec::new();
        };
        let (start, end) = (a.min(b), a.max(b));
        (start.row..=end.row.min(snapshot.rows.saturating_sub(1)))
            .map(|row| {
                let from = if row == start.row { start.column } else { 0 };
                let to = if row == end.row {
                    end.column + 1
                } else {
                    snapshot.columns
                };
                Bounds::new(
                    point(
                        origin.x + cell_width * from as f32,
                        origin.y + line_height * row as f32,
                    ),
                    size(cell_width * to.saturating_sub(from) as f32, line_height),
                )
            })
            .collect()
    }
}

/// The scroll (first column shown of `visible`) that keeps `scroll` if
/// columns `start..end` show, else ends the view at `end` (what comes
/// before it shows too: the typed text before the cursor) but never past
/// `start`.
fn reveal(scroll: usize, visible: usize, start: usize, end: usize) -> usize {
    if start < scroll || end > scroll + visible {
        end.saturating_sub(visible).min(start)
    } else {
        scroll
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("terminal")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .p_1()
            .bg(to_hsla(self.palette.background))
            .font_family(theme::MONO_FONT)
            .text_size(px(theme::EDITOR_FONT_SIZE))
            .line_height(px(theme::EDITOR_LINE_HEIGHT))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_hover(cx.listener(Self::on_hover))
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .when(self.ctrl && self.link.is_some(), |terminal| {
                terminal.cursor(CursorStyle::PointingHand)
            })
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(TerminalElement {
                view: cx.entity(),
                pin_path: SharedString::default(),
            })
    }
}

struct TerminalLayout {
    /// From the first column in view.
    lines: Vec<ShapedLine>,
    backgrounds: Vec<(Bounds<Pixels>, Hsla)>,
    cursor: Option<Bounds<Pixels>>,
    selection: Vec<Bounds<Pixels>>,
    /// Where column 0 would be (left of the view when scrolled sideways).
    origin: Point<Pixels>,
    line_height: Pixels,
    background: Hsla,
    cursor_color: Hsla,
    hbar: Option<HScrollbar>,
    /// The screen's text for Pinpoint.
    screen: pin_text::Screen,
}

struct TerminalElement {
    view: Entity<TerminalView>,
    /// The enclosing pin (`…/terminal:<n>`), whose text is the screen's.
    pin_path: SharedString,
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = Rc<TerminalLayout>;

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
        self.pin_path = crate::pinpoint::current_path().unwrap_or_default();
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Rc<TerminalLayout> {
        self.view
            .update(cx, |view, _| view.layout(bounds, &self.pin_path, window))
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        layout: &mut Rc<TerminalLayout>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let view = self.view.read(cx);
        let focused = view.focus_handle.is_focused(window);
        let hbar_active = view.hbar_hovered || view.hbar_drag.is_some();
        // A thumb drag follows the mouse beyond the terminal too.
        if view.hbar_drag.is_some() {
            let view = self.view.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                if phase == DispatchPhase::Bubble {
                    view.update(cx, |view, cx| view.drag_hbar(event.position.x, cx));
                }
            });
        }
        window.paint_quad(fill(bounds, layout.background));
        // Rows can be wider than the view: show what is scrolled in.
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for (cell, color) in &layout.backgrounds {
                window.paint_quad(fill(*cell, *color));
            }
            for selected in &layout.selection {
                window.paint_quad(fill(*selected, theme::selection()));
            }
            if let Some(cursor) = layout.cursor {
                if focused {
                    window.paint_quad(fill(cursor, layout.cursor_color.opacity(0.7)));
                } else {
                    window.paint_quad(outline(cursor, layout.cursor_color, BorderStyle::Solid));
                }
            }
            for (row, line) in layout.lines.iter().enumerate() {
                let origin = point(
                    bounds.left(),
                    bounds.top() + layout.line_height * row as f32,
                );
                line.paint(origin, layout.line_height, window, cx).ok();
            }
        });
        if let Some(bar) = &layout.hbar {
            bar.paint(hbar_active, window);
        }
    }
}

pub enum TerminalPanelEvent {
    /// Open a file, at a 0-based line and column when known (Ctrl+click
    /// in a terminal).
    OpenFile {
        path: PathBuf,
        position: Option<(usize, usize)>,
    },
}

impl EventEmitter<TerminalPanelEvent> for TerminalPanel {}

pub struct TerminalPanel {
    project: Project,
    tabs: Vec<(Entity<TerminalView>, Subscription)>,
    active: usize,
    focus_handle: FocusHandle,
    error: Option<SharedString>,
    /// A shell was started (or offered to start) once; later the panel
    /// stays as the user leaves it.
    started: bool,
}

impl TerminalPanel {
    pub fn new(project: Project, cx: &mut Context<Self>) -> Self {
        Self {
            project,
            tabs: Vec::new(),
            active: 0,
            focus_handle: cx.focus_handle(),
            error: None,
            started: false,
        }
    }

    /// Focuses the active terminal, starting the default shell if none runs.
    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty()
            && let Some(profile) = profiles(&self.project).into_iter().next()
        {
            self.spawn(&profile, window, cx);
        }
        match self.tabs.get(self.active) {
            Some((view, _)) => window.focus(&view.focus_handle(cx)),
            None => window.focus(&self.focus_handle),
        }
    }

    /// Runs `command_line` in a tab of the default shell (started in `dir`,
    /// default the first folder) named `title`, so its output stays
    /// visible; running the same title again replaces that tab (stopping
    /// what still runs there) in place.
    pub fn run_command(
        &mut self,
        title: &str,
        command_line: &str,
        dir: Option<&Path>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = profiles(&self.project).into_iter().next() else {
            return;
        };
        let previous = self.tabs.iter().position(|(view, _)| {
            view.read(cx)
                .run_name
                .as_ref()
                .is_some_and(|name| name.as_ref() == title)
        });
        if let Some(view) = self.spawn_in(&profile, dir, window, cx) {
            let line = format!("{command_line}\r");
            view.update(cx, |view, cx| {
                view.run_name = Some(title.to_owned().into());
                view.write(line.into_bytes(), cx);
            });
            if let Some(index) = previous {
                // Dropping the old view shuts its shell down.
                drop(self.tabs.remove(index));
                let tab = self.tabs.pop().expect("just spawned");
                self.tabs.insert(index, tab);
                self.active = index;
            }
        }
    }

    /// Runs `program` (with `args`, in the project root where the project
    /// lives) in a tab named `title` that stays when it exits, showing the
    /// exit code; running the same title again replaces that tab (ending
    /// what still runs there) in place. Focus stays where it is. `tap`
    /// follows the output.
    pub fn run_program(
        &mut self,
        title: &str,
        program: &str,
        args: &[String],
        tap: Option<OutputTap>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Entity<TerminalView>, String> {
        let view = spawn_program(Some(&self.project), title, program, args, tap, cx)
            .map_err(|err| format!("could not start {program}: {err}"))?;
        let subscription = self.subscribe_view(&view, window, cx);
        let previous = self
            .tabs
            .iter()
            .position(|(tab, _)| tab.read(cx).run_name() == Some(title));
        match previous {
            Some(index) => {
                // Dropping the old view ends its process.
                self.tabs[index] = (view.clone(), subscription);
                self.active = index;
            }
            None => {
                self.tabs.push((view.clone(), subscription));
                self.active = self.tabs.len() - 1;
            }
        }
        self.started = true;
        self.error = None;
        cx.notify();
        Ok(view)
    }

    /// Shows and focuses `view`'s tab; false if it is not one of ours.
    pub fn show(
        &mut self,
        view: &Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(index) = self.tabs.iter().position(|(tab, _)| tab == view) else {
            return false;
        };
        self.active = index;
        window.focus(&view.focus_handle(cx));
        cx.notify();
        true
    }

    /// The shown tab's view and the number of tabs (`next_code context`).
    pub(crate) fn active_view(&self) -> (Option<&Entity<TerminalView>>, usize) {
        (
            self.tabs.get(self.active).map(|(view, _)| view),
            self.tabs.len(),
        )
    }

    /// The tabs' views (tests).
    #[cfg(test)]
    pub fn views(&self) -> Vec<Entity<TerminalView>> {
        self.tabs.iter().map(|(view, _)| view.clone()).collect()
    }

    /// Starts the default shell in a new tab and focuses it, in the folder
    /// of `near` (the active editor's file), else the first folder.
    pub fn new_terminal(
        &mut self,
        near: Option<&Path>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(profile) = profiles(&self.project).into_iter().next() {
            let dir = shell_folder(&self.project, near);
            self.spawn_in(&profile, dir.as_deref(), window, cx);
        }
    }

    /// Starts the default shell in `dir` (a project folder) in a new tab
    /// and focuses it (the Files tree's Open in Terminal).
    pub fn new_terminal_in(&mut self, dir: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(profile) = profiles(&self.project).into_iter().next() {
            self.spawn_in(&profile, Some(dir), window, cx);
        }
    }

    /// The virtualenv (`.venv`) of the folder at `folder` as `(dir, bin
    /// dir)`, if any.
    fn venv(&self, folder: &Path) -> Option<(PathBuf, PathBuf)> {
        let venv = self.project.join(folder, ".venv");
        let bin = self.project.join(
            &venv,
            if self.project.is_remote() || !cfg!(windows) {
                "bin"
            } else {
                "Scripts"
            },
        );
        matches!(self.project.kind(&bin), Ok(Some(FileKind::Dir))).then_some((venv, bin))
    }

    fn spawn(
        &mut self,
        profile: &Profile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<TerminalView>> {
        self.spawn_in(profile, None, window, cx)
    }

    /// Starts `profile`'s shell in `cwd` (default: the first folder), with
    /// the virtualenv of the folder holding it.
    fn spawn_in(
        &mut self,
        profile: &Profile,
        cwd: Option<&Path>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<TerminalView>> {
        let mut options = TerminalOptions::default_shell();
        // Existing terminals keep the palette they started with.
        let palette = theme::current().terminal;
        let folder = self.project.root_for(cwd.unwrap_or(self.project.root()));
        let venv = self.venv(&folder.path);
        let folder_name: Option<SharedString> = self
            .project
            .is_multi_root()
            .then(|| folder.name.clone().into());
        let started = if profile.remote {
            self.project.spawn_remote_terminal(
                None,
                cwd,
                (options.columns, options.rows, options.scrollback),
                palette,
                None,
            )
        } else {
            if !profile.program.is_empty() {
                options.program = profile.program.into();
                options.args = profile.args.iter().map(|&arg| arg.into()).collect();
            }
            options.cwd = Some(cwd.unwrap_or(self.project.root()).to_path_buf());
            // The project's virtualenv, activated as its `activate` would.
            if let Some((venv, bin)) = &venv {
                let path = std::env::var_os("PATH").unwrap_or_default();
                let path = std::env::join_paths(
                    std::iter::once(bin.clone()).chain(std::env::split_paths(&path)),
                )
                .unwrap_or(path);
                options
                    .env
                    .insert("VIRTUAL_ENV".into(), venv.to_string_lossy().into_owned());
                options
                    .env
                    .insert("PATH".into(), path.to_string_lossy().into_owned());
            }
            Terminal::spawn(options, palette, None)
        };
        let (terminal, events) = match started {
            Ok(started) => started,
            Err(err) => {
                self.error = Some(format!("Could not start {}: {err}", profile.name).into());
                cx.notify();
                return None;
            }
        };
        self.error = None;
        let name = profile.name;
        let view = cx.new(|cx| {
            let mut view = TerminalView::new(terminal, events, name, palette, cx);
            view.folder = folder_name;
            view
        });
        // Remote login shells set PATH from their profiles: activate after.
        if profile.remote
            && let Some((venv, _)) = &venv
        {
            let activate = match cwd {
                None => ". .venv/bin/activate\r".to_owned(),
                Some(_) => {
                    let script = self.project.join(venv, "bin/activate");
                    let quoted = script.to_string_lossy().replace('\'', r"'\''");
                    format!(". '{quoted}'\r")
                }
            };
            view.update(cx, |view, cx| view.write(activate.into_bytes(), cx));
        }
        let subscription = self.subscribe_view(&view, window, cx);
        self.started = true;
        window.focus(&view.focus_handle(cx));
        self.tabs.push((view.clone(), subscription));
        self.active = self.tabs.len() - 1;
        cx.notify();
        Some(view)
    }

    fn subscribe_view(
        &mut self,
        view: &Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(view, window, |this, view, event, window, cx| match event {
            TerminalViewEvent::TitleChanged => cx.notify(),
            // Like VS Code: a shell that exits takes its tab with it; a
            // program's tab stays to show its output.
            TerminalViewEvent::Exited => {
                if view.read(cx).keep_on_exit {
                    cx.notify();
                } else if let Some(index) = this.tabs.iter().position(|(tab, _)| tab == view) {
                    this.close(index, window, cx);
                }
            }
            TerminalViewEvent::OpenLocation { path, line, column } => {
                let path = this.project.path(path);
                let position = line.map(|line| {
                    (
                        (line as usize).saturating_sub(1),
                        column.map_or(0, |column| (column as usize).saturating_sub(1)),
                    )
                });
                cx.emit(TerminalPanelEvent::OpenFile { path, position });
            }
        })
    }

    fn close(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        // Dropping the view shuts its shell down.
        drop(self.tabs.remove(index));
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        if let Some((view, _)) = self.tabs.get(self.active) {
            window.focus(&view.focus_handle(cx));
        }
        cx.notify();
    }
}

impl Focusable for TerminalPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Shown without a shell (restored open from the saved layout):
        // start one, as opening the panel does, but leave focus where it is.
        // Only the first time: closing every shell keeps the panel empty.
        if !self.started {
            self.started = true;
            cx.defer_in(window, |this, window, cx| {
                if this.tabs.is_empty()
                    && let Some(profile) = profiles(&this.project).into_iter().next()
                {
                    let focused = window.focused(cx);
                    this.spawn(&profile, window, cx);
                    if let Some(focused) = focused {
                        window.focus(&focused);
                    }
                }
            });
        }
        let tabs = self.tabs.iter().enumerate().map(|(index, (view, _))| {
            let active = index == self.active;
            let title = view.read(cx).tab_title();
            div()
                .id(("terminal-tab", index))
                .h_full()
                .px_2()
                .flex()
                .items_center()
                .gap_2()
                .cursor_pointer()
                .text_sm()
                .text_color(if active {
                    theme::text()
                } else {
                    theme::muted()
                })
                .when(active, |tab| tab.bg(theme::background()))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.active = index;
                    if let Some((view, _)) = this.tabs.get(index) {
                        window.focus(&view.focus_handle(cx));
                    }
                    cx.notify();
                }))
                .child(
                    title
                        .clone()
                        .pin("title")
                        .text(title.clone())
                        .interactive(false),
                )
                .child(
                    div()
                        .id(("terminal-close", index))
                        .text_color(theme::muted())
                        .hover(|style| style.text_color(theme::text()))
                        .child("×")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.close(index, window, cx);
                        }))
                        .pin("close")
                        .label("Close Terminal"),
                )
                .pin(format!("tab:{}", index + 1))
                .label(title)
                .interactive(true)
        });
        let new_buttons = profiles(&self.project).into_iter().map(|profile| {
            let new_key = format!("new:{}", profile.name);
            let new_label = format!("New {} Terminal", profile.name);
            div()
                .id(SharedString::from(format!("new-terminal-{}", profile.name)))
                .px_2()
                .text_xs()
                .text_color(theme::muted())
                .cursor_pointer()
                .hover(|style| style.text_color(theme::text()))
                .child(format!("+ {}", profile.name))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.spawn(&profile, window, cx);
                }))
                .pin(new_key)
                .label(new_label)
        });
        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(26.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(theme::border())
                    .children(tabs)
                    .child(div().flex_1())
                    .children(new_buttons)
                    .pin("tabs"),
            )
            .children(self.error.clone().map(|error| {
                div()
                    .px_2()
                    .py_1()
                    .text_sm()
                    .text_color(theme::error())
                    .child(selectable("error", error))
            }))
            .child(
                div().flex_1().min_h_0().children(
                    self.tabs
                        .get(self.active)
                        .map(|(view, _)| view.clone().pin(format!("terminal:{}", self.active + 1))),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(keys: &str, key_char: Option<&str>) -> Option<Vec<u8>> {
        let mut keystroke = gpui::Keystroke::parse(keys).unwrap();
        keystroke.key_char = key_char.map(Into::into);
        TerminalView::key_bytes(&keystroke, false)
    }

    #[test]
    fn space_types_even_without_a_key_char() {
        // As gpui reports it on Windows: no `key_char` for space.
        assert_eq!(press("space", None), Some(b" ".to_vec()));
        assert_eq!(press("ctrl-space", None), Some(vec![0]));
        assert_eq!(press("alt-space", None), Some(b"\x1b ".to_vec()));
        assert_eq!(press("backspace", None), Some(vec![0x7f]));
        assert_eq!(press("a", Some("a")), Some(b"a".to_vec()));
    }

    #[test]
    fn new_terminals_start_in_the_active_files_folder() {
        let base = std::env::temp_dir().join("next_code_terminal_roots");
        let (api, web) = (base.join("api"), base.join("web"));
        let project = Project::local_folders(&[&api, &web]);
        let file = |folder: &Path| folder.join("src").join("main.py");
        assert_eq!(shell_folder(&project, Some(&file(&web))), Some(web.clone()));
        // The first folder is the shell's default, as is no file or one
        // outside the folders.
        assert_eq!(shell_folder(&project, Some(&file(&api))), None);
        assert_eq!(shell_folder(&project, None), None);
        assert_eq!(shell_folder(&project, Some(&file(&base))), None);
        // One folder: always the default.
        let single = Project::local(web.clone());
        assert_eq!(shell_folder(&single, Some(&file(&web))), None);
    }

    #[test]
    fn revealing_columns_shows_what_comes_before_them() {
        // 10 columns shown from column 20.
        assert_eq!(reveal(20, 10, 22, 25), 20, "already in view");
        assert_eq!(reveal(20, 10, 15, 16), 6);
        assert_eq!(reveal(20, 10, 5, 8), 0);
        assert_eq!(reveal(20, 10, 33, 34), 24);
        // Wider than the view: its start shows.
        assert_eq!(reveal(0, 10, 40, 60), 40);
    }

    struct Sink;

    impl terminal::RemotePty for Sink {
        fn write(&self, _: Vec<u8>) {}
        fn resize(&self, _: u16, _: u16) {}
        fn close(&self) {}
    }

    type Output = std::sync::mpsc::Sender<terminal::PtyOutput>;

    /// A terminal view filling a test window, fed by the returned sender.
    fn terminal_window(
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<TerminalView>, &mut gpui::VisualTestContext, Output) {
        let (output, output_rx) = std::sync::mpsc::channel();
        let (view, cx) = cx.add_window_view(|_, cx| {
            let (terminal, events) = Terminal::attach(
                40,
                10,
                100,
                theme::current().terminal,
                std::sync::Arc::new(Sink),
                output_rx,
                None,
            )
            .unwrap();
            TerminalView::new(terminal, events, "sh", theme::current().terminal, cx)
        });
        cx.run_until_parked();
        (view, cx, output)
    }

    /// Sends `data` and redraws once the screen ends with `last`.
    fn feed(
        view: &Entity<TerminalView>,
        cx: &mut gpui::VisualTestContext,
        output: &Output,
        data: &str,
        last: &str,
    ) {
        output
            .send(terminal::PtyOutput::Data(data.as_bytes().to_vec()))
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !view
            .read_with(cx, |view, _| view.terminal.snapshot().text())
            .trim_end()
            .ends_with(last)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "{last:?} never showed"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
    }

    /// The window point of cell (`row`, `column`) of the view (not
    /// counting the columns scrolled out).
    fn cell_center(
        view: &Entity<TerminalView>,
        cx: &mut gpui::VisualTestContext,
        row: usize,
        column: usize,
    ) -> Point<Pixels> {
        view.read_with(cx, |view, _| {
            view.cell_origin
                + point(
                    view.cell_size.width * (column as f32 + 0.5),
                    view.cell_size.height * (row as f32 + 0.5),
                )
        })
    }

    #[gpui::test]
    fn wide_lines_scroll_sideways_with_a_bar(cx: &mut gpui::TestAppContext) {
        let (view, cx, output) = terminal_window(cx);
        let columns = view.read_with(cx, |view, _| usize::from(view.grid.0));
        assert!(columns > 20, "{columns} columns");
        assert!(
            view.read_with(cx, |view, _| view.hbar.is_none()),
            "nothing overflows yet"
        );
        // A line 2.5 views wide, its characters cycling through a-z.
        let width = columns * 5 / 2;
        let line: String = (0..width)
            .map(|i| (b'a' + (i % 26) as u8) as char)
            .collect();
        feed(&view, cx, &output, &format!("{line}\r\n$ "), "$");
        let (content, scroll, bar) = view.read_with(cx, |view, _| {
            (view.content_columns, view.scroll_x, view.hbar)
        });
        assert_eq!(content, width, "the line is one row, not wrapped");
        assert_eq!(scroll, 0);
        let bar = bar.expect("a bar while the line overflows");

        // Shift+wheel down scrolls right (Wayland's unturned delta).
        let middle = cell_center(&view, cx, 3, 3);
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: middle,
            delta: gpui::ScrollDelta::Lines(point(0., -2.)),
            modifiers: gpui::Modifiers::shift(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        let scroll = view.read_with(cx, |view, _| view.scroll_x);
        assert!(scroll > 0, "shift+wheel scrolled sideways");
        // A plain wheel does not (nothing to scroll back to either).
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: middle,
            delta: gpui::ScrollDelta::Lines(point(0., -2.)),
            modifiers: gpui::Modifiers::none(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        assert_eq!(view.read_with(cx, |view, _| view.scroll_x), scroll);

        // Selecting maps to the cells scrolled in: columns 2..=5 of the view.
        let (from, to) = (cell_center(&view, cx, 0, 2), cell_center(&view, cx, 0, 5));
        cx.simulate_mouse_down(from, MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(to, MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_up(to, MouseButton::Left, gpui::Modifiers::none());
        assert_eq!(
            view.read_with(cx, |view, _| view.selected_text()),
            Some(line[scroll + 2..=scroll + 5].to_owned())
        );

        // Pressing the track's right end jumps there; dragging past it
        // stops at the end of the line.
        let y = bar.track.center().y;
        let right = point(bar.track.right() - px(2.), y);
        cx.simulate_mouse_down(right, MouseButton::Left, gpui::Modifiers::none());
        cx.simulate_mouse_move(
            point(bar.track.right() + px(200.), y),
            MouseButton::Left,
            gpui::Modifiers::none(),
        );
        cx.simulate_mouse_up(right, MouseButton::Left, gpui::Modifiers::none());
        assert_eq!(view.read_with(cx, |view, _| view.scroll_x), width - columns);

        // New output keeps the offset (and follows at the bottom).
        feed(&view, cx, &output, "more\r\n$ ", "$");
        assert_eq!(view.read_with(cx, |view, _| view.scroll_x), width - columns);
        // Pressing the track's left end goes back to the start.
        let bar = view.read_with(cx, |view, _| view.hbar).unwrap();
        let left = point(bar.track.left() + px(2.), y);
        cx.simulate_click(left, gpui::Modifiers::none());
        assert_eq!(view.read_with(cx, |view, _| view.scroll_x), 0);

        // Scrolled right again, a clear (screen and history) ends it.
        view.update(cx, |view, _| view.scroll_x = 10);
        feed(&view, cx, &output, "\x1b[H\x1b[2J\x1b[3Jdone", "done");
        view.read_with(cx, |view, _| {
            assert_eq!(view.scroll_x, 0);
            assert!(view.hbar.is_none());
        });
    }
}
