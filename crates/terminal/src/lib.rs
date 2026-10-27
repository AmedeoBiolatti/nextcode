//! Terminal sessions: a shell in a pseudo-terminal (ConPTY on Windows, a Unix
//! pty elsewhere, or a pty hosted by a remote agent) driven by
//! `alacritty_terminal`'s emulator, plus a renderer-neutral snapshot of the
//! visible screen.

mod keys;
mod links;
mod shell_state;

use std::{
    borrow::Cow,
    collections::HashMap,
    io::{self, Read},
    ops::Range,
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc,
    },
    thread,
};

pub use alacritty_terminal::vte::ansi::Rgb;
use alacritty_terminal::{
    event::{Event, EventListener, WindowSize},
    event_loop::{EventLoop, EventLoopSender, Msg},
    grid::{Dimensions, Scroll},
    index::{Column, Line},
    sync::FairMutex,
    term::{Config, Term, TermMode, cell::Flags, color::Colors},
    tty::{self, EventedPty, EventedReadWrite, Shell},
    vte::ansi::{Color, CursorShape, NamedColor, Processor, StdSyncHandler},
};
use futures::channel::mpsc;
pub use keys::{Key, Modifiers, encode_key, encode_paste};
pub use links::{Link, LinkTarget, find_links};
use parking_lot::Mutex;
use shell_state::ShellScanner;
pub use shell_state::ShellState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    /// The screen changed; take a new snapshot.
    Wakeup,
    Title(String),
    Bell,
    /// The shell exited (with its exit code when known).
    Exited(Option<i32>),
}

/// What a terminal's process printed, as raw bytes, then its end; for
/// callers that follow the output (e.g. to wait for a line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output<'a> {
    Data(&'a [u8]),
    /// The process exited (with its exit code when known); all its output
    /// was reported before.
    Exited(Option<i32>),
}

/// Receives a terminal's [`Output`], on its reader thread: never block.
pub type OutputTap = Arc<dyn Fn(Output<'_>) + Send + Sync>;

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

/// A pseudo-terminal whose process runs elsewhere, e.g. inside WSL behind
/// `next_code_agent`. Its output is fed to [`Terminal::attach`].
pub trait RemotePty: Send + Sync + 'static {
    fn write(&self, data: Vec<u8>);
    fn resize(&self, columns: u16, rows: u16);
    /// Ends the remote process; called when the [`Terminal`] is dropped.
    fn close(&self);
}

/// What a [`RemotePty`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyOutput {
    Data(Vec<u8>),
    /// The process exited (with its exit code when known); nothing follows.
    Exited(Option<i32>),
}

/// Where input, resizes and shutdown go.
#[derive(Clone)]
enum Io {
    Local(EventLoopSender),
    Remote(Arc<dyn RemotePty>),
}

impl Io {
    fn input(&self, bytes: Cow<'static, [u8]>) {
        match self {
            Self::Local(sender) => {
                sender.send(Msg::Input(bytes)).ok();
            }
            Self::Remote(pty) => pty.write(bytes.into_owned()),
        }
    }

    fn resize(&self, size: WindowSize) {
        match self {
            Self::Local(sender) => {
                sender.send(Msg::Resize(size)).ok();
            }
            Self::Remote(pty) => pty.resize(size.num_cols, size.num_lines),
        }
    }

    fn shutdown(&self) {
        match self {
            Self::Local(sender) => {
                sender.send(Msg::Shutdown).ok();
            }
            Self::Remote(pty) => pty.close(),
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

/// The UI's colors come from its theme; tests use these.
#[cfg(test)]
const fn rgb(value: u32) -> Rgb {
    Rgb {
        r: (value >> 16) as u8,
        g: (value >> 8) as u8,
        b: value as u8,
    }
}

#[cfg(test)]
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
    pub fn indexed(&self, index: usize) -> Rgb {
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

/// The visible screen, resolved to colors: on the primary screen one row
/// per line however wide (see `Rows`), so rows can be wider than the pty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Row width: the pty's columns, or the widest row (and the cursor
    /// after it) when wider.
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
    /// Per row: the line continues on the next row (a line cut at the
    /// widest row, or the alternate screen's soft wrap).
    pub wrapped: Vec<bool>,
    /// OSC 8 hyperlinks by cell index (`row * columns + column`), sorted.
    pub hyperlinks: Vec<(usize, Arc<str>)>,
}

/// A link on screen: the cells it covers (per row, a range of columns)
/// and what it opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenLink {
    pub cells: Vec<(usize, Range<usize>)>,
    pub target: LinkTarget,
}

impl ScreenLink {
    pub fn contains(&self, row: usize, column: usize) -> bool {
        self.cells
            .iter()
            .any(|(at, columns)| *at == row && columns.contains(&column))
    }
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

    fn hyperlink(&self, index: usize) -> Option<&Arc<str>> {
        self.hyperlinks
            .binary_search_by_key(&index, |(at, _)| *at)
            .ok()
            .map(|found| &self.hyperlinks[found].1)
    }

    /// The link at a cell: an OSC 8 hyperlink, else a URL or file location
    /// in the text of its (soft-wrapped) line.
    pub fn link_at(&self, row: usize, column: usize) -> Option<ScreenLink> {
        if row >= self.rows || column >= self.columns {
            return None;
        }
        let mut first = row;
        while first > 0 && self.wrapped.get(first - 1) == Some(&true) {
            first -= 1;
        }
        let mut last = row;
        while last + 1 < self.rows && self.wrapped.get(last) == Some(&true) {
            last += 1;
        }
        // The line's characters and the cell each came from.
        let mut text = String::new();
        let mut positions: Vec<(usize, usize)> = Vec::new();
        for at in first..=last {
            for (col, cell) in self.row(at).iter().enumerate() {
                if !cell.spacer {
                    text.push(if cell.c == '\0' { ' ' } else { cell.c });
                    positions.push((at, col));
                }
            }
        }
        let hovered = positions
            .iter()
            .position(|&(at, col)| at == row && col == column)
            // The second half of a wide character belongs to the first.
            .or_else(|| {
                positions
                    .iter()
                    .position(|&(at, col)| at == row && col + 1 == column)
            })?;
        let to_screen = |range: Range<usize>| {
            let mut cells: Vec<(usize, Range<usize>)> = Vec::new();
            for &(at, col) in &positions[range] {
                let width = if self.row(at)[col].wide { 2 } else { 1 };
                match cells.last_mut() {
                    Some((last_row, columns)) if *last_row == at && columns.end == col => {
                        columns.end = col + width;
                    }
                    _ => cells.push((at, col..col + width)),
                }
            }
            cells
        };

        let index_of = |(at, col): (usize, usize)| at * self.columns + col;
        if let Some(uri) = self.hyperlink(index_of(positions[hovered])) {
            let same = |i: usize| self.hyperlink(index_of(positions[i])) == Some(uri);
            let mut start = hovered;
            while start > 0 && same(start - 1) {
                start -= 1;
            }
            let mut end = hovered + 1;
            while end < positions.len() && same(end) {
                end += 1;
            }
            return Some(ScreenLink {
                cells: to_screen(start..end),
                target: LinkTarget::Url(uri.to_string()),
            });
        }
        links::find_links(&text)
            .into_iter()
            .find(|link| link.range.contains(&hovered))
            .map(|link| ScreenLink {
                cells: to_screen(link.range),
                target: link.target,
            })
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
    /// Set right after the pty exists; replies to terminal queries.
    writer: Arc<OnceLock<Io>>,
    window_size: Arc<Mutex<WindowSize>>,
    palette: Palette,
    exited: Arc<AtomicBool>,
    tap: Option<OutputTap>,
    /// The child's exit code, reported to `tap` once its output is drained.
    exit_code: Arc<Mutex<Option<i32>>>,
    tap_exited: Arc<AtomicBool>,
}

impl Listener {
    fn reply(&self, text: String) {
        if let Some(writer) = self.writer.get() {
            writer.input(text.into_bytes().into());
        }
    }

    fn exited(&self, code: Option<i32>) {
        self.tap_exit(code);
        if !self.exited.swap(true, Ordering::SeqCst) {
            self.events.unbounded_send(TerminalEvent::Exited(code)).ok();
        }
    }

    fn tap_exit(&self, code: Option<i32>) {
        if let Some(tap) = &self.tap
            && !self.tap_exited.swap(true, Ordering::SeqCst)
        {
            tap(Output::Exited(code));
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
                *self.exit_code.lock() = status.code();
                self.exited.store(true, Ordering::SeqCst);
                TerminalEvent::Exited(status.code())
            }
            // After the exited child's output was drained.
            Event::Exit => {
                self.tap_exit(*self.exit_code.lock());
                if self.exited.swap(true, Ordering::SeqCst) {
                    return;
                }
                TerminalEvent::Exited(None)
            }
            _ => return,
        };
        self.events.unbounded_send(event).ok();
    }
}

/// The local pty, with what is read from it also passed to the tap.
struct TappedPty {
    pty: tty::Pty,
    tap: Option<OutputTap>,
    shell: Arc<Mutex<ShellScanner>>,
}

impl Read for TappedPty {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.pty.reader().read(buffer)?;
        if read > 0 {
            self.shell.lock().feed(&buffer[..read]);
            if let Some(tap) = &self.tap {
                tap(Output::Data(&buffer[..read]));
            }
        }
        Ok(read)
    }
}

impl EventedReadWrite for TappedPty {
    type Reader = Self;
    type Writer = <tty::Pty as EventedReadWrite>::Writer;

    unsafe fn register(
        &mut self,
        poller: &Arc<polling::Poller>,
        interest: polling::Event,
        mode: polling::PollMode,
    ) -> io::Result<()> {
        // SAFETY: the pty lives as long as this wrapper, which the event
        // loop deregisters before dropping.
        unsafe { self.pty.register(poller, interest, mode) }
    }

    fn reregister(
        &mut self,
        poller: &Arc<polling::Poller>,
        interest: polling::Event,
        mode: polling::PollMode,
    ) -> io::Result<()> {
        self.pty.reregister(poller, interest, mode)
    }

    fn deregister(&mut self, poller: &Arc<polling::Poller>) -> io::Result<()> {
        self.pty.deregister(poller)
    }

    fn reader(&mut self) -> &mut Self {
        self
    }

    fn writer(&mut self) -> &mut Self::Writer {
        self.pty.writer()
    }
}

impl EventedPty for TappedPty {
    fn next_child_event(&mut self) -> Option<tty::ChildEvent> {
        self.pty.next_child_event()
    }
}

impl alacritty_terminal::event::OnResize for TappedPty {
    fn on_resize(&mut self, size: WindowSize) {
        self.pty.on_resize(size);
    }
}

pub struct Terminal {
    term: Arc<FairMutex<Term<Listener>>>,
    io: Io,
    window_size: Arc<Mutex<WindowSize>>,
    palette: Palette,
    shell: Arc<Mutex<ShellScanner>>,
    /// The local shell's process id (Unix), for its directory when it reports none.
    pid: Option<u32>,
}

impl Terminal {
    /// Starts `options.program` in a local pty; `tap` (if any) gets its
    /// output and exit.
    pub fn spawn(
        options: TerminalOptions,
        palette: Palette,
        tap: Option<OutputTap>,
    ) -> io::Result<(Self, mpsc::UnboundedReceiver<TerminalEvent>)> {
        let (term, listener, events) = Self::emulator(
            options.columns,
            options.rows,
            options.scrollback,
            palette,
            tap.clone(),
        );

        let mut env = options.env;
        env.entry("TERM".into())
            .or_insert_with(|| "xterm-256color".into());
        env.entry("COLORTERM".into())
            .or_insert_with(|| "truecolor".into());
        // Windows adds `escape_args`, which the default fills in.
        #[cfg_attr(not(windows), allow(clippy::needless_update))]
        let pty_options = tty::Options {
            shell: Some(Shell::new(options.program, options.args)),
            working_directory: options.cwd,
            drain_on_exit: true,
            env,
            ..tty::Options::default()
        };
        let shell = Arc::new(Mutex::new(ShellScanner::default()));
        let pty = TappedPty {
            pty: tty::new(&pty_options, *listener.window_size.lock(), 0)?,
            tap,
            shell: shell.clone(),
        };
        #[cfg(unix)]
        let pid = Some(pty.pty.child().id());
        #[cfg(not(unix))]
        let pid = None;
        let event_loop = EventLoop::new(term.clone(), listener.clone(), pty, true, false)?;
        let io = Io::Local(event_loop.channel());
        listener.writer.set(io.clone()).ok();
        event_loop.spawn();

        let terminal = Self {
            term,
            io,
            window_size: listener.window_size.clone(),
            palette,
            shell,
            pid,
        };
        Ok((terminal, events))
    }

    /// Emulates a terminal for a process running behind `pty`, whose output
    /// arrives on `output`. A closed `output` counts as an exit. `tap` (if
    /// any) gets the output and the exit.
    pub fn attach(
        columns: u16,
        rows: u16,
        scrollback: usize,
        palette: Palette,
        pty: Arc<dyn RemotePty>,
        output: std_mpsc::Receiver<PtyOutput>,
        tap: Option<OutputTap>,
    ) -> io::Result<(Self, mpsc::UnboundedReceiver<TerminalEvent>)> {
        let (term, listener, events) =
            Self::emulator(columns, rows, scrollback, palette, tap.clone());
        let io = Io::Remote(pty);
        listener.writer.set(io.clone()).ok();
        let reader_term = term.clone();
        let reader_listener = listener.clone();
        let shell = Arc::new(Mutex::new(ShellScanner::default()));
        let reader_shell = shell.clone();
        thread::Builder::new()
            .name("terminal-remote".into())
            .spawn(move || {
                let mut parser = Processor::<StdSyncHandler>::new();
                let mut code = None;
                for chunk in output {
                    match chunk {
                        PtyOutput::Data(data) => {
                            if let Some(tap) = &tap {
                                tap(Output::Data(&data));
                            }
                            reader_shell.lock().feed(&data);
                            parser.advance(&mut *reader_term.lock(), &data);
                            reader_listener.send_event(Event::Wakeup);
                        }
                        PtyOutput::Exited(exit) => {
                            code = exit;
                            break;
                        }
                    }
                }
                reader_listener.exited(code);
            })?;

        let terminal = Self {
            term,
            io,
            window_size: listener.window_size.clone(),
            palette,
            shell,
            pid: None,
        };
        Ok((terminal, events))
    }

    fn emulator(
        columns: u16,
        rows: u16,
        scrollback: usize,
        palette: Palette,
        tap: Option<OutputTap>,
    ) -> (
        Arc<FairMutex<Term<Listener>>>,
        Listener,
        mpsc::UnboundedReceiver<TerminalEvent>,
    ) {
        let (events, events_rx) = mpsc::unbounded();
        let listener = Listener {
            events,
            writer: Arc::default(),
            window_size: Arc::new(Mutex::new(window_size(columns, rows))),
            palette,
            exited: Arc::default(),
            tap,
            exit_code: Arc::default(),
            tap_exited: Arc::default(),
        };
        let config = Config {
            scrolling_history: scrollback,
            ..Config::default()
        };
        let size = Size {
            columns: usize::from(columns),
            rows: usize::from(rows),
        };
        let term = Arc::new(FairMutex::new(Term::new(config, &size, listener.clone())));
        (term, listener, events_rx)
    }

    /// What the shell reported (OSC 7 etc.); without a reported directory, a
    /// local Linux shell's is read from /proc (its foreground job's, else its own).
    /// Read on request only.
    pub fn shell_state(&self) -> ShellState {
        let mut state = self.shell.lock().state.clone();
        if state.cwd.is_none() {
            state.cwd = self.pid.and_then(proc_cwd);
        }
        state
    }

    /// Sends input bytes to the shell.
    pub fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        self.io.input(bytes.into());
    }

    /// Ends the process now (as dropping the terminal does) while keeping
    /// what it printed on screen.
    pub fn kill(&self) {
        self.io.shutdown();
    }

    pub fn resize(&self, columns: u16, rows: u16) {
        let size = window_size(columns.max(2), rows.max(1));
        *self.window_size.lock() = size;
        self.term.lock().resize(Size {
            columns: usize::from(size.num_cols),
            rows: usize::from(size.num_lines),
        });
        self.io.resize(size);
    }

    /// Scrolls the view `lines` display rows into history (positive) or
    /// back toward the output.
    pub fn scroll(&self, lines: i32) {
        let mut term = self.term.lock();
        let offset = term.grid().display_offset() as i32;
        let target = {
            let rows = Rows::of(&*term);
            let screen = rows.screen(rows.bottom - offset, term.screen_lines());
            let end = rows.scrolled(&screen, lines);
            (rows.bottom - end).clamp(0, -rows.top)
        };
        term.scroll_display(Scroll::Delta(target - offset));
    }

    pub fn scroll_to_bottom(&self) {
        self.term.lock().scroll_display(Scroll::Bottom);
    }

    /// Scrolls the view so the lowest line holding `text` (on screen or in
    /// the history) sits mid-screen; false when no line holds it.
    pub fn scroll_to_text(&self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        let mut term = self.term.lock();
        let (rows, columns) = (term.screen_lines() as i32, term.columns());
        let history = term.grid().history_size() as i32;
        // Grid lines run from -history (oldest) to rows - 1; with display
        // offset d, line l shows on screen row l + d.
        let found = (-history..rows).rev().find(|&line| {
            let row = &term.grid()[Line(line)];
            let text_of_row: String = (0..columns).map(|column| row[Column(column)].c).collect();
            text_of_row.contains(text)
        });
        let Some(line) = found else {
            return false;
        };
        let offset = (rows / 2 - line).clamp(0, history);
        term.scroll_display(Scroll::Bottom);
        term.scroll_display(Scroll::Delta(offset));
        true
    }

    /// What shows: on the primary screen each line is one row however wide
    /// (its soft-wrapped rows joined, see [`Rows`]), so the view scrolls
    /// sideways rather than wrapping; the alternate screen as it is.
    pub fn snapshot(&self) -> Snapshot {
        let term = self.term.lock();
        let content = term.renderable_content();
        let (grid_columns, rows) = (term.columns(), term.screen_lines());
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
        let grid = term.grid();
        let lines = Rows::of(&*term);
        let spans = lines.screen(lines.bottom - content.display_offset as i32, rows);
        let mode = content.mode;
        let cursor_point = content.cursor.point;
        let cursor = (mode.contains(TermMode::SHOW_CURSOR)
            && content.cursor.shape != CursorShape::Hidden)
            .then(|| {
                let line = cursor_point.line.0;
                let row = spans
                    .iter()
                    .position(|span| (span.start..=span.end).contains(&line))?;
                let joined = (line - spans[row].start) as usize;
                Some((row, joined * grid_columns + cursor_point.column.0))
            })
            .flatten();
        // A joined row is as wide as its text (and the cursor after it);
        // the rest at least the screen.
        let columns = spans
            .iter()
            .filter(|span| span.end > span.start)
            .map(|span| {
                let last = &grid[Line(span.end)];
                let used = (0..grid_columns)
                    .rev()
                    .find(|&column| !is_blank(&last[Column(column)]))
                    .map_or(0, |column| column + 1);
                (span.end - span.start) as usize * grid_columns + used
            })
            .chain(cursor.map(|(_, column)| column + 1))
            .fold(grid_columns, usize::max);
        let mut cells = vec![blank; columns * rows];
        let mut wrapped = vec![false; rows];
        let mut hyperlinks: Vec<(usize, Arc<str>)> = Vec::new();
        for (row, span) in spans.iter().enumerate() {
            wrapped[row] = lines.wraps(span.end);
            for line in span.start..=span.end {
                let base = (line - span.start) as usize * grid_columns;
                let joined = line < span.end;
                let grid_row = &grid[Line(line)];
                for column in 0..grid_columns.min(columns - base) {
                    let cell = &grid_row[Column(column)];
                    let flags = cell.flags;
                    // A wide character that did not fit went to the next
                    // row; joined, the gap it left is a blank.
                    if joined && flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                        continue;
                    }
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
                    let index = row * columns + base + column;
                    if let Some(link) = cell.hyperlink() {
                        // One allocation per link, not per cell.
                        let uri = match hyperlinks.last() {
                            Some((_, last)) if **last == *link.uri() => last.clone(),
                            _ => Arc::from(link.uri()),
                        };
                        hyperlinks.push((index, uri));
                    }
                    cells[index] = SnapshotCell {
                        c: cell.c,
                        fg,
                        bg,
                        bold,
                        italic: flags.contains(Flags::ITALIC),
                        underline: flags.intersects(Flags::ALL_UNDERLINES),
                        strikeout: flags.contains(Flags::STRIKEOUT),
                        wide: flags.contains(Flags::WIDE_CHAR),
                        spacer: flags
                            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
                    };
                }
            }
        }

        Snapshot {
            columns,
            rows,
            cells,
            cursor,
            display_offset: content.display_offset,
            history: grid.history_size(),
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
            wrapped,
            hyperlinks,
        }
    }
}

/// Widest joined row: a longer line is cut every this many columns (in
/// whole screen rows), so a huge line costs a bounded snapshot.
const MAX_ROW_COLUMNS: usize = 2048;

/// Grid lines `start..=end` (rows of one soft-wrapped line) shown as one
/// display row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: i32,
    end: i32,
}

/// How grid lines (`top` = -history ..= `bottom` = last screen row) show
/// as display rows. On the primary screen a line the emulator soft-wrapped
/// is one row again, so long lines scroll sideways instead of wrapping:
/// the pty stays as wide as the view (programs and shells line-edit at
/// the width they are told, and the emulator wraps there as they expect),
/// and only the display joins the rows. Joining is cut every `per` rows
/// (from the line's start). The alternate screen (full-screen programs,
/// sized to the view) shows its rows as they are.
struct Rows<W> {
    top: i32,
    bottom: i32,
    per: i32,
    /// Whether a grid line continues on the next (soft wrap).
    wraps: W,
    join: bool,
}

impl<'a> Rows<Box<dyn Fn(i32) -> bool + 'a>> {
    fn of<T>(term: &'a Term<T>) -> Self {
        let grid = term.grid();
        let last = Column(term.columns() - 1);
        Self {
            top: -(grid.history_size() as i32),
            bottom: term.screen_lines() as i32 - 1,
            per: (MAX_ROW_COLUMNS / term.columns()).max(1) as i32,
            wraps: Box::new(move |line| grid[Line(line)][last].flags.contains(Flags::WRAPLINE)),
            join: !term.mode().contains(TermMode::ALT_SCREEN),
        }
    }
}

impl<W: Fn(i32) -> bool> Rows<W> {
    /// Whether `line` continues on the next grid line.
    fn wraps(&self, line: i32) -> bool {
        line < self.bottom && (self.wraps)(line)
    }

    /// The display row holding grid line `line`.
    fn span(&self, line: i32) -> Span {
        if !self.join {
            return Span {
                start: line,
                end: line,
            };
        }
        let mut first = line;
        while first > self.top && self.wraps(first - 1) {
            first -= 1;
        }
        let mut last = line;
        while self.wraps(last) {
            last += 1;
        }
        let start = first + (line - first) / self.per * self.per;
        Span {
            start,
            end: last.min(start + self.per - 1),
        }
    }

    fn before(&self, span: Span) -> Option<Span> {
        (span.start > self.top).then(|| self.span(span.start - 1))
    }

    fn after(&self, span: Span) -> Option<Span> {
        (span.end < self.bottom).then(|| self.span(span.end + 1))
    }

    /// `count` display rows ending with the one holding grid line
    /// `anchor`; with too few above it (the top of the history), more
    /// from below it.
    fn screen(&self, anchor: i32, count: usize) -> Vec<Span> {
        let mut spans = vec![self.span(anchor.clamp(self.top, self.bottom))];
        while spans.len() < count
            && let Some(before) = self.before(spans[spans.len() - 1])
        {
            spans.push(before);
        }
        spans.reverse();
        while spans.len() < count
            && let Some(after) = self.after(spans[spans.len() - 1])
        {
            spans.push(after);
        }
        spans
    }

    /// The grid line that ends the screen after scrolling `screen` by
    /// `lines` display rows (positive: into the history).
    fn scrolled(&self, screen: &[Span], lines: i32) -> i32 {
        let Some(mut span) = screen.last().copied() else {
            return self.bottom;
        };
        for _ in 0..lines.unsigned_abs() {
            let next = if lines > 0 {
                self.before(span)
            } else {
                self.after(span)
            };
            match next {
                Some(next) => span = next,
                None => break,
            }
        }
        span.end
    }
}

/// A cell that shows nothing: a space (or nothing) on the default
/// background.
fn is_blank(cell: &alacritty_terminal::term::cell::Cell) -> bool {
    matches!(cell.c, ' ' | '\0' | '\t')
        && cell.bg == Color::Named(NamedColor::Background)
        && !cell
            .flags
            .intersects(Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
}

#[cfg(test)]
mod rows {
    use super::*;

    /// Grid lines -`history`..`screen`, those in `wrapping` continuing on
    /// the next.
    fn rows(history: i32, screen: i32, wrapping: &[i32], per: i32) -> Rows<impl Fn(i32) -> bool> {
        let wrapping = wrapping.to_vec();
        Rows {
            top: -history,
            bottom: screen - 1,
            per,
            wraps: move |line| wrapping.contains(&line),
            join: true,
        }
    }

    fn span(start: i32, end: i32) -> Span {
        Span { start, end }
    }

    #[test]
    fn wrapped_rows_join_and_the_screen_fills_from_history() {
        // Lines 1-2 are one line: the screen of 4 takes one more from history.
        let rows = rows(5, 4, &[1], 100);
        assert_eq!(
            rows.screen(3, 4),
            [span(-1, -1), span(0, 0), span(1, 2), span(3, 3)]
        );
        // A line ending below the anchor shows whole.
        assert_eq!(rows.screen(1, 2), [span(0, 0), span(1, 2)]);
    }

    #[test]
    fn at_the_top_of_history_the_screen_continues_below() {
        let rows = rows(0, 5, &[0, 1], 100);
        assert_eq!(rows.screen(2, 3), [span(0, 2), span(3, 3), span(4, 4)]);
        // Scrolling up there stays; down moves one display row.
        let screen = rows.screen(2, 2);
        assert_eq!(screen, [span(0, 2), span(3, 3)]);
        assert_eq!(rows.scrolled(&screen, 3), 2);
        assert_eq!(rows.scrolled(&screen, -1), 4);
    }

    #[test]
    fn huge_lines_are_cut_every_per_rows() {
        // One line over grid lines 0..=6, 3 rows per display row.
        let rows = rows(0, 8, &[0, 1, 2, 3, 4, 5], 3);
        assert_eq!(rows.span(4), span(3, 5));
        assert_eq!(rows.span(6), span(6, 6));
        assert_eq!(
            rows.screen(7, 4),
            [span(0, 2), span(3, 5), span(6, 6), span(7, 7)]
        );
        assert!(rows.wraps(5), "a cut row continues on the next");
    }

    #[test]
    fn scrolling_counts_display_rows() {
        // History -4..=-1 (lines -3,-2 joined), screen 0..=2.
        let rows = rows(4, 3, &[-3], 100);
        let screen = rows.screen(2, 3);
        assert_eq!(screen, [span(0, 0), span(1, 1), span(2, 2)]);
        assert_eq!(rows.scrolled(&screen, 1), 1);
        assert_eq!(rows.scrolled(&screen, 4), -2, "the joined line is one row");
        assert_eq!(rows.scrolled(&rows.screen(-2, 3), -1), 0);
    }

    #[test]
    fn the_alternate_screen_keeps_its_rows() {
        let mut rows = rows(0, 3, &[0], 100);
        rows.join = false;
        assert_eq!(rows.screen(2, 3), [span(0, 0), span(1, 1), span(2, 2)]);
        assert!(rows.wraps(0));
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.io.shutdown();
    }
}

/// A color the program set via OSC, else the palette's.
fn color_at(colors: &Colors, palette: &Palette, index: usize) -> Rgb {
    colors[index].unwrap_or_else(|| palette.indexed(index))
}

/// The directory of `shell`'s foreground job (from /proc), else the shell's.
#[cfg(target_os = "linux")]
fn proc_cwd(shell: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{shell}/stat")).ok();
    let foreground = stat.as_deref().and_then(foreground_group);
    foreground
        .into_iter()
        .chain([shell])
        .find_map(|pid| std::fs::read_link(format!("/proc/{pid}/cwd")).ok())
        .map(|path| path.to_string_lossy().into_owned())
}

#[cfg(not(target_os = "linux"))]
fn proc_cwd(_shell: u32) -> Option<String> {
    None
}

/// `tpgid` (the terminal's foreground process group) of a `/proc/<pid>/stat` line.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn foreground_group(stat: &str) -> Option<u32> {
    // `pid (comm) state ppid pgrp session tty_nr tpgid …`; comm may hold spaces or `)`.
    let fields = &stat[stat.rfind(')')? + 1..];
    let tpgid: i64 = fields.split_whitespace().nth(5)?.parse().ok()?;
    u32::try_from(tpgid).ok().filter(|pid| *pid > 0)
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
    fn foreground_group_reads_tpgid_past_odd_names() {
        let stat = "4242 (my (odd) sh) S 1 4242 4242 34816 5000 4194560 0 0";
        assert_eq!(foreground_group(stat), Some(5000));
        assert_eq!(foreground_group("1 (init) S 0 1 1 0 -1 4194560"), None);
    }

    /// The shell's directory follows `cd` (from /proc on Linux; OSC 7 when printed).
    #[cfg(target_os = "linux")]
    #[test]
    fn reports_the_shell_directory_and_last_exit() {
        let dir = std::env::temp_dir().join(format!("nc-term-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (options, _) = test_shell();
        let (terminal, mut events) = Terminal::spawn(options, Palette::default(), None).unwrap();
        terminal.write(format!("cd '{}'\n", dir.display()).into_bytes());
        let deadline = Instant::now() + Duration::from_secs(10);
        while terminal.shell_state().cwd.as_deref() != Some(dir.to_str().unwrap()) {
            assert!(Instant::now() < deadline, "{:?}", terminal.shell_state());
            std::thread::sleep(Duration::from_millis(20));
        }
        terminal.write(&b"printf '\\033]7;file://h/elsewhere\\007\\033]133;D;4\\007'\n"[..]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while terminal.shell_state().last_exit != Some(4) {
            assert!(Instant::now() < deadline, "{:?}", terminal.shell_state());
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(terminal.shell_state().cwd.as_deref(), Some("/elsewhere"));
        drop(events.try_recv());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn runs_commands_resizes_and_reports_exit() {
        let (options, command) = test_shell();
        let (terminal, mut events) = Terminal::spawn(options, Palette::default(), None).unwrap();
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
        // Lines wider than 40 columns (a long prompt) stay one wider row.
        assert_eq!(snapshot.rows, 10);
        assert!(snapshot.columns >= 40, "{}", snapshot.columns);
        assert_eq!(snapshot.cells.len(), snapshot.columns * 10);

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
    fn taps_see_a_programs_output_then_its_exit_code() {
        let mut options = TerminalOptions::default_shell();
        if cfg!(windows) {
            options.program = "powershell.exe".into();
            options.args = [
                "-NoLogo",
                "-NoProfile",
                "-Command",
                "echo ('ta' + 'pped'); exit 3",
            ]
            .map(String::from)
            .to_vec();
        } else {
            options.program = "/bin/sh".into();
            options.args = ["-c", "echo ta''pped; exit 3"].map(String::from).to_vec();
        }
        let (tx, rx) = std_mpsc::channel();
        let tx = Mutex::new(tx);
        let tap: OutputTap = Arc::new(move |output| {
            let event = match output {
                Output::Data(data) => Ok(data.to_vec()),
                Output::Exited(code) => Err(code),
            };
            tx.lock().send(event).ok();
        });
        let (_terminal, _events) = Terminal::spawn(options, Palette::default(), Some(tap)).unwrap();
        let mut text = Vec::new();
        let exit = loop {
            match rx.recv_timeout(Duration::from_secs(30)).expect("no exit") {
                Ok(data) => text.extend(data),
                Err(code) => break code,
            }
        };
        assert!(
            String::from_utf8_lossy(&text).contains("tapped"),
            "{}",
            String::from_utf8_lossy(&text)
        );
        assert_eq!(exit, Some(3));
        // Nothing after the exit.
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[derive(Default)]
    struct FakePty {
        written: Mutex<Vec<u8>>,
        sizes: Mutex<Vec<(u16, u16)>>,
        closed: AtomicBool,
    }

    impl RemotePty for FakePty {
        fn write(&self, data: Vec<u8>) {
            self.written.lock().extend(data);
        }
        fn resize(&self, columns: u16, rows: u16) {
            self.sizes.lock().push((columns, rows));
        }
        fn close(&self) {
            self.closed.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn attached_terminals_render_remote_output_and_answer_through_the_remote() {
        let pty = Arc::new(FakePty::default());
        let (output, output_rx) = std_mpsc::channel();
        let (terminal, mut events) =
            Terminal::attach(30, 5, 100, Palette::default(), pty.clone(), output_rx, None).unwrap();

        // Text plus a cursor-position query, which the emulator must answer.
        output
            .send(PtyOutput::Data(b"hello remote\x1b[6n".to_vec()))
            .unwrap();
        assert!(wait_for(&mut events, |_| terminal
            .snapshot()
            .text()
            .contains("hello remote")));
        assert_eq!(pty.written.lock().as_slice(), b"\x1b[1;13R");

        terminal.write(&b"ls\r"[..]);
        terminal.resize(50, 8);
        assert_eq!(pty.written.lock().as_slice(), b"\x1b[1;13Rls\r");
        assert_eq!(pty.sizes.lock().as_slice(), [(50, 8)]);

        output.send(PtyOutput::Exited(Some(3))).unwrap();
        assert!(wait_for(&mut events, |event| *event
            == TerminalEvent::Exited(Some(3))));
        drop(terminal);
        assert!(pty.closed.load(Ordering::SeqCst));
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

#[cfg(test)]
mod screen_links {
    use super::*;

    struct Sink;
    impl RemotePty for Sink {
        fn write(&self, _: Vec<u8>) {}
        fn resize(&self, _: u16, _: u16) {}
        fn close(&self) {}
    }

    /// A 20-column screen once it shows `last` (the end of `output`).
    fn screen(output: &[u8], last: &str) -> Snapshot {
        let (tx, rx) = std_mpsc::channel();
        let (terminal, _events) =
            Terminal::attach(20, 4, 10, Palette::default(), Arc::new(Sink), rx, None).unwrap();
        tx.send(PtyOutput::Data(output.to_vec())).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let snapshot = terminal.snapshot();
            if snapshot.text().contains(last) || std::time::Instant::now() > deadline {
                return snapshot;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn a_line_wrapped_by_the_emulator_shows_as_one_wide_row() {
        // "see " + a 28-character URL + " done": 37 columns on a 20-column
        // screen, which the emulator wraps; the snapshot joins it.
        let snapshot = screen(b"see https://example.com/abcdefgh done\r\n$ ", "$");
        assert_eq!(snapshot.columns, 37);
        assert_eq!(snapshot.cells.len(), 37 * 4);
        assert_eq!(
            snapshot.text().lines().next(),
            Some("see https://example.com/abcdefgh done")
        );
        assert_eq!(snapshot.wrapped, [false; 4]);
        // The cursor after the prompt, on the next row.
        assert_eq!(snapshot.cursor, Some((1, 2)));
        let url = LinkTarget::Url("https://example.com/abcdefgh".into());
        let link = snapshot.link_at(0, 10).unwrap();
        assert_eq!(link.target, url);
        assert_eq!(link.cells, [(0, 4..32)]);
        assert_eq!(snapshot.link_at(0, 25), Some(link));
        assert_eq!(snapshot.link_at(0, 34), None, "`done` is not part of it");
    }

    #[test]
    fn scrolling_moves_by_display_rows() {
        let long = "x".repeat(45);
        let output = format!("a\r\n{long}\r\nb\r\nc\r\nd");
        let (tx, rx) = std_mpsc::channel();
        let (terminal, _events) =
            Terminal::attach(20, 4, 10, Palette::default(), Arc::new(Sink), rx, None).unwrap();
        tx.send(PtyOutput::Data(output.into_bytes())).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !terminal.snapshot().text().ends_with('d') {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let lines = |terminal: &Terminal| -> Vec<String> {
            terminal
                .snapshot()
                .text()
                .lines()
                .map(str::to_owned)
                .collect()
        };
        assert_eq!(lines(&terminal), [long.as_str(), "b", "c", "d"]);
        terminal.scroll(1);
        assert_eq!(lines(&terminal), ["a", long.as_str(), "b", "c"]);
        terminal.scroll(5);
        assert_eq!(lines(&terminal)[0], "a", "the top of the history");
        terminal.scroll(-1);
        assert_eq!(lines(&terminal), [long.as_str(), "b", "c", "d"]);
    }

    #[test]
    fn the_cursor_after_a_wrapped_line_stays_on_its_row() {
        // 25 characters on a 20-column screen, the cursor after them.
        let snapshot = screen(b"abcdefghijklmnopqrstuvwxy", "y");
        assert_eq!(snapshot.cursor, Some((0, 25)));
        assert_eq!(snapshot.columns, 26, "wide enough to show the cursor");
    }

    #[test]
    fn osc8_hyperlinks_open_their_target_not_their_text() {
        let snapshot = screen(
            b"\x1b]8;;https://docs.rs/\x1b\\the docs\x1b]8;;\x1b\\ end",
            "end",
        );
        let link = snapshot.link_at(0, 5).unwrap();
        assert_eq!(link.target, LinkTarget::Url("https://docs.rs/".into()));
        assert_eq!(link.cells, [(0, 0..8)]);
        assert_eq!(snapshot.link_at(0, 10), None);
    }
}
