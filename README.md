# next_code

Native, AI-first IDE written in Rust on [gpui](https://crates.io/crates/gpui).

## Layout

| Crate | Purpose |
|---|---|
| `crates/text` | UI-agnostic rope buffer: edits, undo/redo, grapheme/word motion, UTF-16 mapping, line endings, change log |
| `crates/syntax` | Incremental tree-sitter parsing and highlight spans for Bash, C, C++, CSS, Dockerfile, Go, HTML, JavaScript, JSON, Markdown, Python, Rust, SQL, TOML, TypeScript/TSX, YAML; loads the grammars from `grammars/` libraries on first use |
| `crates/grammar_abi` | The contract between the editor and its grammar libraries (entry table, ABI version, `export!`) |
| `grammars/*` | One shared library per tree-sitter grammar (`nc_grammar_<name>.dll`/`.so`), built with the editor and loaded when a file of that language opens |
| `crates/search` | Gitignore-aware project text search with regex, globs and streaming results |
| `crates/watcher` | Debounced project file watching with `.gitignore` filtering |
| `crates/terminal` | Shell process and terminal emulation, local or attached to a remote pty; links (URLs, file locations, OSC 8) in its output |
| `crates/worktree` | Project file operations shared by the editor and the agent: listing, ignore rules, tool lookup |
| `crates/proto` | Versioned editor ↔ agent wire protocol (framing, requests, events) |
| `crates/agent` | `next_code_agent`: headless Linux process serving files, watching, search, language servers and shells |
| `vendor/gpui` | gpui 0.2.2 with startup patches to its Windows platform (`NEXT_CODE_PATCHES.md` there) |
| `crates/instance` | One editor process per user: the launch request channel (named pipe / Unix socket), its JSON protocol, client and server |
| `crates/launcher` | `next_code`: the small launcher users run; hands the command line to the running editor or starts `next_code_editor` |
| `crates/remote` | Agent client and the bootstraps that install and start it in WSL or a container (`docker exec`) |
| `crates/devcontainer` | `devcontainer.json` (JSONC) and bringing dev containers up with the docker CLI |
| `crates/python` | `pyproject.toml` / `uv.lock`, uv command lines, pytest node ids |
| `crates/notebook` | Notebooks: `.ipynb` and marimo file formats (round-trip exact), marimo's static analysis and cell graph, kernel output folding, ANSI colours, the kernel bridge (`assets/bridge.py`) and its JSON-lines protocol |
| `crates/git` | Git through its CLI: porcelain v2 status, stage/unstage/discard/commit, hunk patches, branches, worktrees, fetch/pull/push, history, diffs |
| `crates/spell` | Spell checking: Hunspell dictionaries through `spellbook` (bundled en_US), word lists, word splitting, which parts of a document are prose |
| `crates/ui` | `next_code_editor` binary: editor, docks, LSP, terminal, search, problems, git and Python panels, diffs, fuzzy file finder (`Ctrl+P`) |

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

### Startup

The build makes two programs: `next_code` (`crates/launcher`), a small
launcher without gpui, and `next_code_editor` (`crates/ui`), the editor.
Run `next_code`. It hands its command line to the editor already running
(over a named pipe on Windows, a Unix socket in the state directory
elsewhere), which opens the window in a fraction of the time a process
start takes, and starts `next_code_editor` next to it when none runs. Like
VS Code, `next_code <dir>` from a second shell opens in the same process;
a project already open comes forward. `cargo run -p next_code` runs the
editor directly, which joins a running editor the same way.

- `--new-instance` starts a separate editor process that neither joins
  nor serves launches (tests and `--automation` use it).
- `--quit` asks the running editor to exit.
- `"keep_running"` in `settings.json`: `true` keeps the editor running
  after its last window closes, ready for the next launch; `false` exits;
  `"auto"` (the default) keeps it except for builds run from a cargo
  `target` directory, which cargo must be able to rewrite. A resident
  editor exits by itself when its executable has been replaced (the next
  launch starts the new build), on `--quit`, and on **File > Exit**.
- `scripts\install.ps1` copies the release build to
  `%LOCALAPPDATA%\next_code\bin` (asking a resident editor to exit first),
  where launches do not hold the cargo output; add that folder to `PATH`.
- The tree-sitter grammars (about 14 MB of parse tables) are not in the
  executable: each is a library, `nc_grammar_<name>.dll` on Windows
  (`libnc_grammar_<name>.so` on Linux), built with the editor and loaded
  the first time a file of its language opens. The editor looks in the
  `NEXT_CODE_GRAMMARS` directory, next to its executable, then in its
  `deps` directory (where cargo puts libraries built as dependencies, so
  `cargo run` finds them). A missing or stale library (the ABI is checked)
  leaves that language as plain text, with the reason on stderr;
  `cargo build --release --workspace` puts the libraries next to the
  executable, and `scripts\install.ps1` copies them.
- Size, Windows release build: 55 MB at the start of this work, 38 MB
  with `panic = "abort"`, thin LTO and gpui's `image` trimmed, 25 MB with
  the grammars in libraries, 24 MB with cold dependencies (LSP types,
  notebooks, workflows, git, SVG and image decoding, ...) at `opt-level =
  "s"`. `cargo build --profile dist` (fat LTO, one codegen unit) takes
  another 2 MB off at about four times the build time; `scripts\install.ps1
  -Profile dist` installs it.

Set `NEXT_CODE_TRACE_STARTUP=1` to print startup milestones (time since the
process was created) to stderr, e.g. from PowerShell:
`$env:NEXT_CODE_TRACE_STARTUP=1; .\next_code_editor.exe C:\dev\p 2> trace.txt`.
The WSL connection (or, for local projects, git status) starts on a thread
before the window, alongside gpui's platform setup. gpui is a patched copy
(`vendor/gpui`, see `NEXT_CODE_PATCHES.md` there): its Windows platform
creates the Direct3D device once (upstream probed with a throwaway device
first) and on a thread of its own, loads DirectWrite's font list on
another, and creates the shell's drag-and-drop helper on the first drag
instead of at startup. The release profile is `panic = "abort"` with thin
LTO and gpui's `image` crate trimmed to the formats shown, for a smaller
executable: on machines whose endpoint security scans every image load,
each MB of executable costs milliseconds per launch, and a rebuilt
executable's first launch pays a full scan.

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
binary's location. Each agent that starts points the stable link
`~/.local/share/next_code/next_code_agent` at its build, so CLI agents working
in the distro or container can call `next_code_agent pinpoint …` (see
[docs/pinpoint-for-agents.md](docs/pinpoint-for-agents.md#from-inside-wsl-or-a-dev-container)).

The real-WSL tests are ignored by default:

```powershell
$env:NEXT_CODE_WSL_AGENT = "$PWD\target\release\next_code_agent"
$env:NEXT_CODE_TEST_WSL_DISTRO = "Ubuntu"
cargo test -p remote --test wsl -- --ignored
# notebooks in WSL (needs uv in the distro; the agent next to next_code.exe)
cargo test -p next_code --test headless -- --ignored notebooks_in_wsl
```

The headless notebook tests use `NEXT_CODE_NOTEBOOK_PYTHON` when set; else
they create (once, with `uv`, which needs the network that first time) a
virtualenv with ipykernel and marimo in cargo's `target/tmp/notebook-env`,
else they use a `python3`/`python` that already imports ipykernel; without
any, they print why and pass as skipped.

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

## Git credentials and ssh-agent (dev containers and WSL)

Like VS Code's Dev Containers, git inside a dev container (or WSL) project
uses this machine's credentials, so `git push`/`git fetch` there need no
second login:

- **HTTPS**: the agent configures `next_code_agent git-credential` as the
  container user's `credential.helper` (`git config --global`, i.e.
  `~/.gitconfig` there; never the repository's config), only when that
  user has no helper of their own. It asks the editor, which runs
  `git credential fill|approve|reject` here (Windows: `git.exe`, so Git
  Credential Manager, which may show its sign-in window; a Linux/WSL editor:
  its git and helpers) with prompts off and a 3-minute limit (30 s for
  store/erase). Only `http`/`https` credentials are lent; when this machine
  has none, the answer is empty and git in the container prompts as usual.
  Requests are answered only while the project is open; the helper then
  stays in `~/.gitconfig` but answers nothing (git prompts). Credentials
  are never logged.
- **Azure DevOps**: when installing its helper, the agent also sets
  `credential.https://dev.azure.com.useHttpPath=true` if that setting is unset.
  This preserves the organisation path needed by Windows Git Credential
  Manager. Existing Azure-specific settings and user-defined helpers are
  left alone; no credentials are stored in the remote Git configuration.
- **SSH**: this machine's ssh-agent (`SSH_AUTH_SOCK`; on Windows, if unset,
  the OpenSSH agent's `\\.\pipe\openssh-ssh-agent`) is forwarded as a Unix
  socket in the container (`/tmp/next_code-<uid>/host/…-ssh.sock`, or under
  `$XDG_RUNTIME_DIR`), exported as `SSH_AUTH_SOCK` to terminals, tasks,
  language servers and git run through the editor (`ssh-add -l` there lists
  this machine's keys). Pageant is not supported.
- **Identity**: this machine's `user.name`/`user.email` (`git config
  --global`) are copied into the container user's global config when unset
  there.

WSL projects get the same (VS Code also uses Windows' Git Credential
Manager in WSL); a helper the user already set in the distro, e.g. GCM
from Windows, is left alone and keeps working. Settings:
`"devcontainer": { "forward_git_credentials": false }` lends nothing and
removes the helper set earlier on the next open; by hand, in the container:

```sh
git config --global --unset-all credential.helper "^!'.*' git-credential$"
```

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

Terminal lines are not wrapped at the panel's edge: a line wider than the
view (a long `printf`, output printed while the panel was wider, a long
command being typed) stays one row, and a left-right bar appears over the
bottom edge while any row on screen is wider. Drag its thumb, press the
track (the thumb centres there and drags), turn the wheel over it, or use
Shift+wheel or a trackpad's sideways scroll anywhere in the terminal. The
offset stays where you put it while output arrives (following the end of
the output as before); typing brings the cursor back into view, and it
returns to the start when nothing on screen is wider (`clear`,
full-screen programs). Programs still see the panel's width (the pty
follows the view, so `vim`/`htop` fit it and shells wrap their input as
they expect): the emulator wraps there and only the display joins the
rows, so a line keeps its full width when the panel narrows or widens.
Selection, copy, Ctrl+click on links and Pinpoint use the cells shown.
Lines over 2048 columns are cut into rows of that width.

## Notebooks

Jupyter notebooks (`.ipynb`, nbformat 4) and [marimo](https://marimo.io)
notebooks (a `.py` file with `import marimo`, `app = marimo.App(…)` and
`@app.cell` functions) open in a notebook tab; other `.py` files open as
text. A marimo tab has **Open as Text**; a marimo file shown as text offers
**Open as Notebook** (also in the command palette).

- **Cells**: code cells are editors (highlighting, brackets, undo, Vim,
  spell checking, the language server below, completion from the kernel); Markdown cells render
  (headings, emphasis, lists, tables, code, links, images from attachments,
  `data:` URIs or files next to the notebook) and switch to an editor on
  Enter or double-click. The gutter shows `[n]` / `[*]`, a run button and the
  elapsed time; marimo cells also show *stale* (edited since they ran) and
  *error*. Only the cells on screen get editors, so long notebooks scroll
  smoothly.
- **Toolbar**: Run All, Run Above, Run Below, Interrupt, Restart, Restart &
  Run All, Clear Outputs, Save, the kernel status (idle, busy, starting,
  dead) and the interpreter; clicking it picks another one (the project's
  virtualenvs, uv's Pythons, Jupyter kernelspecs, or a typed path).
- **Keys** (Jupyter's): edit mode — Shift+Enter run and select below (a new
  cell after the last), Ctrl+Enter run, Alt+Enter run and insert below, Esc
  command mode (with Vim, from Normal mode), Ctrl+Shift+- split at the
  cursor, Up/Down across cell edges, Ctrl+K Ctrl+I the kernel's help for the
  name at the cursor. Command mode — Enter edit, A/B insert above/below,
  D D delete, Z / Shift+Z undo / redo cell operations, M/Y/R Markdown /
  code / raw, Up/Down/J/K select, Shift+Up/Down/J/K extend, Home/End,
  C/X/V copy/cut/paste cells (Shift+V above), Ctrl+Shift+Up/Down move, O
  collapse outputs, I I interrupt, 0 0 restart, Ctrl+S save. Text edits undo
  per cell. **Collapse/Expand Cell Source** (palette) folds a cell's code to
  one line; clicking it or Enter shows it again.
- **Language server** (Python notebooks, as VS Code does): the code cells,
  in order, are one module for the project's Python server (basedpyright by
  default; locally, in WSL or in the dev container like any file), so a
  name defined in cell 1 is known in cell 3. In cells: diagnostics
  (squiggles, hover, Problems entries named `notebook.ipynb [cell 3]` with
  the line in the cell; clicking one opens the notebook at that cell),
  hover, completion (the server's items first, then names only the kernel
  knows, such as a DataFrame's columns), parameter hints, go to
  definition/declaration/type definition/implementation and find
  references (targets in the notebook open that cell, others their file;
  the picker names cells), rename and code actions (edits land in the
  cells, one undo step per cell), semantic highlighting and inlay hints.
  **Format Cell** (`Shift+Alt+F` in a cell) and **Format
  Notebook** (`Shift+Alt+F` in command mode) run the Python formatter
  (`ruff format` by default, see Formatting) on each code cell; cells with
  IPython magics go to ruff as a one-cell notebook so the magics stay.
  The server sees `<notebook>.nc-notebook.py` next to the notebook (never
  written): each cell keeps its lines, IPython syntax (`%magic`, `!cmd`,
  `x = !cmd`, `name?`) is replaced on its line by a placeholder, `%%bash`
  and other non-Python cell magics blank their cell, and Jupyter notebooks
  start with stubs for `display`, `get_ipython`, `In` and `Out`. Diagnostics
  that only come from joining cells are dropped: "expression value is
  unused" on a cell's last expression (Jupyter shows it), redeclarations
  of a name redefined in another cell, top-level `await`, anything on an
  IPython line. An edit sends only the lines it changed (one change per
  keystroke or structural change). Limits: the server reads the cells as
  one file in notebook order, not in run order; document highlights and
  document symbols (outline, breadcrumbs) are not offered in cells;
  notebooks whose kernelspec names another language get no server;
  formatting with a formatter other than ruff skips cells with magics;
  a cell's inlay hints refresh when it is edited or its diagnostics change;
  cells have no status bar, so editor notices ("No definition found")
  are not shown there.
- **Outputs** scroll with the cells: streams (ANSI colours, `\r` progress
  lines), errors with coloured tracebacks, PNG/JPEG/SVG images, Markdown,
  JSON (pretty), LaTeX as text, plain text. `clear_output(wait=True)` and
  display updates work. Text outputs are selectable; text longer than
  `"notebook": { "output_max_lines": 200 }` (`settings.json`) shows **Show
  more**.
- **HTML** outputs render natively when they stay within a safe subset:
  headings, paragraphs, `div`/`span`, bold/italic/code, links (open in the
  browser), lists, quotes, rules, `<pre>`, `data:` and inline SVG images
  (remote images show as links), and tables with `colspan`/`rowspan`. Inline
  `style` honours only `color`, `background(-color)`, `font-weight` and
  `text-align` (also `#id` rules of `<style>` blocks, which is how pandas'
  Styler colours cells); colours that would not read on the theme are
  dropped. pandas DataFrames: bold header and index, numeric columns
  right-aligned, alternating rows, the header sticks to the top while a long
  table scrolls by, wide tables scroll sideways (Shift+wheel), and pandas'
  `...` truncation row/column show as such; past 500 rows a table shows its
  first 500. HTML with `<script>`, `<iframe>`, `<canvas>`, form controls or
  other elements (Plotly, Bokeh, Altair, xarray…) shows its text version with
  a note saying why; Plotly figures (`application/vnd.plotly.v1+json`) get
  the same note. Every HTML output keeps **Open in browser** (Plotly's opens
  a page drawing the figure with plotly.js).
- **ipywidgets** work through the Jupyter comm protocol (the bridge forwards
  `comm_open`/`comm_msg`/`comm_close` both ways, with binary buffers):
  Int/Float/FloatLog/IntRange/FloatRange sliders (dragging sends updates
  while `continuous_update`, else at the end), Int/Float/Bounded text,
  Text, Textarea (Enter or typing sends, per `continuous_update`), Checkbox,
  ToggleButton(s), Dropdown, RadioButtons, Select, SelectMultiple, Button
  (`on_click`), Label, HTML/HTMLMath (through the HTML renderer), Image,
  Int/FloatProgress, HBox/VBox/Box/GridBox (as flowing rows/columns),
  Accordion, Tab and Stack, and Output widgets (what they capture renders as
  outputs), so `interact`/`interactive` work. `description`, `disabled`,
  `layout.width`/`display` and the readout format are honoured. The widget
  state is saved in the notebook's `metadata.widgets` as Jupyter does, so a
  reopened notebook (or one whose kernel ended) shows the last values,
  disabled until the cell runs again. Widgets of other libraries
  (anywidget, ipyleaflet, Plotly's FigureWidget, bqplot…) show their text
  and a note.
- **Files**: saving keeps everything the file had (metadata, cell ids,
  attachments, `nbformat_minor`) and writes Jupyter's own JSON layout
  (sorted keys, the file's indentation, a final newline), so `git diff`
  shows only what changed; outputs, execution counts and collapsed
  outputs/sources are saved (`metadata.jupyter.outputs_hidden` with the
  older `collapsed`, and `source_hidden`, as JupyterLab and VS Code read
  them). marimo files are written as marimo writes them (header,
  `__generated_with`, cell names and options kept; each cell's parameters
  and returns recomputed from its code); they have no place for collapsed
  state, which then lasts while the notebook is open. Changes on disk
  reload a clean notebook (a modified one asks): cells are matched by text,
  marimo cell name and position, so unchanged cells keep their outputs
  (also when a tool stripped the outputs from the file); in a marimo
  notebook an edited cell and its descendants turn *stale*, as after an
  edit in the editor.
- **Kernel**: an ipykernel in the project's interpreter (the folder's
  `.venv`, else `uv run python` in a uv project, else `python3`/`python`),
  started on the first run, in the notebook's folder, where the project
  lives (also in WSL and dev containers, where its ports are not
  forwarded). A small Python bridge drives it
  with `jupyter_client` (no ZeroMQ in the editor); it is sent to the
  interpreter on stdin, so nothing is installed. Without ipykernel a banner
  offers **Install ipykernel** (`uv add --dev ipykernel` in uv projects,
  else `python -m pip install ipykernel`, in a terminal) and **Choose
  Interpreter**. Closing the tab or the editor stops the kernel; a kernel
  whose editor died exits by itself.
- **marimo**: cells run in one kernel with marimo's rules: running a cell
  also runs its stale or never-run ancestors and then every cell that uses
  what it defines, in dependency order; a name defined by two cells, or a
  cycle, is an error on those cells; cells after a failure are skipped.
  Each cell runs with only the names it uses and publishes the names it
  defines (`_private` names stay in the cell); top-level `await` works
  (`async def` cells, on the kernel's event loop); `mo.md(…)` and other
  marimo outputs render. `mo.ui` sliders, numbers, text fields,
  checkboxes, switches, dropdowns, radio groups and buttons are drawn
  natively, also inside `mo.md(f"… {slider} …")`: changing one sets its
  value in the kernel as marimo does (conversion, `on_change`) and re-runs
  the cells that use it. Other elements (tables, forms, arrays, dates,
  files…) show as text: **Open in marimo** runs `marimo edit` in a terminal
  and opens its URL in the browser (forwarding the port for WSL and
  containers).

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

A workflow is one YAML file that starts a multi-project development
environment: preconditions, commands in the terminals of several projects
(opened as needed, dev containers included), ordering with readiness
conditions, browser links and manual gates. Files are found in
`<config>/next_code/workflows/*.yaml` (or `.yml`; next to `settings.json`;
`%APPDATA%\next_code\workflows` on Windows) and in each open project's
`.next_code/workflows/*.yaml`, and in any folder listed in `settings.json`
(`"workflows": {"folders": ["C:/team/workflows", "~/shared/workflows"]}`;
Run > **Add Workflow Folder…** adds one) or in `NEXT_CODE_WORKFLOW_FOLDERS`
(separated as in `PATH`). Run > **Open Workflow File…** opens a workflow
file from anywhere, listed for the rest of the session (headless: any
command's `path` may be an absolute path to a file anywhere). They are
read again before every Run All.
The format borrows what other tools made familiar: steps keyed by id with
`depends_on` and its conditions, durations and `x-` keys with anchors from
Docker Compose, `inputs` from GitHub Actions, `${…}` variables from VS
Code. JSON works too (it is YAML).

```yaml
name: Fleet dev
steps:
  # A process on this machine (PowerShell on Windows); exit 0 passes.
  vpn:
    title: VPN
    check: if (-not (Test-Connection -Count 1 -Quiet intranet.corp)) { exit 1 }
    on_fail: Connect the VPN, then press Retry.
  api:
    depends_on: [vpn]
    project: {devcontainer: C:/src/api}
    run: uv run uvicorn app:app --port 8000
    terminal: API
    ready: {port: 8000}
    timeout: 10m
  worker:
    depends_on: [vpn]
    project: {devcontainer: {wsl: Ubuntu, path: ~/src/worker}}
    run: uv run worker
    ready: {output: worker started}
  web:
    depends_on: [api]
    project: {devcontainer: C:/src/web}
    run: npm run dev
    ready: {output: 'ready in \d+ ms'}
    open_urls: true
  login:
    depends_on: [web]
    ready: {manual: 'Log in in the browser, then Continue.'}
  tools:
    depends_on: [login, worker]
    project: {wsl: Ubuntu, path: ~/src/tools}
    run: ./sync.sh --env dev
    env: {TOKEN: '${env:FLEET_TOKEN}'}
    cwd: scripts
  # Checkpoints on Ctrl+C: 2 minutes before a stop kills it.
  eval:
    depends_on: [api]
    project: {devcontainer: C:/src/api}
    run: uv run python eval.py
    stop_grace_period: 2m
    artifacts: [{label: metrics, path: out/metrics.json}]
  # Runs once `eval` finished, also when it failed.
  report:
    depends_on: {eval: completed}
    project: {devcontainer: C:/src/api}
    run: uv run python report.py ${steps.eval.exit_code} ${steps.eval.artifacts.metrics}
```

Top level: `name` (default: the file name), `description`, `inputs`,
`env` (environment every step gets; a group's and a step's own win),
`steps`, and `x-…` keys holding shared parts for YAML anchors
(`x-api: &api {project: …}`, then `<<: *api` in steps, as in Compose
files). Step fields (the step's id is its key in `steps`): `title`,
`depends_on`, `project`, `run`, `terminal`, `check`, `on_fail`, `ready`,
`timeout`, `stop_grace_period`, `artifacts`, `open_urls`, `env`, `cwd`,
`env_file`, `restart`, `healthcheck`, `if`.
`depends_on` is an id, a list of ids (each must be ready; independent
steps run in parallel), or a mapping of ids to a condition: `ready` (the
default) or `completed` (finished, ready or failed), also as Compose's
`{condition: completed}`. Durations are Compose's: `90s`, `10m`, `1h30m`,
`500ms` (a plain number is seconds). Unknown keys, unknown dependencies,
cycles and `${steps.…}` variables naming a step outside `depends_on` make
the workflow invalid: it is listed with the error (located at the key or
value it is about) and cannot run. Keys of the old JSON format say what
they became.

**Editing workflow files.** next_code has its own language server for
workflow files (`crates/workflow_lsp`), built in and run in-process, so it
needs nothing installed and works the same for local, WSL and
dev-container projects. It serves the workflow tab's editor and YAML files
opened in a project (only workflow files: under `next_code/workflows/` or
`.next_code/workflows/`, or starting with the schema line below; other
YAML gets nothing from it):

- completion of keys for where the cursor is (a step's, a group's, an
  input's by its type, `ready`'s conditions, `project` shapes, an
  artifact's), of values (`depends_on` → the sibling steps not listed yet,
  `ready`/`completed`, input types, a choice's options as `default`,
  durations, `ready`/`project` snippets) and of `${…}` variables
  (`${steps.` → only the steps the step may read, then `status`,
  `exit_code`, `artifacts.` and the step's artifact labels; `${inputs.` →
  the inputs; a `}` is added when missing);
- next_code's own problems as diagnostics, on the key or value they are
  about;
- hovers: the schema's description of a key, a step's summary (title,
  command, readiness, dependencies) on its id or a dependency, an input's
  on `${inputs.…}`, what a variable reads;
- Go to Definition from a dependency or `${steps.<id>.…}` to the step,
  from `${inputs.<id>}` to the input; Find References; Rename of a step
  id (its key, the siblings' `depends_on` and the variables that read it,
  scoped as the scheduler resolves them) or an input id;
- the inputs and steps (groups nested) as document symbols (outline,
  breadcrumbs, Go to Symbol).

Headless, the same comes through `open`, `diagnostics`, `definition`,
`document_symbols`, `rename` and the other language-server commands
(`open` reports `server: "next_code workflows"`). Another YAML server can
replace it: `"languages": { "yaml": { "server": { "command":
"yaml-language-server", "args": ["--stdio"] } } }` in `settings.json`.

The format is also a JSON Schema (`crates/workflow/schema/workflow.schema.json`,
headless `workflow_schema`): next_code keeps a copy as
`workflow.schema.json` in the user's workflows folder, and new files there
start with `# yaml-language-server: $schema=./workflow.schema.json`, so
editors with a YAML language server (VS Code's YAML extension, JetBrains,
Zed, Neovim) complete keys, describe them on hover and flag mistakes. For
project workflows copy the schema beside them or map it in `yaml.schemas`
(see the manual §2.1). next_code's own validation still checks more
(dependencies, cycles, variables, regexes).

**Services, conditions and env files.** `ready: {http: <url>}` waits for
a URL to answer (200–399, or `{url, status}`) where the step runs. A ready
step whose command exits non-zero crashed and fails. `restart:
on_failure` (or `always`, or `{policy, max, backoff}`) starts a failed
command again after its backoff (exit codes, crashes, timeouts,
unhealthy), up to `max`. `healthcheck: {test: <command> | http: <url> |
port: N, interval, timeout, retries, start_period}` probes a ready step
every `interval` (any command every N seconds); `retries` failures in a
row make it unhealthy: it fails, and restarts with a policy. `if:
inputs.env == 'prod'` (also `steps.<id>.status`, `.exit_code`,
`success()`, `failure()`, `always()`, `==`, `&&`, `||`, …) skips a step
when false; steps needing it ready are skipped too; skipped steps count
as done. `env_file: .env` (or a list, `{path, required: false}`; on the
workflow, groups and steps) reads dotenv files where the step runs,
relative to its `cwd`; `env` wins. Details: the manual §3.6–§3.8.

Files of the old JSON format (`.json`/`.jsonc` with a `steps` array) are
listed with a problem and do not run; their tab has **Convert to YAML**
(headless: `workflow_convert`), which writes `<name>.yaml` beside the old
file with its comments kept and renames the old file to `.bak`.

- `project`: `{local: C:/src/p}`, `{wsl: Ubuntu, path: ~/p}`,
  `{devcontainer: <folder>, config: <devcontainer.json>}` (folder: a
  path string or the `wsl` shape), a typed location (`'Ubuntu:~/p'`,
  `'devcontainer:C:\src\p'`), or the editor's own shape from
  `recent.json` (`{Local: ...}`, `{Wsl: {...}}`, `{DevContainer: {...}}`).
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
  `on_fail`: the step shows Failed with that message and Retry; without it
  the workflow fails and everything it started is stopped.
- `ready`: `{exit: true}` (the default with `run`: exit code 0; without
  `run`, the check passing), `{output: <regex>}` (the command's output,
  ANSI codes stripped, across chunks and lines), `{port: N}` (listening
  where the step runs: the agent's listening sockets for WSL/containers, a
  TCP connect to localhost otherwise), `{manual: <prompt>}` (a Continue
  button), `{delay: <duration>}`. A command that exits before it is ready
  fails the step. `timeout` (from the step's start) fails it too. Quote
  text with a comma inside `{…}` (`{manual: 'Log in, then Continue'}`): a
  comma there separates keys.
- `open_urls`: http(s) URLs on complete output lines open in the browser
  (each once) until the step is ready plus 3 s.
- `env`, `cwd` (relative to the project; the home folder without one) and
  the command lines expand `${workspaceFolder}` (the home folder without a
  project), `${userHome}` and `${env:NAME}` (the environment where the step
  runs; read with `env` for WSL/containers); so do artifact paths.
- `stop_grace_period` (1s–1h, default 3s): every stop of the step's
  command (Stop All, Restart, `timeout`, Run All replacing the run, the
  workflow failing) sends Ctrl+C to its terminal and kills it only if it
  still runs after that long; meanwhile the step is `stopping` with a
  countdown. A second Stop All kills at once.
- `artifacts`: files (relative to `cwd`, else the project) and http(s)
  links the step produces: `out/metrics.json`, `https://…`,
  `{label: ..., path: ...}`, `{label: ..., url: ...}`.
  When the step finishes they are resolved and looked up where it runs
  (through the project for WSL/containers); the details strip links them
  (a file opens in an editor tab of the step's project, a URL in the
  browser; missing files dimmed, Check again), cards show `↗ N`.
- `${steps.<id>.status}`, `${steps.<id>.exit_code}`,
  `${steps.<id>.artifacts.<label>}` / `.<index>` in `run`, `check`, `cwd`,
  `env`, artifact paths: what a step listed in `depends_on` left,
  replaced when this step starts. Paths are as seen where the producing
  step runs (another machine or container may not see them).
- `steps` (a group): a step with `steps` of its own is a group of them.
  A group takes `title`, `depends_on`, `steps` and defaults its steps
  inherit unless they set their own: `project`, `cwd`, `env` (merged, the
  step's values winning); nothing else (no `run`, `check`, `ready`, …),
  and at least one step. Groups nest. Ids are unique among siblings and
  contain no `/`; `depends_on` inside a group names its other steps only.
  A group's `depends_on` gates all its steps; steps outside wait for the
  group like for a step, by its state: running while a step in it runs,
  ready when all are, failed (`2 of 5 steps failed`) when none can
  progress and some failed, waiting/blocked on its own `depends_on`,
  stopped. `${steps.<id>.…}` in a step reads its siblings in its
  `depends_on` and what its groups list in theirs (by their id at that
  level); `${steps.<group>.status}` reads a group (it has no exit code or
  artifacts). Elsewhere steps are named by qualified id: `api/migrate`
  (events, `workflow_status`, `workflow_restart`, …).

  ```yaml
  api:
    depends_on: [vpn]
    project: {devcontainer: C:/src/api}
    env: {PROFILE: dev}
    steps:
      build: {run: uv sync}
      migrate: {depends_on: [build], run: uv run alembic upgrade head}
      serve: {depends_on: [migrate], run: uv run uvicorn app:app, ready: {port: 8000}}
  ```

Inputs: a top-level `inputs` mapping declares parameters each run gets,
read in steps as `${inputs.<id>}` (the shape of GitHub Actions'
`workflow_dispatch` inputs):

```yaml
inputs:
  env:
    label: Environment
    type: choice
    options: [dev, staging, {value: prod, label: Production}]
    default: dev
    description: Where to deploy
  branch: {type: string, default: main, placeholder: branch name, required: true}
  workers: {type: number, default: 4, min: 1, max: 64, integer: true}
  clean: {type: boolean, default: false}
```

| Key | Meaning |
|---|---|
| (its key) | the id: unique; no `/`, `.`, `{`, `}`, `$`, spaces |
| `type` | `string` (default; a textbox), `number` (a textbox checked as a number), `boolean` (a checkbox), `choice` (a dropdown) |
| `label`, `description` | the form's label (default: the id) and helper text |
| `default` | of the input's type and valid for it (unquoted numbers keep their text for strings: `1.10`); without one `""`, empty, `false`, the first option |
| `required` | `string`/`number`: must not be empty |
| `placeholder` | `string`/`number`: shown in the empty textbox |
| `options` | `choice`: values or `{value, label}` |
| `min`, `max`, `integer` | `number`: inclusive bounds, whole numbers only |

Unknown keys, keys of another type, invalid defaults and
`${inputs.<id>}` naming no input are validation errors (located in the
file). `${inputs.<id>}` works in `run`, `check`, `cwd`, `env` values,
artifact paths and `project` strings, at any depth (groups' too); values
are fixed when the run starts and inserted as they are (strings and
options as written, numbers in their shortest form, `true`/`false`, an
empty number as nothing; no `${` in values). Restart and Retry Failed
reuse the run's values. Run All (toolbar, Shift+F10, palette "Run
Workflow: …") on a workflow with inputs first shows a form over the
graph: label, control, description and what is wrong with the value per
input, prefilled with the values the workflow last ran with (kept per
workflow file in `workflow_inputs.json` in the local data dir; values
that no longer fit — an option removed, a bound or type changed — fall
back to the default), Reset to defaults, Cancel (Escape) and Run (Enter;
disabled while a value is invalid). Tab moves between fields, Space
toggles a checkbox; a dropdown opens with a click, Space, Up or Down
(Up/Down, Enter picks, Escape closes). Workflows without inputs run at
once. The toolbar shows the current run's values (`env=dev ·
branch=main`; the whole text on hover). Headless `workflow_run` takes
`"inputs": { "env": "prod", "workers": 8 }` (strings that parse are fine
for numbers and booleans); the others get their defaults (not the form's
remembered values), and invalid, unknown or missing required values fail
it with every problem listed (``input `workers`: must be ≤ 64``).

States: pending → waiting (its `depends_on`) → starting (opening the
project, the check) → running → ready | failed | stopped, with stopping
(Ctrl+C sent, grace running) before stopped (or failed for a timeout). A
failed step blocks the steps that need it ready (shown as blocked) but
not those that wait for it `completed`; a stopped one blocks both; others
keep going. When nothing
can progress and not every step is ready, the run is Failed (finished
with failures; the failed steps keep their reason). Restart reruns one
step (its old command is stopped with its grace first, then it starts in
the same terminal; dependents that already run are left alone), on a
group every step in it; Retry Failed reruns failed steps (on a group:
its failed steps); Stop All stops every command the run started
(project tabs and containers stay); Run All starts afresh (steps whose
old command still stops start once it is gone).

UI: each workflow opens as its own tab in the window's tab row (marked ◈,
colored by the run's state; opening the same file again shows its tab;
restored with the window). Run > Open Workflow… picks one (user and open
projects' workflows); Run > New Workflow… writes a commented template to
the user folder and opens its tab; the command palette's "Run Workflow:
<name>" opens the tab and runs it. The tab has the file in an editor on
the left (Ctrl+S saves; drag the divider to resize) and a graph on the
right: one column per dependency depth (longest chain of `depends_on`),
cards ordered for the fewest edge crossings found (median and
barycenter sweeps with swapping and sifting, then an exhaustive search
while small), chains kept on one row (Brandes–Köpf placement, compacted),
edges spanning several columns routed between the cards they pass, never
through them. Each card shows title, id and what it does (`after` when
it waits for a step `completed`), where it runs, status icon/color and elapsed time. Edges
leave and enter cards horizontally (several on one side get a port each)
and end in an arrowhead they meet square on; edges of `ready`
dependencies turn green when their upstream is ready, dashed edges of
`completed` ones light up when their upstream finished (ready or
failed). The graph opens fitted to its pane
(at most 100 %, at least 40 %, centered) and re-fits on resize and after
edits until you zoom or pan: Ctrl+scroll zooms around the pointer, the
corner overlay has −, the zoom, + and Fit (back to fitting); scroll
(Shift+scroll sideways) or drag pans. The
graph follows the buffer 200 ms after an edit; a file with problems shows
them in a banner with line and column (click: go there) over the last
good graph, dimmed. Clicking a card puts the cursor on its key; the
card holding the cursor is highlighted. A selected card shows details
below the graph: state, wait/failure reason or manual prompt, last output
lines (not wrapped: wider lines scroll sideways with Shift+wheel or the
left-right bar over the box's bottom edge, as in terminals),
artifacts (links), Terminal (the project's terminal tab, or this
machine's terminal inside the workflow tab), Continue, Restart/Retry. A
terminal inside the tab fills the details' pane, which a divider
separates from the graph: drag it to resize (the graph keeps 90 px, the
pane 240 px), double-click it to maximize the pane and back; the size is
kept per tab while it is open (not saved with the window). A group is a
card of the same size: its state, title, `▸ N steps` and elapsed time
over a miniature of its graph (its steps as small boxes in their state's
colors, nested groups outlined, thin edges, dashed for `completed`).
Clicking it selects it and expands it in place into a box around its
graph of real cards (selectable, with their details, terminals and
actions); clicking the box's header (`▾`) collapses it. Several groups
can be expanded at once, nested ones too; columns widen and rows grow to
fit an expanded box and edges attach to its sides. A selected group's
details show its state, its steps counted by state, Restart, Retry
Failed and Expand/Collapse. Groups stay expanded while the tab is open,
across edits; the graph re-fits on each expand/collapse unless you
zoomed. The cursor inside a collapsed group's step highlights the group.
The toolbar shows the
state (Idle, Running, All ready, Failed, Stopped), Run All (saves the file
first if edited, then runs what was saved; Shift+F10), Stop All (Ctrl+F2)
and Retry Failed. Workflow files in remote projects are read and written
through the project (its tab opens first when a restored workflow tab
needs it). Workflows run in the window that started them.

Headless: `workflows_list` (groups with their `steps` nested, each entry
with `kind` and qualified `path`; each workflow's `inputs`),
`workflow_schema` (the format's JSON Schema), `workflow_convert` (an old
JSON file to YAML), `workflow_run`
(`inputs`), `workflow_status` (the run's `inputs`; a
row per step and group: qualified `id`, `kind`, `parent`), `workflow_stop`,
`workflow_continue`, `workflow_restart` (a group: all its steps),
`workflow_retry` (optional `step`: that step or a group's failed steps),
`workflow_check_artifacts` and `workflow_event` notifications (groups
report their state changes too; see `crates/ui/src/headless.rs`).
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
frame (Up/Down for history). Long lines are not wrapped: the output
scrolls sideways (Shift+wheel, trackpad, the left-right bar at its bottom).
A bar above the editors shows the session's
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

### Stash and shelf

**Git > Stash Changes…** (also in the Commit window's right-click menu, for
the chosen files) asks for an optional message and **Include untracked
files** (preset when the chosen files include unversioned ones), then runs
`git stash push` and reverts those changes. **Git > Unstash Changes…** lists
the stashes (`stash@{n}`, message, branch, age) with the selected one's
files below; double-click a file (or **View Diff**) for its diff (the commit
it was made on ↔ the stash; untracked files against nothing). **Apply** keeps
the stash, **Pop** drops it once applied, **Reinstate index** restores what
was staged as staged, **Drop…** asks first. When the stash conflicts with the
files now, the Commit window shows the files under Merge Conflicts and the
stash is kept.

The **shelf** is PyCharm's: **Shelve Changes…** (the Commit window's
right-click menu, or Git > Shelve Changes… for the selected, else checked,
files) asks for a name, prefilled from the commit message's first line or
the file names, saves the changes as a patch against HEAD
(`.next_code/shelf/<id>/shelf.patch`, binary-safe, plus `info.json` with
the name, date, base commit and files; a `.gitignore` of `*` in
`.next_code/shelf/` keeps them out of git) and rolls the files back
(modified and deleted files return to HEAD, new files are deleted). The
Commit window's **Shelf** tab (next to **Changes**; Git > Show Shelf) lists
shelves newest first with their file count and age; expand one for its
files, double-click a file for its diff. **Unshelve…** (toolbar or
right-click) applies the patch to the working tree, removing the shelf or
keeping it as you choose; if the patch no longer applies cleanly it is
merged three-way and conflicting files show under Merge Conflicts (the
shelf is then kept). **Delete…** asks first. Shelves live under the
repository root where the project lives (WSL, container), and headless
commands use the same code.

Limits: unshelving restores changes as unstaged (staged/unstaged is not
kept), and a three-way unshelve refuses files that have unstaged changes
of their own ("does not match index"): commit, stash or roll them back
first. Shelving needs at least one commit.

### Changes and blame in the editor

**Change markers**: the editor's gutter (between the line numbers and the
fold arrows) marks lines that differ from the last commit — a green bar for
added lines, a blue bar for modified ones, a grey triangle where lines were
deleted — or from git's index with `"git": { "staging_area": true }`.
They follow typing (the diff runs on a background thread 100 ms after you
stop, 0.4–1 s for files over 256 KiB / 2 MiB) and refresh with the Commit
window: after saves, commits, staging, checkouts and changes from a
terminal. Untracked and ignored files have none; a file staged but never
committed is all added. Line endings do not count (a CRLF checkout of an LF
file shows no changes); adding or removing the final line break marks the
last line.

Clicking a marker opens the change: the committed lines (red) above the
current ones (green), with **↑/↓** (previous/next change), **Rollback**
(puts the committed lines back in the buffer; one undo step), **Stage
Hunk** (staging-area mode: only that change goes into the index, the file
on disk is untouched), **Show Diff** (the file's diff tab, scrolled to the
change; it shows the saved file) and **✕** (or `Escape`). Keys: Next /
Previous Change `Ctrl+Alt+Shift+Down` / `Up` (PyCharm; VS Code's
`Alt+F5` / `Shift+Alt+F5` too; Go menu), **Rollback Lines** `Ctrl+Alt+Z`
(every change touching the selected lines; Git menu).

**Annotate with Git Blame** (right-click in the gutter, Git menu or the
palette; again to close) adds a column left of the line numbers with the
date and author of the commit that last changed each line, written once
per run of lines from the same commit (commits of the last 30 days in the
normal text color, older ones muted). Hovering shows the commit's hash,
author, date and full message; clicking selects it in the Git tool window
(filters cleared). Blame runs on the editor's text, so unsaved lines read
"Not committed", and lines move with your edits; it runs again after a
commit. With `"git": { "inline_blame": true }` the cursor's line also shows
`author, age • summary` after its end (VS Code's inline blame).
The gutter's right-click menu also has Rollback Lines and Show Change on a
changed line.

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
- **File > Recent Projects** lists recent projects and workspaces; **Open
  Recent…** (`Ctrl+R`) also accepts a typed location such as
  `Ubuntu:~/project`, `devcontainer:C:\src\app` or a workspace file.
- **File > Open Workspace from File…**, **Add Folder to Workspace…** and
  **Save Workspace As…**: see [multi-root workspaces](#multi-root-workspaces).
- Opening a project that is already open (in any window) switches to its
  tab. `Ctrl+Alt+]` / `Ctrl+Alt+[` cycle the tabs.
- Drag project or workflow tabs left/right to reorder them, just like file
  tabs. Drop on a tab to insert before it, or on `+`/empty strip space to
  append. The current project stays active, and the order survives a restart.
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
`next_code <dir>`, `--wsl`, `--devcontainer`) opens just that project,
in the editor already running if there is one (see
[Startup](#startup)); a launch without a project while the editor runs
brings its windows forward, or reopens the saved ones when it stayed
running with none.
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

**Copying text from the UI.** Messages, errors, notices, banners, hover
and parameter-hint docs, Problems entries (with their `source(code)`, e.g.
`rustc(E0308)`), workflow details and step output, test failures and the
status bar can be selected like web-page text: drag (also past the text's
end), double-click a word, triple-click the whole message; `Ctrl+C` (or
`Ctrl+Insert`, Edit > Copy) copies, `Ctrl+A` selects the whole message,
`Escape` clears and returns focus where it was. Right-click shows **Copy**
and **Copy All**. A plain click still does what it did (a Problems row or
the workflow banner still jumps to the location), and in rows where a
double-click does something (Commit window and Git log files, branches,
Tests, Call Hierarchy, debugger values) a double-click on the text still
does it instead of selecting a word; dragging still selects. A selection
stays within one message.

## Multi-root workspaces

A project tab can hold several folders, like a VS Code `.code-workspace`
window. The folders live on one machine: all local, or all in one WSL
distro (a dev container project has a single folder; adding one, saving it
as a workspace or opening a workspace in a container is refused with a
message).

- **File > Add Folder to Workspace…** adds folders (the system folder
  dialog for local projects; a typed path for WSL projects, or where there
  is no dialog). A folder on another machine is refused. The Files tree's
  folder rows have **Add Folder to Workspace…** and **Remove Folder from
  Workspace** (disabled for the last folder; also in the command palette
  while the tree has focus).
- **File > Save Workspace As…** writes a `*.next_code-workspace` file
  (the extension is added when missing) and reopens the tab from it; the
  tab is then named after the file (`all (Workspace)`). Adding or removing
  folders later rewrites that file.
- **File > Open Workspace from File…**, `Ctrl+R` with a typed path, a
  Recent entry or the command line open `*.next_code-workspace` and VS Code
  `*.code-workspace` files (`Ubuntu:/home/me/all.code-workspace` for one in
  WSL). VS Code files are read, never written: changing the folders of one
  turns the tab into an untitled workspace (save it as a next_code file to
  keep it).
- Unsaved workspaces are kept too: Recent and window restore reopen them
  with their folders (and each folder's session).

The project's folders are fixed for the life of its workspace: adding or
removing one saves the session (open editors, splits, layout) for the new
set and reopens the project in the same tab, as VS Code reloads its window
when a folder becomes a workspace. Files with unsaved changes must be saved
or closed first (a notice says so).

```jsonc
{
  "folders": [
    { "path": "." },                          // relative to the file
    { "path": "../api", "name": "API" },      // shown as "API"
    { "path": "/srv/shared/lib" }             // absolute
  ],
  "settings": { "format_on_save": true, "languages": { "python": { "word_wrap": true } } }
}
```

Comments and trailing commas are allowed. VS Code files may list folders
as `path` or `file:` `uri` (other URIs are skipped); of their `settings`,
`editor.wordWrap`, `editor.formatOnSave`, `editor.inlayHints.enabled`,
`editor.autoClosingBrackets`, `editor.minimap.enabled`,
`editor.stickyScroll.enabled`, `git.blame.editorDecoration.enabled`,
`python.analysis.typeCheckingMode` and per-language blocks
(`"[python]": { "editor.formatOnSave": … }`) are translated; the rest is
ignored.

**Settings layering.** A file's settings are the user's `settings.json`,
under the workspace file's `settings`, under its folder's own
`.next_code/settings.json` (any folder, also single-folder projects; same
schema as `settings.json`). They apply to word wrap, inlay hints, format on
save and formatters, language servers and tests; app-wide settings (theme,
window restore, menus) stay the user's. A broken workspace or folder file
shows a notice and is skipped; changes to a folder file apply when it is
saved.

**Across folders.** With more than one folder, lists label paths with their
folder, `api › src/main.rs`:

- **Files tree**: one bold row per folder, in the workspace's order (the
  first expanded). Folder rows cannot be renamed, moved or deleted; files
  can be dragged or pasted between folders. Copy Relative Path is relative
  to the item's folder.
- **Go to File** (`Ctrl+P`) lists every folder's files; typing a folder's
  name narrows to it (`web main`). **Find/Replace in Files** searches every
  folder (a file inside two nested folders counts once); include/exclude
  globs apply relative to each folder. Replace notices, references, Go to
  Symbol in Workspace and the breadcrumb file browser (whose `..` at a
  folder's top lists the folders) use the same labels.
- **Git** (PyCharm's multi-repository mode): each folder's repository is
  found (folders in one repository share it; folders outside git are
  skipped; a refused repository gets its own Trust banner). The Commit
  window shows one collapsible header per repository (`name`, branch, file
  count, checkbox); **Commit** commits each repository that has checked
  files with the same message and reports per repository; Commit and Push
  opens a Push dialog per repository; Fetch, Pull and Update Project act
  on all of them. The Shelf tab and the Git log have a repository
  selector; the log follows the active editor's repository until one is
  picked. The title-bar branch widget shows the active file's repository
  (`api: main`), and the branches popup has a row of repositories to
  switch between. Change markers, blame, Stage Hunk and Show Diff use each
  file's own repository.
- **Language servers** start with every folder in `workspaceFolders`
  (`rootUri` is the first) and answer `workspace/workspaceFolders` with all
  of them; folders never change while a server runs, so
  `didChangeWorkspaceFolders` is not needed. A server that does not
  advertise workspace folder support gets one instance per folder (in the
  folder, with its settings and interpreter; logs `lsp-<lang>-<n>.log`),
  and each document goes to its folder's server. **Problems** are grouped
  under a header per folder.
- **Terminal**: New Terminal starts in the active file's folder and
  activates that folder's `.venv`; tab titles end with `(folder)`.
- **Run configurations** come from every folder (`.next_code/run.json`,
  `.vscode/launch.json`, pyproject/Cargo) as `name (folder)`, resolve
  `${workspaceFolder}` to their folder and run there; Edit Configurations
  opens the active file's folder's `run.json`. The **Tests** panel has a
  node per folder (test ids `folder › id`), each run in its folder.

Limits: the Python panel shows the first folder's uv project; a folder
nested in another shows both under its parent and as its own row;
`exec` and most `python_*` headless commands run in the first folder.

## Files tree

The Files panel works like PyCharm's Project view (VS Code's Explorer where
PyCharm has no equivalent), for local, WSL and dev container projects alike.

- **Selection**: click selects and opens a file (focus stays in the tree;
  double-click or `Enter` focuses the editor), `Ctrl`+click toggles a row,
  `Shift`+click / `Shift+Up/Down` select a range, `Up/Down/Home/End` move,
  `Right/Left` open/close folders, `Ctrl+A` selects every visible row.
- **Context menu** (right-click a row, or empty space for the project
  root): New File / Folder / Python File (New File with Python chosen) /
  Python Package
  (folder with `__init__.py`), Cut, Copy, Paste, Duplicate, Copy Path,
  Copy Relative Path, Rename, Delete, Reveal in File Explorer (local
  projects), Open in Terminal (a shell in that folder). `Alt+Insert` shows
  the New items under the selected row (`Up/Down/Enter` choose).
- **New File…** is also in the File menu and the command palette wherever
  focus is: it shows the Files panel and starts in the active file's
  folder.
- **Names** are typed in place (VS Code style): New puts an input at the top
  of the folder (the selected folder, or the selected file's folder); a
  path such as `pkg/sub/mod.py` creates the folders too. Rename (`F2` or
  `Shift+F6`) selects the name without its extension. Problems show under
  the input while typing: empty name, `/`, `.`/`..`, on local Windows
  projects `\ < > : " | ? *`, control characters, trailing dot/space and
  reserved names (`CON`, `NUL`, `COM1`…), and names already taken.
  `Enter` applies, `Escape` cancels, clicking elsewhere applies a valid
  name. A new file opens in an editor (a notebook in the notebook view).
- **File types**: New File's row has a type selector beside the name,
  **Auto** by default (the name is used as typed; the type its extension
  implies shows faintly). `Tab` (or `Alt+Down`, or a click) opens the type
  list; typing filters it (by name or extension), `Up/Down` and `Enter`
  (or `Tab`) choose, `Escape` or `Shift+Tab` go back to the name (a second
  `Escape` cancels). Types: Text, Python, Jupyter Notebook, marimo
  Notebook, Markdown, JSON, YAML, TOML, Rust, JavaScript, TypeScript,
  TypeScript React, HTML, CSS, Shell script, PowerShell, C, C++, Go, SQL,
  Dockerfile, Dev Container config, NextCode Workflow, .gitignore. A
  chosen type adds its extension when the name has none of its own
  (`analysis` → `analysis.ipynb`, `tool.py` stays) and writes its starter
  text: an empty notebook with one code cell (Python kernel), a marimo
  app with one cell, the commented workflow template (and its schema
  beside it), a dev container `name` + `image`, a `#!/usr/bin/env bash`
  line; the others start empty. The name may stay empty for the types
  that name their file: Dockerfile (`Dockerfile`, or `<name>.Dockerfile`),
  .gitignore (a typed name is its folder), Dev Container config
  (`.devcontainer/devcontainer.json`, or `.devcontainer/<name>/…`, at the
  workspace folder's root) and NextCode Workflow
  (`.next_code/workflows/<name>.yaml` at the project root; no name:
  `workflow.yaml`, `workflow-2.yaml`, …).
- **Cut/Copy/Paste** (`Ctrl+X/C/V`) paste into the selected folder (or the
  selected file's folder); copies whose name is taken become `name
  copy.ext`, `name copy 2.ext`…; a cut entry moves (refused if the name is
  taken there). `Ctrl+D` duplicates next to the original. The paths are
  also put on the clipboard as text.
- **Drag and drop** rows onto a folder (onto a file: its folder; onto empty
  space: the root) to move them; hold `Ctrl` at the drop to copy; more than
  five items ask first; `Escape` cancels the drag; a folder dropped into
  itself does nothing.
- **Delete** (`Delete` key) asks first, naming the item (or listing up to
  ten) and any unsaved files among them. Local Windows projects move to the
  Recycle Bin; Linux, WSL and dev container projects delete permanently
  (the dialog says so). Editor tabs of deleted files close.
- **Open editors follow** renames and moves (path, title, unsaved edits,
  language server document, breakpoints); the watcher's events for the
  tree's own changes are not treated as external edits (no reload prompt,
  no "deleted on disk" notice, no second tree refresh).
- **Language servers**: before a rename or move, running servers that
  registered `workspace/willRenameFiles` are asked for edits (5 s each),
  which are applied first; `didRenameFiles` follows. rust-analyzer updates
  `mod` lines and paths; basedpyright updates Python imports
  (`from helpers import x` → `from tools import x`). Servers without it are
  skipped; failures and timeouts are reported with the operation's notice.
- Errors (taken names, permissions, agent failures) appear as selectable
  notices above the editor.

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

**Minimap.** File editors show the whole document scaled down in a column
beside the scrollbar (VS Code): one 2 px row per screen row (wrapped rows
and folds follow the text), each run of same-coloured characters a block
in its syntax colour, up to `max_columns` columns. A translucent slider
marks the visible part; a click centres the text on the clicked row and
dragging the slider scrolls. When the document is taller than the column,
the minimap scrolls with the text so their tops and ends meet. Git change
markers (left edge), errors and warnings, selections and find matches show
over it. It is hidden in editors narrower than 500 px, input fields, text
areas and diff tabs.

**Sticky scroll.** The opening lines of the scopes around the first visible
line (the foldable regions above: definitions, blocks, bracketed and
indented regions; a multi-line signature shows its `def`/`fn` line, never
a decorator, comment or docstring) stay pinned at the top of the text, outermost first,
with their line numbers and syntax colours over a subtle shadow; each
pinned header covers the line below it, so nested headers stack up to
`max_lines` (and at most half the view). The innermost one is pushed up as
its scope's last row scrolls under it. Clicking one scrolls its line back
to where it was pinned and puts the cursor there; the cursor never stays
hidden under the pinned lines when it moves. Folded scopes and wrapped
rows are followed.

```json
{ "editor": {
    "minimap": { "enabled": true, "max_columns": 120, "side": "right" },
    "sticky_scroll": { "enabled": true, "max_lines": 5 } } }
```

`"side": "left"` puts the minimap before the gutter. Both apply live and per
workspace/folder like other settings.

## Vim mode

Optional Vim emulation in file editors, like VS Code Vim / IdeaVim: turn it
on with `"vim_mode": true` in `settings.json`, or **Toggle Vim Mode**
(command palette, View > Vim Mode; until the editor exits). The status bar
shows the mode (`NORMAL`, `INSERT`, `VISUAL`, `V-LINE`, `V-BLOCK`,
`REPLACE`), keys typed so far (`2d`) and `recording @q`; the cursor is a
block in normal and visual mode, a half block while an operator waits for
its motion, an underline in replace mode and the usual bar in insert mode.

Keys reach Vim before the IDE's shortcuts, but Vim only takes what it uses:
insert mode leaves everything but Escape to the editor (completion,
snippets, auto-closing, multi-cursor typing work as usual), and in normal
and visual mode Alt and function keys and Ctrl keys Vim has no meaning for
(`Ctrl+S`, `Ctrl+P`, `Ctrl+Shift+…`) stay the IDE's. Escape first closes an
open completion, parameter-hint, code-action or change popup; Escape in
normal mode with nothing pending goes to the IDE (collapse cursors, close
the find bar). Input fields, the terminal and diff tabs never use Vim; the
Commit message does with `"vim": { "in_commit_message": true }`.

| | |
|---|---|
| Modes | `i a I A o O gi gI`, `v V Ctrl+V` (`Ctrl+Q`), `gv`, `R`, Escape / `Ctrl+[` |
| Motions (with counts) | `h j k l` (arrows, Backspace, Space), `w W b B e E ge gE`, `0 ^ $ g_ \|`, `gg G {n}G`, `+ - Enter`, `f F t T ; ,`, `%` (`{n}%`), `{ }`, `H M L`, `Ctrl+D/U/F/B` (PageUp/Down), `gj gk`, `* #`, `/ ? n N`, marks `'a` `` `a `` `''` `'<` `'>` `'.` `'^` |
| Operators | `d c y > < = g~ gu gU` with a motion, a text object or doubled for lines (`dd`, `>>`, `gUU`); counts multiply (`2d3w`) |
| Text objects | `iw aw iW aW`, `i" a" i' a'`, ``i` a` ``, `i( a( ib ab i[ a[ i{ a{ iB aB i< a<`, `it at`, `ip ap` (with counts) |
| Commands | `x X s S D C Y p P`, `u Ctrl+R`, `.`, `J gJ`, `r{c}`, `~`, `Ctrl+A / Ctrl+X` (decimal and `0x` hex), `m{a-z}`, `q{a-z}` … `q`, `@{a-z}`, `@@`, `zz zt zb`, `Ctrl+E / Ctrl+Y`, `zc zo za zM zR` (folds), `gd` (go to definition) |
| Visual | motions and objects extend; `d x X D c s S C R y Y > < = ~ u U J r{c} p P o`; block `I`/`A` type on every line (`$A` appends to each line's end) |
| Registers | unnamed, `"a`-`"z` (`"A` appends), `"0` last yank, `"1`-`"9` deletes of lines, `"-` small deletes, `"_`, `"+`/`"*` system clipboard, `".` `":` `"/` read-only |

**Command line.** `:` opens it in the status bar (with `'<,'>` from visual
mode, `.,.+n` after a count); `/` and `?` search (also as operator motions,
`d/foo`). Up/Down browse the history, `Ctrl+U` clears, `Ctrl+W` deletes a
word, `Ctrl+V` pastes, Backspace on an empty line or Escape cancels.

| Ex command | |
|---|---|
| `:w`, `:wa`, `:q`, `:q!`, `:wq`, `:x` | save (formats on save like `Ctrl+S`), save all, close the tab (asks about unsaved changes; `!` discards), save and close |
| `:e {file}`, `:e`, `:e!` | open a file (relative to the project folder), reload from disk (`!` drops edits) |
| `:{n}`, `:$`, `:.+3` | go to line |
| `:[range]s/pat/rep/[giI]`, `:%s/…` | substitute; `&` and `\1`…`\9` in the replacement, empty pattern = last search |
| `:[range]d [x]`, `:[range]y [x]`, `:[range]>`, `:[range]<`, `:[range]j` | delete, yank, shift, join lines |
| `:noh`, `:set hls` / `:set nohls` | hide / show search highlights |
| `:sp`, `:vs` | split the editor down / right |

Ranges: `%`, `n`, `.`, `$`, `'x` marks, `+n`/`-n` offsets, `a,b`.
Search patterns use Rust regex syntax plus Vim's `\<` `\>`, with smartcase
(case-sensitive only with an uppercase letter; `\c` / `\C` force it); `*` and
`#` match whole words, case-sensitively. Matches of the last search are
highlighted until `:noh` (only the lines on screen are searched for that).

**Undo and repeat.** A command and the insert mode it opens are one undo
step (`ciwfoo<Esc>` undoes with one `u`), and `.` repeats it with the typed
text (`3.` replaces the count); visual-mode commands repeat on the same
amount of text. Insert counts repeat the text (`3ix<Esc>`, `2oline<Esc>`).

**Several cursors.** Normal-mode commands act on the primary cursor and
drop the others, except `i` and `a`, which insert at every cursor. A
selection made outside Vim (mouse drag, `Ctrl+D`, Select All) is visual
mode, and visual operators act on every selection (`Ctrl+D Ctrl+D c` changes
all occurrences).

```json
{ "vim_mode": true,
  "vim": { "ctrl_keys": true, "in_commit_message": false } }
```

`"ctrl_keys": false` gives `Ctrl+D/U/F/B/E/Y/R/A/X/V/Q` (and `Ctrl+[`) back to
the IDE. Not supported: sentence objects (`is`/`as`), `:s` confirm (`c`),
`Ctrl+O`/`Ctrl+I` jumps, `Ctrl+O` in insert mode, uppercase (file) marks,
`:w {file}`, Vim regex syntax beyond `\<` `\>` `\c` `\C`, `:normal`,
key remapping and `.vimrc`.

## Code navigation

### Go to definition and friends

| Command (Go menu) | Keys |
|---|---|
| Go to Definition | `F12`, `Ctrl+B`, `Ctrl+click` (`Cmd` on macOS) |
| Go to Declaration | Go menu / command bar |
| Go to Type Definition (of the variable or expression's type) | `Ctrl+Shift+B` |
| Go to Implementation (of a trait/interface or its method) | `Ctrl+F12`, `Ctrl+Alt+B` |
| Find References | `Shift+F12`, `Alt+F7` |

One result opens directly, several open the locations picker; `Alt+Left`
goes back. On the definition itself, Go to Definition (and Ctrl+click)
finds its usages instead, as in VS Code and JetBrains IDEs. While `Ctrl`
is held, an identifier under the mouse that the language server has a
definition for is underlined and the pointer becomes a hand (asked once
the mouse rests on a word for a moment, remembered per word until the
next edit); a click goes to its definition without starting a selection.
This works in notebook cells, read-only files and remote (WSL, dev
container) projects alike, and in the editor under the mouse even when
another one (another cell or pane) has focus. Alt+click (add a cursor),
Shift+Alt+drag and middle-drag (column selection) are unchanged.
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

## Find and replace in files

| Command | Keys |
|---|---|
| Find in Files (Search panel) | `Ctrl+Shift+F` |
| Replace in Files (Edit menu) | `Ctrl+Shift+R` (PyCharm), `Ctrl+Shift+H` (VS Code) |
| Replace All (in the panel) | `Ctrl+Alt+Enter` |

The Search panel searches the project where it lives (locally, in WSL or in
the dev container) with case (`Aa`), whole-word (`ab`) and regex (`.*`)
toggles and include/exclude globs. Open files with unsaved changes are
searched as the editor shows them, not as saved. **Replace in Files** (or
the `▸` before the query) shows a replacement field: in regex mode `$1`,
`${name}` expand to capture groups and `$$` is a `$` (the find bar's rules);
otherwise the text is literal. With a replacement typed, every result row is
one match showing the old text struck through and the new text next to it.
On hover, a match row offers **Replace** and **×** (exclude it from the
results), a file row **Replace All** (that file) and **×**; **Replace All**
next to the field replaces every listed match after a confirmation (*Replace
N matches in M files?*). In search-only mode a line row's **×** excludes the
line.

Replacing goes through the same path as language-server edits: files open in
an editor change there (one undo step per file, left unsaved); other files
are written through the project. Each file is checked first: if any of its
listed matches is no longer at the same place with the same text (the file
changed since the search), the file is skipped. A line under the status
reports the counts, skipped files and failures (*Replaced 4 matches in 2
files; skipped a.py (changed since the search)*), and the results refresh
(exclusions are kept until the query changes). Limits: lines longer than
~400 bytes are listed around their first match, so matches outside that
window are neither shown nor replaced; at most 10 000 matches are listed;
matches never span lines.

Headless: `replace` (see [Headless mode](#headless-mode)).

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

## Spell checking

Like VS Code's Code Spell Checker and PyCharm's typo inspection, unknown
words get a dotted underline (not the wavy one of diagnostics):

- in code, only comments, docstrings and strings (from the syntax tree;
  identifiers are never checked);
- in Markdown, everything except code blocks, link targets and fence info
  strings; in plain text (`.txt`, `.rst`, `.adoc`, `.org`, git's
  `COMMIT_EDITMSG`…) and the Commit window's message, everything.

Words are split at camelCase humps, `_`, `-` and `.` (`parseHTTPRequest` →
parse, HTTP, Request). Skipped: URLs, e-mail addresses, paths, `@tags`,
`$variables`, `%formats`, `#hex` colors, hashes, text in backticks, words
shorter than 3 letters, all-caps words up to 4 letters (`TODO`, `JSON`),
words with digits (`utf8`, `x86`) and non-Latin scripts. A built-in list of
programming words (`async`, `regex`, `stdout`, `kwargs`, `json`, …) is
accepted.

**Quick fixes.** On an unknown word, `Alt+Enter` / `Ctrl+.` (or the
lightbulb) lists, before any language-server actions: up to 5 corrections,
*Add to user dictionary* (`<config>/next_code/words.txt`), *Add to project
dictionary* (`.next_code/words.txt` in the file's project folder, written
where the project lives), *Ignore in this file* (in this editor tab, until
it closes). Dictionary files have one word per line (`#` comments; any case
matches). Editing them reloads them: the user's file when the editor saves
a file or a window regains focus, the project's through the file watcher.

Only the visible lines and 100 lines around them are checked, on a
background thread, 300 ms after typing stops (scrolling to unchecked lines
checks at once); results are cached per line text. Dictionaries load on
first use, off the UI thread (en_US: ~10 ms in a release build; checking
300 lines of comments takes about 1.5 ms).

Settings (`settings.json`, workspace or folder settings):

```json
{ "spell": {
    "enabled": true,
    "languages": ["en_US"],
    "check_code": true,
    "in_problems": false },
  "languages": { "json": { "spell": false } } }
```

`languages` lists Hunspell dictionaries: `en_US` is built in (SCOWL, see
`crates/spell/dictionaries/README_en_US.txt` for its licence); others are
read from `<config>/next_code/dictionaries/<language>.aff` and `.dic`
(also replacing the built-in one). A word passes if any dictionary knows
it. `check_code: false` limits checking to Markdown, plain text and commit
messages. `languages.<id>.spell` turns checking on or off for one language.
`in_problems: true` also lists the unknown words of open files in the
Problems panel (information level, source `spelling`; the whole file is
checked a second after it settles). The checker is
[spellbook](https://github.com/helix-editor/spellbook) (MPL-2.0).

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

## Pinpoint

Every part of the interface has a stable path (`menu-bar/run`,
`workspace/left-dock/panel:commit/commit-area/commit`,
`workspace/editor-area/pane:1/editor:src/main.rs/gutter/line:12`) that a
person and an agent can both name, and text is addressable down to single
characters (`src/main.rs:12:5-12:18`). An agent asked "how do I …?" can show
the answer in the running window: outlines with a soft glow, numbered badges
and short notes next to buttons, tints on code; the highlights follow their
targets as panels scroll and resize, and go away with `clear`, Escape (when
no popup uses it) or a note's ×.

```sh
next_code pinpoint list workspace/left-dock --depth 2
next_code pinpoint highlight --clear workspace/left-dock/dock-tabs/tab:commit --step 1 \
  --note "Open the Commit panel" src/main.rs:12 --reveal --note "Here"
next_code pinpoint wait-pin --pick
```

`next_code pinpoint` is a client of the editor already running (it never
opens a window): the editor serves JSON lines on a random loopback port with
a token, announced in `<data dir>/next_code/pinpoint/<pid>.json`; the client
picks the editor whose project holds the current directory. Agents inside
WSL or a dev container use `next_code_agent pinpoint …`, and `--automation`
has a `pinpoint` passthrough. The commands, targets and conventions are in
[docs/pinpoint-for-agents.md](docs/pinpoint-for-agents.md).

The other way round, **Pinpoint: Pick Element** (`Ctrl+Alt+Shift+P`) outlines
the element under the pointer with its path; a click copies a reference
(`nextcode://ui/<path>`, or the characters under the pointer in code and
text) and adds it to the pins agents read (`pins`, `wait_pin`); Shift+click
keeps picking, Escape leaves. **Pinpoint: Clear Highlights** removes the
window's highlights.

**What I'm looking at.** `next_code context` (`next_code_agent context` in
WSL or a dev container) answers an agent's "what are you looking at?" without
the person pasting anything: the focused area, the active file with cursor,
selection, the lines around the cursor, its diagnostics and sticky notes, the
open tabs, the terminal's last lines and exit code, Problems, the Tests
panel's failures, the debugger's stack and locals, the notebook cell with its
error, or the workflow run's failed step. Markdown by default, `--format json`
raw; everything capped (`--max-lines`, `--around`). **Pinpoint: Copy Context
for Agent** (command palette) copies the same Markdown for pasting into a
chat. See [docs/pinpoint-for-agents.md](docs/pinpoint-for-agents.md#what-the-user-is-looking-at-context).

## Sticky notes

Notes between the person and agents, pinned to lines of a file like a sticky
note on a page. They live in `<project>/.next_code/notes.json` (local: the
first note adds `/.next_code/notes.json*` to `.git/info/exclude`, never to
`.gitignore`), one per project root, and follow the code: a note found at
its lines is exact; else the nearest copy of its text (lines inserted above),
else the most similar lines with their context (the code was edited), else it
is orphaned and kept in the list.

In the editor a note is a card in the free space right of the code, at its
first line (author, age, `→ agent`/`→ user`, reply count, the start of the
text), with a marker in the gutter beside its lines. Cards stack downward so
they never overlap; where the code leaves no room (a long line, a narrow
pane) a card becomes a small `✎` icon at the right edge, and `–` collapses
one by hand. Clicking a card (or its gutter marker) opens the thread: every
message, a reply box (`Ctrl+Enter` sends, `Escape` closes), **Resolve** /
**Reopen**, **Delete**, **Edit** on your messages and the `→` button that
changes who the note is for. **Add Note** (`Ctrl+K Ctrl+N`, the text's or
the gutter's right-click menu, Edit menu, palette) notes the selected lines
or the cursor's line, addressed to the agent by default. Resolved notes are
hidden; **Show/Hide Resolved Notes** (palette, Notes panel) shows them
faded. As you type, notes move with their lines (edits above shift them,
edits inside keep them, deleting their lines orphans them); the new anchors
are written when you save, and discarding the edits puts the notes back.

The **Notes** panel (View menu, bottom dock) lists every note of the project
by file with filters (Open / Resolved / All, Anyone / To me / To agents),
its current line, and "lines deleted" for orphaned ones; a click opens the
file with the thread. Its tab shows how many open notes wait for you
(addressed to you, or to anyone by an agent). When an agent adds a note for
you or replies while NextCode is open, a banner quotes it with **Show
Note**. Notes reach pinpoint as `…/editor:<file>/note:<id>` (the card, or the
open thread with `reply`, `send`, `resolve`, `reopen`, `delete`, `to`,
`close`, `message:<n>/edit`), `…/editor:<file>/new-note/{text,to,add,cancel}`
and `…/panel:notes/note:<id>`, so an agent can point at one. The editor reads
and writes the notes file through the project, so WSL and dev-container
projects work the same. Notes on Jupyter notebook cells (`cell`) show as `✎`
icons in their cells; the panel opens the notebook scrolled to the cell with
the thread open.

Agents use the command line; it edits the file directly, so NextCode need not
be running (the editor watches the file):

```sh
next_code notes list                                 # open notes, current lines, threads
next_code notes add src/main.rs:12-14 "Why clone here?" --as claude
next_code notes reply n7f3a "Fixed, have a look"
next_code notes resolve n7f3a
next_code notes wait --timeout 600                   # until the person writes
```

Inside WSL or a dev container the same commands are
`~/.local/share/next_code/next_code_agent notes …`. Writers (the editor, the
Windows CLI, agents through `/mnt` or a bind mount) take a lock file,
re-read, apply their change and rename a temporary file over `notes.json`,
so concurrent replies are never lost. Commands, the file format and the
`wait` loop are in [docs/notes-for-agents.md](docs/notes-for-agents.md).

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

`{"cmd":"pinpoint","request":{…}}` forwards a [Pinpoint](#pinpoint) request
(`list`, `find`, `highlight`, `clear`, `pick`, `pins`, `wait_pin`, `windows`,
`context`)
to the editor and returns its JSON reply as `result`: the paths and bounds of
what is on screen (aim clicks with them) and highlights to check on
screenshots.

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
`create_file` (`type`? creates a file as New File… does for that type),
`create_dir`, `rename_path` (with the language servers'
`willRenameFiles` edits), `copy_path`, `delete_path`,
`search`, `replace` (`pattern`, `replacement`, the search options, optional
`paths`; per-file counts, skipped and failed files), `watch`/`unwatch`,
`open`/`close` (language server documents),
`diagnostics`, `definition`/`declaration`/`type_definition`/`implementation`,
`signature_help` (with the highlighted parameter), `spell_check` (`path`
and/or `text`: unknown words with corrections), `inlay_hints` (a line
range), `incoming_calls`/`outgoing_calls` (call
hierarchy at a position), `document_symbols`, `workspace_symbols`,
`semantic_tokens` (decoded as the editor draws
them), `rename`, `code_actions` / `apply_code_action` (edits are written to
disk and open documents synced; servers' `workspace/applyEdit` requests are
applied too and reported as `applied_edit` events), `format`, `exec` (shell
command in the project root, where
the project lives), `git_status`, `git_diff`, `git_apply_hunk`,
`git_line_changes`, `git_stage_line_change`, `git_blame`, `git_stage`,
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
`git_patch`, `git_stash_list`, `git_stash_push`, `git_stash_apply`,
`git_stash_drop`, `git_stash_changes`, `git_shelve`, `git_unshelve`,
`git_shelf_list`, `git_shelf_diff`, `git_shelf_delete`, `python_info`,
`python_sync`,
`python_lock`, `python_add`, `python_remove`, `python_pin`,
`python_interpreters`, `python_test_at`, `devcontainer_configs`,
`run_configurations`, `debug_request`, `tests_discover`, `tests_run`,
`debug_start` (a debug request), `debug_set_breakpoints`, `debug_continue`,
`debug_step`, `debug_pause`, `debug_stack`, `debug_select_frame`,
`debug_variables`, `debug_evaluate`, `debug_output`, `debug_stop`,
`debug_download_codelldb` (with `debug_event` events: `stopped`, `running`,
`output`, `breakpoints`, `terminated`, `adapter_missing`), `ports_list`,
`ports_forward`, `ports_stop` (with `port_forwarded`, `port_released`,
`port_available` events), `notebook_open` (`python`?/`kernel_name`? pick
the kernel), `notebook_run` (`cells`? indices; waits, returns the outputs of
every cell that ran, marimo dependents included), `notebook_edit`
(`source`, `outputs_hidden`, `source_hidden`: any of them),
`notebook_insert`, `notebook_delete`, `notebook_interrupt`,
`notebook_restart`, `notebook_save`, `notebook_reload` (re-reads the file
when it changed: `{reloaded, cells}`), `notebook_set_ui` (marimo: `cell`,
`element` id from an output's `marimo_ui` parts, `value`; waits like
`notebook_run`), `notebook_close`, `notebook_status`,
`shutdown`;
`--devcontainer` opens the folder's container (with `progress` events).
`--workspace FILE` (or a `*.next_code-workspace` / `*.code-workspace` path
as the project) opens a [multi-root workspace](#multi-root-workspaces):
`info` adds `roots` (`[{index, name, path}]`) and `workspace_file`; every
command takes an optional `root` (index or name) that relative paths are
resolved against (absolute paths work anywhere; an unknown root is an
error); `list_files` without `root` returns `[{root, path}]` for several
folders (the plain list for one); `search`/`replace` results carry `root`
and `relative`; git commands use the `root` folder's repository (default
the first; an absolute path selects its folder's), and `git_status` with
`all: true` returns `[{root, repository_root, status}]`.
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

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state
otherwise, any contribution intentionally submitted for inclusion in this
project shall be dual licensed as above, without any additional terms or
conditions.

`vendor/gpui` is gpui by Zed Industries, under the Apache License 2.0
(`vendor/gpui/LICENSE-APACHE`). The bundled `en_US` dictionary is under
SCOWL's licence (`crates/spell/dictionaries/README_en_US.txt`).
