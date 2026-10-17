//! Terminal panel: shell sessions in tabs, each rendered from a
//! `terminal::Snapshot`.

use std::path::{Path, PathBuf};

use futures::StreamExt;
use gpui::{
    App, BorderStyle, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, Entity,
    EventEmitter, FocusHandle, Focusable, FontStyle, FontWeight, GlobalElementId, Hsla,
    InspectorElementId, KeyBinding, KeyDownEvent, LayoutId, ModifiersChangedEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, NoAction, Pixels, Point, ScrollWheelEvent,
    ShapedLine, SharedString, Size, StrikethroughStyle, Style, Subscription, Task, TextRun,
    UnderlineStyle, Window, div, fill, outline, point, prelude::*, px, relative, rgb, size,
};
use terminal::{
    Key, LinkTarget, Modifiers, OutputTap, Palette, Rgb, ScreenLink, Snapshot, Terminal,
    TerminalEvent, TerminalOptions,
};
use worktree::FileKind;

use crate::{project::Project, selectable::selectable, theme};

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
    _events: Task<()>,
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
                    TerminalEvent::Wakeup => cx.notify(),
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
            _events: task,
        }
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
            cx.notify();
        }
    }

    fn write(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        self.selection = None;
        self.terminal.scroll_to_bottom();
        self.terminal.write(bytes);
        cx.notify();
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
                self.terminal.scroll(self.grid.1 as i32);
                cx.notify();
                true
            }
            (false, true, "pagedown") => {
                self.terminal.scroll(-(self.grid.1 as i32));
                cx.notify();
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

    fn cell_at(&self, position: Point<Pixels>) -> CellPoint {
        let x = ((position.x - self.cell_origin.x) / self.cell_size.width).max(0.);
        let y = ((position.y - self.cell_origin.y) / self.cell_size.height).max(0.);
        CellPoint {
            row: (y as usize).min(usize::from(self.grid.1).saturating_sub(1)),
            column: (x as usize).min(usize::from(self.grid.0).saturating_sub(1)),
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
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
        if !hovered && self.hover.take().is_some() {
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
        if self.selection.is_some_and(|(a, b)| a == b) {
            self.selection = None;
            cx.notify();
        }
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(self.cell_size.height).y / self.cell_size.height;
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
            self.terminal.scroll(lines);
            cx.notify();
        }
    }

    /// Resizes the pty to fit `bounds` and shapes the visible screen.
    fn layout(&mut self, bounds: Bounds<Pixels>, window: &mut Window) -> TerminalLayout {
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let base = style.font();
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

        let snapshot = self.terminal.snapshot();
        self.app_cursor = snapshot.app_cursor;
        self.bracketed_paste = snapshot.bracketed_paste;
        self.alt_screen = snapshot.alt_screen;
        // Text moves under a resting mouse too: find the link every frame.
        self.link = self
            .hover
            .and_then(|cell| snapshot.link_at(cell.row, cell.column));
        let background = self.palette.background;

        let mut lines = Vec::with_capacity(snapshot.rows);
        let mut backgrounds = Vec::new();
        for row in 0..snapshot.rows {
            let cells = snapshot.row(row);
            let mut text = String::with_capacity(cells.len());
            let mut runs: Vec<TextRun> = Vec::new();
            for (column, cell) in cells.iter().enumerate() {
                if cell.bg != background {
                    let width = if cell.wide { 2. } else { 1. };
                    backgrounds.push((
                        Bounds::new(
                            point(
                                bounds.left() + cell_width * column as f32,
                                bounds.top() + line_height * row as f32,
                            ),
                            size(cell_width * width, line_height),
                        ),
                        to_hsla(cell.bg),
                    ));
                }
                if cell.spacer {
                    continue;
                }
                let c = if cell.c == '\0' { ' ' } else { cell.c };
                let mut font = base.clone();
                if cell.bold {
                    font.weight = FontWeight::BOLD;
                }
                if cell.italic {
                    font.style = FontStyle::Italic;
                }
                let color = to_hsla(cell.fg);
                let run = TextRun {
                    len: c.len_utf8(),
                    font,
                    color,
                    background_color: None,
                    underline: (cell.underline
                        || self
                            .link
                            .as_ref()
                            .is_some_and(|link| link.contains(row, column)))
                    .then_some(UnderlineStyle {
                        color: Some(color),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    strikethrough: cell.strikeout.then_some(StrikethroughStyle {
                        color: Some(color),
                        thickness: px(1.),
                    }),
                };
                text.push(c);
                match runs.last_mut() {
                    Some(last)
                        if last.font == run.font
                            && last.color == run.color
                            && last.underline == run.underline
                            && last.strikethrough == run.strikethrough =>
                    {
                        last.len += run.len;
                    }
                    _ => runs.push(run),
                }
            }
            lines.push(text_system.shape_line(text.into(), font_size, &runs, None));
        }

        let cursor = snapshot.cursor.map(|(row, column)| {
            Bounds::new(
                point(
                    bounds.left() + cell_width * column as f32,
                    bounds.top() + line_height * row as f32,
                ),
                size(cell_width, line_height),
            )
        });
        let selection = self.selection_bounds(&snapshot, bounds.origin, cell_width, line_height);
        TerminalLayout {
            lines,
            backgrounds,
            cursor,
            selection,
            line_height,
            background: to_hsla(background),
            cursor_color: to_hsla(self.palette.cursor),
        }
    }

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
            .child(TerminalElement { view: cx.entity() })
    }
}

struct TerminalLayout {
    lines: Vec<ShapedLine>,
    backgrounds: Vec<(Bounds<Pixels>, Hsla)>,
    cursor: Option<Bounds<Pixels>>,
    selection: Vec<Bounds<Pixels>>,
    line_height: Pixels,
    background: Hsla,
    cursor_color: Hsla,
}

struct TerminalElement {
    view: Entity<TerminalView>,
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = TerminalLayout;

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
    ) -> TerminalLayout {
        self.view.update(cx, |view, _| view.layout(bounds, window))
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        layout: &mut TerminalLayout,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focused = self.view.read(cx).focus_handle.is_focused(window);
        window.paint_quad(fill(bounds, layout.background));
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
                .child(view.read(cx).tab_title())
                .child(
                    div()
                        .id(("terminal-close", index))
                        .text_color(theme::muted())
                        .hover(|style| style.text_color(theme::text()))
                        .child("×")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.close(index, window, cx);
                        })),
                )
        });
        let new_buttons = profiles(&self.project).into_iter().map(|profile| {
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
                    .children(new_buttons),
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
                div()
                    .flex_1()
                    .min_h_0()
                    .children(self.tabs.get(self.active).map(|(view, _)| view.clone())),
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
}
