# next_code

Native, AI-first IDE written in Rust on [gpui](https://crates.io/crates/gpui).

## Layout

| Crate | Purpose |
|---|---|
| `crates/text` | UI-agnostic rope buffer: edits, undo/redo, grapheme/word motion, UTF-16 mapping, line endings, change log |
| `crates/syntax` | Incremental tree-sitter parsing and highlight spans for Bash, C, C++, CSS, Dockerfile, Go, HTML, JavaScript, JSON, Markdown, Python, Rust, SQL, TOML, TypeScript/TSX, YAML |
| `crates/search` | Gitignore-aware project text search with regex, globs and streaming results |
| `crates/watcher` | Debounced project file watching with `.gitignore` filtering |
| `crates/terminal` | Shell process and terminal emulation, local or attached to a remote pty; links (URLs, file locations, OSC 8) in its output |
| `crates/worktree` | Project file operations shared by the editor and the agent: listing, ignore rules, tool lookup |
| `crates/proto` | Versioned editor ↔ agent wire protocol (framing, requests, events) |
| `crates/agent` | `next_code_agent`: headless Linux process serving files, watching, search, language servers and shells |
| `crates/remote` | Agent client and the bootstraps that install and start it in WSL or a container (`docker exec`) |
| `crates/devcontainer` | `devcontainer.json` (JSONC) and bringing dev containers up with the docker CLI |
| `crates/python` | `pyproject.toml` / `uv.lock`, uv command lines, pytest node ids |
| `crates/git` | Git through its CLI: porcelain v2 status, stage/unstage/discard/commit, hunk patches, branches, worktrees, fetch/pull/push, history, diffs |
| `crates/ui` | `next_code` binary: editor, docks, LSP, terminal, search, problems, git and Python panels, diffs, fuzzy file finder (`Ctrl+P`) |

## Build and run

Windows (MSVC Build Tools required), macOS, or Linux:

```sh
cargo run --release -p next_code -- C:\path\to\your\project
```

Replace the example path with an existing project folder (`.` is the
current directory). Without an argument, the windows and project tabs that
were open when the editor last closed come back (see
[Window, menus and commands](#window-menus-and-commands)); the first time,
or with nothing to reopen, a window with the welcome screen opens.

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

## Port forwarding (dev containers and WSL)

Like VS Code's Ports view: a server running where the project lives is
reachable on `localhost` here. The **Ports** panel (bottom dock, View >
Ports; Run > Forward a Port…) lists forwarded ports: Port (with its
`portsAttributes` label), Forwarded Address (`localhost:NNNN`, click to open
it in the browser), Running Process, Origin (Auto Forwarded, User Forwarded,
devcontainer.json) and open connections, with Open, Copy (the local
address), Port… (change the local port) and Stop.

- **Forward a Port…** takes `8000` or `host:port` (`db:5432`: the agent
  connects to host `db`, e.g. another compose service). User ports are
  saved per project and forwarded again on reopen.
- **Automatic**: the agent's listening TCP sockets (`/proc/net/tcp{,6}`)
  are polled every 2 s; new ones are forwarded and announced ("Your
  application running on port 8000 is available." with Open in Browser). In
  a container, ports already listening when it opens count too; in WSL only
  ports that start listening afterwards (the distro's services, which WSL
  forwards itself, are left alone). An automatically forwarded port is
  released when its server stops; one the user stopped is not forwarded
  again while it keeps listening. The editor's own debug adapters are
  skipped. Settings: `"ports": { "auto_forward": false }` turns it off.
- **devcontainer.json**: `forwardPorts` (numbers, `"8000"` or
  `"host:port"`) are forwarded on connect, even before anything listens;
  `portsAttributes` (keys: a port, a range `"8000-8010"`, `"host:port"`, or
  a regular expression matched against the process command line) give
  `label`, `protocol` (`http`/`https`, for the browser URL),
  `onAutoForward` (`notify` (default), `openBrowser`, `openBrowserOnce`,
  `openPreview` (opens the browser), `silent`, `ignore`) and
  `requireLocalPort`; `otherPortsAttributes` applies to ports no key
  matches. `elevateIfNeeded` is ignored.
- The local port is the same number on 127.0.0.1 if it is free, else the
  next free one (WSL's own localhost forwarding may hold the same number).
  Each local connection becomes a tunnel through the agent connection
  (protocol 4), so nothing listens inside the container.
- Limits: TCP only (no UDP); local projects have nothing to forward.

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
- **Run**: `[project.scripts]` entry points and the current file run as
  configurations (`uv run …`) in a terminal tab named after them (running
  again replaces the tab); all tests, the current file's tests, or the test
  under the cursor (pytest node id from the enclosing `test_*` function /
  `Test*` class) run in the **Tests** panel (see *Run configurations and
  tests*). Ctrl+click on a traceback line (`File "…", line N` or pytest's
  `path:N:`) opens it.

New terminals activate `.venv` (`VIRTUAL_ENV`/`PATH` locally;
`. .venv/bin/activate` in WSL and containers).

Links in terminal output are underlined under the mouse; Ctrl+click opens
them (the pointer turns into a hand while Ctrl is held). URLs (`http`,
`https`, `file`, `ftp`) open in the browser; file locations open in the
editor at their line and column: compiler-style `path:line:column`,
`path:line`, Python's `File "path", line N`, and paths with a separator
(`src/lib.rs`). Relative paths are resolved against the project root. OSC 8
hyperlinks (`ls --hyperlink`, `gcc`, `cargo` with hyperlinks on) open their
target. A link wrapped over several rows is one link.

## Run configurations and tests

**Run configurations** say what Run and Debug start. They come from:

- `.next_code/run.json` (next_code's own; **Run > Edit Configurations…**
  creates it with a commented example and the automatic configurations as
  templates),
- `.vscode/launch.json` (read for compatibility; comments and trailing
  commas allowed),
- automatic ones: *Python: Current File*, *pytest: All Tests* and `uv run
  <script>` per `[project.scripts]` entry for `pyproject.toml` projects;
  `cargo run: <bin>` per binary and *cargo test: All Tests* from `cargo
  metadata` for Rust projects.

Both files have `launch.json`'s shape:

```jsonc
{
  "configurations": [
    // Python: "program" or "module", "args" (array or one string), "python"
    { "name": "Serve", "type": "python", "module": "app.server", "args": ["--port", "8000"],
      "cwd": "${workspaceFolder}/src", "env": { "LOG_LEVEL": "debug" }, "justMyCode": false },
    { "name": "Unit tests", "type": "pytest", "targets": ["tests/unit"], "args": ["-x"] },
    { "name": "CLI", "type": "uv", "script": "demo-cli", "args": ["--help"] },
    // cargo: "command" run | test, "package", "bin" | "test" | "example" | "bench" | "lib": true
    { "name": "Tool", "type": "cargo", "bin": "tool", "features": ["fast"], "cargoArgs": ["--release"],
      "args": ["--verbose"] },
    { "name": "Lint", "type": "shell", "command": "make lint" }
  ]
}
```

`launch.json` types: `python`/`debugpy` (launch with `program` or `module`;
attach, which can only be debugged), `lldb` (CodeLLDB: a `cargo` section
such as `{"args": ["build", "--bin=tool"], "filter": {...}}`, or `program`;
attach), `cppdbg`/`cppvsdbg` launches (run directly, debugged with LLDB).
Other types, `${command:…}`/`${input:…}` variables and invalid entries are
listed but disabled, with the reason. `preLaunchTask` is not run and
`envFile` is ignored (the picker says so). Keys a type does not use go to
the debug adapter unchanged (`justMyCode`, `stopOnEntry`, `pathMappings`…).
Variables: `${workspaceFolder}`, `${workspaceFolderBasename}`, `${file}`,
`${relativeFile}`, `${relativeFileDirname}`, `${fileBasename}`,
`${fileBasenameNoExtension}`, `${fileExtname}`, `${fileDirname}`,
`${lineNumber}`, `${pathSeparator}`, `${env:NAME}` (local projects only;
empty in WSL/containers). A relative `cwd` is relative to the project folder.

The selected configuration shows at the right of the title bar (click it to
pick another, ▶ runs it, *Debug* debugs it) and is remembered per project.

| Command | Keys |
|---|---|
| Run (selected configuration, in a terminal tab) | `Ctrl+F5`, `Shift+F10` |
| Debug (selected configuration) | `Shift+F9`; `F5` when no session is paused |
| Select Configuration… / Edit Configurations… | Run menu, palette |
| Run All Tests / Run Failed Tests / Show Tests | Run menu, palette |

Run types the command into a terminal tab named after the configuration,
where the project lives (PowerShell syntax for local Windows projects, POSIX
shells otherwise), changing directory and exporting `env` first; running it
again replaces that tab. Python runs with `uv run python …` in uv projects
(else the project's interpreter); pytest with `uv run pytest`; Rust with
`cargo run`/`cargo test`. Debug builds cargo configurations first (`cargo
build` / `cargo test --no-run` with `--message-format=json`, taking the one
executable) and hands the request to the debugger: debugpy for Python and
pytest (`module: pytest`), LLDB for Rust and native programs.

The **Tests** panel (left dock; View > Tests) finds tests when first shown
and after a test file is saved (`test_*.py`, `*_test.py`, `conftest.py`,
`.rs`; debounced):

- pytest: `pytest --collect-only -q --continue-on-collection-errors`
  (through `uv run` or the project's interpreter) for projects with
  `pyproject.toml`, `pytest.ini`, `setup.cfg`, `tox.ini` or `conftest.py`;
  folders → files → classes → tests, parametrized cases grouped under their
  function. Files that fail to import show as errors with pytest's report.
- cargo: `cargo test --no-run --workspace --tests` then each test binary's
  `--list --format terse`; packages → targets (`lib`, `bin x`, `test x`) →
  modules → tests. Doc tests are not listed.

Icons: ○ not run, ◌ running, ✓ passed, ✗ failed, ! error, ⊘ skipped; folders
show `failed ✗ passed/total`. The filter box matches names and ids. A click
opens the test's definition; a double click or the row's ▶ runs it (any
node: folder, file, class, module, target), *Debug* debugs it (pytest under
debugpy; the cargo test binary under LLDB with `--exact <name>`).
**Run All**, **Run Failed** and ⟳ (rediscover) are at the top. The details
area shows the selected test's message, traceback/panic and captured output,
or the last run's full output. Results come from pytest's JUnit XML report
(written to a temp file where the project lives and read back) and libtest's
`test … ok|FAILED|ignored` lines (one `cargo test --no-fail-fast` per
target). Once tests are discovered (the Tests panel was shown or a test run
started), editors show a ▶ in the gutter at tests, test classes and
parametrized functions; clicking it runs that test.

Settings (`settings.json`): `"tests": { "discover_on_save": true,
"pytest_args": [], "cargo_test_args": [] }` (extra arguments for discovery
and runs).

## Workflows

A workflow is one JSONC file that starts a multi-project development
environment: preconditions, commands in the terminals of several projects
(opened as needed, dev containers included), ordering with readiness
conditions, browser links and manual gates. Files are found in
`<config>/next_code/workflows/*.json[c]` (next to `settings.json`;
`%APPDATA%\next_code\workflows` on Windows) and in each open project's
`.next_code/workflows/*.json[c]`. They are read again before every Run All.

```jsonc
{
  "name": "Fleet dev",
  "steps": [
    // A process on this machine (PowerShell on Windows); exit 0 passes.
    { "id": "vpn", "title": "VPN", "check": "if (-not (Test-Connection -Count 1 -Quiet intranet.corp)) { exit 1 }",
      "onFail": "Connect the VPN, then press Retry." },
    { "id": "api", "needs": ["vpn"], "project": { "devcontainer": "C:/src/api" },
      "run": "uv run uvicorn app:app --port 8000", "terminal": "API",
      "ready": { "port": 8000 }, "timeout_s": 600 },
    { "id": "worker", "needs": ["vpn"],
      "project": { "devcontainer": { "wsl": "Ubuntu", "path": "~/src/worker" } },
      "run": "uv run worker", "ready": { "output": "worker started" } },
    { "id": "web", "needs": ["api"], "project": { "devcontainer": "C:/src/web" },
      "run": "npm run dev", "ready": { "output": "ready in \\d+ ms" }, "openUrls": true },
    { "id": "login", "needs": ["web"], "ready": { "manual": "Log in in the browser, then Continue." } },
    { "id": "tools", "needs": ["login", "worker"], "project": { "wsl": "Ubuntu", "path": "~/src/tools" },
      "run": "./sync.sh --env dev", "env": { "TOKEN": "${env:FLEET_TOKEN}" }, "cwd": "scripts" }
  ]
}
```

Step fields: `id` (required, unique), `title`, `needs` (ids; a step starts
when they are all ready, independent steps run in parallel), `project`,
`run`, `terminal`, `check`, `onFail`, `ready`, `timeout_s`, `openUrls`,
`env`, `cwd`. Unknown keys, unknown `needs`, duplicate ids and cycles make
the workflow invalid: it is listed with the error and cannot run.

- `project`: `{ "local": "C:/src/p" }`, `{ "wsl": "Ubuntu", "path": "~/p" }`,
  `{ "devcontainer": <folder>, "config"?: "<devcontainer.json>" }` (folder: a
  path string or the `wsl` shape), a typed location (`"Ubuntu:~/p"`,
  `"devcontainer:C:\\src\\p"`), or the editor's own shape from
  `recent.json` (`{"Local": ...}`, `{"Wsl": {...}}`, `{"DevContainer": {...}}`).
  Without it the step runs on the editor's machine, with no project tab.
- `run`: runs as the program of a terminal tab (named `terminal`, else the
  title, else the id) in the project's Terminal panel, after the project
  tab is opened in the workflow's window (an open tab for the same location
  is reused; new tabs open in the background). Local Windows:
  `powershell.exe -NoLogo -Command <line>`; WSL, containers, Linux: the
  user's login shell, `-lic <line>`. The tab stays when the command exits,
  showing its exit code. Without a project the terminal is shown in the
  workflow's tab (Terminal button). `.venv` is not activated (use `uv run`).
- `check`: a process where the step runs (PowerShell `-NoProfile` on
  Windows, `sh -c` elsewhere), before `run`; exit 0 passes. Failing with
  `onFail`: the step shows Failed with that message and Retry; without it
  the workflow fails and everything it started is stopped.
- `ready`: `{ "exit": true }` (the default with `run`: exit code 0; without
  `run`, the check passing), `{ "output": "<regex>" }` (the command's output,
  ANSI codes stripped, across chunks and lines), `{ "port": N }` (listening
  where the step runs: the agent's listening sockets for WSL/containers, a
  TCP connect to localhost otherwise), `{ "manual": "<prompt>" }` (a
  Continue button), `{ "delay_ms": N }`. A command that exits before it is
  ready fails the step. `timeout_s` (from the step's start) fails it too.
- `openUrls`: http(s) URLs on complete output lines open in the browser
  (each once) until the step is ready plus 3 s.
- `env`, `cwd` (relative to the project; the home folder without one) and
  the command lines expand `${workspaceFolder}` (the home folder without a
  project), `${userHome}` and `${env:NAME}` (the environment where the step
  runs; read with `env` for WSL/containers).

States: pending → waiting (needs) → starting (opening the project, the
check) → running → ready | failed | stopped. A failed step blocks its
dependents (shown as blocked); others keep going. Restart reruns one step
(its terminal tab is replaced; dependents that already run are left
alone); Retry Failed reruns failed steps; Stop All sends Ctrl+C to every
command the run started and kills what still runs 3 s later (project tabs
and containers stay); Run All starts afresh.

UI: each workflow opens as its own tab in the window's tab row (marked ◈,
colored by the run's state; opening the same file again shows its tab;
restored with the window). Run > Open Workflow… picks one (user and open
projects' workflows); Run > New Workflow… writes a commented template to
the user folder and opens its tab; the command palette's "Run Workflow:
<name>" opens the tab and runs it. The tab has the file in an editor on
the left (Ctrl+S saves; drag the divider to resize) and a graph on the
right: one column per dependency depth (longest chain of `needs`), cards
ordered to reduce crossings, each with title, id and what it does, where
it runs, status icon/color and elapsed time; edges turn green when their
upstream is ready. The graph opens fitted to its pane (at most 100 %, at
least 40 %, centered) and re-fits on resize and after edits until you zoom
or pan: Ctrl+scroll zooms around the pointer, the corner overlay has −,
the zoom, + and Fit (back to fitting); scroll (Shift+scroll sideways) or
drag pans. The
graph follows the buffer 200 ms after an edit; a file with problems shows
them in a banner with line and column (click: go there) over the last
good graph, dimmed. Clicking a card puts the cursor on its `"id"`; the
card holding the cursor is highlighted. A selected card shows details
below the graph: state, wait/failure reason or manual prompt, last output
lines, Terminal (the project's terminal tab, or this machine's terminal
inside the workflow tab), Continue, Restart/Retry. The toolbar shows the
state (Idle, Running, All ready, Failed, Stopped), Run All (saves the file
first if edited, then runs what was saved; Shift+F10), Stop All (Ctrl+F2)
and Retry Failed. Workflow files in remote projects are read and written
through the project (its tab opens first when a restored workflow tab
needs it). Workflows run in the window that started them.

Headless: `workflows_list`, `workflow_run`, `workflow_status`,
`workflow_stop`, `workflow_continue`, `workflow_restart`, `workflow_retry`
and `workflow_event` notifications (see `crates/ui/src/headless.rs`).
Other projects open without a window and URLs are reported, not opened.

[docs/workflows-for-agents.md](docs/workflows-for-agents.md) is the full
reference for writing and operating workflows (also for coding agents):
every field and shape, how commands run, failure rules, design recipes,
the validation loop, and the headless protocol with real transcripts.

## Debugging

Debugging speaks the Debug Adapter Protocol to an adapter started where
the project lives (on this machine, inside WSL or inside the dev
container, through the agent like language servers), so paths are the
project's own.

- **Python**: debugpy. The adapter is `python -m debugpy.adapter` from the
  project's `.venv` when debugpy is installed there, else an ephemeral uv
  environment (`uv run --no-project --with debugpy`; the first start
  downloads debugpy), else a `python3` on `PATH` that has debugpy. The
  program runs with the project's interpreter (`.venv`, else `python3`);
  `justMyCode` is on and output goes to the Debug Console unless the
  configuration says otherwise. Without uv or debugpy a notice says how to
  get them.
- **Rust / C / C++**: CodeLLDB (`codelldb` on `PATH`, or downloaded by
  next_code), else `lldb-dap`. Without either, a banner offers **Download
  CodeLLDB**: the release for the machine where the project lives (Windows
  build for local Windows projects, Linux x64/arm64 inside WSL and
  containers) is fetched with curl/wget and unpacked into next_code's data
  directory there (`%LOCALAPPDATA%\next_code\adapters`,
  `~/.local/share/next_code/adapters`); debugging then starts by itself.
- **Any other adapter** speaking DAP over stdio: a configuration with a
  custom adapter command.

`settings.json` can name the adapter command:
`"debug": { "adapters": { "lldb": { "command": "/opt/codelldb/adapter/codelldb" },
"python": { "command": "python3", "args": ["-m", "debugpy.adapter"] } } }`.

**Starting.** `F5` debugs the selected run configuration (see above; Rust
configurations are built first), or with no configuration support the
current Python file (**Debug > Debug Current File**). One session runs at a
time: starting another (or Restart) stops the running one first.

**Breakpoints.** Click the gutter left of the line numbers, or press `F9` /
`Ctrl+F8` on the cursor line. Right-click the gutter (or **Debug > Edit
Breakpoint…**) for a condition, a hit count (the adapter's syntax, e.g.
`5`, `>= 3`) or a log message (`{expression}` interpolated; a logpoint logs
instead of stopping), and to disable or remove it. Dots: filled red =
enabled (verified while a session runs), hollow = disabled or not verified
by the adapter (the Breakpoints list shows the adapter's reason), a bar
= conditional, rounded square = logpoint. Breakpoints follow edits (lines
inserted or deleted above, a line joined into the previous one) and file
renames, are saved with the project's session (`session.json`, with the
watch expressions and exception choices), and are sent to the adapter per
file as they change while a session runs.

**While paused** the line is highlighted with an arrow in the gutter, its
file opened (stepping into code without a file selects the nearest frame
that has one). Hovering an identifier shows its value (`self.items` for
`items` after a dot). The **Debug** panel (right dock, View > Debug) has a
toolbar and sections:

- **Call Stack**: threads (when several) and frames; click a frame to show
  its line (highlighted differently), variables and watches.
- **Variables**: scopes and variables, expanded lazily (expansion is kept
  across steps); long lists load 100 items at a time (*Load more*);
  double-click a value to change it when the adapter supports it.
- **Watch**: expressions re-evaluated at every stop and frame change; type
  one and press Enter; × removes.
- **Breakpoints**: exception filters from the adapter (e.g. *Raised
  Exceptions*, *Uncaught Exceptions*, *C++: on throw*), every breakpoint
  with a checkbox (enable/disable), click to show it, × to remove, *Remove
  all*.

The **Debug Console** (bottom dock) shows the program's stdout, stderr (red)
and the adapter's messages, and evaluates what you type in the selected
frame (Up/Down for history). A bar above the editors shows the session's
state with the same toolbar.

| Command | Keys |
|---|---|
| Start Debugging / Continue | `F5` |
| Pause | `F6` |
| Step Over / Into / Out | `F10` / `F11` / `Shift+F11` |
| Stop | `Shift+F5` |
| Restart | `Ctrl+Shift+F5` |
| Toggle Breakpoint | `F9`, `Ctrl+F8` |
| Edit Breakpoint…, Enable/Disable All, Remove All | Debug menu, palette |

Limits: no `runInTerminal` (program input from a terminal) and no child
sessions (`startDebugging`, e.g. debugpy's multiprocess); both are declined
with a note in the console. No data or function breakpoints, no
disassembly.

## Git and multiple projects

The **Commit** tool window (`Alt+0`, also `Ctrl+Shift+G`; left dock) works
like PyCharm's with its default "no staging area": a toolbar (Refresh,
Rollback, Show Diff, Expand All, Collapse All, Group By Directory), then a
tree of local changes with a checkbox per file, folder and group:
**Changes** (every tracked change against HEAD), **Unversioned Files** and,
during a merge, **Merge Conflicts**. File names are colored by status
(modified blue, added green, deleted grey and struck through, unversioned
red-brown), with the folder dimmed in the flat list. Commit commits exactly
the checked files (`git commit --only`; checked unversioned files are added
first, and what is staged for other files stays staged); new changes start
checked and your choices persist across refreshes. Folder and group boxes
are tri-state. **Amend** rewrites the last commit with the checked files (or
just the message, which it starts from). The message box grows with the
text; `Ctrl+Enter` commits. **Commit and Push…** commits, then opens the Push
dialog: the outgoing commits and the target (`origin/<branch>`, marked new
when the push sets the upstream), Push or Cancel.

Double-click (or Enter, `Ctrl+D`) opens a file's diff (HEAD ↔ working
tree); `F4` jumps to the file; Space toggles the selected row's checkbox.
The right-click menu has Commit File… (checks only those files), Rollback…,
Show Diff, Jump to Source, Add to VCS and Ignore (unversioned files: adds a
root-anchored `/path` pattern to `.gitignore`), Delete… and Copy Path.
**Rollback** (toolbar, menu, Git > Rollback…) acts on the selection, else
the checked files, after asking: tracked files return to HEAD (index and
working tree), added files leave version control but stay on disk, renames
return to the old name, and unversioned files are deleted.

With `"git": { "staging_area": true }` in `settings.json` (PyCharm's
"Enable staging area") the window shows git's index instead: **Staged** /
**Unstaged** groups with stage (`+`) and unstage (`−`) row buttons and menu
items, and Commit commits what is staged. Hunk staging in diffs works in
both modes. While a merge, rebase, cherry-pick or revert is in progress a
banner says so with Abort (and Continue for a rebase); conflicted files show
under Merge Conflicts, open in the editor, and `✓` (Mark Resolved) stages
them; committing during a merge commits the whole result.

Git runs where the project lives (inside WSL for WSL projects), and the
window refreshes on file changes and on index/ref changes (commits or
checkouts from a terminal).

When git refuses a repository whose folder belongs to another user ("dubious
ownership": a folder on another drive, a shared folder, a folder mounted into
WSL), the Commit window says so and offers **Trust This Folder**, which adds it to
git's global `safe.directory` list where the project lives (what git's own
message and VS Code suggest). Inside a dev container, where the bind-mounted
workspace usually belongs to another user than the container's, the editor
runs its own git commands with `-c safe.directory=*` and leaves the config
as it is.

Diffs show unified or side by side, and changed hunks only or the whole
file; the last choice sticks for new diffs. Diffs of changes in the index
or working tree come from `git diff` itself and act per hunk: "Stage hunk"
and "Discard" (asks first) on unstaged changes, "Unstage hunk" on staged
ones.

**Branches**: the title bar shows the current branch (`⎇ main ▾`, with
`↑n`/`↓n` against its upstream and the operation in progress). Clicking it
(or Git > Branches…) opens PyCharm's branches popup: a search box, then
Update Project…, Commit…, Push…, New Branch…, Checkout Tag or Revision…,
and the **Recent** (from HEAD's reflog), **Local** and **Remote** branches
with the current one starred and ahead/behind counts. Each branch opens a
submenu: Checkout (a remote branch gets a tracking local branch), New
Branch from '…'…, Checkout and Rebase onto '<current>', Compare with
'<current>', Show Diff with Working Tree, Rebase '<current>' onto '…',
Merge '…' into '<current>', Update (tracked branches; a branch that is not
checked out is fast-forwarded), Push…, Rename…, Delete (asks again when the
branch has unmerged commits; remote branches are deleted on the remote).
Merges and rebases that stop on conflicts open the Commit window. New
Branch asks for a name with "Checkout branch".

**Update Project** fetches, then merges the upstream (or rebases onto it
with `"git": { "update_method": "rebase" }`). Git > Fetch is `--all
--prune`, Pull is fast-forward only, and Push… (the Push dialog) sets the
upstream on `origin` (or the only remote) the first time. Git never
prompts: credentials come from your credential helper or SSH agent, and
failures are shown in the Commit window.

**Worktrees…** (Git menu) lists the repository's worktrees; choosing one
opens it in a project tab. Typing a branch name adds a worktree for it
(creating the branch if needed) next to the repository as
`<repository>-<branch>` and opens it; `×` removes one (asks; its branch is
kept).

The **Git** tool window (`Alt+9`, View > Git, Git > Show Git Log; bottom
dock) is PyCharm's Log tab. On the left, the **branches tree**: HEAD (the
current branch), Local, Remote (per remote) and Tags; double-clicking a row
filters the log to it. In the middle, a filter bar — text (subject, author
or the start of a hash, as you type), **Branch ▾**, **User ▾** (me and the
authors seen), **Date ▾** (last 24 hours … 12 months), **Paths ▾**
(repository-relative, comma-separated; Git > Show History for Current File
sets it to the active file), `✕` clears them — above the commit graph:
lanes with HEAD's commit ringed, subject, branch/tag labels (HEAD's branch
highlighted, remote branches outlined, tags ◆), author and age.
The log loads 1000 commits at a time and the next page as the list nears
its end, so long histories stay responsive; text, user and date filters
draw the matches as one line (their parents are not listed), while branch
and path filters keep the lanes (`git log --parents` rewrites them).

Click selects a commit, `Ctrl`+click and `Shift`+click (or `Shift+Up/Down`)
select several; `Ctrl+C` copies the selected hashes. On the right, the
selection's **changed files** (for several commits, everything from the
oldest one's parent to the newest one) — double-click opens that diff —
above the **details**: full message, hash, author, date and the branches
containing the commit. The right-click menu has Copy Revision Number,
Create Patch… (`git format-patch` of the selection into a file you
choose), Cherry-Pick (oldest first; merges excluded; already-applied
commits are skipped), Checkout Revision (detached), Compare with Local,
Reset Current Branch to Here… (Soft / Mixed / Hard / Keep), Revert Commit
(newest first; a merge alone, against its first parent), Undo Commit
(HEAD only, unless pushed: a soft reset keeping the changes), Edit Commit
Message… (HEAD; contents and the index are kept), New Branch… and New
Tag…. A cherry-pick or revert that stops on conflicts says which files and
offers Abort; resolve and commit in the Commit window to finish it.

**Compare…** compares the working tree with a branch, tag or any revision
(`HEAD~3`, a commit id). A comparison tab lists the changed files as a tree
or list; its base and target can be changed (the target may be another
revision, and then swapped), and each file opens its diff.

## Window, menus and commands

Top to bottom, a window has the menu bar (File, Edit, Selection, View, Go,
Run, Git, Help) with the command bar in it, the project tabs, and the active
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

Like VS Code (`window.restoreWindows: all`), a launch without a project
reopens the windows that were open when the editor closed: each with its
project tabs, active tab, size, position (moved onto a connected display
if its monitor is gone) and maximized state, focusing the window that had
focus. Each project then reopens its editor tabs (see
[session restore](#split-editor-and-session-restore)); WSL and container
projects reconnect as when opened from the recent list, and local folders
that no longer exist are skipped. Closing a project tab, or a window while
others remain, removes it; quitting (**File > Exit**, a termination
signal on Linux, or closing the last window) keeps every window open at
that moment. A window on the welcome screen is kept only when it is the
only one. Naming a project on the command line (`next_code .`,
`next_code <dir>`, `--wsl`, `--devcontainer`) opens just that project.
The state is `windows.json` next to `recent.json`
(`%LOCALAPPDATA%\next_code\` on Windows, `~/.local/share/next_code/` on
Linux); `"restore_windows": false` in `settings.json` shows the welcome
screen instead.

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

## Editing

Shortcuts follow VS Code (JetBrains where VS Code has none); the menus and
the command bar show each command's keys.

**Multiple cursors.** Every edit, motion, deletion, copy/cut/paste and
undo works at every cursor; selections that overlap (or a cursor touching
one) merge. Pasting N lines into N cursors puts one line at each. Undo
restores the cursors of the undone edit.

| Command | Keys |
|---|---|
| Add / remove a cursor | `Alt+click` |
| Column selection | `Shift+Alt+drag` or middle-drag |
| Add Next Occurrence (first: select the word) | `Ctrl+D` |
| Select All Occurrences | `Ctrl+Shift+L` |
| Add Cursor Above / Below | `Ctrl+Alt+Up` / `Ctrl+Alt+Down` |
| Back to one cursor | `Escape` |

Occurrences started from a cursor on a word match whole words; started from a
selection they match the text anywhere. Both are case-sensitive.

**Lines, comments, indentation.** All work on every selection.

| Command | Keys |
|---|---|
| Move Line Up / Down | `Alt+Up` / `Alt+Down` |
| Copy Line Up / Down | `Shift+Alt+Up` / `Shift+Alt+Down` |
| Delete Line | `Ctrl+Shift+K` |
| Insert Line Below / Above | `Ctrl+Enter` / `Ctrl+Shift+Enter` |
| Toggle Line Comment (block comment in HTML, CSS, Markdown) | `Ctrl+/` |
| Toggle Block Comment | `Shift+Alt+A` or `Ctrl+Shift+/` |
| Indent / Outdent Lines | `Tab` on a multi-line selection, `Ctrl+]` / `Shift+Tab`, `Ctrl+[` |
| Go to Matching Bracket | `Ctrl+Shift+\` |
| Go to Line (`line` or `line:column`, previews while typing, `Escape` returns) | `Ctrl+G` |

Line comments go at the smallest indentation of the selected lines and are
removed only when every non-blank line is commented. The indentation style
(tabs or N spaces) is detected from the file; files without indentation get
4 spaces (2 for JSON, YAML, JavaScript, TypeScript, HTML, CSS). The status
bar shows it.

**Typing helpers.** Typing `(`, `[`, `{`, `"`, `'` (and `` ` `` in
JavaScript/TypeScript, Go, shell, SQL, Markdown; no `'` in Rust or
Markdown) inserts the closer; quotes don't pair right after a letter,
digit or backslash, and nothing pairs before a word. Typing the closer
steps over an inserted one, Backspace between an empty pair deletes both,
and an opener typed over a selection wraps it. `"auto_close_brackets":
false` in `settings.json` turns all of this off. Enter keeps the line's
indentation, adds a level after `{`, `(`, `[` (and Python's `:`), and puts
the closer of `{|}` on its own line. The bracket pair at the cursor is
highlighted (from the syntax tree; brackets in strings and comments don't
match).

**Find and replace.** `Ctrl+F` opens the find bar above the text, seeded
with the selection or the word at the cursor; `Ctrl+H` also shows the
replace field. Matches are highlighted, the current one stands out, and the
bar shows `n of m` (updated as the text changes).

| In the find bar | Keys |
|---|---|
| Next / previous match | `Enter` / `Shift+Enter`, `F3` / `Shift+F3` |
| Match case / whole word / regular expression | `Alt+C` / `Alt+W` / `Alt+R` |
| Select all matches as cursors | `Alt+Enter` |
| Replace (in the replace field) / Replace All | `Enter` / `Ctrl+Alt+Enter` |
| Close and return to the text | `Escape` |

Regular expressions use Rust's `regex` syntax (`^`/`$` match at lines);
replacements expand `$1`, `${name}`. Without regex the replacement is
literal.

**Word wrap.** `Alt+Z` (View > Word Wrap) wraps long lines at the editor's
width for that editor; `"word_wrap": true` in `settings.json` makes it the
default, and `"languages": { "<id>": { "word_wrap": … } }` overrides it per
language (Markdown wraps by default). Rows break after spaces (long words
break anywhere, East Asian wide characters count two columns), continuation
rows keep the line's indentation, and the line number shows on the first
row only. Up/Down move by rows; `Home`/`End` go to the row's start/end
first, then the line's. There is no horizontal scrolling while wrapping.
Shift+Alt+drag column selections count buffer columns, so they take the
same characters of every line.

**Folding.** Foldable regions come from the syntax tree: bracketed blocks,
objects, arrays and argument lists (the closing line stays visible),
multi-line comments, Python blocks (from the `def`/`class`/`if` line),
HTML/JSX elements, Markdown sections and code fences, TOML tables; YAML,
shell, SQL, Dockerfiles and plain text fold by indentation. Chevrons appear
in the gutter while the mouse is over it (folded lines always show `▸`);
click one to fold or unfold. A folded line ends with `⋯`, which unfolds on
click. Folds belong to the editor pane, follow edits, and open when an
edit, a cursor, a jump or a find match lands inside; motions step over them.
Folds are saved with the session.

| Folding | Keys |
|---|---|
| Fold / unfold at the cursor | `Ctrl+Shift+[` / `Ctrl+Shift+]` |
| Toggle fold at the cursor | `Ctrl+K Ctrl+L` |
| Fold / unfold recursively | `Ctrl+K Ctrl+[` / `Ctrl+K Ctrl+]` |
| Fold all / unfold all | `Ctrl+K Ctrl+0` / `Ctrl+K Ctrl+J` |

## Code navigation

### Go to definition and friends

| Command (Go menu) | Keys |
|---|---|
| Go to Definition | `F12`, `Ctrl+B`, `Ctrl+click` |
| Go to Declaration | Go menu / command bar |
| Go to Type Definition (of the variable or expression's type) | `Ctrl+Shift+B` |
| Go to Implementation (of a trait/interface or its method) | `Ctrl+F12`, `Ctrl+Alt+B` |
| Find References | `Shift+F12`, `Alt+F7` |

One result opens directly, several open the locations picker; `Alt+Left`
goes back. While `Ctrl` is held, the identifier under the mouse is
underlined and the pointer becomes a hand: a click goes to its definition.
Commands the server does not support say so in the status bar.

### Parameter hints and inlay hints

**Parameter hints** (signature help) appear above the cursor when typing
one of the server's trigger characters (`(` and `,` for most servers) or
with `Ctrl+Shift+Space`: the signature with the current parameter in
bold, then that parameter's and the function's documentation. They follow
the cursor and close when it leaves the call, on `Escape`, or on a jump.
With several signatures (overloads), `Up`/`Down` (or `Alt+Up`/`Alt+Down`,
or the arrows in the popover) switch between them. The completion menu
can show at the same time; its keys win while it is open.

**Inlay hints** show what the server infers inside the text, muted: types
after `let` names and closure parameters, parameter names before
arguments, chained types, and so on (whatever the server sends). They
are not text: the cursor steps over them, clicks on one land beside it,
and they are never selected, copied or found. Word wrap counts their
width. The visible lines (and a screen above and below) are asked for
250 ms after edits and when scrolling. `"inlay_hints": false` in
`settings.json` (or `"languages": { "<id>": { "inlay_hints": … } }`) turns
them off, live; View > Inlay Hints (`Ctrl+Alt+I`) toggles them for every
editor until the app exits. Theme keys: `inlay_hint`, `inlay_hint_background`.

### Call hierarchy

**Show Call Hierarchy** (`Shift+Alt+H`, JetBrains `Ctrl+Alt+H`; Go menu)
asks the language server about the symbol at the cursor and opens the
**Call Hierarchy** panel (bottom dock; View > Call Hierarchy toggles it). The
header names the symbol and switches between **Incoming** (who calls it, the
default) and **Outgoing** (what it calls); switching starts over from the
symbol. Each level is fetched when it is expanded, so recursive calls can be
followed as deep as wanted. Rows show the kind, name, detail and `file:line`.
`Up`/`Down` select, `Right`/`Left` expand/collapse (or step into the first
child / out to the parent), `Enter` or a click opens the location (an
incoming caller opens at its call site, an outgoing callee at its name; Back
returns), double-click or the chevron expands/collapses.

### Symbols: outline, breadcrumbs, Go to Symbol

Each file editor asks its language server for the document's symbols when it
opens and 300 ms after edits (until then the previous ones move with the
text).

- **Outline** panel (right dock; `Alt+7`, View > Outline): the active editor's
  symbol tree with kind badges, follows tab and pane switches, highlights the
  symbol around the cursor as it moves, and says when there is no language
  server or no symbols. Click a row (or `Enter`) to jump there; the chevron
  collapses/expands. With the panel focused (`Alt+7` focuses it),
  `Up`/`Down` select and `Right`/`Left` expand/collapse or step in/out.
- **Breadcrumbs** above the text: the project-relative path, then the symbols
  containing the cursor. A folder segment lists its entries (choosing a folder
  enters it, `..` goes up; through the project, so WSL and containers work),
  the file segment lists its folder, and a symbol segment lists its siblings.
- **Go to Symbol in File…** (`Ctrl+Shift+O`; Go menu): fuzzy picker over the
  file's symbols (tree order until you type; kind, name, detail or container,
  line). `Enter` jumps (Back returns), `Escape` closes. Opened before the
  symbols arrive, it fills in when they do.
- **Go to Symbol in Workspace…** (`Ctrl+T`, JetBrains `Ctrl+Alt+Shift+N`; Go
  menu): `workspace/symbol` 150 ms after each keystroke (older answers are
  dropped) to the active file's language server, or every running one when
  the active tab has none; rows show kind, name, container and
  `path:line`, and `Enter` opens the file there.

## Refactoring and formatting

Through the file's language server (the server runs where the project
lives: locally, in WSL or in the dev container).

| Command (Refactor menu) | Keys |
|---|---|
| Rename Symbol… | `F2`, `Shift+F6` |
| Quick Fix… (code actions at the cursor or selection) | `Ctrl+.`, `Alt+Enter` |
| Format Document | `Shift+Alt+F` |
| Format Selection | `Ctrl+K Ctrl+F` |

**Rename** opens an input over the symbol with its name selected (the range
and placeholder come from the server's `prepareRename`; its refusal, e.g.
"cannot rename a keyword", shows in the status bar). `Enter` renames it in
every file, `Escape` (or a click elsewhere) cancels. Open files change in
their editor (one undo step per file, unsaved like any edit); other files
change on disk. Servers' file renames (e.g. renaming a Rust module) move the
file and the open tab with it. The status bar reports how many files
changed, or which one failed.

**Code actions.** `Ctrl+.` / `Alt+Enter` (while the find bar is open,
`Alt+Enter` keeps selecting all matches) lists the server's actions for the
selection and the diagnostics under it: quick fixes first, the server's
preferred fix in bold. `Up`/`Down`, `Enter` (or a click) applies, `Escape`
closes. A lightbulb in the gutter marks the cursor line when actions exist
(amber when one is a quick fix); clicking it opens the menu. Actions are
resolved when the server defers their edits, and their commands run after
the edit; edits a command makes come back as `workspace/applyEdit` and are
applied the same way.

**Formatting.** Format Document and Format Selection use the language
server (Format Selection needs its range formatting). Settings choose
another formatter per language; it runs where the project lives, in the
project root, with the text on stdin and `{file}` replaced by the file's
path:

```json
{ "format_on_save": true,
  "languages": {
    "python": { "formatter": { "command": "ruff", "args": ["format", "--stdin-filename", "{file}", "-"] } },
    "rust": { "format_on_save": false },
    "json": { "formatter": "language_server" } } }
```

Python uses `ruff format` by default when `ruff` is installed (the
project's `.venv` first, then `PATH`), else the language server. The result
replaces the text as one undo step through the smallest edits, so cursors
and selections stay where they were. The editor's indentation (tabs or N
spaces, from the status bar) is sent as the server's formatting options.
`"format_on_save"` (global, or per language) formats on `Ctrl+S`; if the
formatter takes longer than 2 seconds (or fails), the file is saved
unformatted with a note in the status bar. Save All and saving from the
close prompt don't format.

**Snippets.** Completions that insert snippets (e.g. rust-analyzer's
`fn name($1) {$0}`, basedpyright's call snippets) place the cursor on the
first placeholder with its text selected; every copy of a placeholder gets a
cursor, so typing changes them together. `Tab` / `Shift+Tab` move to the
next / previous placeholder, the last `Tab` goes to the snippet's end
position; `Escape` or moving the cursor out of the snippet ends it. Choices
(`${1|a,b|}`) insert the first option; snippet variables (`$TM_FILENAME`,
`$TM_SELECTED_TEXT`, …) are filled where known.

## Split editor and session restore

The editor area splits into panes (VS Code's editor groups), each with its
own tabs and active tab. A file open in two panes is two views of one
buffer: edits, undo and the unsaved marker are shared, and closing one view
of an edited file does not ask (closing the last one does). Files opened
from the tree, the finder, search, Problems, the terminal, diffs or Back /
Forward (`Alt+Left` / `Alt+Right`) go to the active pane (the one last
clicked or focused).

| Command (View menu, palette) | Shortcut |
|---|---|
| Split Editor Right (the active file, at the same position) | `Ctrl+\` |
| Split Editor Down | `Ctrl+K Ctrl+\` |
| Focus First / Second / Third Editor Pane (one past the last splits right) | `Ctrl+1` / `Ctrl+2` / `Ctrl+3` |
| Focus Next / Previous Editor Pane | `Ctrl+K Ctrl+Right` / `Ctrl+K Ctrl+Left` |
| Move Tab to Next / Previous Pane (splits one off at the edge) | `Ctrl+Alt+Right` / `Ctrl+Alt+Left` |
| Close Editor Pane (asks once about unsaved files) | `Ctrl+K W` |

Tabs can be dragged onto another pane's tab bar (before a tab or at the
end), into a pane's middle, or onto its left/right/top/bottom quarter to
split it there. The bar between panes resizes them. Closing a pane's last
tab removes the pane. Middle click closes a tab.

Each project remembers its editor tabs per pane, the active tab and pane,
the splits and their sizes, and every editor's selections and scroll, and
reopens them the next time the project opens (local, WSL and container
projects alike). Files that no longer exist are skipped; diff, history and
comparison tabs are not kept. The state is `session.json` next to the
project's saved layout, in `%LOCALAPPDATA%\next_code\projects\<key>\` on
Windows or `~/.local/share/next_code/projects/<key>/` on Linux (the key is
the project's location with other characters than letters and digits as
`_`). `"restore_session": false` in `settings.json` turns it off.

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
  `git_added`, `git_modified`, `git_renamed`, `git_deleted`, `git_unversioned`,
  `git_conflicted`, `graph_lanes` (a list, cycled), `branch`, `tag`,
  `lightbulb`, `breakpoint`, `debug_current_line`, `debug_frame_line`,
  `debug_arrow`, `run_marker`. An
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

`next_code --automation [--port 0] [--width 1440] [--height 900] [--state-dir DIR] -- PROJECT_DIR`
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

Nothing the editor opens leaves the session: there is no D-Bus session bus
(so no desktop portal), and `xdg-open`, `gio`, `wslview` and the
`powershell.exe` launch used on WSL are stand-ins that only record the target.
`{"cmd":"opened"}` returns the URLs and files the editor asked to open, in
order.

Requests execute serially. Input acknowledgements mean the platform operation
finished, not that asynchronous application work or a rendered frame completed.
Use `wait` with `ms` (0–10000), then inspect a screenshot; repeat with a bounded
deadline when waiting for search results, language servers, or other work.
Frame completion tracking and structured action discovery are not exposed by
this backend yet; commands can be selected through the normal command palette.

The server binds only to loopback and requires its random session token. The
display requires an X authority cookie and disables TCP connections. Settings,
caches and data (recent projects, each project's saved layout and session)
are private to each session; `--state-dir DIR` keeps the data in `DIR` across
sessions instead, e.g. to check session restore. Project files are real and
UI actions have their usual effects. The implementation is embedded in the
executable from `scripts/automation.py`, so distributing a separate script is
unnecessary.
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
`diagnostics`, `definition`/`declaration`/`type_definition`/`implementation`,
`signature_help` (with the highlighted parameter), `inlay_hints` (a line
range), `incoming_calls`/`outgoing_calls` (call
hierarchy at a position), `document_symbols`, `workspace_symbols`,
`semantic_tokens` (decoded as the editor draws
them), `rename`, `code_actions` / `apply_code_action` (edits are written to
disk and open documents synced; servers' `workspace/applyEdit` requests are
applied too and reported as `applied_edit` events), `format`, `exec` (shell
command in the project root, where
the project lives), `git_status`, `git_diff`, `git_apply_hunk`, `git_stage`,
`git_unstage`, `git_discard`, `git_commit`, `git_branch_create`,
`git_switch`, `git_branch_delete`, `git_fetch`, `git_pull`, `git_push`,
`git_commit_paths`, `git_rollback`, `git_add`, `git_ignore`,
`git_operation_state`, `git_merge`, `git_rebase`, `git_continue_rebase`,
`git_abort`, `git_branch_rename`, `git_remote_branch_delete`, `git_update`,
`git_update_branch`, `git_branches`, `git_push_target`, `git_push_branch`,
`git_worktrees`, `git_worktree_add`, `git_worktree_remove`, `git_log`,
`git_refs`, `git_changes`, `git_commit_changes`, `git_selection_changes`,
`git_commit_details`, `git_cherry_pick`, `git_revert`, `git_reset`,
`git_undo_commit`, `git_reword`, `git_tag`, `git_checkout_revision`,
`git_patch`, `python_info`, `python_sync`,
`python_lock`, `python_add`, `python_remove`, `python_pin`,
`python_interpreters`, `python_test_at`, `devcontainer_configs`,
`run_configurations`, `debug_request`, `tests_discover`, `tests_run`,
`debug_start` (a debug request), `debug_set_breakpoints`, `debug_continue`,
`debug_step`, `debug_pause`, `debug_stack`, `debug_select_frame`,
`debug_variables`, `debug_evaluate`, `debug_output`, `debug_stop`,
`debug_download_codelldb` (with `debug_event` events: `stopped`, `running`,
`output`, `breakpoints`, `terminated`, `adapter_missing`), `ports_list`,
`ports_forward`, `ports_stop` (with `port_forwarded`, `port_released`,
`port_available` events), `shutdown`;
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
- **M8** debugger (DAP) — *done*
- **M9** WASM extension API
