//! Terminal sessions: a shell in a pseudo-terminal (ConPTY on Windows, a Unix
//! pty elsewhere) driven by `alacritty_terminal`'s emulator, plus a renderer-
//! neutral snapshot of the visible screen.

mod keys;

use std::{
    borrow::Cow,
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

pub use alacritty_terminal::vte::ansi::Rgb;
use alacritty_terminal::{
    event::{Event, EventListener, WindowSize},
    event_loop::{EventLoop, EventLoopSender, Msg},
    grid::{Dimensions, Scroll},
    sync::FairMutex,
    term::{Config, Term, TermMode, cell::Flags, color::Colors},
    tty::{self, Shell},
    vte::ansi::{Color, CursorShape, NamedColor},
};
use futures::channel::mpsc;
pub use keys::{Key, Modifiers, encode_key, encode_paste};
use parking_lot::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    /// The screen changed; take a new snapshot.
    Wakeup,
    Title(String),
    Bell,
    /// The shell exited (with its exit code when known).
    Exited(Option<i32>),
}

#[derive(Debug, Clone)]
pub struct TerminalOptions {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
    pub columns: u16,
    pub rows: u16,
    pub scrollback: usize,
}

impl TerminalOptions {
    /// The platform's default interactive shell.
    pub fn default_shell() -> Self {
        let (program, args) = if cfg!(windows) {
            ("powershell.exe".to_owned(), vec!["-NoLogo".to_owned()])
        } else {
            (
                std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into()),
                Vec::new(),
            )
        };
        Self {
            program,
            args,
            cwd: None,
            env: HashMap::new(),
            columns: 80,
            rows: 24,
            scrollback: 10_000,
        }
    }
}

/// The 16 ANSI colors plus default foreground, background and cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub ansi: [Rgb; 16],
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
}

const fn rgb(value: u32) -> Rgb {
    Rgb {
        r: (value >> 16) as u8,
        g: (value >> 8) as u8,
        b: value as u8,
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            ansi: [
                rgb(0x1e1f22),
                rgb(0xf75464),
                rgb(0x5fb865),
                rgb(0xe5c07b),
                rgb(0x56a8f5),
                rgb(0xc77dbb),
                rgb(0x2aacb8),
                rgb(0xbcbec4),
                rgb(0x5a5d63),
                rgb(0xff6b7a),
                rgb(0x7fd88a),
                rgb(0xf2c55c),
                rgb(0x6ca2ff),
                rgb(0xd89ee0),
                rgb(0x4fd1db),
                rgb(0xffffff),
            ],
            foreground: rgb(0xbcbec4),
            background: rgb(0x1e1f22),
            cursor: rgb(0xced0d6),
        }
    }
}

impl Palette {
    /// Color for a 256-color index or one of alacritty's named slots
    /// (foreground 256, background 257, cursor 258, dim 259–266, 267–268).
    fn indexed(&self, index: usize) -> Rgb {
        match index {
            0..=15 => self.ansi[index],
            16..=231 => {
                let i = index - 16;
                let level = |v: usize| if v == 0 { 0 } else { (55 + 40 * v) as u8 };
                Rgb {
                    r: level(i / 36),
                    g: level((i / 6) % 6),
                    b: level(i % 6),
                }
            }
            232..=255 => {
                let v = (8 + 10 * (index - 232)) as u8;
                Rgb { r: v, g: v, b: v }
            }
            256 | 267 => self.foreground,
            257 => self.background,
            258 => self.cursor,
            259..=266 => dim(self.ansi[index - 259]),
            268 => dim(self.foreground),
            _ => self.foreground,
        }
    }
}

fn dim(color: Rgb) -> Rgb {
    let scale = |v: u8| (u16::from(v) * 2 / 3) as u8;
    Rgb {
        r: scale(color.r),
        g: scale(color.g),
        b: scale(color.b),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotCell {
    pub c: char,
    pub fg: Rgb,
    pub bg: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikeout: bool,
    /// Occupies this and the next column.
    pub wide: bool,
    /// Second half of a wide character; draw nothing.
    pub spacer: bool,
}

/// The visible screen, resolved to colors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub columns: usize,
    pub rows: usize,
    /// Row-major, `columns * rows` cells.
    pub cells: Vec<SnapshotCell>,
    /// Row and column, when the cursor is visible on screen.
    pub cursor: Option<(usize, usize)>,
    /// Lines scrolled back into history (0 = following output).
    pub display_offset: usize,
    pub history: usize,
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    pub alt_screen: bool,
}

impl Snapshot {
    pub fn row(&self, row: usize) -> &[SnapshotCell] {
        &self.cells[row * self.columns..(row + 1) * self.columns]
    }

    /// Screen text with trailing blanks trimmed per line.
    pub fn text(&self) -> String {
        (0..self.rows)
            .map(|row| {
                let line: String = self
                    .row(row)
                    .iter()
                    .filter(|cell| !cell.spacer)
                    .map(|cell| cell.c)
                    .collect();
                line.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

struct Size {
    columns: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

#[derive(Clone)]
struct Listener {
    events: mpsc::UnboundedSender<TerminalEvent>,
    /// Set right after the event loop exists; replies to terminal queries.
    writer: Arc<OnceLock<EventLoopSender>>,
    window_size: Arc<Mutex<WindowSize>>,
    palette: Palette,
    exited: Arc<AtomicBool>,
}

impl Listener {
    fn reply(&self, text: String) {
        if let Some(writer) = self.writer.get() {
            writer.send(Msg::Input(text.into_bytes().into())).ok();
        }
    }
}

impl EventListener for Listener {
    // Runs on the I/O thread, often while the terminal is locked: never lock it here.
    fn send_event(&self, event: Event) {
        let event = match event {
            Event::Wakeup => TerminalEvent::Wakeup,
            Event::Title(title) => TerminalEvent::Title(title),
            Event::ResetTitle => TerminalEvent::Title(String::new()),
            Event::Bell => TerminalEvent::Bell,
            Event::PtyWrite(text) => return self.reply(text),
            Event::ColorRequest(index, format) => {
                return self.reply(format(self.palette.indexed(index)));
            }
            Event::TextAreaSizeRequest(format) => {
                return self.reply(format(*self.window_size.lock()));
            }
            Event::ChildExit(status) => {
                self.exited.store(true, Ordering::SeqCst);
                TerminalEvent::Exited(status.code())
            }
            Event::Exit if !self.exited.swap(true, Ordering::SeqCst) => TerminalEvent::Exited(None),
            _ => return,
        };
        self.events.unbounded_send(event).ok();
    }
}

pub struct Terminal {
    term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    window_size: Arc<Mutex<WindowSize>>,
    palette: Palette,
}

impl Terminal {
    pub fn spawn(
        options: TerminalOptions,
        palette: Palette,
    ) -> io::Result<(Self, mpsc::UnboundedReceiver<TerminalEvent>)> {
        let (events, events_rx) = mpsc::unbounded();
        let window_size = Arc::new(Mutex::new(window_size(options.columns, options.rows)));
        let listener = Listener {
            events,
            writer: Arc::default(),
            window_size: window_size.clone(),
            palette,
            exited: Arc::default(),
        };
        let config = Config {
            scrolling_history: options.scrollback,
            ..Config::default()
        };
        let size = Size {
            columns: usize::from(options.columns),
            rows: usize::from(options.rows),
        };
        let term = Arc::new(FairMutex::new(Term::new(config, &size, listener.clone())));

        let mut env = options.env;
        env.entry("TERM".into())
            .or_insert_with(|| "xterm-256color".into());
        env.entry("COLORTERM".into())
            .or_insert_with(|| "truecolor".into());
        let pty_options = tty::Options {
            shell: Some(Shell::new(options.program, options.args)),
            working_directory: options.cwd,
            drain_on_exit: true,
            env,
            ..tty::Options::default()
        };
        let pty = tty::new(&pty_options, *window_size.lock(), 0)?;
        let event_loop = EventLoop::new(term.clone(), listener.clone(), pty, true, false)?;
        let sender = event_loop.channel();
        listener.writer.set(sender.clone()).ok();
        event_loop.spawn();

        let terminal = Self {
            term,
            sender,
            window_size,
            palette,
        };
        Ok((terminal, events_rx))
    }

    /// Sends input bytes to the shell.
    pub fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        self.sender.send(Msg::Input(bytes.into())).ok();
    }

    pub fn resize(&self, columns: u16, rows: u16) {
        let size = window_size(columns.max(2), rows.max(1));
        *self.window_size.lock() = size;
        self.term.lock().resize(Size {
            columns: usize::from(size.num_cols),
            rows: usize::from(size.num_lines),
        });
        self.sender.send(Msg::Resize(size)).ok();
    }

    /// Scrolls the view into history (positive) or back toward the output.
    pub fn scroll(&self, lines: i32) {
        self.term.lock().scroll_display(Scroll::Delta(lines));
    }

    pub fn scroll_to_bottom(&self) {
        self.term.lock().scroll_display(Scroll::Bottom);
    }

    pub fn snapshot(&self) -> Snapshot {
        let term = self.term.lock();
        let content = term.renderable_content();
        let (columns, rows) = (term.columns(), term.screen_lines());
        let colors = content.colors;
        let resolve = |color: Color, bold: bool| -> Rgb {
            match color {
                Color::Spec(rgb) => rgb,
                Color::Named(named) => {
                    let mut index = named as usize;
                    // Bold draws the 8 base colors in their bright variants.
                    if bold && index < 8 {
                        index += 8;
                    }
                    color_at(colors, &self.palette, index)
                }
                Color::Indexed(index) => {
                    let index = if bold && index < 8 { index + 8 } else { index };
                    color_at(colors, &self.palette, usize::from(index))
                }
            }
        };
        let background = color_at(colors, &self.palette, NamedColor::Background as usize);
        let blank = SnapshotCell {
            c: ' ',
            fg: color_at(colors, &self.palette, NamedColor::Foreground as usize),
            bg: background,
            bold: false,
            italic: false,
            underline: false,
            strikeout: false,
            wide: false,
            spacer: false,
        };
        let mut cells = vec![blank; columns * rows];
        let offset = content.display_offset as i32;
        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset;
            let column = indexed.point.column.0;
            if row < 0 || row as usize >= rows || column >= columns {
                continue;
            }
            let cell = indexed.cell;
            let flags = cell.flags;
            let bold = flags.contains(Flags::BOLD);
            let mut fg = resolve(cell.fg, bold);
            let mut bg = resolve(cell.bg, false);
            if flags.contains(Flags::DIM) {
                fg = dim(fg);
            }
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if flags.contains(Flags::HIDDEN) {
                fg = bg;
            }
            cells[row as usize * columns + column] = SnapshotCell {
                c: cell.c,
                fg,
                bg,
                bold,
                italic: flags.contains(Flags::ITALIC),
                underline: flags.intersects(Flags::ALL_UNDERLINES),
                strikeout: flags.contains(Flags::STRIKEOUT),
                wide: flags.contains(Flags::WIDE_CHAR),
                spacer: flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
            };
        }

        let mode = content.mode;
        let cursor_point = content.cursor.point;
        let cursor_row = cursor_point.line.0 + offset;
        let cursor = (mode.contains(TermMode::SHOW_CURSOR)
            && content.cursor.shape != CursorShape::Hidden
            && (0..rows as i32).contains(&cursor_row))
        .then_some((cursor_row as usize, cursor_point.column.0));

        Snapshot {
            columns,
            rows,
            cells,
            cursor,
            display_offset: content.display_offset,
            history: term.grid().history_size(),
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.sender.send(Msg::Shutdown).ok();
    }
}

/// A color the program set via OSC, else the palette's.
fn color_at(colors: &Colors, palette: &Palette, index: usize) -> Rgb {
    colors[index].unwrap_or_else(|| palette.indexed(index))
}

fn window_size(columns: u16, rows: u16) -> WindowSize {
    WindowSize {
        num_lines: rows,
        num_cols: columns,
        cell_width: 1,
        cell_height: 1,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// A shell without user profiles, plus a command whose output (`abcd`)
    /// differs from its own text, so seeing it proves the shell ran it.
    fn test_shell() -> (TerminalOptions, &'static str) {
        let mut options = TerminalOptions::default_shell();
        let command = if cfg!(windows) {
            options.program = "powershell.exe".into();
            options.args = vec!["-NoLogo".into(), "-NoProfile".into()];
            "echo ('ab' + 'cd')\r"
        } else {
            options.program = "/bin/sh".into();
            options.args = Vec::new();
            "echo ab''cd\r"
        };
        options.columns = 60;
        options.rows = 20;
        (options, command)
    }

    fn wait_for(
        events: &mut mpsc::UnboundedReceiver<TerminalEvent>,
        mut done: impl FnMut(&TerminalEvent) -> bool,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            match events.try_recv() {
                Ok(event) if done(&event) => return true,
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        false
    }

    #[test]
    fn runs_commands_resizes_and_reports_exit() {
        let (options, command) = test_shell();
        let (terminal, mut events) = Terminal::spawn(options, Palette::default()).unwrap();
        terminal.write(command.as_bytes().to_vec());
        // The command's own text never contains `abcd` contiguously.
        let ran = wait_for(&mut events, |_| terminal.snapshot().text().contains("abcd"));
        assert!(
            ran,
            "command output never appeared:\n{}",
            terminal.snapshot().text()
        );

        terminal.resize(40, 10);
        let snapshot = terminal.snapshot();
        assert_eq!(
            (snapshot.columns, snapshot.rows, snapshot.cells.len()),
            (40, 10, 400)
        );

        terminal.write(&b"exit\r"[..]);
        assert!(
            wait_for(&mut events, |event| matches!(
                event,
                TerminalEvent::Exited(_)
            )),
            "no exit event"
        );
    }

    #[test]
    fn palette_covers_the_256_color_cube_and_greys() {
        let palette = Palette::default();
        assert_eq!(palette.indexed(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(
            palette.indexed(231),
            Rgb {
                r: 255,
                g: 255,
                b: 255
            }
        );
        assert_eq!(palette.indexed(196), Rgb { r: 255, g: 0, b: 0 });
        assert_eq!(palette.indexed(232), Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(
            palette.indexed(NamedColor::Background as usize),
            palette.background
        );
    }
}
