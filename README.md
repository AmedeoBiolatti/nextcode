# next_code

Native, AI-first IDE written in Rust on [gpui](https://crates.io/crates/gpui).

## Layout

| Crate | Purpose |
|---|---|
| `crates/text` | UI-agnostic rope buffer: edits, undo/redo, grapheme/word motion, UTF-16 mapping, line endings, change log |
| `crates/syntax` | Incremental tree-sitter parsing and highlight spans for Bash, C, C++, CSS, Dockerfile, Go, HTML, JavaScript, JSON, Markdown, Python, Rust, SQL, TOML, TypeScript/TSX, YAML |
| `crates/search` | Gitignore-aware project text search with regex, globs and streaming results |
| `crates/watcher` | Debounced project file watching with `.gitignore` filtering |
| `crates/terminal` | Shell process and terminal emulation, local or attached to a remote pty |
| `crates/worktree` | Project file operations shared by the editor and the agent: listing, ignore rules, tool lookup |
| `crates/proto` | Versioned editor ↔ agent wire protocol (framing, requests, events) |
| `crates/agent` | `next_code_agent`: headless Linux process serving files, watching, search, language servers and shells |
| `crates/remote` | Agent client and the bootstraps that install and start it in WSL or a container (`docker exec`) |
| `crates/devcontainer` | `devcontainer.json` (JSONC) and bringing dev containers up with the docker CLI |
| `crates/python` | `pyproject.toml` / `uv.lock`, uv command lines, pytest node ids and traceback locations |
| `crates/git` | Git through its CLI: porcelain v2 status, stage/unstage/discard/commit, hunk patches, branches, worktrees, fetch/pull/push, history, diffs |
| `crates/ui` | `next_code` binary: editor, docks, LSP, terminal, search, problems, git and Python panels, diffs, fuzzy file finder (`Ctrl+P`) |

## Build and run

Windows (MSVC Build Tools required), macOS, or Linux:

```sh
cargo run --release -p next_code -- C:\path\to\your\project
```

Replace the example path with an existing project folder. Without an argument, the current directory is opened.

For example, from PowerShell:

```powershell
cargo run --release -p next_code -- C:\dev\next_code
```

Linux additionally needs the gpui system libraries (Wayland/X11, xkbcommon, fontconfig, Vulkan).

On Windows the editor is a GUI-subsystem program: no console window opens
with it. Started from a terminal, it returns to the prompt right away;
argument errors and `--headless` output still print to that terminal (or to
redirected streams), and closing the terminal does not close the editor.

Set `NEXT_CODE_TRACE_STARTUP=1` to print startup milestones (time since the
process was created) to stderr, e.g. from PowerShell:
`$env:NEXT_CODE_TRACE_STARTUP=1; .\next_code.exe C:\dev\p 2> trace.txt`.
The WSL connection (or, for local projects, git status) starts on a thread
before the window, alongside gpui's platform setup; DirectWrite's font list
and the shell's drag-and-drop helper are loaded on threads of their own
while gpui creates its Direct3D devices.

## WSL projects

Projects inside a WSL distro are opened by distro and Linux path; the editor
runs on Windows and every project operation (files, watching, search,
language servers, terminal) runs in the distro through `next_code_agent`,
over `wsl.exe` stdin/stdout. Files are never accessed via `\\wsl.localhost`.

Build the Linux agent once (needs Rust inside the distro; it is linked
statically, so it runs in any x86_64 distro) and it is copied next to
`next_code.exe`:

```powershell
scripts\build-wsl-agent.ps1            # or: -Profile debug, -Distro Ubuntu
cargo run --release -p next_code -- --wsl Ubuntu /home/me/project
```

A `\\wsl.localhost\Ubuntu\home\me\project` argument opens the same way, and
`~/project` is expanded in the distro. On first use the agent is installed
to `~/.local/share/next_code/` in the distro (reinstalled when the build
changes) and started through a login shell, so language servers come from the
user's `PATH` there (`rustup component add rust-analyzer`,
`uv tool install basedpyright`). `NEXT_CODE_WSL_AGENT` overrides the agent
binary's location.

The real-WSL tests are ignored by default:

```powershell
$env:NEXT_CODE_WSL_AGENT = "$PWD\target\release\next_code_agent"
$env:NEXT_CODE_TEST_WSL_DISTRO = "Ubuntu"
cargo test -p remote --test wsl -- --ignored
```

## Dev containers

A folder with `.devcontainer/devcontainer.json` (or `.devcontainer.json`,
or `.devcontainer/<name>/devcontainer.json`) offers **Reopen in Container**
when opened. The container is brought up with the `docker` CLI where the
folder lives (Docker Desktop on Windows, or docker inside the WSL distro for
WSL folders), then the same agent as for WSL is copied into it and started
with `docker exec`: files, search, git, language servers and terminals all run
in the container, as `remoteUser`, in `workspaceFolder`, with `remoteEnv`.

```powershell
next_code --devcontainer C:\src\project
next_code --devcontainer --wsl Ubuntu /home/me/project
```

`devcontainer:<location>` also works in Open Recent. Supported: `image`,
`build` (Dockerfile, args, target), `dockerComposeFile` + `service` (+
`runServices`), `workspaceFolder`, `workspaceMount`, `mounts` (not for
compose), `containerEnv`, `remoteEnv` (`${containerEnv:PATH}` and the other
`${…}` variables), `remoteUser`, `containerUser`, `runArgs`, `init`,
`privileged`, `capAdd`, `securityOpt`, `overrideCommand`, `shutdownAction`,
and the lifecycle commands (`initializeCommand` on the host; `onCreate`,
`updateContent`, `postCreate` after creation; `postStart` after a start;
`postAttach` each time). Dev Container Features are not installed (a note
names them). The Dev Containers CLI is not needed.

Containers are found by the labels VS Code uses (compose ones by project
and service, named as VS Code names them), so a container VS Code created is
reused. A stopped one is started; one that no longer starts (e.g. a mount
from an old session is gone) is recreated. Containers the editor created or
started are stopped as `shutdownAction` says when its project tab closes.

## Python (uv)

The **Python** panel (left dock) reads `pyproject.toml` and `uv.lock`:
project, interpreter (`.venv`), pinned version (`.python-version`) and the
dependencies of each section with their locked versions. It runs uv where
the project lives (Windows, WSL or a dev container):

- **Sync**, **Lock**, **Upgrade all**, per-package **↑** (upgrade in the
  lock) and **×** (remove); a requirement typed in *Add dependency* goes to
  the section shown next to it (click to cycle: dependencies, `dev`, other
  groups, extras).
- **Python…** lists interpreters (`uv python list`) and pins one.
- When `.venv` is out of date with `uv.lock`/`pyproject.toml`
  (`uv sync --check`), the editor offers **Sync**. After a sync the
  language server re-reads the interpreter.
- **Run**: `[project.scripts]` entry points, the current file, all tests,
  the current file's tests, or the test under the cursor (pytest node id
  from the enclosing `test_*` function / `Test*` class), each with `uv run`
  in a new terminal tab. Ctrl+click on a traceback line (`File "…", line N`
  or pytest's `path:N:`) opens it.

New terminals activate `.venv` (`VIRTUAL_ENV`/`PATH` locally;
`. .venv/bin/activate` in WSL and containers).

## Git and multiple projects

The Git panel (`Ctrl+Shift+G`, left dock) shows the branch, ahead/behind
counts and changes in Merge Conflicts / Staged Changes / Changes sections,
as a compact folder tree (single-folder chains like `src/editor` collapse
into one row; folders fold) or as a flat list. Rows stage (`+`), unstage
(`−`) or discard (`↺`, asks first; untracked files are deleted); clicking a
row opens its diff in a tab, and clicking a diff line opens the file there.
Enter in the message box commits what is staged; "Amend" rewrites the last
commit instead (with what is staged, or just its message). Git runs where the
project lives (inside WSL for WSL projects), and the panel refreshes on file
changes and on index/ref changes (commits or checkouts from a terminal).

Diffs show unified or side by side, and changed hunks only or the whole
file; the last choice sticks for new diffs. Diffs of changes in the index
or working tree come from `git diff` itself and act per hunk: "Stage hunk"
and "Discard" (asks first) on unstaged changes, "Unstage hunk" on staged
ones.

**Branches**: clicking the branch name opens the branch picker: switch to a
branch, a remote branch (checks out a tracking branch) or a tag (detached),
type a new name to create a branch from HEAD and switch to it, or `×` to
delete a branch (asks; "Force Delete" for unmerged ones). In History,
"Create branch here…" starts a branch at the selected commit.

**Fetch / Pull / Push** sit under the branch name with the behind/ahead
counts. Fetch is `--all --prune`, pull is fast-forward only (diverged
branches are left to a terminal), and the first push of a branch sets its
upstream on `origin` (or the only remote). Git never prompts: credentials
come from your credential helper or SSH agent, and failures are shown.

**Worktrees…** lists the repository's worktrees; choosing one opens it in
a project tab. Typing a branch name adds a worktree for it (creating the
branch if needed) next to the repository as `<repository>-<branch>` and
opens it; `×` removes one (asks; its branch is kept).

**History** (panel toolbar) opens the commit graph of all branches (or the
current one): lanes, branch/tag labels, author and age. Selecting a commit
shows its details and changed files; each opens its diff against the first
parent, and "Compare with working tree" starts a comparison from it.

**Compare…** compares the working tree with a branch, tag or any revision
(`HEAD~3`, a commit id). A comparison tab lists the changed files as a tree
or list; its base and target can be changed (the target may be another
revision, and then swapped), and each file opens its diff.

## Window, menus and commands

Top to bottom, a window has the menu bar (File, Edit, View, Go, Run, Git,
Help) with the command bar in it, the project tabs, and the active
project. On Windows the menu bar is also the title bar: the system one is
hidden, empty space in the bar moves the window (double click maximizes),
and minimize/maximize/close sit at its right. Each project tab is a whole
workspace (docks, editor tabs, language servers, terminals); closing it
asks about unsaved files first.

- **File > Open Project…** (`Ctrl+O`) picks a folder (a
  `\\wsl.localhost\...` folder opens through WSL) and opens it in a new
  tab; **Open Project in New Window…** does the same in its own window.
- **File > Recent Projects** lists recent projects; **Open Recent…**
  (`Ctrl+R`) also accepts a typed location such as `Ubuntu:~/project` or
  `devcontainer:C:\src\app`.
- Opening a project that is already open (in any window) switches to its
  tab. `Ctrl+Alt+]` / `Ctrl+Alt+[` cycle the tabs.
- The command bar (`Ctrl+Shift+P` or `F1`) lists every command available
  where focus is, with its shortcut; typed words filter it (in any order),
  `Enter` runs the command where focus was.

Every menu item is a command, the same ones the palette runs. Menus are
customizable: **File > Customize Menus…** writes the built-in menus to
`menus.json` in the config dir (`%APPDATA%\next_code` on Windows,
`~/.config/next_code` on Linux) and opens it; the file then replaces the
built-in menus, and edits apply when a menu is next opened:

```json
{ "menus": [
    { "label": "File", "items": [
        "app::OpenProject",
        { "label": "Recent Projects", "list": "recent_projects" },
        "-",
        { "command": "editor::Save", "label": "Save File" },
        { "label": "More", "items": ["app::About"] }
    ] }
] }
```

An item is a command name, `"-"` (separator), a command with its own
label, a submenu, or a list. Command names are `namespace::Name` (`app`,
`workspace`, `editor`, `git`, `python`); unknown ones show disabled with a
notice, and an invalid file falls back to the built-in menus.

## Themes and highlighting

Colors come from a theme: the whole UI, the terminal's palette and syntax
highlighting. `Dark` (the default) and `Light` are built in; user themes are
JSON files in `themes\` in the config dir (`%APPDATA%\next_code\themes` on
Windows, `~/.config/next_code/themes` on Linux), named by their `"name"`
(else the file name; built-in names are reserved). `"theme": "<name>"` in
`settings.json` picks one.

- **File > Select Theme…** lists the built-in and user themes; choosing one
  writes `"theme"` into `settings.json` (keeping the other settings) and
  applies it.
- **File > Customize Theme…** creates `themes/<Name> (custom).json`
  extending the current built-in theme, with one example per section,
  selects it and opens it; on a user theme it just opens its file.

Edits to `settings.json` or to a theme file (or one it extends) apply
without a restart: when the file is saved in the editor, when a window is
activated, and when a menu or the command bar opens. Terminals opened
before a theme change keep their palette.

```json
{ "name": "Mine", "extends": "Dark",
  "ui": { "accent": "#3574f0", "selection": "#21428380" },
  "terminal": { "cursor": "#ced0d6", "ansi": ["#1e1f22", "…16 colors…"] },
  "syntax": {
    "keyword": "#cf8e6d",
    "keyword.control": { "bold": true },
    "comment.doc": { "color": "#5f826b", "italic": true }
  },
  "languages": { "python": { "syntax": { "function.builtin": "#56a8f5" } } },
  "semantic_modifiers": { "deprecated": { "strikethrough": true } } }
```

- Colors are `#rrggbb` or `#rrggbbaa`. A style is a color or an object
  with `color`, `background`, `bold`, `italic`, `underline`,
  `strikethrough`.
- `extends` names any theme; the child wins key by key, and a style set in
  both merges field by field. A theme without `extends` must set every
  `ui` color and the whole `terminal` palette.
- `ui` keys: `background`, `panel`, `border`, `text`, `muted`, `accent`,
  `accent_text`, `on_accent`, `warning`, `error`, `active_row`,
  `hover_row`, `selection`, `cursor`, `diff_added`, `diff_removed`,
  `git_added`, `git_modified`, `git_renamed`, `git_deleted`,
  `git_conflicted`, `graph_lanes` (a list, cycled), `branch`, `tag`. An
  unknown key is an error naming it; the theme then falls back to `Dark`
  with a notice under the menu bar.
- `syntax` keys are highlight scopes, most general part first (`keyword`,
  `keyword.control`, `function.method`, `variable.parameter`,
  `markup.heading`, …). A scope takes the style of every prefix the theme
  defines, the most specific last: `function.method` is drawn with
  `function`'s style unless `function.method` sets its own; scopes the theme
  does not cover use the text color. Unknown scopes are reported in the
  notice but do not stop the theme.
- `languages` overrides layer over `syntax` for one language, keyed by its
  id (`c`, `cpp`, `python`, `rust`, `sql`, …).
- `semantic_modifiers` style language-server token modifiers
  (`deprecated`, `mutable`, …) over the token's style; today `deprecated`
  also styles deprecated completion items. The built-in themes strike
  `deprecated` through and underline `mutable` (as RustRover does).

**Semantic highlighting.** When the file's language server provides
semantic tokens (rust-analyzer, basedpyright, …), the editor layers them
over tree-sitter highlighting: a run gets its tree-sitter style, then the
token type's scope (`parameter` → `variable.parameter`, `struct` →
`type.struct`, `enumMember` → `variant`, `namespace` → `module`,
`selfParameter` → `variable.builtin`, …; the full table is `TOKEN_TYPES` in
`crates/ui/src/editor/semantic.rs`) unless tree-sitter already found a more
specific scope of the same family (a doc comment stays `comment.doc`), then
each modifier's `semantic_modifiers` style. Diagnostics underlines, faded
unused code and IME underlines still take precedence. Tokens are requested
when a file opens on the server, 200 ms after typing stops, and when the
server asks for a refresh (e.g. rust-analyzer after indexing); until a new
answer arrives, the previous tokens follow the edits and those an edit
touches are dropped. `"semantic_highlighting": false` in `settings.json`
turns this off (default `true`); the change applies live like a theme
change.

## Headless mode

### UI automation (Linux / WSL)

`next_code --automation [--port 0] [--width 1440] [--height 900] -- PROJECT_DIR`
starts the normal GUI on a private Xvfb display, without a visible desktop or
terminal window. An agent or supervisor launches this process and reads its
single `ready` JSON line containing a loopback HTTP URL and bearer token.
Keep the process running for the session. All existing UI code, key bindings,
menus, and rendering are used unchanged. This is separate from the service
mode described below.

Requires Python 3, Xvfb, xauth, xdotool, ImageMagick (`import`), xclip, and the
normal Linux GPUI graphics libraries and fonts. On Debian/Ubuntu the automation
tools are available with `apt install xvfb xauth xdotool imagemagick xclip`.
A working Vulkan driver is still required; a software Vulkan driver can be
used on machines without a GPU. This backend currently supports Linux, including
WSL, rather than native Windows or macOS.

Send JSON requests to the reported URL with `Authorization: Bearer TOKEN`:

```json
{"id":1,"cmd":"key","key":"ctrl+p"}
{"id":2,"cmd":"text","text":"src/main.rs"}
{"id":3,"cmd":"key","key":"Return"}
{"id":4,"cmd":"mouse_move","x":500,"y":300}
{"id":5,"cmd":"click","button":1,"count":2}
{"id":6,"cmd":"screenshot"}
```

Replies contain `id`, `ok`, and `result` or `error`. Screenshots return
`{mime_type, base64, width, height}` containing a PNG of the private display,
including application dialogs. Coordinates are display pixels at scale 1.
`info` lists the available operations. Keyboard names use X11 keysyms (for
example `Return`, `Escape`, `Left`, `F1`, `ctrl+shift+p`). `key_down` and
`key_up` allow held keys and modifiers. `mouse_down` and `mouse_up` accept
X11 button numbers, including additional buttons; move between them to drag.
`scroll` accepts `direction` (`up`, `down`, `left`, `right`) and `count`.
`text` pastes Unicode through the private display's clipboard. Clipboard paste
requires the target control's normal paste binding. Key events remain available
for controls with different bindings. `shutdown` stops the server and children;
Ctrl+C or terminating the supervisor also ends the session. The editor closes
its windows on SIGTERM/SIGINT/SIGHUP (no unsaved-files prompt), so dev
containers it started are stopped as on a normal exit; a second signal ends it
immediately.

When the GUI panics, the message and a backtrace are appended to
`next_code/crash.log` in the local data dir (`%LOCALAPPDATA%` on Windows,
`~/.local/share` on Linux); without a console there is otherwise no trace.

Requests execute serially. Input acknowledgements mean the platform operation
finished, not that asynchronous application work or a rendered frame completed.
Use `wait` with `ms` (0–10000), then inspect a screenshot; repeat with a bounded
deadline when waiting for search results, language servers, or other work.
Frame completion tracking and structured action discovery are not exposed by
this backend yet; commands can be selected through the normal command palette.

The server binds only to loopback and requires its random session token. The
display requires an X authority cookie and disables TCP connections. Settings
and caches are isolated for each session; project files are real and UI actions
have their usual effects. The implementation is embedded in the executable from
`scripts/automation.py`, so distributing a separate script is unnecessary.
Its API uses platform input and pixels and contains no panel-specific logic;
new UI features are available without updating the automation backend.

`--headless` runs the same project services without a window, for scripts,
agents and end-to-end tests. Commands are JSON lines on stdin; each gets one
JSON reply line on stdout, and events (`files_changed`, `diagnostics`,
`server_status`, `disconnected`) arrive as extra lines:

```powershell
cargo run --release -p next_code -- --headless --wsl Ubuntu ~/project
```

```json
{"id": 1, "cmd": "open", "path": "src/main.rs"}
{"id": 2, "cmd": "diagnostics", "path": "src/main.rs", "wait_ms": 60000}
{"id": 3, "cmd": "exec", "command": "cargo test"}
{"id": 4, "cmd": "shutdown"}
```

Commands: `info`, `list_dir`, `list_files`, `read`, `write`, `stat`,
`search`, `watch`/`unwatch`, `open`/`close` (language server documents),
`diagnostics`, `definition`, `semantic_tokens` (decoded as the editor draws
them), `exec` (shell command in the project root, where
the project lives), `git_status`, `git_diff`, `git_apply_hunk`, `git_stage`,
`git_unstage`, `git_discard`, `git_commit`, `git_branch_create`,
`git_switch`, `git_branch_delete`, `git_fetch`, `git_pull`, `git_push`,
`git_worktrees`, `git_worktree_add`, `git_worktree_remove`, `git_log`,
`git_refs`, `git_changes`, `git_commit_changes`, `python_info`, `python_sync`,
`python_lock`, `python_add`, `python_remove`, `python_pin`,
`python_interpreters`, `python_test_at`, `devcontainer_configs`, `shutdown`;
`--devcontainer` opens the folder's container (with `progress` events).
Parameters and
results are documented in
`crates/ui/src/headless.rs`.

## Roadmap

- **M0** editor core: window, file tree, tabs, editing, save, syntax highlighting, fuzzy file finder — *done*
- **M1** LSP (completion, go-to-definition, diagnostics), resizable docks, terminal, project search, Problems panel, external file watching — *done*
- **M2** host agent protocol, WSL projects — *done*
- **M3** git panel and diff view, multiple workspaces — *done*
- **M4** git review: commit graph, compare with branches/commits, compact change trees, side-by-side and whole-file diffs — *done*
- **M5** git operations: branches, worktrees, fetch/pull/push, amend, hunk staging — *done*
- **M6** uv projects (Python panel, environments, run/test) and dev containers — *done*
- **M6.1** window chrome: menu bar, command palette, project tabs, customizable menus — *done*
- **M7** AI inline completion, chat, agent
- **M8** debugger (DAP)
- **M9** WASM extension API
