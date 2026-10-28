# Extensions (Rust to WebAssembly)

NextCode loads local, import-free WASM extension packages. The versioned
interface is UTF-8 JSON in WASM linear memory, not Rust's native ABI: other
languages can implement the same interface without changing the editor.
The supplied Rust SDK supports commands, document edits and native declarative
UI panels. API 3 adds declared project programs and refresh timers. API 2
packages remain supported with their existing permissions and envelope. New
SDK builds and the examples use `api_version: 3`; replace/reload or reinstall
a package after rebuilding it.

## Install and run

1. Open **File > Preferences > Manage Extensions…**, or search for **Manage Extensions**
   in the command palette (`Ctrl+Shift+P`).
2. Enter a local package folder and choose **Install path**, or **Browse…**
   to select it. A package contains `extension.json` and its `.wasm` file.
3. Installation validates and copies those files into
   `<config>/next_code/extensions/<id>/`. It does not execute guest code;
   new installations are **disabled**.
4. Choose **Enable…** and approve the displayed document permissions.
   Enabled commands and **Open …** panel entries appear in the palette under
   **Extensions**. The manager also has an Open entry for each panel.

The manager also provides Disable, per-extension **Settings**, **Reload
packages**, cancellation, and an **Extension output** channel. Click an
extension's name to filter its output; **Extension output (all)** restores
all channels. Output is session-only and bounded to 1,000 entries / 4 MiB.
Reload rereads packages and persisted grants and closes existing panels.
Additional permissions or declared programs require approval again. Installed folders are never overwritten.
For development, replace your installed WASM file and use Reload packages;
remove an installed folder manually before reinstalling its package.
Package files and directory components must not be symlinks.

Settings opens `<config>/next_code/extension-settings/<id>.json`, a JSON
object passed to that extension on each invocation. Only explicitly entered
extension settings are shared, not the editor's environment or project
configuration. Enabled IDs and their approved capabilities are stored in
`<config>/next_code/extension-state.json`. Invalid grant files fail closed.

## Build the Rust examples

From the repository root:

```sh
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown --manifest-path examples/extensions/uppercase/Cargo.toml
cargo build --release --target wasm32-unknown-unknown --manifest-path examples/extensions/dashboard/Cargo.toml
```

Copy each emitted `.wasm` beside its example's `extension.json`.
With the default Cargo target directory, in PowerShell:

```powershell
Copy-Item examples/extensions/uppercase/target/wasm32-unknown-unknown/release/uppercase.wasm examples/extensions/uppercase/uppercase.wasm
Copy-Item examples/extensions/dashboard/target/wasm32-unknown-unknown/release/dashboard.wasm examples/extensions/dashboard/dashboard.wasm
```

Install `examples/extensions/uppercase`, enable it, open a text document,
select text and run **Uppercase Selection** from the palette. The Unicode
uppercase transformation is one undo step and does not save the file.
An empty selection reports an error in the notice and extension output.

Install `examples/extensions/dashboard` and enable it, then open a project
and choose **Open Generated Data Dashboard: Generated Dashboard**. Its
native input, Recompute button, checkbox and dropdown calculate a generated
signal, summary table, progress indicator and line/scatter/bar/histogram
charts. The data is explicitly synthetic, not a live feed. Sample counts
must be integers from 8 to 256; invalid values report an error without
replacing the current panel. **Generated Report** opens an independent
instance in an editor tab. No document permissions are needed.

To write another extension, start from that example: an isolated Cargo
workspace, a `cdylib` targeting `wasm32-unknown-unknown`, a dependency on
`crates/extension_sdk`, and `export_extension!(handler)` where
`handler: fn(Request) -> Response`. Match `request.invocation` to handle a
command, panel opening or UI event. Change the package ID, commands and
panels in `extension.json`; versions use semantic versioning. Permissions:

- `read-document`: receive the active ordinary text document, its path and
  primary selection. Without it, `document` is `null`.
- `edit-document`: return edits against that snapshot; requires
  `read-document`. All edits validate before any apply.
- `run-process` (API 3): run the executables listed in `processes`, with
  arguments supplied by the extension, on the project’s machine. Programs
  have that account’s file, network and environment access.

Notebook cells, diff views and input fields are not extension document
targets. WSL/container documents use their existing editor buffers; the
extension cannot bypass the project host to read or write local files.
Results target the original document, not the tab focused on completion.
Changed/renamed/closed documents and closed projects reject pending edits.
Undo/redo, dirty state, linked views and language-server updates use the
normal editor transaction path. Cancelling or disabling a running
extension discards its pending edits; the bounded guest invocation may
finish before the running slot is released.

## Native declarative panels

An extension declares `panels` in its manifest, each with `id`, `title` and
`placement` (`dock` or `editor`). Opening a panel calls WASM with an
`open-panel` invocation; the response contains a `PanelUpdate` with its
declared `id`, a widget-tree `root` and opaque JSON `state`. NextCode draws
the UI; extensions never receive native widget handles or GPUI pointers.

Supported widget `type`s: `row`, `column`, `scroll`, `space`, `label`,
`markdown`, `button`, `text-input`, `checkbox`, `select`, `progress`,
`table`, `chart`. Every widget has a unique `id` within its panel. The
complete field types are in `crates/extension_api/src/lib.rs`; the dashboard
is a working Rust authoring example using every widget and chart kind.
Markdown is resource-free formatted text: image/HTML/link destinations
cannot load files or network resources.

Compact dashboard controls use optional API 3 fields:

- Charts accept `"legend": false` to hide the legend; omitted means `true`.
  Keyboard zoom and selection still work; the repeated keyboard hint is
  no longer printed under each chart.
- Line, scatter and bar series accept `"color": 0` through `11`. Omitted
  colors use the series index. Checkbox widgets accept the same palette
  index and draw a swatch between the box and label, so a run list can act
  as a shared legend without placeholder series.
- Text inputs and selects with `"label": ""` draw only the field. With a
  nonempty label, `"label_inline": true` places `Label: value` inside the
  field instead of reserving a separate label row. The default remains
  a label above the field.
- Editor panels omit the extra manifest-title row because the editor tab
  already names the panel. Moving a panel to the dock restores its header;
  loading, running and error indicators remain available.

```json
{"type":"chart","id":"loss","title":"val/loss","height":140,"legend":false,
 "chart":{"kind":"line","x_label":"step","y_label":"loss",
          "series":[{"name":"r0958","color":2,"points":[{"x":1,"y":0.5}]}]}}
```

These additions do not change event indices, shortcuts or existing API 3
JSON defaults. Rust SDK users set the new optional fields explicitly when
constructing widgets and series.

Dock panels live in the movable/resizable **Extensions** dock, with
internal tabs for each view. **Move to Editor** and **Move to Dock** move
the same view without losing its state. Dock placement/size is remembered;
panel contents are session-only and never restored as file paths or
automatically executed on restart. Closing a panel discards its state.
Opening an already open panel reveals it without reinitialising it.
Explicit Reload packages closes views; disabling an extension closes only
its views. Unrelated installations and opening the manager preserve them.

Text edits stay local until Enter or a button action. Checkbox/dropdown
changes send events immediately. Each event carries all current form
values and the last accepted `state`; return a new root/state to update the
view. Stable widget IDs preserve focus and text typed while an earlier
response was pending. Actions are disabled while WASM is running, but text
can still be edited. Closed/replaced/reloaded views reject stale replies,
including any document edits returned with them.

Tab/Shift+Tab navigate controls; Enter activates buttons/submits text,
Space toggles checkboxes, and arrows select options. Focus a chart and use
`+`/`−` to zoom and `0` to reset, or use its native buttons. Hover shows
values; clicking a point/bar selects it and sends its original series and
datum index (histograms use bin indices). Zoom also sends an event.
Ordinary modified editor/application shortcuts remain available.

A command or opening response can initialise several declared panels.
A UI-event response may update only its originating panel. All response
models are validated before applying them; returning UI/edits with an
error, duplicate widget IDs or undeclared panels is invalid.

## Language-neutral ABI, versions 2 and 3

The manifest's exact fields are `id`, `name`, `version`, `api_version` (2 or 3),
`module` (one plain `.wasm` filename), `commands` (`id`, `title`), `panels`
(`id`, `title`, `placement`) and `permissions`. At least one command or
panel is required. Unknown fields/capabilities, duplicate contributions and
unsafe IDs/filenames are errors. See the example manifests.

The core wasm32 module exports:

- `memory`: ordinary unshared linear memory, with 64 KiB pages.
- `nextcode_alloc(len: i32) -> i32`: guest-owned input allocation.
- `nextcode_run(ptr: i32, len: i32) -> i64`: handle UTF-8 JSON and return
  `(output_length << 32) | output_pointer`, both unsigned 32-bit values.

The host checks all pointer/length ranges and copies the output before
discarding the instance. Each invocation gets a fresh instance, including its
start function; load/install never runs start. There are **no imports**,
WASI, filesystem, network, processes, environment access, native DLLs,
embedded browser or arbitrary native widget code. Extended constant
expressions, memory64, custom page sizes and multiple memories are not
supported.

Request:

```json
{
  "api_version": 3,
  "invocation": {"kind": "command", "id": "uppercase-selection"},
  "document": {
    "version": 7,
    "path": "/project/example.txt",
    "text": "hello",
    "selection": {"start": 0, "end": 5}
  },
  "settings": {}
}
```

Response (fields default to empty lists / `null`):

```json
{
  "edits": [{"start": 0, "end": 5, "text": "HELLO"}],
  "panels": [],
  "logs": ["Uppercased the selection"],
  "error": null
}
```

Opening uses `"invocation": {"kind": "open-panel", "id": "dashboard"}`.
A minimal UI response (the panel must be declared):

```json
{
  "panels": [{
    "id": "dashboard",
    "root": {"type": "label", "id": "result", "text": "Ready"},
    "state": {"interactions": 0}
  }]
}
```

A dashboard button event's invocation, inside the normal request envelope:

```json
{
  "kind": "ui-event",
  "panel": "dashboard",
  "revision": 3,
  "state": {"interactions": 2},
  "inputs": {"samples": "96", "smooth": true, "scale": "2"},
  "event": {"widget": "generate", "kind": {"kind": "click"}}
}
```

Event kinds: `click`, `submit`, `change` (with `value`), `chart-select`
(`series`, `index`) and `chart-zoom` (`start`, `end`). Inputs are strings or
booleans keyed by current control IDs; the host validates type, membership,
enabled target and chart bounds against its current model. Widget event
`kind` is an object containing the tagged event payload, as shown above.

All selection/edit offsets are **UTF-8 bytes**, ordered and on character
boundaries. Edits refer to the original request snapshot, cannot overlap
or contain duplicate insertions, and are rejected together if invalid.
Returning both edits and an error is invalid. Live version/path checks
run immediately before applying the batch.

Parsing/compilation and invocation run off the UI thread. Each invocation
has 10 million fuel units (including start/allocation), at most 64 MiB
linear memory and one 16,384-element table. Limits: 64 KiB manifest,
16 MiB WASM module, 8 MiB editor document, 1 MiB settings, 16 MiB encoded
request, 1 MiB encoded response, 4,096 edits and 1,024 log entries.
UI limits: 8 declared panels per extension, 128 widgets/depth 8 per panel,
64 KiB aggregate panel text, 4 KiB per text-input value, 16 KiB encoded
panel state, 32 select options, 16 table columns/1,024 cells, 6 series per
chart and 4,096 total chart data points/values per panel. Histogram bins:
1–64. Numeric chart values must be finite and within ±1e12; progress is
finite in [0, 1]. Layout gaps/spaces are 0–48 px; scroll heights 32–600 px,
chart heights 120–500 px. The encoded response limit includes all panels.
JSON escaping counts toward encoded limits. Failures are shown in output
and command errors also appear in the editor notice.

## Project programs and refresh timers (API 3)

A manifest declares the executable names its panel may run:

```json
{"permissions":["run-process"],"processes":[{"program":"kx"}]}
```

Enable displays these names; grants record both permissions and programs.
Adding another program needs approval again. Names are bare executables,
without paths or shell syntax. This limits which program starts; it does
not sandbox that program or restrict its subcommands.

A response to `open-panel`, `ui-event`, `refresh` or `process-exit` may
start programs bound to the originating panel:

```json
{"processes":[{"id":"status","program":"kx","args":["nextcode","panel"],"timeout_ms":30000}]}
```

The host executes argv directly, without a shell or globbing, in the
project's first root, on its local, WSL, container or SSH host. Stdin is
closed; the process inherits the host environment. Programs must be
installed where the project lives. Four processes may run per extension,
and a panel may have at most four active or queued results. Process IDs
must be unique among that panel's outstanding jobs. Timeout defaults to
30 seconds and must be between 1 ms and 120 seconds. Each output stream
is drained but retained only up to 256 KiB, before UTF-8 decoding and JSON
escaping; excess output sets `truncated`. Invalid UTF-8 is replaced.

Completion invokes the extension with the current panel revision, state
and form inputs, plus:

```json
{"kind":"process-exit","panel":"overview","revision":4,"state":{},"inputs":{},"process":{"id":"status","exit_code":0,"stdout":"...","stderr":"","truncated":false,"error":null}}
```

Start errors, timeout and cancellation use a null exit code and an error
message. Processes and descendants are ended on cancellation, panel or
project closure, package reload and disable. Replacement by an unrelated
panel response cancels outstanding jobs and discards their replies.
Sibling completions are delivered serially, using the updated revision
after each reply. Moving a panel between dock and editor preserves jobs.
Process waits do not occupy the application's WASM invocation slot.
Cancelling in Manage Extensions discards cancelled jobs and stops their
timer until another panel update or a user action.

`"response":"stdout"` on a process skips WASM on exit zero with complete
output. Stdout is parsed and validated with the same manifest, permissions,
panel scope and live-origin checks as a WASM reply. Errors still invoke
`process-exit`. This is useful for tools that produce an entire native
panel response and avoids reparsing nested JSON inside WASM's fuel budget.

A panel update can include `"refresh_ms":10000` (1 second to 24 hours).
It schedules one `refresh` invocation with `panel`, `revision`, `state`
and `inputs`, with no `process` or `event`. The timer waits for that
panel's invocation, active processes and queued completions; missed time
produces one refresh, never a backlog. The next update must set the timer
again to keep polling. Updating without `refresh_ms` cancels it. An empty
response leaves the displayed panel intact. An opening response may start
processes without a panel update; the host shows a loading placeholder.
