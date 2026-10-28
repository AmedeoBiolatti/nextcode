# Debugging

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
current Python file (**Run > Debugging > Debug Current File**). One session runs at a
time: starting another (or Restart) stops the running one first.

**Breakpoints.** Click the gutter left of the line numbers, or press `F9` /
`Ctrl+F8` on the cursor line. Right-click the gutter (or **Run > Breakpoints > Edit
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
`items` after a dot). The **Debug** panel (right dock, View > Panels > Debug) has a
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
| Edit Breakpoint…, Enable/Disable All, Remove All | Run > Breakpoints, palette |

Limits: no `runInTerminal` (program input from a terminal) and no child
sessions (`startDebugging`, e.g. debugpy's multiprocess); both are declined
with a note in the console. No data or function breakpoints, no
disassembly.
