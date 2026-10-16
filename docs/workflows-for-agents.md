# Workflows: a manual for agents

A **workflow** is one JSONC file that brings up a multi-project development
environment: preconditions, commands in terminals of several projects
(local folders, WSL, dev containers), ordering with readiness conditions,
browser links and manual gates — and pipelines such as "evaluate, then
always report", with the files they produce linked. next_code runs it in
the GUI (a workflow tab with a live graph) or headless (JSON lines on
stdin/stdout).

This manual is for agents that **write** workflow files and **operate**
them. Everything here matches the parser (`crates/workflow/src/file.rs`),
the scheduler (`crates/workflow/src/run.rs`) and the headless commands
(`crates/ui/src/headless/workflows.rs`); the transcripts are real.

---

## 1. Quick start

```jsonc
// .next_code/workflows/dev.jsonc
{
  "name": "Dev stack",
  "steps": [
    { "id": "vpn", "check": "test -d /tmp", "onFail": "Connect the VPN, then Retry" },
    { "id": "api", "needs": ["vpn"], "project": { "devcontainer": "/home/me/src/api" },
      "run": "uv run uvicorn app:app --port 8000", "ready": { "port": 8000 }, "timeout_s": 600 },
    { "id": "login", "needs": ["api"], "ready": { "manual": "Log in in the browser, then Continue" } },
    { "id": "seed", "needs": ["login"], "project": { "local": "/home/me/src/api" },
      "run": "uv run python seed.py" }
  ]
}
```

Validate it, then run it:

```json
{"id": 1, "cmd": "workflows_list"}
{"id": 2, "cmd": "workflow_run", "name": "Dev stack"}
```

---

## 2. Where files live

| Location | Scope |
|---|---|
| `<config>/next_code/workflows/*.json` or `*.jsonc` (Windows: `%APPDATA%\next_code\workflows\`; Linux: `~/.config/next_code/workflows/`) | Personal; use for stacks spanning several repos |
| `<project>/.next_code/workflows/*.json[c]` of every **open** project | Shared with the repo; found only while that project is open |

Files are re-read before every headless workflow command and before every
Run All, so edit freely between runs. Prefer `.jsonc` (comments and
trailing commas are allowed in both).

---

## 3. File format

Top level: `{ "name": string?, "steps": [Step, ...] }`. `name` defaults to
the file name without extension. Other top-level keys (`$schema`,
`description`, …) are ignored.

### Step fields

Unknown keys are **errors** (they catch typos). Allowed keys:

| Field | Type | Meaning |
|---|---|---|
| `id` | string, **required**, unique | Referenced by `needs`, `after` and `${steps.<id>.…}`; used in commands (`step`) |
| `title` | string | Shown in the UI; default `id` |
| `needs` | string or [string] | Step ids that must be **ready** first. No `needs`/`after` = starts immediately. Independent steps run in parallel |
| `after` | string or [string] | Step ids that must have **finished** first: ready **or failed** (§5.1). Use it for a step that must run even when the other one failed (a report after an evaluation). An id may not be in both `needs` and `after`, nor twice in `after` |
| `project` | see §3.1 | Where the step runs. Omitted = the machine running next_code, no project tab |
| `run` | string | Command line run in a terminal of that project (§4) |
| `terminal` | string | Terminal tab name; default `title`, else `id`. Needs `run` |
| `check` | string | Command run as a plain process **before** `run`; exit 0 passes (§4) |
| `onFail` | string | Message shown when `check` fails; the step then waits for Retry instead of failing the workflow. Needs `check` |
| `ready` | see §3.2 | When the step counts as done |
| `timeout_s` | positive integer | Fails the step if not ready this many seconds after it started (its command is stopped, with its grace) |
| `stop_grace_s` | integer 1–3600, default 3 | Seconds the command gets after Ctrl+C before it is killed, on every stop: Stop All, Restart, `timeout_s`, Run again, the workflow failing. Give it to anything that checkpoints or drains on Ctrl+C. Needs `run` |
| `artifacts` | array, see §3.3 | Files and links the step produces: checked when it finishes, linked in the UI, readable by later steps as `${steps.<id>.artifacts.…}` |
| `openUrls` | bool | http(s) URLs printed by `run` open in the browser (once each) until ready + 3 s. Needs `run` |
| `env` | object of string/number/bool | Extra environment for `run`/`check` |
| `cwd` | string | Working directory, relative to the project root (home folder without a project) |

### 3.1 `project` shapes

| Shape | Example |
|---|---|
| Local folder | `{ "local": "C:/src/api" }` or `{ "local": "/home/me/api" }` |
| WSL folder | `{ "wsl": "Ubuntu", "path": "~/src/tools" }` |
| Dev container of a local folder | `{ "devcontainer": "C:/src/api" }` |
| Dev container of a WSL folder | `{ "devcontainer": { "wsl": "Ubuntu", "path": "~/src/worker" } }` |
| Specific devcontainer.json | `{ "devcontainer": "C:/src/api", "config": "C:/src/api/.devcontainer/gpu/devcontainer.json" }` |
| Typed location string | `"Ubuntu:~/p"`, `"\\\\wsl.localhost\\Ubuntu\\home\\me\\p"`, `"devcontainer:C:\\src\\p"` |
| next_code's own shape (copy from `recent.json`) | `{ "Local": "C:\\src\\api" }`, `{ "Wsl": { "distro": "Ubuntu", "path": "/home/me/p" } }`, `{ "DevContainer": { "folder": { "Local": "C:\\src\\api" }, "config": null } }` |

Tip: to get a project's exact location, copy its entry from
`%LOCALAPPDATA%\next_code\recent.json` (Windows) / `~/.local/share/next_code/recent.json`.

Steps with the same location share one project tab (and one container).

### 3.2 `ready` shapes

Exactly one key:

| Shape | Ready when | Use for |
|---|---|---|
| `{ "exit": true }` (default with `run`) | `run` exited with code 0 (without `run`: the check passed) | one-shot setup: migrations, installs, logins that exit |
| `{ "output": "<regex>" }` | the regex matches the command's output (ANSI codes stripped, across chunks and lines; Rust regex syntax) | servers that print a "ready" line |
| `{ "port": N }` | something listens on TCP port N where the step runs | servers with a known port (most robust) |
| `{ "manual": "<prompt>" }` | someone sends Continue | approvals, browser confirmations, anything only a human can verify |
| `{ "delay_ms": N }` | N ms after the command started (or the check passed) | last resort; avoid |

### 3.3 `artifacts` entries

| Entry | Meaning |
|---|---|
| `"out/metrics.json"` | A file, relative to the step's `cwd` (else its project root, else the home folder); absolute paths stay |
| `"https://dash.example/run/${env:RUN_ID}"` | A link (a string starting with `http://` or `https://`) |
| `{ "label": "report", "path": "out/report.html" }` | A labelled file |
| `{ "label": "board", "url": "http://localhost:6006" }` | A labelled link |

- Paths and URLs may use the variables of §4 (`${workspaceFolder}`,
  `${env:NAME}`, `${steps.<id>.…}`); they are expanded where the step runs.
- Labels are optional, unique within the step, not a number (numbers
  address artifacts by position) and without `}`.
- When the step finishes (ready, failed or stopped, once it got to where
  it runs) next_code expands the paths, makes them absolute and checks
  which files exist **where the step runs** (inside WSL or the container
  for remote projects). Steps waiting for it (`needs`/`after`) start only
  after that check. **Check again** (GUI) / `workflow_check_artifacts`
  (headless) repeats it on demand.
- A missing file is not an error: it shows as missing (dimmed in the UI,
  `"exists": false` headless).

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
  runs**), and the step-result variables below. Put secrets in the
  environment and reference them as `${env:NAME}`; never write them in the
  file.
- JSON escaping: backslashes double (`"C:\\src"`, regex `"ready in \\d+ ms"`).
- A `run` command that keeps running (a server) must use `output` or
  `port` readiness; with the default `exit` it never becomes ready.
- A `run` command that exits **before** it is ready fails the step
  (`exited before it was ready (exit code N)`), except with `manual`.
- Stops send Ctrl+C to the command's terminal (its pty: the foreground
  process group on Linux/WSL/containers; a console Ctrl+C on Windows).
  Trap `SIGINT` (Python: `KeyboardInterrupt`; PowerShell: `try {…}
  finally {…}`) to checkpoint, and set `stop_grace_s` to how long that
  takes. The terminal echoes `^C` into the output.

### 4.1 Step-result variables

| Variable | Value |
|---|---|
| `${steps.<id>.status}` | `ready`, `failed` or `stopped` |
| `${steps.<id>.exit_code}` | the command's exit code; empty if it has none (no `run`, still running, killed) |
| `${steps.<id>.artifacts.<label>}` | that artifact's resolved path (or URL) |
| `${steps.<id>.artifacts.<index>}` | the same by position in `artifacts`, from 0 |

- Only for steps listed in this step's `needs` or `after`; anything else
  (another step, an unknown field, a label that step does not declare, an
  index past its artifacts) is a validation error pointing at the
  variable.
- Replaced **when the step starts**, from what the other step left
  (with `needs` it is ready, with `after` ready or failed). Restarting the
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
pending → waiting (for needs / after) → starting (opening project / running check)
        → running (command runs, or waiting for port/output/delay/Continue)
        → ready | failed | stopped
running/ready with a live command → stopping (Ctrl+C sent; grace running) → stopped | failed
```

`stopping` lasts until the command exits (which ends it early) or its
`stop_grace_s` runs out (then it is killed); `detail` counts down, e.g.
`stopping (12 s left)`, `restarting; stopping (2 s left)`, `timed out
after 600 s; stopping (3 s left)`.

Workflow outcomes: `idle`, `running`, `ready` (every step ready),
`failed`, `stopped`. **`failed`** means either a check without `onFail`
failed (everything was stopped), or the run **finished with failures**:
nothing is pending, waiting (unblocked), starting, running or stopping
any more, and not every step is ready. A step failed by a check with
`onFail` counts as failed here: the run waits for a human, so the outcome
is `failed` with that step's reason visible. Retry or Restart puts the
outcome back to `running`. In the GUI the chip says **Failed**.

### 5.1 Ordering: `needs` and `after`

| The other step is | `needs` it | `after` it |
|---|---|---|
| ready | starts | starts |
| failed (command, timeout, project) | blocked: `<id> did not get ready` | starts |
| failed by a check with `onFail` (waits for Retry) | blocked: `<id> did not get ready` | blocked: `<id> waits for Retry` |
| stopped | blocked: `<id> did not get ready` | blocked: `<id> was stopped` |
| blocked itself | blocked: `<id> can never run` | blocked: `<id> can never run` |
| anything else (incl. stopping, artifacts being checked) | waits: `needs <id>` | waits: `after <id>` |

Blocked steps stay `waiting` with that `detail` (headless: `"blocked":
true`); several reasons are joined with `; `. They start by themselves
once Retry/Restart unblocks them.

Failure rules:

| Event | Effect |
|---|---|
| `check` fails, step has `onFail` | step `failed` with reason = the `onFail` text; waits for Retry; others continue |
| `check` fails, no `onFail` | the **workflow fails**: everything it started is stopped (each with its grace) |
| `run` exits non-zero (ready `exit`) | step `failed` (`failed (exit code N)`); `after` steps run |
| command exits before ready (output/port/delay) | step `failed` (`exited before it was ready …`) |
| `timeout_s` reached | step `stopping` (detail `timed out after N s; stopping …`), then `failed` (`timed out after N s`) once the command is gone |
| a step failed | `needs` dependents are blocked (`blocked: <id> did not get ready`); `after` dependents run; independent steps keep going; when nothing can progress the outcome is `failed` |

Controls: **Restart** a step (its command, if still running, is stopped
with its grace first; then it reruns after its needs in the same
terminal), **Retry** (reruns every failed step), **Continue** (manual
steps only), **Stop** (Ctrl+C to every command the run started, each
killed after its `stop_grace_s`; a **second Stop while steps are still
stopping kills them at once**; project tabs and containers stay), **Run**
(starts afresh; a running run of the same workflow is stopped first with
each step's grace, and a step whose old command is still stopping waits
for it — detail `waiting for its previous command to end` — before
starting in the same terminal).

---

## 6. Designing a workflow

Recipes:

- **Precondition (VPN, credentials, Docker running):** a step with only
  `check` and an `onFail` telling the human what to do. Make everything
  else `needs` it.
  ```jsonc
  { "id": "vpn", "check": "if (-not (Test-NetConnection git.corp -Port 443 -InformationLevel Quiet)) { exit 1 }",
    "onFail": "Connect the VPN, then Retry" }
  ```
- **Server:** `run` + `port` (preferred) or `output`, plus a generous
  `timeout_s` when a container may need building.
- **Browser login (device code / SSO):** `run` the login with
  `"openUrls": true`; ready by `exit` if the CLI exits after login, else by
  an `output` regex of its success line. If success can only be confirmed
  by a human, add a separate `manual` step after it.
- **Fan-out / fan-in:** several steps with the same `needs`, then one
  step needing all of them.
- **Per-project terminals:** give `terminal` names that a human will
  recognise in the project's Terminal panel.
- **Evaluate, then always report:** the evaluation checkpoints on Ctrl+C
  (give it the time with `stop_grace_s`) and declares what it writes; the
  report runs `after` it (also when it exits 1) and reads its exit code
  and files. The evaluation's failure stays visible: it shows failed, the
  report ready, the outcome `failed` (finished with failures).
  ```jsonc
  { "id": "eval", "project": { "devcontainer": "C:/src/ml" },
    "run": "uv run python eval.py --out out", "stop_grace_s": 120, "timeout_s": 7200,
    "artifacts": [{ "label": "metrics", "path": "out/metrics.json" },
                  { "label": "board", "url": "http://localhost:6006" }] },
  { "id": "report", "after": "eval", "project": { "devcontainer": "C:/src/ml" },
    "run": "uv run python report.py --eval-exit '${steps.eval.exit_code}' --metrics '${steps.eval.artifacts.metrics}'",
    "artifacts": [{ "label": "report", "path": "out/report.html" }] }
  ```
  `eval.py` should catch `KeyboardInterrupt`, write its checkpoint and
  exit; a timeout or Stop then waits up to 120 s for it. A `report` step
  that must not run after a **stopped** evaluation needs nothing more
  (`after` blocks on stopped steps).

Checklist before handing a workflow over:

1. Every `id` unique, short, lowercase (`api`, `mcp`, `login`).
2. Every long-running `run` has `output` or `port` readiness.
3. Every network/credential precondition is a `check` with `onFail`.
4. Steps that can take minutes (container build, first `uv sync`) have `timeout_s`.
5. Commands that checkpoint or drain on Ctrl+C have `stop_grace_s`.
6. No secrets in the file; use `${env:NAME}`.
7. Steps that must run whatever happened before use `after`, not `needs`.
8. `workflows_list` reports `"error": null` for it.

---

## 7. Validation loop

`workflows_list` parses every file and returns, per workflow, either the
`steps` or an `error` (one problem per line):

```json
{"id":"/tmp/wfh|/tmp/wfh/.next_code/workflows/bad.jsonc","name":"bad","path":"/tmp/wfh/.next_code/workflows/bad.jsonc","source":"/tmp/wfh","error":"step 2: unknown key `colour` (known: id, title, needs, after, project, run, terminal, check, onFail, ready, timeout_s, openUrls, env, cwd, stop_grace_s, artifacts)","steps":[],"outcome":null}
```

Notes:

- An **invalid** workflow is listed under its **file name** (`bad`), not its
  `name`, and `workflow_run` on it fails with `<name> cannot run: <error>`.
  Address it by `path` to be safe.
- Problems are reported in two passes: per-step problems first (unknown
  keys, wrong types, missing `id`, `` `a`: `after` lists `b` twice ``,
  `` `b` is in both `needs` and `after` (keep one) ``, `stop_grace_s`
  outside 1–3600, bad `artifacts` entries, duplicate or numeric labels,
  …); only when every step is valid are graph problems checked
  (`` `a` needs unknown step `b` ``, `` `a` runs after unknown step `b` ``,
  duplicate ids, `${steps.…}` variables — e.g. `` `report`:
  `${steps.x.status}` refers to `x`, which is not in this step's `needs`
  or `after` `` — and `dependency cycle: a → c → b → a`, which follows
  `needs` and `after` alike). Re-list after each fix.
- In the GUI the same problems appear as a banner with line and column
  over the graph while the file is edited.

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
`workflow_status`. URLs from `openUrls` are **reported** as events, not
opened: open or relay them yourself.

### Commands

Workflows are named by `"name"`, `"path"` or `"id"` (from
`workflows_list`). Steps by `"step": "<step id>"`.

| Command | Params | Result |
|---|---|---|
| `workflows_list` | – | array of `{id, name, path, source, error, steps[{id, title, needs, after, project, run, check, ready, stop_grace_s, artifacts[{label, path} or {label, url}]}], outcome}` |
| `workflow_run` | name/path/id | `{run, id}`; starts afresh (a run of it still going is stopped first, each command with its grace) |
| `workflow_status` | name/path/id | `{run, workflow, outcome, steps[{id, title, status, reason, detail, blocked, last_line, check_output, exit_code, exited, elapsed_ms, location, terminal, artifacts[{label, name, path or url, resolved, exists}], artifacts_error}]}` (error if it has not run). `resolved`/`exists` are `null` until the artifacts were checked; `artifacts_error` says why they could not be |
| `workflow_continue` | name, `step` | `null`; error `the step is not waiting for Continue` unless it is a running `manual` step |
| `workflow_restart` | name, `step` | bool (its old command is stopped with its grace first) |
| `workflow_retry` | name | bool (reruns failed steps) |
| `workflow_stop` | name | bool (Ctrl+C, kill after each step's `stop_grace_s`; again while steps are `stopping`: kill now) |
| `workflow_check_artifacts` | name, `step` | bool: `false` if the step has no artifacts or never got to where it runs; else they are looked up again — read `workflow_status` after a moment |

### Events

`workflow_event` with `type`:

| type | fields |
|---|---|
| `step` | `run`, `workflow`, `step`, `status` (incl. `stopping`), `reason` (failed only), `detail` (what it waits for, e.g. `needs svc`, `after eval`, `waiting for output /…/`, the manual prompt, `blocked: …`, `stopping (20 s left)`) |
| `outcome` | `run`, `workflow`, `outcome` |
| `url` | `run`, `workflow`, `step`, `url` |

Events carry `run`: after a new `workflow_run` the replaced run may still
report its steps going `stopping` → `stopped`; ignore runs you did not
start last.

### Real transcript (abridged)

```
<< {"root":"/tmp/wfh","remote":false,"event":"ready"}
>> {"id": 2, "cmd": "workflow_run", "name": "Demo"}
<< {"id":2,"ok":true,"result":{"run":1,"id":"/tmp/wfh|/tmp/wfh/.next_code/workflows/demo.jsonc"}}
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
vpn    failed   reason "Connect the VPN, then Retry"     (check failed, onFail set)
boom   failed   reason "failed (exit code 3)"            exit 3
after  waiting  detail "blocked: boom did not get ready" blocked true
report ready                                            (runs `after` boom)
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
                    failed or detail blocked: …) or a check without onFail
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

- **Run > New Workflow…** writes a commented template to the user folder
  and opens it; **Run > Open Workflow…** lists all workflows (user folder
  and open projects); palette **Run Workflow: <name>** opens and runs.
- A workflow opens as a tab (◈) next to project tabs: the file on the left
  (Ctrl+S saves), the dependency graph on the right (columns by
  dependency depth, live status colours, problems as a banner with line and
  column while you type). `needs` edges are solid (green once the need is
  ready); `after` edges are **dashed** (lit once the step finished, ready
  or failed) and the card's kind line says `after`. A card with artifacts
  shows `↗ N`; a stopping card shows its countdown.
- Click a card: the editor jumps to the step; the details strip shows the
  state, reason, last output lines, **Artifacts** (each a link: a file
  opens in an editor tab of the step's project — a file on this machine in
  the project tab holding it, else its folder as a new tab; a URL in the
  browser; missing files are dimmed and marked `missing`; **Check again**
  looks them up again) and Terminal / Continue / Restart / Retry.
- Toolbar: Run All (Shift+F10; saves first), Stop All (Ctrl+F2), Retry
  Failed. Graph: opens fitted; Ctrl+scroll zooms; − / % / + / Fit in the
  corner; drag or scroll pans.
- Agents with UI automation (`next_code --automation`, see README) can use
  the same: the command palette (`Ctrl+Shift+P`) and clicks.

---

## 10. Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `no workflow \`X\`` | Not discovered: the file is not in a scanned folder, its project is not open, or it is invalid and listed under its file name. Run `workflows_list` |
| Step stuck `running` with `waiting for output /…/` | The regex does not match what is printed (check `last_line`); escape backslashes twice in JSON; the program buffers stdout (e.g. Python: `python -u` or `PYTHONUNBUFFERED=1` in `env`) |
| Step stuck waiting for a port | Server binds another port or only inside a network namespace; check `last_line`; in containers the port must listen inside the container |
| `exited before it was ready` | The command ended (crash, wrong directory, missing tool). Read `last_line`; set `cwd`; tools from `~/.local/bin` need the login shell (fine in `run`, not in `check`) |
| `failed (exit code 127)` | Command not found where the step runs (wrong shell or PATH) |
| Check passes on Linux but not on Windows | Step without `project` runs PowerShell on Windows: rewrite the check in PowerShell |
| Dev container step slow the first time | Image build/pull; give `timeout_s` ≥ 600 |
| Step `stopping` for long, then `stopped`/`failed` | It ignores or slowly handles Ctrl+C; it is killed when `stop_grace_s` runs out. Lower `stop_grace_s`, or Stop again to kill now |
| Checkpoint not written on Stop | Killed before it finished: raise `stop_grace_s`. The program itself must handle Ctrl+C (a `trap … INT` in a `sh` script, Python's `KeyboardInterrupt`, PowerShell `finally`); try Ctrl+C in a terminal of that project to see what it does |
| Report step never runs | It `needs` the evaluation (ready only); use `after`. Or the evaluation was stopped / waits for Retry (`blocked: … was stopped` / `waits for Retry`) |
| Outcome `failed` though the last step is ready | Finished with failures: some other step failed or is blocked. See each step's `reason`/`detail` |
| Artifact `missing` | The path is relative to the step's `cwd` (else its project root), checked where the step runs; compare `resolved` with where the file was written; Check again after writing it later |
| A step fails with `` `${steps.x.artifacts.y}`: the artifacts of `x` could not be resolved `` | `x`'s environment could not be read or its path uses an unknown variable (`artifacts_error` of `x`) |

---

## 11. Limits

- JSONC only (no YAML).
- `openUrls` matches complete lines only; headless reports URLs instead of opening them.
- One run per workflow per window; Run All restarts it.
- Steps without a project run on the editor's machine; headless on a WSL project runs them on that machine (where next_code runs), not inside WSL.
- `${env:…}` reads the environment where the step runs, at start; changes later are not seen.
- `${steps.<id>.artifacts.…}` paths are those of the producing step's machine/container; nothing is copied between machines.
- Artifacts are checked for existence only (not size or freshness); a file left over from an earlier run counts as present.
- There is no per-step Stop: Stop All stops everything (a step can be Restarted).
