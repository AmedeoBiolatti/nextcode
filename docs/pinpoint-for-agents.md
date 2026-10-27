# Pinpoint for agents

Pinpoint lets an agent see what is on the user's NextCode screen, by name, and
point at it: outline a button with a numbered badge and a short note, tint a
range of code, or ask the user to click something and read back what they
picked. Use it when the user asks "how do I …?" or "where is …?": show the
answer in their window instead of describing where to click.

Everything goes through one JSON request/reply API. The easiest way in is the
command line of the editor itself:

```sh
next_code pinpoint list --visible --depth 3      # what is on screen
next_code pinpoint find "Commit"                 # by label or text
next_code pinpoint highlight menu-bar/run --note "Run the app" --step 1
next_code pinpoint clear
next_code pinpoint wait-pin --pick --timeout 120 # let the user point at something
next_code context                                # what the user is looking at
next_code pinpoint --help
```

`next_code pinpoint` never opens a window: it talks to the NextCode that is
already running (see [Reaching the editor](#reaching-the-editor)). Text output
is compact; add `--format json` for the raw reply. Exit codes: 0 ok, 1 the
editor answered with an error (printed on stderr, with suggestions), 2 bad
arguments.

## Paths

Every part of the interface that a person would name has a **path**: the ids
of its pinned ancestors, outside in, joined by `/`:

```text
menu-bar/run
workspace/left-dock/dock-tabs/tab:commit
workspace/left-dock/panel:commit/…
workspace/editor-area/pane:1/tab-bar/tab:src/main.rs
workspace/editor-area/pane:1/editor:src/main.rs/gutter/line:12
command-palette/item:<command id>
```

- A segment is `name` or `kind:key`. Names and kinds are lowercase
  kebab-case (`run-all`, `tab`, `row`). The key is the stable identity of the
  thing: a project-relative file path (it may contain `/`; with several
  project folders it starts with the folder's name), a command or step id, a
  panel id, or a 1-based number when nothing better exists (`cell:3`,
  `line:12`).
- Paths are stable across sessions and re-renders; they never depend on
  pixel positions. Menu items use their label in kebab-case
  (`menu:file/item:open-recent`).
- Popups nest under what opened them (a menu's items under its title).
- The top level: `menu-bar` (menus, Run/Debug, the run configuration, branch,
  search), `project-tabs`, then `welcome`, `workspace`, `workflow:<file>` or
  `connecting` for the shown project tab; `notice`; modal popups
  (`command-palette`, `project-picker`, `theme-picker`, …). Inside
  `workspace`: `left-dock` / `right-dock` / `bottom-dock` › `dock-tabs` ›
  `tab:<panel>` and `panel:<panel>` (`files`, `search`, `commit`, `python`,
  `terminal`, `problems`, `git-log`, `ports`, `outline`, `call-hierarchy`,
  `debug`, `debug-console`, `tests`), `editor-area` › `pane:<n>` (reading
  order) › `tab-bar` › `tab:<file>` › `close`, and `editor:<file>` /
  `notebook:<file>` / `diff:<file>@<sides>` / `compare:<sides>`; `notice`,
  `offer:<kind>` › `accept`/`dismiss`, `debug-bar`, `panel-menu`, and the
  workspace's pickers (`file-finder`, `symbol-picker`, `location-picker`,
  `revision-picker`, `worktree-picker`, `run-config-picker`). Pickers have
  `input` and `item:<key>` (a file key, `name@line`, a command id…).

Never guess a path: `list` and `find` tell you what exists right now. Only
what is drawn exists; a closed panel's contents have no nodes until it opens.

### Panel paths (files, search, git, problems, outline)

Rows are keyed by what they show, so they can be named before looking
(still check with `find`: only drawn rows exist, and lists scroll):

```text
panel:files/tree/row:src/main.rs                     file or folder row
panel:files/menu/item:rename                         its right-click menu
panel:search/query, …/match-case, …/whole-word, …/regex, …/include, …/exclude
panel:search/results/file:src/main.rs                a file's header row
panel:search/results/match:src/main.rs:12            a matching line (1-based)
panel:commit/toolbar/refresh                         rollback, show-diff, …
panel:commit/changes/group:changes                   staged, unstaged, unversioned, conflicts
panel:commit/changes/file:changes/src/main.rs/check  the file's Include in Commit box
panel:commit/commit-area/message                     amend, commit, commit-and-push
panel:git-log/commits/commit:<full sha>              label = subject
panel:git-log/branches/branch:main                   toolbar/filter:branch, details/files/file:<path>
panel:problems/problems/problem:src/main.rs:12:5     file:<path> rows above
panel:outline/symbols/symbol:Parser/parse            nested names joined by /
diff:<…>/rows/line:<old>:<new>                       `-` for the missing side
```

(Each under `workspace/<dock>/`, as `list` shows.) Git rows use the path
relative to the repository; with several repositories the commit panel puts
`<repo name>/` before the group (`file:api/changes/src/a.rs`). Dialogs sit
at `workspace/<dialog>`: `branches-popup`, `input-dialog`, `push-dialog`,
`stash-dialog`, `reset-dialog`.

### Notebook, workflow, Python, tests, terminal, ports, debugger paths

```text
notebook:demo.ipynb/toolbar/run-all                 run-above, run-below, interrupt, restart, restart-run-all, clear-outputs, save, kernel
notebook:demo.ipynb/cell:2                          cells 1-based in document order
notebook:demo.ipynb/cell:2/gutter/run               prompt ([n]), elapsed, marker
notebook:demo.ipynb/cell:2/editor                   the cell's code (Code targets with "cell": 2)
notebook:demo.ipynb/cell:2/output:1/table/row:2/cell:2:1   an HTML (DataFrame) table, rows/columns 1-based, head included
notebook:demo.ipynb/cell:3/output:1/widget:Count    ipywidgets / marimo mo.ui by description (else model id); option:<label> inside
notebook:demo.ipynb/cell:2/collapse                 show-outputs, show-source, help/close-help, banner:<kind>
workflow:dev.yaml/toolbar/run-all                   stop-all, retry-failed, outcome, inputs-summary
workflow:dev.yaml/graph/card:api/serve              qualified step id; group:<id> for groups; label = title
workflow:dev.yaml/details/artifacts/artifact:log    details: terminal, continue, restart|retry, close, output
workflow:dev.yaml/inputs-form/field:env             reset, cancel, run; zoom/zoom-in, problems/convert
panel:python/toolbar/sync                           lock, upgrade-all, python-version, refresh; script:<name>, dep:<kind>:<name>/remove
panel:tests/tree/row:<test id>/run                  debug, toggle; toolbar/run-all, run-failed, discover; filter; details
panel:terminal/tabs/tab:1/close                     tabs/new:<profile>; terminal:<n> is the shown terminal
panel:ports/row:8000/open                           copy, local-port, stop, address; toolbar/forward-port
panel:debug/toolbar/step-over                       continue, pause, step-into, step-out, restart, stop
panel:debug/frame:1                                 thread:<id>, scope:<name>, variable:<path>, watch:<expr>, breakpoint:<abs path>:<line>
panel:debug-console/input                           clear, output/line:<n>
```

## Commands

Requests are JSON objects with `cmd`; every reply has `ok` and, on failure,
`error` plus hints. With the CLI use the flags shown; `--json '{…}'` sends a
raw request.

| Command | Request | Reply |
| --- | --- | --- |
| `windows` | | `windows: [{window, title, active, size, roots}]` |
| `list` | `prefix`?, `visible`?, `interactive`?, `depth`? (below the prefix), `limit`? (1000), `full_text`? | `nodes: [{path, label?, text?, bounds: [x,y,w,h], visible, interactive}]`, `total`, `truncated`?, `duplicates`? |
| `find` | `query`, `limit`? (20) | `nodes` as in `list`, best match first |
| `highlight` | `targets: [...]`, options below | `ids`, `shown`, `results: [{id, target, status, rects? / reason, nearest_visible?, did_you_mean?}]` |
| `clear` | `ids`? (else all of the window's), `all_windows`? | `cleared: [ids]` |
| `pick` | | turns the picker on in the window |
| `pins` | `clear`? | `pins: [pin]` (oldest first), `last_seq` |
| `wait_pin` | `timeout`? (s, 120), `since`? (a `seq`; default: picks after this request), `pick`? (turn the picker on first) | `pin` |
| `context` | `max_lines`? (40), `around`? (20) | what the user is looking at; see [below](#what-the-user-is-looking-at-context) |

Node fields: `label` is what a person calls the element (a button's tooltip),
`text` what it shows (when its owner reports it), `bounds` window pixels,
`visible` whether any of it is on screen (not scrolled away), `interactive`
something to click or type in.

Every request may add `window` (an id from `windows`). Without it the CLI
picks the window whose project holds the current directory, else the active
window.

### Targets

```jsonc
{"path": "workspace/left-dock/dock-tabs/tab:commit"}          // an element
{"path": "...", "text": "Commit", "nth": 1}                    // characters of its text
{"path": "...", "range": [0, 6]}                               // byte offsets into its text
{"file": "src/main.rs", "line": 12, "column": 5, "end_line": 12, "end_column": 18}
{"file": "nb.ipynb", "cell": 3, "line": 2}                     // a notebook cell's line
{"terminal": "active", "text": "error[E0308]"}                 // terminal text (or 1-based index; line/column)
{"find": "Commit and Push"}                                    // the best visible match
"menu-bar/run"                                                 // a path as a string
"nextcode://ui/menu-bar/run"                                   // the same, as pickers copy it
"src/main.rs:12:5-12:18"                                       // a code reference
```

Code: lines are 1-based, columns count characters (1-based), `end_column` is
inclusive. Without a column the whole line(s); without `end_line` just
`line`. `file` is project-relative (multi-folder: `<folder name>/…`) or
absolute. Code reference strings: `file:line`, `file:line:col`,
`file:line:col-col`, `file:line:col-line:col`, `file:line-line`, with
`#cell=<n>` after a notebook's name. A string that is an existing path is an
element, otherwise a code reference is tried.

Each target may carry its own options; the request's apply to the rest:

- `note`: a short callout next to the target (keep it under ~12 words).
- `step` (or `badge`): a numbered badge; `"steps": true` numbers the targets
  1..n.
- `style`: `outline` (default for elements: outline with a soft glow),
  `fill` (default for text and code: a tint), `underline`.
- `color`: `accent` (default), `warning`, `error`, `green`, `blue`,
  `#rrggbb`.

Request options: `clear: true` removes the window's earlier highlights first
(use it for each new explanation), `duration` (seconds; else they stay until
cleared or the user dismisses them with Escape or the callout's ×),
`reveal: true` (open the file / scroll the code or terminal into view; the
reply waits up to 3 s for it to show), `pulse: false` (no pulse on appear).

Highlights follow their targets every frame (scrolling, resizing, panels
moving). A target that is not drawn shows nothing and comes back as
`not_visible` (`nearest_visible`: the closest ancestor on screen, e.g. the
collapsed tree or the scrolled list holding it) or `not_found` (with
`deepest_existing`, its `children`, and `did_you_mean`). They stay active and
appear as soon as their target is drawn, so you can highlight the second step
of a guide before the first step opens it.

### Pins: what the user points at

The user picks with **Pinpoint: Pick Element** (command palette, or
`Ctrl+Alt+Shift+P`; `Cmd+Alt+Shift+P` on macOS): the pointer outlines the
element under it with its path; a click pins it (Shift+click pins and keeps
picking, Escape or a right click leaves). Inside code or terminal text the
click picks the characters under the pointer. A pin is copied to the
clipboard and added to the list `pins` and `wait_pin` read:

```json
{"seq": 3, "window": 1, "kind": "element", "path": "workspace/left-dock/panel:commit/…",
 "label": "Amend", "reference": "nextcode://ui/workspace/left-dock/panel:commit/…",
 "bounds": [12, 410, 80, 22], "time": 1792300000000}
{"seq": 4, "kind": "code", "path": "…/editor:src/main.rs/text",
 "reference": "src/main.rs:12:5-12:18", "target": {"file": "src/main.rs", "line": 12, …},
 "file_absolute": "/home/me/project/src/main.rs"}
```

The user may also paste a reference into the chat (`nextcode://ui/…`,
`src/main.rs:12:5-12:18`, or the editor's **Copy Reference**): every reference
is a valid target, so you can highlight exactly what they meant.

## What the user is looking at (`context`)

Instead of asking the user to paste the error, the selection or the failing
test, ask NextCode what they are looking at:

```sh
next_code context                    # Markdown, ready to read
next_code context --format json      # the raw reply
next_code context --around 5 --max-lines 80
next_code_agent context              # inside WSL or a dev container
```

`next_code context` is `next_code pinpoint context` (same window choice,
`--window`, `--format json`). The person can hand you the same thing with
**Pinpoint: Copy Context for Agent** (command palette), which copies the
Markdown to the clipboard. Everything is read when you ask (nothing is
tracked meanwhile) from the window's shown project tab:

| Member | What |
| --- | --- |
| `window`, `title`, `project` | window id and title; `project.roots: [{name, path}]`, `project.remote` |
| `focus` | `editor`, `notebook`, `workflow`, `panel:<key>` (`panel:terminal`, `panel:tests`, …) or null |
| `editor` | the active text tab: `file` (project path, as `file_key` writes it), `path` (absolute), `language`, `dirty`, `line_count`, `cursors: [{line, column}]` (primary first), `selections: [{start, end, text, truncated}]`, `visible_lines: [first, last]`, `around: {start_line, text, truncated}` (`around` lines above and below the cursor), `diagnostics: [{line, column, end_line, end_column, severity, message, source, code, at_cursor}]`, `diagnostics_total`, `notes: [{id, line, end_line, anchor (exact/moved/fuzzy/orphaned), status (open/resolved), to, author, text, replies, visible}]` (sticky notes on the file, where they are now; see [notes-for-agents.md](notes-for-agents.md)) |
| `notebook` | the active notebook tab: `file`, `path`, `dirty`, `cells`, `cell: {index (1-based), kind, source, source_truncated, execution_count, run (idle/queued/running/ok/failed), outputs: [{type, text, truncated}], error ("Name: value")}`, `notes` (sticky notes on any cell, the editor's fields plus `cell` (1-based), `line`s within the cell, no `visible`) |
| `workflow` | a shown workflow tab instead of a project: `name`, `file`, `path`, `dirty`, `outcome` (idle/running/ready/failed/stopped), `problems: [{message, line, column}]` (why the file cannot run: it does not parse or is invalid), `steps: [{id, status, detail?, error?, last_lines?, reason?}]` (once it ran) |
| `tabs` | open tabs: `{file, title, kind (editor/notebook/diff/compare), dirty, active}` |
| `open_panels` | the panels shown in open docks (`terminal`, `tests`, …) |
| `terminal` | the Terminal panel's shown tab: `title`, `tabs` (count), `lines` (last `max_lines` lines of the screen), `truncated`, `running`, `exit_code` (once its process ended), `folder`, `run_name`, `cwd` (the shell's directory now: what the shell reports with OSC 7 / `633;P;Cwd=` / `9;9;`, else for a local Linux shell its foreground job's directory from `/proc`; absent otherwise, e.g. Windows or WSL/container shells that report nothing), `last_command_exit_code` (only when the shell reports finished commands with shell integration `OSC 133;D;<code>` or `633;D;<code>`) |
| `problems` | `errors`, `warnings`, `total`, `items: [{file, line, column, severity, message, code}]` |
| `tests` | after tests were found or run: `status` (the panel's status line), `running`, `passed`, `failed`, `skipped`, `failures: [{id, label, file, line, status, message, output_tail, output_truncated}]` |
| `debugger` | the last debug session: `name`, `state` (starting/running/paused/stopping/ended), `stop_reason` (the adapter's DAP reason: breakpoint/step/exception/entry/pause…), `stop_description` (its wording, e.g. "division by zero"), `exception` ("ZeroDivisionError: division by zero" when stopped on one), `location: {file, line}`, `frames: [{name, file, line, selected}]` (top 10), `scope` + `locals: [{name, value, type}]` (what the Debug panel already fetched; debugpy's "special variables"-style groups left out), `exit_code`, `output_tail` + `output_truncated` (the program's last output lines in the Debug Console, e.g. a traceback), `error` |

Lines and columns are 1-based (columns count characters). Size and privacy:
each text is cut to `max_lines` lines (a terminal and outputs keep their last
lines) and 8000 characters, lists to 50 entries, and no file content is sent
beyond the selections and the `around` lines (`--around 0`: none). From WSL or
a dev container, `file`s and paths come back as that machine's absolute paths.

## Text and code

Letters are addressed through the element that shows them; rects come back
one per screen row (a range over 3 lines is 3 rects, a wrapped line one per
row it wraps to).

- **Code** (`{"file", "line", …}`): every editor showing the file answers
  (split panes too). Columns count characters, so `é`, `日` and emoji are one
  column each (an emoji's rect is two cells wide); tabs are one column;
  inlay hints (`: i32`) are drawn but never part of a range. An empty line
  inside a range gets a narrow mark. A range that is partly scrolled away,
  cut off sideways or inside a fold reports the part on screen (a fold's
  `⋯` stands for its hidden lines) and is marked as needing `reveal`.
- **`reveal: true`** shows the file in the pane that already has it, else
  opens it in the active pane; scrolls the range to the middle of the view
  (its top when taller than the view), sideways when needed, and opens
  folds over it. The cursor, selections and keyboard focus stay where the
  user has them.
- **Notebooks**: `{"file": "nb.ipynb", "cell": 3, "line": 2, …}` — `cell` is
  1-based in document order, `line`/`column` count within the cell's
  source. `reveal` scrolls the notebook to the cell. Only cells showing
  their source are code (a rendered Markdown cell is `cell:<n>/markdown`).
- The editor's code area is the node `…/editor:<file>/text` (in a notebook
  `…/cell:<n>/editor/text`); its `text` is the text of the lines on
  screen, so `{"path": "…/text", "text": "fn main"}` finds visible code
  without line numbers.
- **Terminal**: `{"terminal": 1, "text": "error", "nth": 2}` (or
  `"active"`) matches the visible screen: rows joined with `\n`, trailing
  blanks trimmed (a match does not span a soft-wrapped row). With `reveal`
  the Terminal panel opens on that terminal and, for `text`, scrolls back
  through the history to the lowest line holding it.
  `{"terminal": 1, "line": 3, "column": 1, "end_column": 5}` counts rows of
  the visible screen (1-based) and characters. The terminal's node is
  `…/panel:terminal/terminal:<n>` and its `text` is the screen.
- **UI text**: selectable labels and messages (status bar, notices, error
  text, rows built from text) give their element character rects:
  `{"path": "…/status-bar/…", "text": "error"}`. When an element holds
  several such texts, its `text` is them joined with `\n`. Elements whose
  text is only known as a caption (buttons) highlight whole
  (`approximate`).

What the user gives you:

- **Copy Reference** (`Ctrl+Alt+Shift+C`, Edit menu, command palette, or
  right-click on code): the selection as `src/main.rs:12:5-12:18`, whole
  selected lines as `src/main.rs:12-14`, the cursor's line as
  `src/main.rs:12`, one reference per line for several selections; in a
  notebook cell `nb.ipynb#cell=3:2:5-2:9`.
- **Picking** in code takes the selection under the pointer, else the word
  (letters, digits, `_`) or the single character there, and pins it as a
  code reference. In terminal or UI text it pins
  `nextcode://ui/<path>#text=<word>&nth=<k>` (the k-th occurrence in that
  element's text), itself a valid target.

## Examples

### "How do I commit only some files?"

1. Find the pieces (paths are stable, so once is enough):

   ```sh
   next_code pinpoint list workspace/left-dock --depth 2
   next_code pinpoint list workspace/left-dock/panel:commit --interactive
   ```

   The Commit panel lists each changed file as
   `…/panel:commit/changes/file:changes/<file>` with its checkbox `…/check`
   (unversioned files: `file:unversioned/<file>`); the button is
   `…/panel:commit/commit-area/commit`, the message box
   `…/commit-area/message`.

2. Show the steps at once (when the panel is closed, steps 2 and 3 come
   back `not_visible`/`not_found` and appear as soon as the user opens it):

   ```sh
   next_code pinpoint highlight --clear \
     workspace/left-dock/dock-tabs/tab:commit --step 1 --note "Open the Commit panel" \
     workspace/left-dock/panel:commit/changes/file:changes/src/mod2.rs/check \
       --step 2 --note "Untick the files you don't want in this commit" \
     workspace/left-dock/panel:commit/commit-area/commit \
       --step 3 --note "Write a message, then click Commit"
   ```

   ```text
   #4 shown {"path":"workspace/left-dock/dock-tabs/tab:commit"}
   #5 shown {"path":"workspace/left-dock/panel:commit/changes/file:changes/src/mod2.rs/check"}
   #6 shown {"path":"workspace/left-dock/panel:commit/commit-area/commit"}
   ```

   Or as JSON (`--json`, the `--automation` passthrough, or a remote agent):

   ```json
   {"cmd": "highlight", "clear": true, "targets": [
     {"path": "workspace/left-dock/dock-tabs/tab:commit", "step": 1, "note": "Open the Commit panel"},
     {"path": "workspace/left-dock/panel:commit/changes/file:changes/src/mod2.rs/check", "step": 2,
      "note": "Untick the files you don't want in this commit"},
     {"path": "workspace/left-dock/panel:commit/commit-area/commit", "step": 3,
      "note": "Write a message, then click Commit"}
   ]}
   ```

3. Read `results`: for each `not_visible`/`not_found` target, say what to do
   first ("open the Commit panel") or fix the path from `did_you_mean`.
   `clear` when the user is done (they can also press Escape or click a
   callout's ×). `find` is handy to discover, but several elements often
   share a label ("Commit" is both the tab and the button): highlight by
   path.

### Pointing at code

```sh
next_code pinpoint highlight src/parser.rs:120:9-120:31 --reveal \
  --note "This match arm drops the error" --color warning
next_code pinpoint highlight '{"path": "workspace/editor-area/pane:1/editor:src/parser.rs/gutter/line:120"}' \
  --note "Click here to set a breakpoint"
```

`--reveal` opens the file if needed and scrolls the range into view.

### Asking the user to point

```sh
next_code pinpoint wait-pin --pick --timeout 120
```

Tell the user first ("click the button you mean"); the reply holds the pin.
Without `--pick` the user starts picking with the shortcut.

## Reaching the editor

- **CLI** (`next_code pinpoint …`): finds running editors through their
  instance files `<data dir>/next_code/pinpoint/<pid>.json` (`{port, token,
  pid, started, roots}`; `%LOCALAPPDATA%` on Windows, `~/.local/share` on
  Linux), skips stale ones, and picks the editor whose project holds the
  current directory, else the most recently started. From WSL, call the
  Windows binary: `next_code.exe pinpoint list`.
- **Raw protocol**: TCP on 127.0.0.1 at `port`, one JSON request per line with
  `"token": "<token>"` added, one JSON reply line each (an optional `id` is
  echoed). Connections may send several requests.
- **`--automation`**: `{"cmd": "pinpoint", "request": {…}}` forwards to the
  editor and returns its reply as `result`.
- **`next_code context`** is the same client (`next_code pinpoint context`).
- Agents inside WSL or a dev container use the remote agent binary (see
  [below](#from-inside-wsl-or-a-dev-container)).

## From inside WSL or a dev container

An agent working where a remote project's files live (the WSL distro or dev
container NextCode opened the project in) uses the agent binary NextCode
already runs there. Commands, options, output and exit codes are the same as
`next_code pinpoint`:

```sh
~/.local/share/next_code/next_code_agent pinpoint list --visible --depth 3
~/.local/share/next_code/next_code_agent pinpoint highlight src/main.rs:12:5-12:18 --note "this loop"
~/.local/share/next_code/next_code_agent pinpoint wait-pin --pick --timeout 120
```

- **Where it is.** NextCode installs each agent build as
  `~/.local/share/next_code/next_code_agent-<protocol>-<hash>` in the home of
  the user it runs as (the container's `remoteUser`), and every agent that
  starts points the stable symlink `~/.local/share/next_code/next_code_agent`
  at itself, so the path survives editor updates. Put the directory on `PATH`
  or `alias next_code_agent=~/.local/share/next_code/next_code_agent`.
- **Which window.** While the project is open, its agent listens on a Unix
  socket and relays each call over the editor's connection to the agent to
  the window of that project (`--window` still overrides). With several
  projects open on one machine, the caller's working directory picks the
  innermost project folder containing it, else the most recently opened.
  Sockets live in `$XDG_RUNTIME_DIR/next_code/` or `/tmp/next_code-<uid>/`
  (mode 0700; only the same user, or root, may connect).
  `NEXT_CODE_PINPOINT_SOCKET=<dir>/<pid>-<n>.sock` picks one explicitly (root
  calling an editor agent that runs as another user), `NEXT_CODE_PINPOINT_DIR`
  moves the directory (for the agent and callers alike).
- **Paths are the machine's own.** Code targets may be absolute
  (`/workspaces/app/src/main.rs:12`), relative to the working directory
  (made absolute when the file exists there), or project-relative as
  NextCode copies them (`src/main.rs:12:5`). Pins and other answers name
  files by their absolute path on that machine
  (`/workspaces/app/src/main.rs:12:5-12:18`); element paths are unchanged.
- Without an open remote project on that machine the call fails with "no
  NextCode editor serves pinpoint on this machine".
