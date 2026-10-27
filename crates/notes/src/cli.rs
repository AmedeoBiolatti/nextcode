//! `next_code notes …` and `next_code_agent notes …`: reads and changes
//! `.next_code/notes.json` directly (no running editor needed; the editor
//! watches the file).

use std::{
    collections::HashSet,
    env,
    io::{self, Read as _, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    Anchored, Author, AuthorKind, NOTES_FILE, Note, Notes, Status, Store, anchor_from, format_time,
    parse_time, reanchor,
};

pub const USAGE: &str = "\
usage: next_code notes <command> [options]

Sticky notes on lines of project files, shared with the person in NextCode
(see docs/notes-for-agents.md). Works without a running NextCode.

  list [FILE|DIR] [--open|--resolved|--all] [--to agent|user|NAME]
        notes (open ones by default) with their current lines and threads
  show ID
        one note's thread and the lines it is on
  add FILE:LINE[-LINE] TEXT... [--to user|agent|NAME] [--cell N] [--color C]
        a new note (FILE:LINE:COL-LINE:COL marks columns too; TEXT '-' reads
        stdin; --cell N: line in notebook cell N); --to defaults to the other
        side (user for agents)
  reply ID TEXT...
  resolve ID | reopen ID | delete ID
  wait [--to agent|user|NAME] [--timeout SECONDS] [--since TIME]
        block until someone else writes: by default any message by the user,
        or in a note addressed to you; --to: only notes addressed to that;
        --since: messages after TIME (RFC 3339, printed as `since:`), else
        after now. Exit 1 on timeout.

Common options: --root DIR (else the nearest folder up with
.next_code/notes.json, else with .git, else with .next_code/, else the current
one), --as NAME (author name; default $NEXT_CODE_AGENT_NAME, else \"agent\"),
--from agent|user (author kind; default agent), --format json (raw JSON).
IDs may be shortened to a unique prefix.
Exit codes: 0 ok, 1 error (or wait timed out), 2 bad arguments.";

#[derive(Debug, Clone, PartialEq)]
pub struct Invocation {
    pub root: Option<PathBuf>,
    pub json: bool,
    /// `--as`.
    pub name: Option<String>,
    /// `--from`.
    pub kind: AuthorKind,
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Help,
    List {
        file: Option<String>,
        /// `None`: all.
        status: Option<Status>,
        to: Option<String>,
    },
    Show {
        id: String,
    },
    Add {
        file: String,
        start: usize,
        end: usize,
        columns: Option<(usize, usize)>,
        cell: Option<usize>,
        /// `-`: read stdin.
        text: String,
        to: Option<String>,
        color: Option<String>,
    },
    Reply {
        id: String,
        text: String,
    },
    Resolve {
        id: String,
    },
    Reopen {
        id: String,
    },
    Delete {
        id: String,
    },
    Wait {
        to: Option<String>,
        timeout: Option<Duration>,
        /// Unix milliseconds.
        since: Option<u64>,
    },
}

impl Invocation {
    /// Who writes: `--from`/`--as`, else `$NEXT_CODE_AGENT_NAME` (agents) or
    /// the login name (users).
    pub fn author(&self) -> Author {
        let from_env = |vars: &[&str]| {
            vars.iter()
                .find_map(|var| env::var(var).ok().filter(|name| !name.trim().is_empty()))
        };
        match self.kind {
            AuthorKind::Agent => Author::agent(
                self.name
                    .clone()
                    .or_else(|| from_env(&["NEXT_CODE_AGENT_NAME"]))
                    .unwrap_or_else(|| "agent".into()),
            ),
            AuthorKind::User => Author::user(
                self.name
                    .clone()
                    .or_else(|| from_env(&["USER", "USERNAME"]))
                    .unwrap_or_else(|| "user".into()),
            ),
        }
    }
}

/// Reads `args` (after `notes`).
pub fn parse(args: &[String]) -> Result<Invocation, String> {
    let mut args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut invocation = Invocation {
        root: None,
        json: false,
        name: None,
        kind: AuthorKind::Agent,
        command: Command::Help,
    };
    // Options valid with every command, anywhere.
    let mut index = 0;
    while index < args.len() {
        let option = args[index];
        match option {
            "-h" | "--help" => return Ok(invocation),
            "--root" | "--as" | "--from" | "--format" => {}
            _ => {
                index += 1;
                continue;
            }
        }
        let value = value_of(&args, index)?;
        args.drain(index..index + 2);
        match option {
            "--root" => invocation.root = Some(PathBuf::from(value)),
            "--as" => invocation.name = Some(value.to_owned()),
            "--from" => {
                invocation.kind = match value {
                    "agent" => AuthorKind::Agent,
                    "user" => AuthorKind::User,
                    other => return Err(format!("--from is agent or user, not {other:?}")),
                }
            }
            _ => {
                invocation.json = match value {
                    "json" => true,
                    "text" => false,
                    other => return Err(format!("--format is json or text, not {other:?}")),
                }
            }
        }
    }
    let Some((&command, rest)) = args.split_first() else {
        return Ok(invocation);
    };
    invocation.command = match command {
        "help" => Command::Help,
        "list" | "ls" => list(rest)?,
        "show" => Command::Show {
            id: one_id(rest, "show")?,
        },
        "add" => add(rest)?,
        "reply" => {
            let (id, words) = rest
                .split_first()
                .ok_or("reply needs a note ID and the text")?;
            Command::Reply {
                id: (*id).to_owned(),
                text: text_of(words, "reply")?,
            }
        }
        "resolve" => Command::Resolve {
            id: one_id(rest, "resolve")?,
        },
        "reopen" => Command::Reopen {
            id: one_id(rest, "reopen")?,
        },
        "delete" | "rm" => Command::Delete {
            id: one_id(rest, "delete")?,
        },
        "wait" => wait_args(rest)?,
        other => {
            return Err(format!(
                "unknown command {other:?}: use list, show, add, reply, resolve, reopen, delete or wait"
            ));
        }
    };
    Ok(invocation)
}

fn value_of<'a>(args: &[&'a str], index: usize) -> Result<&'a str, String> {
    args.get(index + 1)
        .copied()
        .ok_or_else(|| format!("{} needs a value", args[index]))
}

fn one_id(args: &[&str], command: &str) -> Result<String, String> {
    match args {
        [id] if !id.starts_with("--") => Ok((*id).to_owned()),
        [] => Err(format!("{command} needs a note ID")),
        _ => Err(format!("{command} takes one note ID, not {args:?}")),
    }
}

fn text_of(words: &[&str], command: &str) -> Result<String, String> {
    if let Some(option) = words.iter().find(|word| word.starts_with("--")) {
        return Err(format!("{command}: unexpected {option:?}"));
    }
    if words.is_empty() {
        return Err(format!("{command} needs the text"));
    }
    Ok(words.join(" "))
}

fn list(args: &[&str]) -> Result<Command, String> {
    let mut file = None;
    let mut status = Some(Status::Open);
    let mut to = None;
    let mut index = 0;
    while index < args.len() {
        match args[index] {
            "--open" => status = Some(Status::Open),
            "--resolved" => status = Some(Status::Resolved),
            "--all" => status = None,
            "--to" => {
                to = Some(value_of(args, index)?.to_owned());
                index += 1;
            }
            path if !path.starts_with("--") && file.is_none() => file = Some(path.to_owned()),
            other => return Err(format!("list: unexpected {other:?}")),
        }
        index += 1;
    }
    Ok(Command::List { file, status, to })
}

fn number(text: &str, what: &str) -> Result<usize, String> {
    text.parse::<usize>()
        .ok()
        .filter(|&n| n > 0)
        .ok_or_else(|| format!("{what} takes a number from 1, not {text:?}"))
}

/// `FILE:LINE[:COL][-LINE[:COL]]` → file, lines, columns.
#[allow(clippy::type_complexity)]
fn location(spec: &str) -> Option<(String, usize, usize, Option<(usize, usize)>)> {
    let lines = |tail: &str| -> Option<(usize, usize, Option<(usize, usize)>)> {
        let (from, to) = match tail.split_once('-') {
            Some((from, to)) => (from, Some(to)),
            None => (tail, None),
        };
        let point = |part: &str| -> Option<(usize, Option<usize>)> {
            let (line, column) = match part.split_once(':') {
                Some((line, column)) => (line, Some(column.parse().ok().filter(|&c| c > 0)?)),
                None => (part, None),
            };
            Some((line.parse().ok().filter(|&l| l > 0)?, column))
        };
        let (start, start_column) = point(from)?;
        let (end, end_column) = match to {
            Some(to) => point(to)?,
            None => (start, start_column),
        };
        let columns = match (start_column, end_column) {
            (Some(a), Some(b)) => Some((a, b)),
            (None, None) => None,
            _ => return None,
        };
        (end >= start).then_some((start, end, columns))
    };
    // The first `:` whose rest reads as lines (`C:\…` keeps its drive).
    spec.char_indices()
        .filter(|&(index, c)| c == ':' && index > 0)
        .find_map(|(index, _)| {
            let (start, end, columns) = lines(&spec[index + 1..])?;
            Some((spec[..index].to_owned(), start, end, columns))
        })
}

fn add(args: &[&str]) -> Result<Command, String> {
    let mut spec = None;
    let mut words = Vec::new();
    let (mut to, mut cell, mut color) = (None, None, None);
    let mut index = 0;
    while index < args.len() {
        match args[index] {
            "--to" => {
                to = Some(value_of(args, index)?.to_owned());
                index += 1;
            }
            "--cell" => {
                cell = Some(number(value_of(args, index)?, "--cell")?);
                index += 1;
            }
            "--color" => {
                color = Some(value_of(args, index)?.to_owned());
                index += 1;
            }
            option if option.starts_with("--") => {
                return Err(format!("add: unexpected {option:?}"));
            }
            word if spec.is_none() => spec = Some(word),
            word => words.push(word),
        }
        index += 1;
    }
    let spec = spec.ok_or("add needs FILE:LINE[-LINE] and the text")?;
    let (file, start, end, columns) = location(spec)
        .ok_or_else(|| format!("add: {spec:?} is not FILE:LINE[-LINE] (lines from 1)"))?;
    Ok(Command::Add {
        file,
        start,
        end,
        columns,
        cell,
        text: text_of(&words, "add")?,
        to,
        color,
    })
}

fn wait_args(args: &[&str]) -> Result<Command, String> {
    let (mut to, mut timeout, mut since) = (None, None, None);
    let mut index = 0;
    while index < args.len() {
        let value = || value_of(args, index);
        match args[index] {
            "--to" => to = Some(value()?.to_owned()),
            "--timeout" => {
                let text = value()?;
                let seconds = text
                    .parse::<f64>()
                    .ok()
                    .filter(|s| *s > 0. && s.is_finite())
                    .ok_or_else(|| format!("--timeout takes seconds, not {text:?}"))?;
                timeout = Some(Duration::from_secs_f64(seconds));
            }
            "--since" => {
                let text = value()?;
                since = Some(parse_time(text).ok_or_else(|| {
                    format!("--since takes an RFC 3339 time (as printed by wait), not {text:?}")
                })?);
            }
            other => return Err(format!("wait: unexpected {other:?}")),
        }
        index += 2;
    }
    Ok(Command::Wait { to, timeout, since })
}

/// The project root for `cwd`: the nearest folder up with
/// `.next_code/notes.json`, else with `.git`, else with `.next_code/`,
/// else `cwd`.
pub fn find_root(cwd: &Path) -> PathBuf {
    let has = |relative: &str| {
        cwd.ancestors()
            .find(|dir| dir.join(relative).exists())
            .map(Path::to_path_buf)
    };
    has(NOTES_FILE)
        .or_else(|| has(".git"))
        .or_else(|| has(".next_code"))
        .unwrap_or_else(|| cwd.to_path_buf())
}

/// Runs `notes` with `args` (after `notes`); `program` names the binary in
/// the usage. Returns the exit code.
pub fn run(args: &[String], program: &str) -> i32 {
    let usage = USAGE.replacen("next_code notes", &format!("{program} notes"), 1);
    let invocation = match parse(args) {
        Ok(invocation) => invocation,
        Err(err) => {
            eprintln!("{err}\n\n{usage}");
            return 2;
        }
    };
    if invocation.command == Command::Help {
        println!("{usage}");
        return 0;
    }
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut stdout = io::stdout().lock();
    match execute(&invocation, &cwd, &mut stdout, Duration::from_millis(300)) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{program} notes: {err}");
            1
        }
    }
}

/// Runs a parsed command from `cwd`, writing its output to `out`; `poll`:
/// how often `wait` re-reads the file.
pub fn execute(
    invocation: &Invocation,
    cwd: &Path,
    out: &mut dyn Write,
    poll: Duration,
) -> io::Result<i32> {
    let root = match &invocation.root {
        Some(root) => cwd.join(root),
        None => find_root(cwd),
    };
    let store = Store::open(&root);
    let project = Project { root: &root, cwd };
    let json = invocation.json;
    let author = invocation.author();
    let print_note = |out: &mut dyn Write, note: &Note| -> io::Result<i32> {
        let at = project.locate(note);
        if json {
            writeln!(out, "{}", pretty(&note_json(note, &at)))?;
        } else {
            write_note(out, note, &at, None)?;
        }
        Ok(0)
    };
    match &invocation.command {
        Command::Help => Ok(0),
        Command::List { file, status, to } => {
            let notes = store.load()?;
            let file = file.as_deref().map(|file| project.relative(file));
            let shown: Vec<&Note> = notes
                .notes
                .iter()
                .filter(|note| status.is_none_or(|status| note.status == status))
                .filter(|note| to.is_none() || note.to == *to)
                .filter(|note| {
                    file.as_deref().is_none_or(|file| {
                        file.is_empty()
                            || note.file == file
                            || note
                                .file
                                .strip_prefix(file)
                                .is_some_and(|rest| rest.starts_with('/'))
                    })
                })
                .collect();
            if json {
                #[derive(Serialize)]
                struct Reply<'a> {
                    root: String,
                    notes: Vec<Located<'a>>,
                }
                let notes = shown
                    .iter()
                    .map(|note| note_json(note, &project.locate(note)))
                    .collect();
                let root = root.display().to_string();
                writeln!(out, "{}", pretty(&Reply { root, notes }))?;
            } else if shown.is_empty() {
                writeln!(out, "no notes")?;
            } else {
                for (index, note) in shown.iter().enumerate() {
                    if index > 0 {
                        writeln!(out)?;
                    }
                    write_note(out, note, &project.locate(note), None)?;
                }
            }
            Ok(0)
        }
        Command::Show { id } => {
            let notes = store.load()?;
            let note = notes.find(id)?;
            let at = project.locate(note);
            if json {
                writeln!(out, "{}", pretty(&note_json(note, &at)))?;
            } else {
                write_note(out, note, &at, None)?;
                if let Some(text) = project.text(note) {
                    let lines: Vec<&str> = text.lines().collect();
                    let shown = (at.end_line - at.start_line + 1).min(10);
                    for line in at.start_line..at.start_line + shown {
                        if let Some(text) = lines.get(line - 1) {
                            writeln!(out, "  {line:>5} | {}", text.trim_end_matches('\r'))?;
                        }
                    }
                }
            }
            Ok(0)
        }
        Command::Add {
            file,
            start,
            end,
            columns,
            cell,
            text,
            to,
            color,
        } => {
            let relative = project.relative(file);
            let source = project.read(&relative, *cell).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    match cell {
                        Some(cell) => {
                            format!("no cell {cell} in {relative} (under {})", root.display())
                        }
                        None => format!("no file {relative} under {}", root.display()),
                    },
                )
            })?;
            let lines = source.lines().count().max(1);
            if *start > lines {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{relative} has {lines} lines, not {start}"),
                ));
            }
            let text = message_text(text)?;
            let to = to.clone().or_else(|| {
                Some(match author.kind {
                    AuthorKind::Agent => "user".into(),
                    AuthorKind::User => "agent".into(),
                })
            });
            let anchor = anchor_from(&source, *start, *end, *columns);
            let color = color.clone();
            let note = store.update(|notes| {
                let id = notes
                    .add(&relative, *cell, anchor, &text, author, to)
                    .id
                    .clone();
                if color.is_some() {
                    notes.set_color(&id, color)?;
                }
                Ok(notes.get(&id).cloned().expect("just added"))
            })?;
            print_note(out, &note)
        }
        Command::Reply { id, text } => {
            let text = message_text(text)?;
            let note = store.update(|notes| {
                let id = notes.find(id)?.id.clone();
                notes.reply(&id, &text, author).cloned()
            })?;
            print_note(out, &note)
        }
        Command::Resolve { id } => {
            let note = store.update(|notes| {
                let id = notes.find(id)?.id.clone();
                notes.resolve(&id, author).cloned()
            })?;
            print_note(out, &note)
        }
        Command::Reopen { id } => {
            let note = store.update(|notes| {
                let id = notes.find(id)?.id.clone();
                notes.reopen(&id).cloned()
            })?;
            print_note(out, &note)
        }
        Command::Delete { id } => {
            let note = store.update(|notes| {
                let id = notes.find(id)?.id.clone();
                notes.delete(&id)
            })?;
            if json {
                writeln!(out, "{}", pretty(&json!({ "deleted": note.id })))?;
            } else {
                writeln!(
                    out,
                    "deleted {} ({}: {})",
                    note.id,
                    note.file,
                    first_line(note.text())
                )?;
            }
            Ok(0)
        }
        Command::Wait { to, timeout, since } => {
            match wait(&store, &author, to.as_deref(), *timeout, *since, poll)? {
                Some((notes, new, since)) => {
                    if json {
                        #[derive(Serialize)]
                        struct New<'a> {
                            note: &'a str,
                            index: usize,
                        }
                        #[derive(Serialize)]
                        struct Reply<'a> {
                            new: Vec<New<'a>>,
                            notes: Vec<Located<'a>>,
                            since: String,
                        }
                        let notes = notes
                            .notes
                            .iter()
                            .filter(|note| new.iter().any(|(id, _)| *id == note.id))
                            .map(|note| note_json(note, &project.locate(note)))
                            .collect();
                        let new = new
                            .iter()
                            .map(|(note, index)| New {
                                note,
                                index: *index,
                            })
                            .collect();
                        writeln!(out, "{}", pretty(&Reply { new, notes, since }))?;
                    } else {
                        for note in &notes.notes {
                            if new.iter().any(|(id, _)| *id == note.id) {
                                write_note(out, note, &project.locate(note), Some(&new))?;
                                writeln!(out)?;
                            }
                        }
                        writeln!(out, "since: {since}")?;
                    }
                    Ok(0)
                }
                None => {
                    if json {
                        writeln!(out, "{}", pretty(&json!({ "new": [], "timed_out": true })))?;
                    } else {
                        eprintln!("timed out: no new messages");
                    }
                    Ok(1)
                }
            }
        }
    }
}

/// New messages: their notes as read, `(note id, message index)` of each,
/// and the newest message time (for the next `--since`). `None`: timed out.
type Waited = (Notes, Vec<(String, usize)>, String);

fn wait(
    store: &Store,
    me: &Author,
    to: Option<&str>,
    timeout: Option<Duration>,
    since: Option<u64>,
    poll: Duration,
) -> io::Result<Option<Waited>> {
    let started = Instant::now();
    let relevant = |note: &Note, index: usize| {
        let message = &note.thread[index];
        if message.author == *me {
            return false;
        }
        match to {
            Some(to) => note.to.as_deref() == Some(to),
            None => {
                message.author.kind == AuthorKind::User
                    || (note.to.is_some() && me.is_addressed_by(note.to.as_deref()))
            }
        }
    };
    let keys = |notes: &Notes| -> HashSet<(String, usize)> {
        notes
            .notes
            .iter()
            .flat_map(|note| (0..note.thread.len()).map(|index| (note.id.clone(), index)))
            .collect()
    };
    // Without --since, "new" is what was not there when waiting started.
    let seen = match since {
        Some(_) => HashSet::new(),
        None => keys(&store.load()?),
    };
    loop {
        // A reading error (a half-synced file) is retried at the next poll.
        if let Ok(notes) = store.load() {
            let mut new = Vec::new();
            for note in &notes.notes {
                for (index, message) in note.thread.iter().enumerate() {
                    let fresh = match since {
                        Some(since) => parse_time(&message.time).is_some_and(|time| time > since),
                        None => !seen.contains(&(note.id.clone(), index)),
                    };
                    if fresh && relevant(note, index) {
                        new.push((note.id.clone(), index));
                    }
                }
            }
            if !new.is_empty() {
                let newest = notes
                    .notes
                    .iter()
                    .flat_map(|note| &note.thread)
                    .filter_map(|message| parse_time(&message.time))
                    .max()
                    .unwrap_or(0);
                return Ok(Some((notes, new, format_time(newest))));
            }
        }
        if timeout.is_some_and(|timeout| started.elapsed() >= timeout) {
            return Ok(None);
        }
        let left = timeout.map_or(poll, |timeout| timeout.saturating_sub(started.elapsed()));
        thread::sleep(poll.min(left));
    }
}

fn message_text(text: &str) -> io::Result<String> {
    if text != "-" {
        return Ok(text.to_owned());
    }
    let mut text = String::new();
    io::stdin().read_to_string(&mut text)?;
    let text = text.trim_end().to_owned();
    if text.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty text on stdin",
        ));
    }
    Ok(text)
}

/// Paths of the project the command runs on.
struct Project<'a> {
    root: &'a Path,
    cwd: &'a Path,
}

impl Project<'_> {
    /// `file` (relative to the current folder, absolute, or relative to the
    /// root) as a root-relative `/` path.
    fn relative(&self, file: &str) -> String {
        let root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.to_path_buf());
        let candidates = [self.cwd.join(file), self.root.join(file)];
        for candidate in &candidates {
            let Ok(path) = candidate.canonicalize() else {
                continue;
            };
            if let Ok(relative) = path.strip_prefix(&root) {
                return slashes(relative);
            }
        }
        // Not there (a deleted file's notes): lexically under the root.
        let joined = self.cwd.join(file);
        match joined.strip_prefix(self.root) {
            Ok(relative) => slashes(relative),
            Err(_) => file.replace('\\', "/").trim_start_matches("./").to_owned(),
        }
    }

    /// The file's text, or a notebook cell's source.
    fn read(&self, relative: &str, cell: Option<usize>) -> Option<String> {
        let mut path = self.root.to_path_buf();
        path.extend(relative.split('/'));
        let text = std::fs::read_to_string(path).ok()?;
        match cell {
            None => Some(text),
            Some(cell) => cell_source(&text, cell),
        }
    }

    fn text(&self, note: &Note) -> Option<String> {
        self.read(&note.file, note.cell)
    }

    /// Where the note is now (orphaned if its file is gone).
    fn locate(&self, note: &Note) -> Anchored {
        reanchor(&note.anchor, &self.text(note).unwrap_or_default())
    }
}

fn slashes(path: &Path) -> String {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// A Jupyter cell's source (1-based).
fn cell_source(notebook: &str, cell: usize) -> Option<String> {
    let notebook: Value = serde_json::from_str(notebook).ok()?;
    match notebook
        .get("cells")?
        .get(cell.checked_sub(1)?)?
        .get("source")?
    {
        Value::String(source) => Some(source.clone()),
        Value::Array(lines) => Some(lines.iter().filter_map(Value::as_str).collect()),
        _ => None,
    }
}

fn pretty(value: &impl Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// The note as stored plus `"at"`: its lines now.
#[derive(Serialize)]
struct Located<'a> {
    #[serde(flatten)]
    note: &'a Note,
    at: Anchored,
}

fn note_json<'a>(note: &'a Note, at: &Anchored) -> Located<'a> {
    Located { note, at: *at }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

fn short_time(time: &str) -> String {
    // `2026-10-18T12:30:05.123Z` → `2026-10-18 12:30Z`.
    match (time.get(..10), time.get(11..16)) {
        (Some(day), Some(minute)) if time.ends_with('Z') => format!("{day} {minute}Z"),
        _ => time.to_owned(),
    }
}

/// `n7f3a  src/main.rs:12-14  open  to user  (moved)` and the thread;
/// `new` marks new messages with `*`.
fn write_note(
    out: &mut dyn Write,
    note: &Note,
    at: &Anchored,
    new: Option<&[(String, usize)]>,
) -> io::Result<()> {
    let lines = if at.start_line == at.end_line {
        at.start_line.to_string()
    } else {
        format!("{}-{}", at.start_line, at.end_line)
    };
    let cell = note
        .cell
        .map(|cell| format!(" cell {cell}"))
        .unwrap_or_default();
    let status = match note.status {
        Status::Open => "open".to_owned(),
        Status::Resolved => match &note.resolved_by {
            Some(by) => format!("resolved by {}", by.name),
            None => "resolved".to_owned(),
        },
    };
    let to = note
        .to
        .as_ref()
        .map(|to| format!("  to {to}"))
        .unwrap_or_default();
    let state = match at.state {
        crate::AnchorState::Exact => String::new(),
        state => format!(
            "  ({})",
            serde_json::to_value(state)
                .unwrap_or_default()
                .as_str()
                .unwrap_or("")
        ),
    };
    writeln!(
        out,
        "{}  {}{cell}:{lines}  {status}{to}{state}",
        note.id, note.file
    )?;
    for (index, message) in note.thread.iter().enumerate() {
        let marker = match new {
            Some(new) if new.iter().any(|(id, i)| *id == note.id && *i == index) => "* ",
            _ => "  ",
        };
        let kind = match message.author.kind {
            AuthorKind::User => "user",
            AuthorKind::Agent => "agent",
        };
        let who = if message.author.name == kind {
            kind.to_owned()
        } else {
            format!("{kind} {}", message.author.name)
        };
        let edited = if message.edited.is_some() {
            " (edited)"
        } else {
            ""
        };
        writeln!(
            out,
            "{marker}[{who}, {}{edited}]",
            short_time(&message.time)
        )?;
        for line in message.text.lines() {
            writeln!(out, "    {line}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
