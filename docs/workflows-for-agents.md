# Workflows: a manual for agents

A **workflow** is one YAML file that brings up a multi-project development
environment: preconditions, commands in terminals of several projects
(local folders, WSL, dev containers), ordering with readiness conditions,
browser links and manual gates — and pipelines such as "evaluate, then
always report", with the files they produce linked. next_code runs it in
the GUI (a workflow tab with a live graph) or headless (JSON lines on
stdin/stdout).

The format borrows what other tools made familiar: steps keyed by id with
`depends_on` and its conditions, Compose-style durations, `x-` keys with
YAML anchors (Docker Compose, process-compose), `inputs` (GitHub Actions'
`workflow_dispatch`), and `${…}` variables (VS Code tasks). JSON is YAML,
so a JSON object in this shape works too.

This manual is for agents that **write** workflow files and **operate**
them. Everything here matches the parser (`crates/workflow/src/file.rs`,
YAML read by `crates/workflow/src/yaml.rs`), the scheduler
(`crates/workflow/src/run.rs`) and the headless commands
(`crates/ui/src/headless/workflows.rs`); the transcripts are real.

---

## 1. Quick start

```yaml
# .next_code/workflows/dev.yaml
name: Dev stack
steps:
  vpn:
    check: test -d /tmp
    on_fail: Connect the VPN, then Retry
  api:
    depends_on: [vpn]
    project: {devcontainer: /home/me/src/api}
    run: uv run uvicorn app:app --port 8000
    ready: {port: 8000}
    timeout: 10m
  login:
    depends_on: [api]
    ready: {manual: Log in in the browser and press Continue}
  seed:
    depends_on: [login]
    project: {local: /home/me/src/api}
    run: uv run python seed.py
```

Validate it, then run it:

```json
{"id": 1, "cmd": "workflows_list"}
{"id": 2, "cmd": "workflow_run", "name": "Dev stack"}
```

With `inputs` (§3.5) pass values: `{"cmd": "workflow_run", "name": "Deploy",
"inputs": {"env": "prod", "workers": 8}}`.

---

## 2. Where files live

| Location | Scope |
|---|---|
| `<config>/next_code/workflows/*.yaml` or `*.yml` (Windows: `%APPDATA%\next_code\workflows\`; Linux: `~/.config/next_code/workflows/`) | Personal; use for stacks spanning several repos |
| `<project>/.next_code/workflows/*.yaml` of every **open** project | Shared with the repo; found only while that project is open |
| Each folder of `"workflows": {"folders": […]}` in `settings.json` (absolute or `~/…`; GUI: Run > Add Workflow Folder…) and of `NEXT_CODE_WORKFLOW_FOLDERS` (separated as in `PATH`) | Team or shared folders on this machine; `source` is the folder |
| Any file: Run > Open Workflow File…, or headless `"path": "<absolute path>"` | Listed for the rest of the session |

Files are re-read before every headless workflow command and before every
Run All, so edit freely between runs.

Files of the **old JSON format** (`*.json`/`*.jsonc` with a `steps` array
of objects with `id`, `needs`/`after`, `onFail`, `timeout_s`, …) are listed
with an error and cannot run. Convert them: `workflow_convert` (headless,
§8) or **Convert to YAML** in their tab. The converter writes
`<name>.yaml` beside the old file (comments and blank lines kept, every
field renamed, seconds turned into durations) and renames the old file to
`<name>.jsonc.bak`; it refuses when the YAML file exists. In the user's
folder the converted file starts with the schema line (§2.2).

### 2.1 Editing: the workflow language server

next_code has a language server for workflow files built in
(`crates/workflow_lsp`, run in-process): in the workflow tab and in YAML
files of a project's `.next_code/workflows/` (or the user folder, or any
YAML file starting with the schema line of §2.2) it completes keys,
values, step ids and `${…}` variables for where the cursor is, shows
next_code's problems as you type, describes keys on hover, goes from a
dependency or `${steps.<id>.…}` to its step, renames step and input ids
everywhere they are used (scoped as runs resolve them), and lists the
steps as symbols.

Agents reach it headless with the generic language-server commands, on a
project's workflow file:

```json
{"id": 1, "cmd": "open", "path": ".next_code/workflows/dev.yaml"}
{"id": 2, "cmd": "diagnostics", "path": ".next_code/workflows/dev.yaml", "wait_ms": 5000}
{"id": 3, "cmd": "definition", "path": ".next_code/workflows/dev.yaml", "line": 2, "character": 21}
{"id": 4, "cmd": "rename", "path": ".next_code/workflows/dev.yaml", "line": 1, "character": 3, "new_name": "postgres"}
```

`open` answers `{"language": "yaml", "server": "next_code workflows", …}`;
`rename` writes the file (the step's key, the `depends_on` entries naming
it and the `${steps.<id>.…}` that read it). `workflows_list` (§7) stays
the way to validate every file at once.

### 2.2 The JSON Schema

The format is also a JSON Schema (draft-07,
`crates/workflow/schema/workflow.schema.json`; headless `workflow_schema`
returns it). next_code keeps a copy, `workflow.schema.json`, in the user's
workflows folder (written when the folder is listed, replaced when a new
version differs; never listed as a workflow), and new and converted files
there start with the line that points YAML editors at it:

```yaml
# yaml-language-server: $schema=./workflow.schema.json
```

Editors with a YAML language server (VS Code with Red Hat's YAML
extension, JetBrains IDEs, Zed, Neovim) then complete keys, show each
field's description on hover and flag mistakes while typing. For a
project's `.next_code/workflows/`, copy the file there (commit it with the
workflows) and add the same line, or map it in VS Code's settings:

```json
"yaml.schemas": {
  "C:/Users/<you>/AppData/Roaming/next_code/workflows/workflow.schema.json": [
    "**/.next_code/workflows/*.yaml", "**/next_code/workflows/*.yaml"
  ]
}
```

The schema checks shapes, keys, types, durations, ports and which fields
need which; next_code's own validation (§7) is the authority and checks
more: that dependencies exist, cycles, `${steps.…}`/`${inputs.…}`
variables, regexes, a default against its input's options and bounds,
duplicate labels. Old-format keys (`needs`, `onFail`, …) and planned ones
(`restart`, …) are rejected with a description saying why.

---

## 3. File format

Top level, a mapping:

| Key | Meaning |
|---|---|
| `name` | string; default: the file name without extension |
| `description` | string; documentation only |
| `inputs` | mapping of input ids to their settings (§3.5) |
| `env` | mapping of names to values every step gets (a group's and the step's own `env` win) |
| `env_file` | dotenv files every step reads (§3.8) |
| `steps` | **required**: mapping of step ids to steps, in the order they run and show |
| `x-…` | any key starting with `x-`: shared parts for YAML anchors (below); ignored |
| `$schema` | ignored |

Other keys are **errors**. YAML 1.2 rules apply: `yes`/`no`/`on`/`off` are
strings, `~`/`null`/an empty value is null, unquoted numbers are numbers
(string fields take their text as written: `title: 1.10` is `"1.10"`).

**Shared parts** work as in Compose files: put them under an `x-` key with
an anchor and merge them with `<<` (keys written in the step win):

```yaml
x-api: &api
  project: {devcontainer: C:/src/api}
  env: {PROFILE: dev}
steps:
  build: {<<: *api, run: uv sync}
  serve:
    <<: *api
    depends_on: [build]
    run: uv run uvicorn app:app --port 8000
    env: {PROFILE: prod}   # replaces the anchor's env (a merge is shallow)
```

### Step fields

A step's id is its key in `steps`: unique among its siblings (YAML
rejects duplicate keys), no `/`. Unknown keys are **errors** (they catch
typos); keys of the old JSON format say what they became; `x-…` keys are
ignored. Allowed keys:

| Field | Type | Meaning |
|---|---|---|
| `title` | string | Shown in the UI; default the id |
| `depends_on` | id, list of ids, or mapping | The siblings it waits for (§3.0). None = starts immediately; independent steps run in parallel |
| `project` | see §3.1 | Where the step runs. Omitted = the machine running next_code, no project tab |
| `run` | string | Command line run in a terminal of that project (§4) |
| `terminal` | string | Terminal tab name; default `title`, else the id. Needs `run` |
| `check` | string | Command run as a plain process **before** `run`; exit 0 passes (§4) |
| `on_fail` | string | Message shown when `check` fails; the step then waits for Retry instead of failing the workflow. Needs `check` |
| `ready` | see §3.2 | When the step counts as done |
| `timeout` | duration | Fails the step if not ready this long after it started (its command is stopped, with its grace) |
| `stop_grace_period` | duration, 1s–1h, default `3s` | How long the command gets after Ctrl+C before it is killed, on every stop: Stop All, Restart, `timeout`, Run again, the workflow failing. Give it to anything that checkpoints or drains on Ctrl+C. Needs `run` |
| `artifacts` | list, see §3.3 | Files and links the step produces: checked when it finishes, linked in the UI, readable by later steps as `${steps.<id>.artifacts.…}` |
| `open_urls` | bool | http(s) URLs printed by `run` open in the browser (once each) until ready + 3 s. Needs `run` |
| `env` | mapping of string/number/bool | Extra environment for `run`/`check` |
| `cwd` | string | Working directory, relative to the project root (home folder without a project) |
| `restart` | `no`, `on_failure`, `always`, or `{policy, max, backoff}` | Starts its command again by itself when it fails (§3.6). Needs `run` |
| `healthcheck` | mapping | Probed while it is ready; unhealthy fails it (§3.6) |
| `if` | expression | Runs only when true once what it depends on is done; else `skipped` (§3.7) |
| `env_file` | path or list | Dotenv files its commands read; its `env` wins (§3.8) |
| `steps` | mapping, non-empty | Makes the step a **group** of these steps (§3.4) |

**Durations** are Compose's: `90s`, `10m`, `1h30m`, `500ms`, `1.5s` (no
spaces); a plain number is seconds.

**Quoting.** Most commands need no quotes. Quote a value (single quotes:
`'…'`, a quote inside doubled `''`) when it starts with a YAML indicator
(`- ? : , [ ] { } # & * ! | > ' " % @` and backtick), contains `: ` or
` #`, or is inside `{…}`/`[…]` and contains a comma or brackets — a comma
inside `{…}` separates keys: `ready: {manual: 'Log in, then Continue'}`.
For long commands use a block scalar (`run: |` then the lines, indented).
Regexes need no doubled backslashes: `output: 'ready in \d+ ms'`.

### 3.0 `depends_on`

| Shape | Meaning |
|---|---|
| `depends_on: api` | `api` must be **ready** |
| `depends_on: [api, db]` | each must be ready |
| `depends_on: {api: ready, eval: completed}` | per dependency: `ready` (the default; also an empty value) or `completed` |
| `depends_on: {eval: {condition: completed}}` | Compose's long form, same meaning |

- `ready`: the step starts once that one is ready; if it fails, this step
  is blocked.
- `completed`: the step starts once that one **finished, ready or failed**
  (§5.1). Use it for a step that must run even when the other one failed
  (a report after an evaluation). A stopped step blocks it.
- An id may be listed once. Ids name siblings only (§3.4).

### 3.1 `project` shapes

| Shape | Example |
|---|---|
| Local folder | `{local: C:/src/api}` or `{local: /home/me/api}` |
| WSL folder | `{wsl: Ubuntu, path: ~/src/tools}` |
| Dev container of a local folder | `{devcontainer: C:/src/api}` |
| Dev container of a WSL folder | `{devcontainer: {wsl: Ubuntu, path: ~/src/worker}}` |
| Specific devcontainer.json | `{devcontainer: C:/src/api, config: C:/src/api/.devcontainer/gpu/devcontainer.json}` |
| Typed location string | `'Ubuntu:~/p'`, `'\\wsl.localhost\Ubuntu\home\me\p'`, `'devcontainer:C:\src\p'` |
| next_code's own shape (copy from `recent.json`) | `{Local: 'C:\src\api'}`, `{Wsl: {distro: Ubuntu, path: /home/me/p}}`, `{DevContainer: {folder: {Local: 'C:\src\api'}, config: null}}` |

Tip: to get a project's exact location, copy its entry from
`%LOCALAPPDATA%\next_code\recent.json` (Windows) / `~/.local/share/next_code/recent.json`
(JSON is YAML: it can be pasted as it is).

Steps with the same location share one project tab (and one container).

### 3.2 `ready` shapes

Exactly one condition:

| Shape | Ready when | Use for |
|---|---|---|
| `{exit: true}` (default with `run`) | `run` exited with code 0 (without `run`: the check passed) | one-shot setup: migrations, installs, logins that exit |
| `{output: <regex>}` | the regex matches the command's output (ANSI codes stripped, across chunks and lines; Rust regex syntax) | servers that print a "ready" line |
| `{port: N}` | something listens on TCP port N where the step runs | servers with a known port (most robust) |
| `{manual: <prompt>}` | someone sends Continue | approvals, browser confirmations, anything only a human can verify |
| `{http: <url>}` or `{http: {url, status}}` | the URL answers where the step runs with an accepted status (default 200–399; `status: 200` or `[200, 204]`); tried every second | servers with a health endpoint (better than `port`: the app answers) |
| `{delay: <duration>}` | that long after the command started (or the check passed) | last resort; avoid |

### 3.3 `artifacts` entries

| Entry | Meaning |
|---|---|
| `out/metrics.json` | A file, relative to the step's `cwd` (else its project root, else the home folder); absolute paths stay |
| `https://dash.example/run/${env:RUN_ID}` | A link (a string starting with `http://` or `https://`) |
| `{label: report, path: out/report.html}` | A labelled file |
| `{label: board, url: 'http://localhost:6006'}` | A labelled link |

- Paths and URLs may use the variables of §4 (`${workspaceFolder}`,
  `${env:NAME}`, `${steps.<id>.…}`); they are expanded where the step runs.
- Labels are optional, unique within the step, not a number (numbers
  address artifacts by position) and without `}`.
- When the step finishes (ready, failed or stopped, once it got to where
  it runs) next_code expands the paths, makes them absolute and checks
  which files exist **where the step runs** (inside WSL or the container
  for remote projects). Steps waiting for it start only after that check.
  **Check again** (GUI) / `workflow_check_artifacts` (headless) repeats it
  on demand.
- A missing file is not an error: it shows as missing (dimmed in the UI,
  `"exists": false` headless).

### 3.4 Groups

A step with `steps` is a group: one node in the graph made of a sub-graph.

```yaml
api:
  title: API service
  depends_on: [vpn]
  project: {devcontainer: C:/src/api}
  env: {PROFILE: dev}
  cwd: backend
  steps:
    build: {run: uv sync}
    migrate: {depends_on: [build], run: uv run alembic upgrade head}
    serve:
      depends_on: [migrate]
      run: uv run uvicorn app:app --port 8000
      ready: {port: 8000}
      env: {LOG_LEVEL: debug}
```

- **Keys:** `title`, `depends_on`, `if`, `steps`, and the defaults
  `project`, `cwd`, `env`, `env_file`. Anything else (`run`, `check`,
  `on_fail`, `ready`, `timeout`, `open_urls`, `terminal`,
  `stop_grace_period`, `artifacts`, `restart`, `healthcheck`) is an error
  (`a group cannot have …`), as is an empty `steps`. Groups nest to any
  depth. A group's `if` that is false skips all its steps.
- **Inheritance:** every step inside (at any depth) gets the nearest
  group's `project` and `cwd` unless it sets its own; `env` is merged
  (the workflow's, outer groups, then inner ones, then the step; the step
  wins).
- **Scoping:** ids are unique among siblings only (two groups may both
  have a `build`), never contain `/`. `depends_on` inside a group names
  its other steps; a step cannot reach out of its group or into another
  one: make the **group** depend on what it needs. Errors say where:
  `` `a` depends on unknown step `top` in group `svc` ``; cycles are
  checked per group.
- **Qualified ids:** outside the file a step in a group is `group/step`
  (`api/migrate`, deeper `a/b/c`); top-level ids stay as they are. Events,
  `workflow_status` and the `step` parameter of `workflow_continue`,
  `workflow_restart`, `workflow_retry`, `workflow_check_artifacts` use
  them; a bare `migrate` is not found.
- **Gating:** the group's `depends_on` gates all its steps: a step
  without sibling dependencies starts once the group may start (its
  `detail` shows the group's wait, e.g. `needs vpn`).
- **Derived state:** `pending` before the run; `waiting` for its own
  `depends_on` (blocked when it can never be met: `blocked: vpn did not
  get ready`); `running` while a step in it starts or runs (`stopping`
  when its only active steps stop); `ready` when every step is; `failed`
  (`reason`: `2 of 5 steps failed`) when no step in it can progress and
  some failed; `stopped` after Stop All. Steps outside wait for a group
  by this state with the usual rules (§5.1): `ready` wants it ready,
  `completed` wants it finished (ready or failed).
- **Variables:** `${steps.<id>.…}` in a step reads what its own
  `depends_on` lists, then what each enclosing group lists (by the id at
  that level, the nearest first). `${steps.<group>.status}` reads a
  group's state; groups have no `exit_code` or `artifacts` (an error).
  Variables in a group's `env`/`cwd` (and the workflow's `env`) are
  checked in each step that inherits them.
- **Actions:** Restart on a group restarts every step in it; Retry on a
  group reruns its failed steps; Stop All is unchanged. The run's outcome
  counts the steps that do something (not groups).

### 3.5 Inputs

`inputs` declares parameters every run gets: the GUI asks for them in a
form before it starts, headless takes them in `workflow_run`. Steps read
them as `${inputs.<id>}`. The shape is GitHub Actions'
`workflow_dispatch` inputs, with a few more settings.

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
steps:
  deploy:
    project: {wsl: Ubuntu, path: '~/src/${inputs.env}'}
    env: {BRANCH: '${inputs.branch}'}
    run: ./deploy.sh --env ${inputs.env} -j ${inputs.workers} --clean=${inputs.clean}
```

| Key | Type | Meaning |
|---|---|---|
| (its key) | | The id: unique; `${inputs.<id>}` names it; no `/`, `.`, `{`, `}`, `$` or spaces |
| `type` | `string` (default), `number`, `boolean`, `choice` | The control: textbox, number textbox, checkbox, dropdown |
| `label` | string | Shown in the form; default the id |
| `description` | string | Helper text under the field |
| `default` | of the type | What a run gets when not given a value, and what the form starts with. Without it: `""`, empty (number), `false`, the first option. Must be valid (a choice's option, within `min`/`max`, …); written as text it keeps its form (`default: 1.10` is `1.10` for a string) |
| `required` | bool (`string`, `number`) | A string must not be empty (spaces count as empty), a number must be given |
| `placeholder` | string (`string`, `number`) | Shown in an empty textbox |
| `options` | list of values or `{value, label}` (`choice`, **required**) | The dropdown's entries; values unique; `label` defaults to the value |
| `min`, `max` | number (`number`) | Bounds, inclusive |
| `integer` | bool (`number`) | Whole numbers only |

An input without settings (`note:` alone) is a string input.

- Unknown keys, keys of another type (`min` on a string, `options`
  without `type: choice`) and invalid defaults are validation errors
  located at the key they are about.
- `${inputs.<id>}` works in `run`, `check`, `cwd`, `env` values (the
  workflow's too), artifact paths and every string of `project` (paths,
  distro, `config`), at any depth (a group's `project`/`cwd`/`env` too,
  inherited as usual). An unknown id is a validation error at the
  variable. Elsewhere (`title`, `terminal`, `on_fail`, `ready`) it stays
  literal.
- Values are fixed when the run starts and inserted as they are (quote
  them for the shell when they may hold spaces): strings and choices as
  written, numbers in their shortest form (`8`, `0.5`), booleans `true` /
  `false`, an empty number as nothing. Values must not contain `${` (they
  are put in before the other variables). Restart and Retry reuse the
  run's values; a new run (Run All, `workflow_run`) takes new ones.
- Headless takes JSON values (`"workers": 8`, `"clean": true`); strings
  that parse are accepted for numbers and booleans (`"8"`, `"true"`).
  Inputs not given get their **defaults**; the GUI form's remembered
  values are not used. Unknown ids, invalid values and missing required
  ones fail `workflow_run` with every problem:
  ``Deploy cannot run: unknown input `colour` (inputs: env, branch, workers, clean); input `env`: `qa` is not one of dev, staging, prod; input `branch`: required; input `workers`: must be ≤ 64``.

### 3.6 Keeping services up: `restart` and `healthcheck`

```yaml
api:
  run: uv run uvicorn app:app --port 8000
  ready: {http: 'http://localhost:8000/health'}
  restart: {policy: on_failure, max: 5, backoff: 5s}
  healthcheck:
    test: curl -fs localhost:8000/health   # or http: …, or port: 8000
    interval: 30s                          # default 30s
    timeout: 10s                           # one probe, default 10s
    retries: 3                             # failures in a row, default 3
    start_period: 1m                       # failures right after ready do not count
```

- A **ready** step whose command exits with a non-zero code **crashed**:
  it fails (`exited after it was ready (exit code 1)`). A clean exit (0)
  after ready leaves it ready.
- `restart`: `no` (the default), `on_failure` or `always`, or
  `{policy, max, backoff}` (`max`: restarts before giving up, default no
  limit; `backoff`: the wait before each, default `2s`). `on_failure`
  restarts after a failure of its **command**: a non-zero exit before or
  after ready, a `timeout`, unhealthy. Not after a failed `check`, a
  project that could not open or a variable that could not be read (they
  need a human). `always` also restarts after a clean exit (a `ready:
  exit` step then runs again and again: its dependents start after its
  first run). While it waits the step is `waiting` with detail `restart 2
  in 5s: <why>`; it then starts as usual (after its dependencies).
  `restarts` (status rows) counts them; after `max` it fails with `… (gave
  up after 5 restarts)`. Run All, Restart and Retry count from 0 again.
- `healthcheck`: one of `test` (a command where the step runs, in its
  shell, `cwd`, env files and `env`; exit code 0 is healthy: **any check
  every N seconds**), `http` (a URL answers 200–399, or `{url, status}`)
  or `port`. It is probed every `interval` while the step is ready (also
  a step without `run`: watch a VPN, a tunnel, a remote service).
  Failures within `start_period` of getting ready do not count; `retries`
  failures in a row make it unhealthy: it fails (`unhealthy: exit code
  7`), its command is stopped (with its grace), and with a `restart`
  policy it starts again. While failures add up the detail says `health
  check failed (1 of 3): …`.
- HTTP probes (`ready.http`, `healthcheck.http`) run where the step runs:
  plain `http://` directly (through the agent for WSL and dev containers),
  `https://` with `curl` there (PowerShell on this Windows machine).
- Steps that depend on a step that restarts are left alone; they waited
  for it once.

### 3.7 Conditions: `if` and `skipped`

```yaml
deploy: {run: ./deploy.sh, if: "inputs.env == 'prod'"}
smoke: {depends_on: [deploy], run: pytest smoke}         # skipped with deploy
notify:
  depends_on: {eval: completed}
  if: failure()                                          # only after eval failed
  run: ./notify.sh '${steps.eval.exit_code}'
```

- Evaluated when the step could start (what it depends on is done). False
  → `skipped` (reason `if inputs.env == 'prod'`); a group's `if` that is
  false skips all its steps.
- Reads `inputs.<id>`, `steps.<id>.status` (`ready`, `failed`, `stopped`,
  `skipped`), `steps.<id>.exit_code` (steps in its `depends_on` or a
  group's, as for variables), and `success()` (every step it waits for is
  ready or skipped), `failure()` (one failed: wait for it `completed`),
  `always()`. Strings in quotes, numbers, `true`/`false`/`null`; `==`,
  `!=`, `<`, `<=`, `>`, `>=`, `&&`, `||`, `!`, parentheses. `false`, `0`,
  `''` and `null` are false. Quote the whole `if` when it starts with a
  quote or holds `: ` or ` #` (`if: "inputs.env == 'prod'"`).
- A step that needs a skipped step **ready** is skipped too (`deploy was
  skipped`); one that waits for it `completed` runs (`${steps.deploy.status}`
  is `skipped`). Skipped steps count as done: the outcome is `ready` when
  every step is ready or skipped.
- Unknown inputs, steps outside the scope, unknown fields and syntax
  errors are validation errors at the place in the expression.

### 3.8 `env_file`

```yaml
env_file: .env                                   # every step
steps:
  api:
    env_file: [config/dev.env, {path: .env.local, required: false}]
    env: {LOG_LEVEL: debug}                      # wins over the files
```

- Dotenv files: `NAME=value` lines, `export ` allowed, `#` comments,
  values unquoted (` #` starts a comment), `'single'` (as written) or
  `"double"` (`\n`, `\t`, `\"` escaped; may span lines).
- Read **where the step runs** (inside WSL or the container), relative to
  the step's `cwd` (else its project root, else the home folder), when
  its check or command starts; `${…}` variables work in paths. The
  workflow's files come first, then each group's, then the step's: a
  later file's variable wins, and `env` wins over all of them. `${env:NAME}`
  reads them too.
- A missing file fails the step (`env_file /p/.env: No such file …`)
  unless `required: false`.

---

## 4. How commands run

| Where | `run` (terminal tab) | `check` (process) |
|---|---|---|
| Local Windows | `powershell.exe -NoLogo -Command <line>` | PowerShell `-NoProfile` |
| WSL, dev container, Linux | user's login shell `-lic <line>` (PATH as in a terminal) | `sh -c <line>` |

Consequences for authors:

- Write `check` and `run` lines for the shell **where the step runs**: a
  step without `project` on Windows is PowerShell (`if (...) { exit 1 }`,
  `Test-NetConnection`), a WSL/container step is bash/sh.
- `check` uses `sh`, not the login shell: tools installed in `~/.local/bin`
  may not be on its PATH. Use absolute paths or `bash -lc '...'` inside
  the check if needed.
- `.venv` is **not** activated; use `uv run …` (or the venv's python path).
- Variables expanded in `run`, `check`, `cwd`, `env` values and artifact
  paths: `${workspaceFolder}` (project root; home folder without a
  project), `${userHome}`, `${env:NAME}` (the environment **where the step
  runs**), the step-result variables below and the inputs (§3.5;
  `project` too). Put secrets in the environment and reference them as
  `${env:NAME}`; never write them in the file.
- YAML quoting (§3): plain and single-quoted values take backslashes as
  they are (`C:\src`, `'ready in \d+ ms'`); inside double quotes they
  escape (`"C:\\src"`). A command with `: ` or ` #` needs quotes or a block
  scalar (`run: |`).
- A `run` command that keeps running (a server) must use `output` or
  `port` readiness; with the default `exit` it never becomes ready.
- A `run` command that exits **before** it is ready fails the step
  (`exited before it was ready (exit code N)`), except with `manual`.
- Stops send Ctrl+C to the command's terminal (its pty: the foreground
  process group on Linux/WSL/containers; a console Ctrl+C on Windows).
  Trap `SIGINT` (Python: `KeyboardInterrupt`; PowerShell: `try {…}
  finally {…}`) to checkpoint, and set `stop_grace_period` to how long
  that takes. The terminal echoes `^C` into the output.

### 4.1 Step-result variables

| Variable | Value |
|---|---|
| `${steps.<id>.status}` | `ready`, `failed` or `stopped` |
| `${steps.<id>.exit_code}` | the command's exit code; empty if it has none (no `run`, still running, killed) |
| `${steps.<id>.artifacts.<label>}` | that artifact's resolved path (or URL) |
| `${steps.<id>.artifacts.<index>}` | the same by position in `artifacts`, from 0 |

- Only for steps listed in this step's `depends_on`; anything else
  (another step, an unknown field, a label that step does not declare, an
  index past its artifacts) is a validation error pointing at the
  variable.
- Replaced **when the step starts**, from what the other step left
  (with `ready` it is ready, with `completed` ready or failed). Restarting the
  other step later does not change a step that already started.
- Paths are **as seen where the producing step runs**: a container step's
  `/workspaces/api/out/metrics.json` means nothing to a step on Windows or
  in another container. Read artifacts from steps in the same project (or
  pass URLs).
- The value is inserted as is: quote it for the shell if a path may hold
  spaces (`'${steps.eval.artifacts.report}'`).
- If the other step's artifacts could not be resolved (e.g. its project's
  environment could not be read), a step reading
  `${steps.<id>.artifacts.…}` fails with that reason instead of starting.

---

## 5. Lifecycle

Step states (`status` in events and status replies):

```
pending → waiting (for its depends_on) → starting (opening project / running check)
        → running (command runs, or waiting for port/http/output/delay/Continue)
        → ready | failed | stopped
waiting → skipped (its if, or a group's, is false; a step it needs ready was skipped)
running/ready with a live command → stopping (Ctrl+C sent; grace running) → stopped | failed
failed command with a restart policy → waiting (restart N in 5s: …) → starting …
```

`stopping` lasts until the command exits (which ends it early) or its
`stop_grace_period` runs out (then it is killed); `detail` counts down,
e.g. `stopping (12 s left)`, `restarting; stopping (2 s left)`, `timed
out after 10m; stopping (3 s left)`.

Workflow outcomes: `idle`, `running`, `ready` (every step ready or
skipped; a step that crashes or restarts later puts it back to `running`),
`failed`, `stopped`. **`failed`** means either a check without `on_fail`
failed (everything was stopped), or the run **finished with failures**:
nothing is pending, waiting (unblocked), starting, running or stopping
any more, and not every step is ready. A step failed by a check with
`on_fail` counts as failed here: the run waits for a human, so the
outcome is `failed` with that step's reason visible. Retry or Restart
puts the outcome back to `running`. In the GUI the chip says **Failed**.

### 5.1 Ordering: `ready` and `completed` dependencies

| The other step is | `depends_on: {x: ready}` | `depends_on: {x: completed}` |
|---|---|---|
| ready | starts | starts |
| failed (command, timeout, project) | blocked: `<id> did not get ready` | starts |
| failed by a check with `on_fail` (waits for Retry) | blocked: `<id> did not get ready` | blocked: `<id> waits for Retry` |
| stopped | blocked: `<id> did not get ready` | blocked: `<id> was stopped` |
| blocked itself | blocked: `<id> can never run` | blocked: `<id> can never run` |
| anything else (incl. stopping, artifacts being checked) | waits: `needs <id>` | waits: `after <id>` |

Blocked steps stay `waiting` with that `detail` (headless: `"blocked":
true`); several reasons are joined with `; `. They start by themselves
once Retry/Restart unblocks them.

Failure rules:

| Event | Effect |
|---|---|
| `check` fails, step has `on_fail` | step `failed` with reason = the `on_fail` text; waits for Retry; others continue |
| `check` fails, no `on_fail` | the **workflow fails**: everything it started is stopped (each with its grace) |
| `run` exits non-zero (ready `exit`) | step `failed` (`failed (exit code N)`); `completed` dependents run |
| command exits before ready (output/port/http/delay) | step `failed` (`exited before it was ready …`) |
| a ready step's command exits non-zero | step `failed` (`exited after it was ready (exit code N)`) |
| health check: `retries` failures in a row | step `failed` (`unhealthy: …`), its command stopped |
| any of the four above, with `restart` | instead of failing: `waiting` (`restart N in 2s: …`), then it starts again; `… (gave up after N restarts)` after `max` |
| `timeout` reached | step `stopping` (detail `timed out after 10m; stopping …`), then `failed` (`timed out after 10m`) once the command is gone |
| a step failed | `ready` dependents are blocked (`blocked: <id> did not get ready`); `completed` dependents run; independent steps keep going; when nothing can progress the outcome is `failed` |

Controls: **Restart** a step (its command, if still running, is stopped
with its grace first; then it reruns after its dependencies in the same
terminal), **Retry** (reruns every failed step), **Continue** (manual
steps only), **Stop** (Ctrl+C to every command the run started, each
killed after its `stop_grace_period`; a **second Stop while steps are
still stopping kills them at once**; project tabs and containers stay),
**Run** (starts afresh; a running run of the same workflow is stopped
first with each step's grace, and a step whose old command is still
stopping waits for it — detail `waiting for its previous command to end`
— before starting in the same terminal).

---

## 6. Designing a workflow

Recipes:

- **Precondition (VPN, credentials, Docker running):** a step with only
  `check` and an `on_fail` telling the human what to do. Make everything
  else depend on it.
  ```yaml
  vpn:
    check: if (-not (Test-NetConnection git.corp -Port 443 -InformationLevel Quiet)) { exit 1 }
    on_fail: Connect the VPN, then Retry
  ```
- **Server:** `run` + `port` (preferred) or `output`, plus a generous
  `timeout` when a container may need building.
- **Browser login (device code / SSO):** `run` the login with
  `open_urls: true`; ready by `exit` if the CLI exits after login, else by
  an `output` regex of its success line. If success can only be confirmed
  by a human, add a separate `manual` step after it.
- **Fan-out / fan-in:** several steps with the same `depends_on`, then one
  step depending on all of them.
- **Per-project terminals:** give `terminal` names that a human will
  recognise in the project's Terminal panel.
- **Shared settings:** an `x-` key with an anchor (`x-api: &api {project:
  …, env: …}`) and `<<: *api` in each step that runs there — or a group,
  when the steps also belong together in the graph.
- **Evaluate, then always report:** the evaluation checkpoints on Ctrl+C
  (give it the time with `stop_grace_period`) and declares what it
  writes; the report depends on it `completed` (it runs also when the
  evaluation exits 1) and reads its exit code and files. The evaluation's
  failure stays visible: it shows failed, the report ready, the outcome
  `failed` (finished with failures).
  ```yaml
  eval:
    project: {devcontainer: C:/src/ml}
    run: uv run python eval.py --out out
    stop_grace_period: 2m
    timeout: 2h
    artifacts:
      - {label: metrics, path: out/metrics.json}
      - {label: board, url: 'http://localhost:6006'}
  report:
    depends_on: {eval: completed}
    project: {devcontainer: C:/src/ml}
    run: uv run python report.py --eval-exit '${steps.eval.exit_code}' --metrics '${steps.eval.artifacts.metrics}'
    artifacts: [{label: report, path: out/report.html}]
  ```
  `eval.py` should catch `KeyboardInterrupt`, write its checkpoint and
  exit; a timeout or Stop then waits up to 2 minutes for it. A `report`
  step that must not run after a **stopped** evaluation needs nothing
  more (`completed` blocks on stopped steps).
- **Per-service group: build → migrate → serve.** One group per service
  carries its `project`, `cwd` and `env` once; inside, a chain; outside,
  other steps depend on the whole service:
  ```yaml
  db:
    project: {wsl: Ubuntu, path: ~/infra}
    run: docker compose up postgres
    ready: {port: 5432}
  api:
    depends_on: [db]
    project: {devcontainer: C:/src/api}
    env: {DATABASE_URL: 'postgresql://localhost:5432/dev'}
    steps:
      build: {run: uv sync}
      migrate: {depends_on: [build], run: uv run alembic upgrade head}
      serve:
        depends_on: [migrate]
        run: uv run uvicorn app:app --port 8000
        ready: {port: 8000}
        terminal: API
  web:
    depends_on: [api]
    project: {devcontainer: C:/src/web}
    run: npm run dev
    ready: {output: 'ready in \d+ ms'}
  smoke:
    depends_on: {api: completed, web: completed}
    run: pytest smoke --api-state ${steps.api.status}
  ```
  Restart `api` (GUI: select the group, Restart; headless:
  `workflow_restart` with `"step": "api"`) rebuilds, migrates and serves
  again; `api/migrate` alone restarts one step.
- **One workflow, several targets:** a `choice` input for the
  environment and a `string` for the branch instead of a copy of the file
  per target; the GUI asks before each run (remembering the last
  choice), an agent passes them:
  ```yaml
  inputs:
    env: {type: choice, options: [dev, staging, prod], default: dev}
    branch: {default: main, required: true}
  steps:
    checkout:
      project: {wsl: Ubuntu, path: ~/src/app}
      run: git fetch && git switch '${inputs.branch}'
    deploy:
      depends_on: [checkout]
      project: {wsl: Ubuntu, path: ~/src/app}
      env: {TARGET: '${inputs.env}'}
      run: ./deploy.sh
      artifacts: ['logs/deploy-${inputs.env}.log']
  ```
  `{"cmd": "workflow_run", "name": "Deploy", "inputs": {"env": "staging", "branch": "fix-42"}}`.

Checklist before handing a workflow over:

1. Every step id short, lowercase (`api`, `mcp`, `login`).
2. Every long-running `run` has `output` or `port` readiness.
3. Every network/credential precondition is a `check` with `on_fail`.
4. Steps that can take minutes (container build, first `uv sync`) have `timeout`.
5. Commands that checkpoint or drain on Ctrl+C have `stop_grace_period`.
6. No secrets in the file; use `${env:NAME}`.
7. Steps that must run whatever happened before depend on it `completed`.
8. Groups hold only steps and defaults (`project`, `cwd`, `env`); their
   steps' `depends_on` name siblings.
9. Inputs have defaults that make a safe run (`dev`, not `prod`); values
   that may hold spaces are quoted in `run`.
10. Text with a comma inside `{…}` is quoted.
11. `workflows_list` reports `"error": null` for it.

---

## 7. Validation loop

`workflows_list` parses every file and returns, per workflow, either the
`steps` or an `error` (one problem per line):

```json
{"id":"/tmp/wfh|/tmp/wfh/.next_code/workflows/bad.yaml","name":"bad","path":"/tmp/wfh/.next_code/workflows/bad.yaml","source":"/tmp/wfh","error":"`api`: unknown key `colour` (known: title, depends_on, project, run, terminal, check, on_fail, ready, timeout, stop_grace_period, open_urls, env, cwd, artifacts, steps)","steps":[],"outcome":null}
```

Notes:

- An **invalid** workflow is listed under its **file name** (`bad`), not its
  `name`, and `workflow_run` on it fails with `<name> cannot run: <error>`.
  Address it by `path` to be safe.
- YAML syntax errors come first, alone (`invalid YAML: …`, and `duplicate
  key `x``). Then per-step problems (unknown keys, wrong types, `` `b`:
  `depends_on` lists `a` twice ``, `stop_grace_period` outside 1s–1h, bad
  durations, bad `artifacts` entries, duplicate or numeric labels, …),
  named by the step's qualified id (`` `api/migrate`: … ``); only when
  every step is valid are graph problems checked (`` `a` depends on
  unknown step `b` ``, `${steps.…}` variables — e.g. `` `report`:
  `${steps.x.status}` refers to `x`, which is not in this step's
  `depends_on` `` — and `dependency cycle: a → c → b → a`, which follows
  `ready` and `completed` dependencies alike). Re-list after each fix.
- Keys of the old JSON format say what they became (`` `needs` is from
  the old JSON format; it is now `depends_on` ``); planned keys say they
  are not supported yet (§12).
- In the GUI the same problems appear as a banner with line and column
  over the graph while the file is edited; each points at the key or
  value it is about (a dependency at its id, a variable where it is
  written).
- Inside groups, graph problems name the group (`` `a` depends on unknown
  step `b` in group `api` ``); line and column point into the nested step.

---

## 8. Operating headless

Start next_code headless on any project (its own folder or the one whose
`.next_code/workflows` you need):

```
next_code --headless /path/to/project            # Linux / local Windows folder
next_code --headless --wsl Ubuntu ~/project      # WSL
next_code --headless --devcontainer C:\src\api   # inside its dev container
```

Protocol: one JSON object per line on stdin; each gets one reply line
`{"id": N, "ok": true, "result": …}` or `{"id": N, "ok": false, "error": "…"}`;
events are extra lines carrying `"event"`. The first line is
`{"event":"ready",…}`; wait for it. **Replies can arrive out of order**:
match them by `id`, and wait for every reply before sending `shutdown`.

Steps for other projects open those projects without a window (dev
containers are started). Terminals are invisible; their last line is in
`workflow_status`. URLs from `open_urls` are **reported** as events, not
opened: open or relay them yourself.

### Commands

Workflows are named by `"name"`, `"path"` or `"id"` (from
`workflows_list`). Steps by `"step": "<qualified id>"` (`svc`, `api/migrate`).

| Command | Params | Result |
|---|---|---|
| `workflows_list` | – | array of `{id, name, path, source, error, inputs, steps, outcome}`; each step also has `policies: {restart: {policy, max, backoff}, healthcheck: {probe, interval, timeout, retries, start_period} or null, if, env_file: [{path, required}]}` and `ready: {http: {url, status}}` for HTTP readiness; groups have `if`; `inputs` as declared: `{id, label, type, description, default, required, placeholder}` plus `options[{value, label}]` (choice) or `min`, `max`, `integer` (number); each step `{id, path, kind: "step", title, depends_on: {"<id>": "ready" or "completed"}, project, run, check, ready, timeout, stop_grace_period, artifacts[{label, path} or {label, url}]}`, each group `{id, path, kind: "group", title, depends_on, project, steps[…]}` (nested). `path` is the qualified id; `project` is where it runs (inherited from its groups); durations as in files (`10m`, `3s`; `ready.delay` too); `${inputs.…}` show unexpanded. Old-format files have an `error` saying to convert them |
| `workflow_schema` | – | `{schema, file, modeline}`: the format's JSON Schema (§2.2), the name of the copy in the user's workflows folder, and the line that points YAML editors at a copy beside a workflow |
| `workflow_convert` | name/path/id of an old-format file | `{path, backup, problems}`: the YAML file written beside it (comments kept; an error if it exists), the old file renamed to `<name>.bak`, and the converted file's problems (as the old one had them; `[]` usually). Files are read again |
| `workflow_run` | name/path/id, `inputs`? (`{"<id>": value}`) | `{run, id}`; starts afresh (a run of it still going is stopped first, each command with its grace). Inputs not given get their defaults; invalid ones are an error listing each (§3.5) |
| `workflow_status` | name/path/id | `{run, workflow, outcome, inputs, steps[…]}` (error if it has not run); `inputs` the run's values by id (`{"env": "prod", "workers": 8}`). One row per step and group in file order, groups before their steps. Step rows: `{id, kind: "step", parent, title, status, reason, detail, blocked, last_line, check_output, exit_code, exited, restarts, health_failures, elapsed_ms, location, terminal, artifacts[{label, name, path or url, resolved, exists}], artifacts_error}`; group rows: `{id, kind: "group", parent, title, status, reason, detail, blocked, elapsed_ms, steps[qualified ids]}`. `id`/`parent` are qualified ids (`parent` null at the top). `resolved`/`exists` are `null` until the artifacts were checked; `artifacts_error` says why they could not be |
| `workflow_continue` | name, `step` | `null`; error `the step is not waiting for Continue` unless it is a running `manual` step |
| `workflow_restart` | name, `step` | bool (its old command is stopped with its grace first; a group: every step in it) |
| `workflow_retry` | name, `step`? | bool: without `step` reruns every failed step; with it that step if it failed, or a group's failed steps (`false` if none had failed) |
| `workflow_stop` | name | bool (Ctrl+C, kill after each step's `stop_grace_period`; again while steps are `stopping`: kill now) |
| `workflow_check_artifacts` | name, `step` | bool: `false` if the step has no artifacts or never got to where it runs; else they are looked up again — read `workflow_status` after a moment |

### Events

`workflow_event` with `type`:

| type | fields |
|---|---|
| `step` | `run`, `workflow`, `step` (qualified id; groups too), `status` (incl. `stopping`), `reason` (failed only; a group's `N of M steps failed`), `detail` (what it waits for, e.g. `needs svc`, `after eval`, `waiting for output /…/`, the manual prompt, `blocked: …`, `stopping (20 s left)`; a running group's `2 of 3 ready`) |
| `outcome` | `run`, `workflow`, `outcome` |
| `url` | `run`, `workflow`, `step`, `url` |

Events carry `run`: after a new `workflow_run` the replaced run may still
report its steps going `stopping` → `stopped`; ignore runs you did not
start last.

### Real transcript (abridged)

```
<< {"root":"/tmp/wfh","remote":false,"event":"ready"}
>> {"id": 2, "cmd": "workflow_run", "name": "Demo"}
<< {"id":2,"ok":true,"result":{"run":1,"id":"/tmp/wfh|/tmp/wfh/.next_code/workflows/demo.yaml"}}
<< {"type":"step","run":1,"workflow":"Demo","step":"svc","status":"waiting","reason":null,"detail":"needs pre","event":"workflow_event"}
<< {"type":"outcome","run":1,"workflow":"Demo","outcome":"running","event":"workflow_event"}
<< {"type":"step","run":1,"workflow":"Demo","step":"pre","status":"ready","reason":null,"detail":"","event":"workflow_event"}
<< {"type":"step","run":1,"workflow":"Demo","step":"svc","status":"running","reason":null,"detail":"waiting for output /listening on \\d+/","event":"workflow_event"}
<< {"type":"step","run":1,"workflow":"Demo","step":"svc","status":"ready","reason":null,"detail":"","event":"workflow_event"}
<< {"type":"step","run":1,"workflow":"Demo","step":"gate","status":"running","reason":null,"detail":"Approve, then Continue","event":"workflow_event"}
>> {"id": 3, "cmd": "workflow_continue", "name": "Demo", "step": "gate"}
<< {"id":3,"ok":true,"result":null}
<< {"type":"step","run":1,"workflow":"Demo","step":"done","status":"ready","reason":null,"detail":"","event":"workflow_event"}
<< {"type":"outcome","run":1,"workflow":"Demo","outcome":"ready","event":"workflow_event"}
>> {"id": 5, "cmd": "workflow_stop", "name": "Demo"}
<< {"id":5,"ok":true,"result":true}
```

A failing run, as `workflow_status` reports it:

```
vpn    failed   reason "Connect the VPN, then Retry"     (check failed, on_fail set)
boom   failed   reason "failed (exit code 3)"            exit 3
after  waiting  detail "blocked: boom did not get ready" blocked true
report ready                                            (depends on boom `completed`)
outcome failed                                          (finished with failures)
```

### Driving loop

```
send workflows_list            → stop if this workflow's error != null; fix the file
send workflow_run
loop on events for this run:
  step status=running with a manual prompt in detail
      → do/ask for what the prompt says, then workflow_continue
  type=url        → open it or hand it to the human
  step status=failed
      → workflow_status for last_line / check_output / exit_code / artifacts
      → fix the cause (or ask the human: VPN, login), then workflow_retry
  outcome=ready   → done; leave it running
  outcome=failed  → finished with failures (nothing can progress; steps show
                    failed or detail blocked: …) or a check without on_fail
                    failed: report the failures (reason, last_line, artifacts),
                    fix, then workflow_retry / workflow_restart / workflow_run
```

Rules of thumb:

- Never `workflow_continue` on a human's behalf for approvals you cannot
  verify; relay the prompt.
- After `workflow_stop` the projects and containers stay up; Run again is
  cheap.
- `last_line` is the last non-empty output line; use it in failure
  reports, quoted verbatim.
- A step `stopping` is waiting for its command to finish its Ctrl+C
  handling; send `workflow_stop` again only to kill it now.

---

## 9. Operating in the GUI

- **Run > New Workflow…** writes a commented template (`workflow.yaml`)
  to the user folder and opens it; **Run > Open Workflow…** lists all workflows (user folder
  and open projects); palette **Run Workflow: <name>** opens and runs.
- A workflow opens as a tab (◈) next to project tabs: the file on the left
  (Ctrl+S saves), the dependency graph on the right (columns by
  dependency depth, live status colours, problems as a banner with line and
  column while you type). Steps are ordered for the fewest edge crossings
  found, chains stay on one row, and an edge spanning several columns runs
  between the cards it passes, never through them; edges leave and enter
  cards horizontally, at a port of their own when a side has several.
  Edges of `ready` dependencies are solid (green once the upstream is
  ready); edges of `completed` ones are **dashed** (lit once the step
  finished, ready or failed) and the card's kind line says `after`. A card with artifacts shows `↗ N`; a
  stopping card shows its countdown.
- A **group** is a card of the same size: state, title, `▸ N steps`,
  elapsed time, and a miniature of its graph (its steps as small boxes in
  their state colours, nested groups outlined, edges dashed for
  `completed`).
  Click it to select it and expand it in place: a box around its graph of
  real cards (click them as usual; edges attach to the box's sides).
  Click the box's header (`▾`) to collapse it. Several groups (nested
  ones too) can be open at once; they stay open while the tab is, across
  edits, and the graph re-fits unless you zoomed. A selected group's
  details show its state, its steps counted by state, Restart, Retry
  Failed and Expand/Collapse. With the cursor inside a collapsed group's
  step, the group is highlighted.
- Click a card: the editor jumps to the step; the details strip shows the
  state, reason, last output lines, **Artifacts** (each a link: a file
  opens in an editor tab of the step's project — a file on this machine in
  the project tab holding it, else its folder as a new tab; a URL in the
  browser; missing files are dimmed and marked `missing`; **Check again**
  looks them up again) and Terminal / Continue / Restart / Retry. With a
  step on this machine, Terminal shows its terminal under the details;
  drag the divider above them to resize that pane (double-click: maximize
  it / back; the size is kept while the tab is open).
- Toolbar: Run All (Shift+F10; saves first), Stop All (Ctrl+F2), Retry
  Failed, and the current run's inputs (`env=prod · branch=main`,
  selectable; the whole text on hover when cut). Graph: opens fitted (and
  re-fits when its pane is resized); Ctrl+scroll zooms; − / % / + / Fit
  in the corner; drag or scroll pans.
- **Inputs form:** Run All (toolbar, Shift+F10, palette **Run Workflow:**)
  on a workflow with `inputs` first shows a form over the graph: a row
  per input (label, `*` when required, the control, its description, what
  is wrong with its value in red). It starts with the values the workflow
  last ran with (kept per workflow file across restarts) where they still
  fit — an option removed, a bound changed, a type changed fall back to
  the default — else the defaults; **Reset to defaults** restores those.
  Tab / Shift+Tab move between fields, Space toggles a checkbox, a
  dropdown opens with a click, Space, Up or Down (Up/Down move, Enter
  picks, Escape closes it); Enter (or Run All again) runs once every value
  is valid (Run is disabled before), Escape or Cancel closes the form
  without running. Workflows without inputs run at once.
- Agents with UI automation (`next_code --automation`, see README) can use
  the same: the command palette (`Ctrl+Shift+P`) and clicks.

---

## 10. Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `no workflow \`X\`` | Not discovered: the file is not in a scanned folder, its project is not open, or it is invalid and listed under its file name. Run `workflows_list` |
| Step stuck `running` with `waiting for output /…/` | The regex does not match what is printed (check `last_line`); inside double quotes backslashes escape (`"\\d+"`), plain or single-quoted they do not (`'\d+'`); the program buffers stdout (e.g. Python: `python -u` or `PYTHONUNBUFFERED=1` in `env`) |
| `invalid YAML: …` | Usually indentation (spaces only, children indented under their key), an unquoted value with `: ` or ` #`, or text starting with `{`, `[`, `*`, `&`, `!`, `%`, `@`. Quote it (`'…'`) or use `run: \|` with the lines below |
| ``expected exactly one condition … (a comma inside `{…}` separates keys …)`` | `ready: {manual: Log in, then Continue}` reads as two keys: quote the text |
| `` `needs` is from the old JSON format `` (or `onFail`, `timeout_s`, …) | The file mixes the formats: use the new key the message names, or convert the old `.jsonc` file instead (§2) |
| Step stuck waiting for a port | Server binds another port or only inside a network namespace; check `last_line`; in containers the port must listen inside the container |
| `exited before it was ready` | The command ended (crash, wrong directory, missing tool). Read `last_line`; set `cwd`; tools from `~/.local/bin` need the login shell (fine in `run`, not in `check`) |
| `failed (exit code 127)` | Command not found where the step runs (wrong shell or PATH) |
| Check passes on Linux but not on Windows | Step without `project` runs PowerShell on Windows: rewrite the check in PowerShell |
| Dev container step slow the first time | Image build/pull; give `timeout: 10m` or more |
| Step `stopping` for long, then `stopped`/`failed` | It ignores or slowly handles Ctrl+C; it is killed when `stop_grace_period` runs out. Lower it, or Stop again to kill now |
| Checkpoint not written on Stop | Killed before it finished: raise `stop_grace_period`. The program itself must handle Ctrl+C (a `trap … INT` in a `sh` script, Python's `KeyboardInterrupt`, PowerShell `finally`); try Ctrl+C in a terminal of that project to see what it does |
| Report step never runs | It depends on the evaluation `ready`; use `depends_on: {eval: completed}`. Or the evaluation was stopped / waits for Retry (`blocked: … was stopped` / `waits for Retry`) |
| A server shows `failed: exited after it was ready` | Its command crashed after it got ready (before, such steps stayed "ready"). Read `last_line`; add `restart: on_failure` to bring it back by itself |
| Step stays `waiting` with `restart N in …` | Its restart policy waits for the backoff; it gives up after `max` (then `failed … (gave up after N restarts)`). Stop All or Retry interrupts it |
| `unhealthy: …` | The health check failed `retries` times in a row; the message is the probe's (exit code and last line, or `… answered 503`). Raise `start_period` for slow starts |
| HTTP readiness never passes | The URL must answer where the step runs (inside the container for dev containers) with 200–399, or set `status`; `https://` needs `curl` there |
| Step `skipped` unexpectedly | Its `if` was false (the reason quotes it) or a step it needs ready was skipped; wait for that one `completed` to run anyway |
| `env_file …: No such file` | Paths are relative to the step's `cwd` (else its project root), where the step runs; `required: false` for optional files |
| Outcome `failed` though the last step is ready | Finished with failures: some other step failed or is blocked. See each step's `reason`/`detail` |
| Artifact `missing` | The path is relative to the step's `cwd` (else its project root), checked where the step runs; compare `resolved` with where the file was written; Check again after writing it later |
| A step fails with `` `${steps.x.artifacts.y}`: the artifacts of `x` could not be resolved `` | `x`'s environment could not be read or its path uses an unknown variable (`artifacts_error` of `x`) |

---

## 11. Limits

- Files of the old JSON format do not run until converted (§2).
- YAML: one document per file; merge keys (`<<`) are shallow (an `env` in the step replaces the anchor's).
- `open_urls` matches complete lines only; headless reports URLs instead of opening them.
- One run per workflow per window; Run All restarts it (asking for its inputs again).
- Inputs are plain values (no file pickers, multi-select or inputs depending on other inputs); values cannot contain `${`.
- Steps without a project run on the editor's machine; headless on a WSL project runs them on that machine (where next_code runs), not inside WSL.
- `${env:…}` reads the environment where the step runs, at start; changes later are not seen.
- `${steps.<id>.artifacts.…}` paths are those of the producing step's machine/container; nothing is copied between machines.
- Artifacts are checked for existence only (not size or freshness); a file left over from an earlier run counts as present.
- There is no per-step Stop: Stop All stops everything (a step can be Restarted).
- Groups only group: no `run`/`check`/`ready` of their own, no artifacts;
  a step cannot depend on a step inside another group (depend on the
  group). The miniature of a collapsed group draws nested groups as
  single boxes.

---

## 12. Planned (designed, not supported yet)

These are designed into the format so adding them will not change it
again. The shapes follow the tools they come from.

| Feature | Shape (planned) | From |
|---|---|---|
| Daemons | several conditions in `ready`, all of them: `ready: {exit: true, port: 5432}` for `docker compose up -d` (the command exits 0, then the port listens) | process-compose `is_daemon` |
| Partial runs | headless `workflow_run` with `"steps": ["api"]` (and the GUI's Run Step With Dependencies): only those steps and what they depend on; per-step Stop | `docker compose up api` |
| Step outputs | named groups in `ready.output` (`'listening on port (?<port>\d+)'`) read as `${steps.api.outputs.port}` | GitHub Actions step outputs |

---

## 13. Coming from other tools

| You know | Here |
|---|---|
| Compose `services:` / process-compose `processes:` | `steps:` (one per command, with `project` for where it runs) |
| `depends_on: {db: {condition: service_healthy}}` | `depends_on: {db: ready}` (`ready` covers healthy/started-and-ready) |
| `condition: service_completed_successfully` | `depends_on: [migrate]` with `ready: {exit: true}` (the default with `run`) on `migrate` |
| process-compose `process_completed` | `depends_on: {eval: completed}` |
| process-compose `ready_log_line` | `ready: {output: <regex>}` |
| Compose `stop_grace_period` | `stop_grace_period` (same durations) |
| Compose `working_dir`, `environment` | `cwd`, `env` (a mapping) |
| Compose `x-` keys and anchors | the same |
| GitHub Actions `jobs.<id>.needs` | `depends_on` |
| GitHub Actions `if: always()` | `depends_on: {x: completed}` (per dependency) |
| GitHub Actions `timeout-minutes` | `timeout: 30m` |
| GitHub Actions `workflow_dispatch.inputs` | `inputs` (same `type`, `default`, `required`, `options`, `description`) and `${inputs.<id>}` |
| VS Code tasks `dependsOn`, `isBackground` + `endsPattern` | `depends_on`, `ready: {output: …}` |
| VS Code `${workspaceFolder}`, `${env:NAME}`, `${input:x}` | `${workspaceFolder}`, `${env:NAME}`, `${inputs.x}` |
| Taskfile `preconditions: [{sh, msg}]` | `check` + `on_fail` |
| Compose `restart: on-failure` / process-compose `availability` | `restart: on_failure` or `{policy, max, backoff}` |
| Compose `healthcheck: {test, interval, timeout, retries, start_period}` | `healthcheck` with the same keys (`test` a command line; or `http`, `port`) |
| process-compose `readiness_probe.http_get` | `ready: {http: …}` |
| Compose / Taskfile `env_file`, `dotenv` | `env_file` (string or list; `{path, required: false}`) |
| GitHub Actions `if:` with `success()`, `failure()`, `always()` | `if:` (no `${{ }}`; `inputs.<id>`, `steps.<id>.status`) |
