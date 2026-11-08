# Extensions (Rust to WebAssembly)

NextCode loads local, import-free WASM extension packages. The versioned
interface is UTF-8 JSON in WASM linear memory, not Rust's native ABI: other
languages can implement the same interface without changing the editor.
The supplied Rust SDK supports commands, document edits and native declarative
UI panels. API 3 adds declared project programs and refresh timers; API 4
adds dashboard layout (grids, sidebars, tabs, sections, stat tiles), tables
with sized columns and clickable or checkable rows, and richer charts. API 2
and 3 packages remain supported with their existing permissions and
envelope. New SDK builds and the examples use `api_version: 4`;
replace/reload or reinstall a package after rebuilding it.

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
and choose **Open Generated Data Dashboard: Generated Dashboard**. It uses
every widget: tabs, a sidebar of controls beside a growing column, stat
tiles and charts in grids, a table whose ticked rows choose the charted
channels (its swatches are their colors), synced line charts in each series
style, a log axis picked from a chart's title row, and a section that folds
away. The data is explicitly synthetic, not a live feed. Sample counts must
be integers from 8 to 256; invalid values report an error without replacing
the current panel. **Generated Report** opens an independent instance in an
editor tab. No document permissions are needed.

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

Widget `type`s: `row`, `column`, `grid`, `section`, `scroll`, `space`,
`label`, `markdown`, `stat`, `button`, `text-input`, `checkbox`, `select`,
`tabs`, `progress`, `table`, `chart`. Every widget has a unique `id` within
its panel. The complete field types are in `crates/extension_api/src/lib.rs`;
the dashboard is a working Rust authoring example using every widget and
chart kind. Markdown is resource-free formatted text: image/HTML/link
destinations cannot load files or network resources.

Optional fields can be left out of the JSON: `gap` (8), `disabled` (false),
`label`/`placeholder`/`value` of fields (empty), a chart's `title` (empty),
`height` (220) and `legend` (true), axis labels (empty). Rust authors build
widgets with constructors and setters instead of struct literals, so new
optional fields never break them:

```rust
Widget::row("work", [
    Widget::column("runs", [runs_table]).width(320),
    Widget::column("charts", [Widget::grid("curves", 360, charts)]).grow(),
])
Widget::chart("loss", "val/loss", ChartSpec::line([
    ChartSeries::new("r12", points).color(2).style(SeriesStyle::Step),
]).scales(Scale::Linear, Scale::Log)).legend(false).sync("curves")
```

### Layout (API 4)

- `column` takes `"width"` (120–1600 px) or `"grow": true`. A row holding a
  growing column is a sidebar layout: it does not wrap, its children align
  at the top, and the growing column takes the width left, which its charts
  and grids then fill. When such a row of columns is the panel's root, it
  fills the panel's height and each column scrolls on its own, so a
  sidebar stays in place beside long content.
- `row` takes `"align"`: `start`, `center` or `end` (the default without a
  growing column, so labelled fields line up with buttons).
- `grid` (`min_width` 120–1600 px, default 320; `gap`) lays its children
  out in as many equal columns as fit, at least `min_width` each.
- `section` (`title`, `open`, `children`) is a header that folds its
  children away. Clicking it shows or hides them at once and sends
  `change` with the new `open` (a boolean input); an extension may also
  leave out the children of a closed section.
- `tabs` (`value`, `options`) is a tab strip. Picking a tab sends `change`
  with its value, as a select does; the extension draws the page below.
- `stat` (`label`, `value`, optional `detail` and palette `color`) is a
  dashboard tile with a headline number.

### Tables (API 4)

```json
{"type":"table","id":"runs","columns":["run","score","hypothesis"],"rows":[...],
 "widths":[70,90,0],"selectable":true,"selected":2,"max_height":600,
 "check":true,"checked":[0,1],"disabled":[5],"swatches":[0,1,null],
 "marks":[[[], [{"icon":"check","color":5,"label":"Ada approved"}]]]}
```

- `widths`: px per column, `0` shares what is left. Without it, columns
  share the width in proportion to their content.
- Cells are one line, cut with `…`; hovering a row with long cells shows it
  whole. Columns of numbers are aligned right.
- `selectable`: clicking a row (or Enter on it) sends
  `{"kind":"select-row","index":2}`; `selected` highlights one.
- `check`: a checkbox starts every row. Ticking one sends
  `{"kind":"check-row","index":3,"value":true}` and shows at once.
  `checked` lists ticked rows, `disabled` rows that can't be ticked, and
  `swatches` a palette index (or `null`) per row, drawn next to the box, so
  the table can be the charts' legend. Swatches also work without `check`,
  as a status dot at the start of each row.
- `marks`: small icons before a cell's text, one list per cell, by row
  then column (shorter rows and cells have none). Each has an `icon`
  (`check`, `cross`, `clock`, `circle`, `dot`, `minus`, `warning`,
  `comment` or `person`), an optional palette `color` (muted without one)
  and a `label`, shown on hover and read with the row. Up to 12 a cell and
  4,096 a table. A column with marks keeps its text on the left, even when
  it is numbers. Use them for states that read at a glance, such as one tick
  per approval.
- `max_height`: 120–1200 px (default 320); the header stays in place.
- A focused table moves its row with Up/Down.

### Charts

Charts lay out their own axes: ticks fall on round values (1, 2 or 5 ×
10ⁿ) with their gridlines, written short (`0 200k 400k`, `1.2M`, `1e-6`);
the y axis rounds out to its outer ticks; empty axis labels take no room.
Hovering a line chart shows a crosshair at the nearest x and a tooltip with
every series' value there; scatter charts show the nearest dot, and bar
charts a category's bars. Drag across a plot to zoom to that range,
double-click to reset; `+`/`−`/`0` work on a focused chart, and the small
zoom buttons appear while it is hovered or zoomed. Clicking a point or bar
selects it and sends its original series and datum index (histograms use
bin indices).

- Series take `"color"`: 0–11. The first eight are a categorical palette
  chosen to stay distinguishable with color-vision deficiencies, in light
  and dark themes. Omitted colors use the series index.
- Line and scatter series take `"style"`: `line`, `step` (holds a value
  until the next point), `dashed`, `points` or `area`. The default is
  `line` in a line chart and `points` in a scatter chart. A line marks only
  the points it can't draw (a series of one point) and the hovered one.
- Points take `"label"`, shown on hover instead of the series name.
- Line and scatter charts take `"x_scale"`/`"y_scale"`: `linear` or `log`
  (positive values only), and `"x_format"`/`"y_format"`: `number`, `date`
  (Unix seconds, shown in UTC), `duration` (seconds: `45s`, `12m`, `3h20`)
  or `percent` (a fraction shown as `83%`). Bar charts take `y_format`.
- `"legend": false` hides the legend; with one series there is none.
- `"width"`: 180–1600 px; omitted, a chart fills the width available to it.
  `"height"` (120–600 px) is the whole chart, title and legend included.
- `"controls"`: up to four small selects, buttons or checkboxes drawn in
  the chart's title row (a y-axis picker); they are panel widgets with the
  usual inputs and events.
- `"sync": "<group>"`: charts of one group share their hover line and zoom.

```json
{"type":"chart","id":"loss","title":"val/loss","height":180,"legend":false,"sync":"curves",
 "controls":[{"type":"select","id":"y","value":"val/loss","options":[{"value":"val/loss","label":"val/loss"}]}],
 "chart":{"kind":"line","x_label":"step","y_scale":"log",
          "series":[{"name":"r0958","color":2,"style":"step","points":[{"x":1,"y":0.5,"label":"r0958 · dart"}]}]}}
```

Declared programs run with `NEXT_CODE_VERSION` (the editor's version) and
`NEXT_CODE_EXTENSION_API` (the newest API version the host accepts, `4`) in
their environment, on every kind of host, so a tool that draws panels can
use newer fields only where they are accepted (older hosts refuse unknown
fields).

Editor panels omit the extra manifest-title row because the editor tab
already names the panel. Moving a panel to the dock restores its header.
Work longer than a moment shows a small **Updating…** badge over the
panel's top-right corner; it never moves the panel's contents.

Dock panels live in the movable/resizable **Extensions** dock, with
internal tabs for each view. **Move to Editor** and **Move to Dock** move
the same view without losing its state. Dock placement/size is remembered;
panel contents are session-only and never restored as file paths or
automatically executed on restart. Closing a panel discards its state.
Opening an already open panel reveals it without reinitialising it.
Explicit Reload packages closes views; disabling an extension closes only
its views. Unrelated installations and opening the manager preserve them.

Text edits stay local until Enter or a button action. Checkbox, dropdown,
tab and section changes send events immediately. Each event carries all
current form values and the last accepted `state`; return a new root/state
to update the view. Stable widget IDs preserve focus, scroll positions,
chart zoom and text typed while an earlier response was pending. While the
extension or its processes are running, controls stay usable: checkboxes,
selects, tabs, sections and table ticks change at once, and the latest
action is sent when the panel is idle (dropped if the new panel no longer
has that widget). Closed/replaced/reloaded views reject stale replies,
including any document edits returned with them.

Tab/Shift+Tab navigate controls (skipping those in closed sections); Enter
activates buttons, submits text and selects a table row, Space toggles
checkboxes, sections and table ticks, and arrows select options and tabs.
Zoom sends a `chart-zoom` event. Ordinary modified editor/application
shortcuts remain available.

A command or opening response can initialise several declared panels.
A UI-event response may update only its originating panel. All response
models are validated before applying them; returning UI/edits with an
error, duplicate widget IDs or undeclared panels is invalid.

## Language-neutral ABI, versions 2 to 4

The manifest's exact fields are `id`, `name`, `version`, `api_version` (2, 3 or 4),
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
(`series`, `index`), `chart-zoom` (`start`, `end`), `select-row` (`index`)
and `check-row` (`index`, `value`). Inputs are strings or
booleans keyed by current control IDs; the host validates type, membership,
enabled target and chart bounds against its current model. Widget event
`kind` is an object containing the tagged event payload, as shown above.

All selection/edit offsets are **UTF-8 bytes**, ordered and on character
boundaries. Edits refer to the original request snapshot, cannot overlap
or contain duplicate insertions, and are rejected together if invalid.
Returning both edits and an error is invalid. Live version/path checks
run immediately before applying the batch.

Parsing/compilation and invocation run off the UI thread. Each invocation
has 500 million fuel units (about a fifth of a second of work; a 40 KiB
panel built and encoded in Rust takes about 10 million) (including start/allocation), at most 64 MiB
linear memory and one 16,384-element table. Limits: 64 KiB manifest,
16 MiB WASM module, 8 MiB editor document, 1 MiB settings, 16 MiB encoded
request, 1 MiB encoded response, 4,096 edits and 1,024 log entries.
UI limits (the same for every API version, in
`extension_api::limits`): 8 declared panels per extension, 256
widgets/depth 12 per panel, 256 KiB aggregate panel text (point labels
included), 4 KiB per text-input value, 64 KiB encoded panel state, 32
select or tab options, 16 table columns/4,096 cells, 12 series per chart,
4 chart controls and 16,384 total chart data points/values per panel.
Histogram bins: 1–64. Numeric chart values must be finite and within
±1e12; progress is finite in [0, 1]. Layout gaps/spaces are 0–48 px;
scroll heights 32–1200 px, chart heights 120–600 px. The encoded response limit includes all panels.
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
is drained but retained only up to 256 KiB (stdout of a
`"response":"stdout"` process: 1 MiB, the response limit), before UTF-8
decoding and JSON escaping; excess output sets `truncated`. Invalid UTF-8 is replaced.

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
