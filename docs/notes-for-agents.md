# Sticky notes for agents

Sticky notes are short threads pinned to lines of a project file, between the
person using NextCode and any agent (Claude Code, Codex, a script…). The person
sees them as cards in the editor's right margin; an agent reads and writes
them with a command line. Use them to ask about a line instead of guessing,
to leave a finding where it applies ("this lock is taken twice"), or to hand
back work for review; the person answers in the card.

NextCode never runs or prompts agents: the notes are a shared file, and every
command works without a running NextCode (it edits the file; the editor
watches it and shows changes live).

```sh
next_code notes list                               # open notes, current lines, threads
next_code notes add src/parser.rs:120-124 "Why does this arm drop the error?"
next_code notes reply n7f3a "Fixed in the last commit, please check."
next_code notes resolve n7f3a
next_code notes wait --timeout 600                 # block until the person writes
next_code notes --help
```

Inside WSL or a dev container use the agent binary NextCode installed there,
with the same commands and output (see
[Pinpoint: where it is](pinpoint-for-agents.md#from-inside-wsl-or-a-dev-container)):

```sh
~/.local/share/next_code/next_code_agent notes list
```

Text output is compact; `--format json` prints the raw notes. Exit codes: 0
ok, 1 error (message on stderr) or `wait` timed out, 2 bad arguments.

## Where they live

One file per project root: `<root>/.next_code/notes.json`. It is local, not
committed: the first note adds `/.next_code/notes.json*` to the repository's
`.git/info/exclude` (never `.gitignore`).

The root is `--root DIR`, else the nearest folder above the current directory
holding `.next_code/notes.json`, else one holding `.git`, else one holding
`.next_code/`, else the current directory. Run commands from inside the
project NextCode has open (or pass `--root`) so both sides use the same file.

## Commands

| Command | What it does |
| --- | --- |
| `list [FILE\|DIR] [--open\|--resolved\|--all] [--to WHO]` | Notes (open ones by default), each with its current lines and whole thread. |
| `show ID` | One note, plus the lines it is on now (up to 10). |
| `add FILE:LINE[-LINE] TEXT… [--to WHO] [--cell N] [--color C]` | A new note. `FILE:LINE:COL-LINE:COL` marks columns too. `TEXT` is Markdown; `-` reads it from stdin. |
| `reply ID TEXT…` | Adds a message to the thread. |
| `resolve ID`, `reopen ID`, `delete ID` | Resolved notes are hidden in the editor unless the person shows them. |
| `wait [--to WHO] [--timeout S] [--since TIME]` | Blocks until someone else writes (below). |

- **Files** are relative to the current directory, absolute, or relative to
  the root; notes store them root-relative with `/`.
- **IDs** look like `n7f3a`; any unique prefix works.
- **Author.** `--as NAME` (default `$NEXT_CODE_AGENT_NAME`, else `agent`)
  names you in the thread; set `NEXT_CODE_AGENT_NAME=claude` once instead.
  `--from user` writes as the person (for scripts acting for them).
- **`--to`** says who should act: `user`, `agent` or a name. `add` defaults to
  the other side: `user` when an agent writes.
- **Notebooks.** `--cell N` anchors in Jupyter cell N (1-based); `LINE` is
  then a line of that cell.

Text output of a note:

```text
n7f3a  src/parser.rs:121-125  open  to user  (moved)
  [agent claude, 2026-10-18 12:30Z]
    Why does this arm drop the error?
  [user amedeo, 2026-10-18 12:34Z]
    It is logged by the caller.
```

`(moved)`, `(fuzzy)` or `(orphaned)` follow the line range when the text is
no longer exactly where it was written (see [Anchors](#anchors)).

## Waiting for the person

`wait` polls the file (every 300 ms) and returns as soon as a message appears
that someone else wrote and that is for you:

- by default: any message by the person, or any message in a note addressed
  to `agent` or to your `--as` name;
- with `--to WHO`: any message in notes addressed to `WHO`.

It prints the notes with new messages (new ones marked `*`) and a last line
`since: <time>`. Pass that time back as `--since` so nothing written between
two waits is missed; without `--since`, only messages written after the call
started count. A conversation loop:

```sh
since=$(next_code notes wait --timeout 900 | sed -n 's/^since: //p')
# … act on the notes, reply …
next_code notes wait --timeout 900 --since "$since"
```

With `--format json`: `{"new": [{"note": ID, "index": N}], "notes": [...],
"since": TIME}`; on timeout `{"new": [], "timed_out": true}` and exit 1.

To see where the person is when a note arrives (file, cursor, selection,
errors, terminal, failing tests), ask NextCode with `next_code context`
(`next_code_agent context` inside WSL or a dev container); it lists the
notes in the active file too. See "What the user is looking at" in
[pinpoint-for-agents.md](pinpoint-for-agents.md).

## The file

`.next_code/notes.json` is pretty-printed JSON, safe to read directly:

```json
{
  "version": 1,
  "notes": [
    {
      "id": "n7f3a",
      "file": "src/parser.rs",
      "anchor": {
        "start_line": 120,
        "end_line": 124,
        "text": "        Err(_) => None,\n…",
        "before": ["    match parse(input) {", "        Ok(v) => Some(v),"],
        "after": ["    }", ""]
      },
      "status": "open",
      "to": "user",
      "created": "2026-10-18T12:30:05.123Z",
      "updated": "2026-10-18T12:34:40.002Z",
      "thread": [
        {
          "author": { "kind": "agent", "name": "claude" },
          "text": "Why does this arm drop the error?",
          "time": "2026-10-18T12:30:05.123Z"
        }
      ]
    }
  ]
}
```

- Lines and columns are 1-based, inclusive. `start_column`/`end_column`
  (characters) are present when a selection was noted; `cell` for notebook
  cells; `color` when one was chosen; `resolved_by` (an author) when resolved;
  `edited` (a time) on edited messages. `to` absent: anyone.
- Times are RFC 3339 UTC with milliseconds.
- `--format json` replies add `"at": {"start_line", "end_line", "state"}`:
  where the note is in the file now.

**Write through the commands, not by editing the file.** NextCode, the
Windows CLI and agents in WSL or containers (through `/mnt/…` or a bind
mount) all change the same file. Every change takes the lock file
`.next_code/notes.json.lock` (created exclusively; one older than 10 seconds
is a crashed writer's and is taken over), re-reads the file, applies the one
change and writes a temporary file renamed over `notes.json`. So concurrent
writers never lose each other's messages and readers never see half a file.
A `notes.json` that does not parse is reported and never overwritten.

## Anchors

A note stores the lines it was written on, their text (up to 40 lines) and
two lines of context above and below. When the file changes, the note is
found again:

- **exact**: the same text (ignoring indentation and CRLF) is still at the
  stored lines;
- **moved**: the same text elsewhere, the occurrence nearest the old place
  (lines were inserted or removed above); for a short snippet such as `}`
  the context decides;
- **fuzzy**: the most similar lines, context included (the noted code was
  edited);
- **orphaned**: not found (the code was deleted). The note keeps its old
  line numbers and stays in `list` and the editor's Notes panel; it is never
  deleted on its own.

While the person edits a file in NextCode, the editor moves anchors with the
text and saves them. Changes made outside the editor are resolved this way
whenever notes are read.
