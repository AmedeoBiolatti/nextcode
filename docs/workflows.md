# Workflows

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

[docs/workflows-for-agents.md](workflows-for-agents.md) is the full
reference for writing and operating workflows (also for coding agents):
every field and shape, how commands run, failure rules, design recipes,
the validation loop, and the headless protocol with real transcripts.
