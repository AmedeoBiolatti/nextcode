//! `next_code --headless`: the editor's project services without a window,
//! driven by JSON lines on stdin and reporting JSON lines on stdout, for
//! automation and end-to-end tests. It uses the same `Project` (local or WSL
//! agent) and `LspStore` as the GUI.
//!
//! Each request is one line: `{"id": 1, "cmd": "read", "path": "src/main.rs"}`.
//! Each gets one reply line, `{"id": 1, "ok": true, "result": ...}` or
//! `{"id": 1, "ok": false, "error": "..."}`; replies of slow commands may
//! overtake earlier ones. Unsolicited lines carry an `"event"` field.
//! Paths are absolute or relative to the project root: the first folder of
//! a multi-root workspace, or the folder named by the request's `root`
//! (index or name) for any command taking `path`, `paths`, `dir`,
//! `new_path` or `file`. Git commands use the repository of that folder
//! (default: the first); an absolute `path` of `git_diff`,
//! `git_apply_hunk`, `git_line_changes`, `git_stage_line_change`,
//! `git_blame` or `git_shelf_diff` picks the repository of the folder
//! holding it instead.
//!
//! | `cmd` | parameters | result |
//! |---|---|---|
//! | `info` | | `root` (the first folder), `roots: [{index, name, path}]`, `workspace_file` (`null` if none), `remote`, `location` |
//! | `list_dir` | `path`? (default: the folder) | `[{name, is_dir}]` |
//! | `list_files` | `root`? | folder-relative paths of that folder; without `root`, a multi-root workspace lists every folder's as `[{root, path}]` (`root`: folder index) |
//! | `read` | `path` | `{text}` |
//! | `write` | `path`, `text` | `null`; open documents are synced |
//! | `stat` | `path` | `{kind: "file" \| "dir" \| null}` |
//! | `create_file` / `create_dir` | `path` | `{path}`; missing parent folders are created; fails if something is there or the name is invalid where the project lives |
//! | `create_file` with `type` | `type` (New File's types: `auto`, `text`, `python`, `jupyter`, `marimo`, `markdown`, `json`, `yaml`, `toml`, `rust`, `javascript`, `typescript`, `tsx`, `html`, `css`, `shell`, `powershell`, `c`, `cpp`, `go`, `sql`, `dockerfile`, `devcontainer`, `workflow`, `gitignore`), `path`? (the typed name: its extension is added when missing; an existing folder, or none for the root, leaves the name empty) | `{path}` of the file, with its starter text (an empty notebook with one code cell, a marimo app, the workflow template and its schema, a dev container `name` + `image`, a shell's `#!` line): `dockerfile` → `Dockerfile` or `<name>.Dockerfile`, `gitignore` → `<path>/.gitignore`, `devcontainer` → `.devcontainer/[<name>/]devcontainer.json` in the folder's root, `workflow` → `.next_code/workflows/<name>.yaml` in the project root (no name: `workflow.yaml`, `workflow-2.yaml`, …) |
//! | `rename_path` | `path`, `new_path` (rename or move; the target must not exist, a folder cannot go into itself) | `{path, edits: [{server, applied, changed, failure}], problems}`: running language servers that registered for `workspace/willRenameFiles` (e.g. rust-analyzer) edit their references first (`problems`: servers that failed or timed out); open documents follow; `didRenameFiles` after |
//! | `copy_path` | `path`, `dir`? (default: its folder) | `{path}` of the copy: the same name if free in `dir`, else `name copy.ext`, `name copy 2.ext`, … |
//! | `delete_path` | `path` or `paths`, `trash`? (Recycle Bin, local Windows projects only) | `null`; folders go with their contents, open documents close |
//! | `search` | `pattern`, `regex`?, `case_sensitive`?, `whole_word`?, `include`?, `exclude`? | `{files: [{path, lines: [{line, column, text}]}], matches, files_searched, limit_hit}`; every folder is searched, and in a multi-root workspace files also carry `root` (folder index) and `relative` (folder-relative path) |
//! | `replace` | `pattern`, `replacement`, `regex`?, `case_sensitive`?, `whole_word`?, `include`?, `exclude`?, `paths`? (only files under these) | `{files: [{path, replacements}], replacements, skipped: [{path, reason}], failed: [{path, error}], limit_hit}` (`files` with `root` and `relative` as for `search`); `$1`/`${name}` expand in regex mode |
//! | `watch` / `unwatch` | | starts/stops `files_changed` events |
//! | `open` | `path`, `timeout_ms`? | starts the language server, opens the document: `{language, server, encoding}` |
//! | `close` | `path` | `null` |
//! | `diagnostics` | `path`?, `wait_ms`? | `[{path, diagnostics}]` (raw LSP); `wait_ms` waits until the server published for `path` and stayed idle 1 s |
//! | `definition` / `declaration` / `type_definition` / `implementation` | `path` (open), `line`, `character` (LSP position) | `[{path, range}]` |
//! | `incoming_calls` / `outgoing_calls` | `path` (open), `line`, `character` | `[{name, kind, detail, path, range, selection_range, from_ranges}]`: the callers / callees of the first call hierarchy item there (`kind`: LSP `SymbolKind`; `from_ranges`: call sites, in the caller's file for incoming, in `path` for outgoing); `[]` when there is none |
//! | `document_symbols` | `path` (open) | `[{name, detail, kind, kind_name, depth, parent, range, selection_range}]`: the symbol tree in pre-order (siblings by position; flat server answers nested by range containment); `kind` is the LSP `SymbolKind` number, `parent` an index into the list |
//! | `workspace_symbols` | `query` | `[{name, kind, container, path, range}]` from every running server that lists workspace symbols (`range` `null` when the server gave only the file) |
//! | `semantic_tokens` | `path` (open) | `[{line, start, end, text, scope, modifiers}]`: the server's tokens as the editor decodes them; `start`/`end` are UTF-8 byte columns, `scope` is the highlight scope (`null`: modifiers only) |
//! | `signature_help` | `path` (open), `line`, `character` | `null` outside a call, else `{active_signature, active_parameter, signatures: [{label, parameters, active}]}`: `parameters` are the parameters' label texts, `active` the highlighted one's byte range in `label` (as the popover shows it) or `null` |
//! | `inlay_hints` | `path` (open), `start_line`? (0), `end_line`? (last line) | `[{line, character, label, kind, padding_left, padding_right}]` (LSP position; `kind`: `"type"`, `"parameter"` or `null`) |
//! | `spell_check` | `path` and/or `text`, `language`? (language id; default: from `path`) | `{misspellings: [{word, start, end, line, column, suggestions}]}`: unknown words as the editor underlines them (`start`/`end` byte offsets, `line`/`column` LSP position), with up to 5 corrections; `text` is checked as the file at `path` (unsaved contents) |
//! | `rename` | `path` (open), `line`, `character`, `new_name` | `{applied, changed, failure}`: the server's workspace edit, written to disk (open documents synced); `changed` lists edited/created/renamed files, `failure` `{index, path, message}` or `null` |
//! | `code_actions` | `path` (open), `line`, `character`, `end_line`?, `end_character`? | `[{index, title, kind, preferred}]`, quick fixes first; the diagnostics touching the range are sent along |
//! | `apply_code_action` | `index` (from the last `code_actions`) | `{applied, changed, failure}` after resolving the action, applying its edit and running its command |
//! | `format` | `path` | `{text, changed}`: formats like Format Document (settings' `formatter`, else the language server, which needs the file `open`) and writes the file |
//! | `exec` | `command`, `timeout_ms`? | `{code, stdout, stderr}`; runs in the project root where it lives |
//! | `git_status` | `all`? (every folder's repository) | `{root, prefix, branch, unborn, upstream, ahead, behind, entries: [{path, orig_path, staged, unstaged}]}`; with `all`, `[{root, repository_root, status}]`: one per repository (`root`: the first folder in it) |
//! | `git_stage` / `git_unstage` / `git_discard` | `paths` (repository-relative) | `null`; discarding deletes untracked files |
//! | `git_commit` | `message`, `amend`? | `{summary}` (`<hash> <subject>`) |
//! | `git_diff` | `path` (repository-relative); `staged`?, or `old` (revision) and `new`? (revision or `"worktree"`, the default); `whole_file`? | `{rows, added, removed, binary}`; index diffs come from `git diff` and include `hunk` rows `{index, header}` |
//! | `git_apply_hunk` | `path`, `hunk` (index from `git_diff`), `action` (`"stage"` \| `"unstage"` \| `"discard"`), `staged`?, `whole_file`? (as for the `git_diff` that numbered the hunk) | `null` |
//! | `git_line_changes` | `path`, `index`? (compare with the index instead of HEAD), `contents`? (the text to compare; the working-tree file by default) | `{base: "blob" \| "new_file" \| "none", changes: [{kind: "added" \| "modified" \| "deleted", new: {start, end}, old: {start, end}}]}` (0-based lines; the editor's change markers) |
//! | `git_stage_line_change` | `path`, `index: true`, `change` (index in `git_line_changes`), `contents`? | `null`; the index gets only that change (Stage Hunk from the editor) |
//! | `git_blame` | `path`, `contents`? (unsaved text: lines git cannot find are not committed) | `{commits: [{id, author, author_mail, time, summary, message}], lines: [commit index \| null]}` |
//! | `git_branch_create` | `name`, `start`? (revision; HEAD), `switch`? (true) | `null` |
//! | `git_switch` | `name` (branch, remote branch: tracked locally, or tag: detached) | `null` |
//! | `git_branch_delete` | `name`, `force`? | `null` |
//! | `git_fetch` / `git_pull` / `git_push` | | `null`; fetch is `--all --prune`, pull is fast-forward only, a first push sets the upstream |
//! | `git_commit_paths` | `paths`, `message`, `amend`? | `{summary}`: commits exactly those files (`git commit --only`, untracked ones added first; a full commit while merging) |
//! | `git_rollback` | `paths` | `null`: back to HEAD; added files leave version control, untracked files are deleted |
//! | `git_add` / `git_ignore` | `paths` | `null`: add untracked files / append `/path` patterns to `.gitignore` |
//! | `git_operation_state` | | `"none"`, `"merging"`, `"rebasing"`, `"cherry_picking"` or `"reverting"` |
//! | `git_merge` | `name` | `"done"` or `"conflicts"` |
//! | `git_rebase` | `onto`, `branch`? (checked out first) | `"done"` or `"conflicts"` |
//! | `git_continue_rebase` / `git_abort` | | `"done"`/`"conflicts"` / `null` (aborts what is in progress) |
//! | `git_branch_rename` | `name`, `new_name` | `null` |
//! | `git_remote_branch_delete` | `name` (`origin/x`) | `null` |
//! | `git_update` | `rebase`? | `"done"` or `"conflicts"`: fetch, then merge (or rebase onto) the upstream |
//! | `git_update_branch` | `name` | `null`: fast-forwards a tracked branch that is not checked out |
//! | `git_branches` | | `{tracking: [{name, upstream, ahead, behind}], recent}` |
//! | `git_push_target` / `git_push_branch` | `name`? (default: current branch) | `{target: {remote, branch, new}, outgoing}` / `{target}` (pushes, `-u` when new) |
//! | `git_worktrees` | | `[{path, head, branch, current, bare, locked, prunable}]` |
//! | `git_worktree_add` | `name` (branch), `create`? (new branch), `start`?, `path`? (default: `<repository>-<branch>` next to it) | `{path}` |
//! | `git_worktree_remove` | `path`, `force`? | `null` |
//! | `git_log` | `limit`? (200), `skip`? (0), `branches`? (revisions; default every branch, remote branch and tag), `all`? (false without `branches`: HEAD only), `text`? (subject, author or hash prefix, case-insensitive), `authors`?, `since`?/`until`? (Unix seconds, commit date), `paths`? | `{commits, graph, next}`: graph rows give each commit's lane and edges (one line when text/author/date filters hide parents); `next` is the `skip` of the following page, `null` at the end |
//! | `git_refs` | | `[{name, kind, target, head}]` |
//! | `git_changes` | `base` (revision), `target`? (revision or `"worktree"`) | `[{path, orig_path, change}]` |
//! | `git_commit_changes` | `commit` | `{commit, changes}` (against its first parent) |
//! | `git_selection_changes` | `newest`, `oldest`? (default `newest`) | `{base, target, changes}`: from `oldest`'s first parent (`"empty"` for a root commit) to `newest` |
//! | `git_commit_details` | `commit` | `{message, branches, pushed}` (full message, local and remote branches containing it, whether a remote branch does) |
//! | `git_cherry_pick` | `commits` (oldest first; no merges) | `{result: "done", empty}` (`empty`: already applied, skipped) or `{result: "conflicts", paths}` |
//! | `git_revert` | `commits` (newest first; a merge only alone, against its first parent) | as `git_cherry_pick` |
//! | `git_reset` | `commit`, `mode`? (`"soft"`, `"mixed"` (default), `"hard"`, `"keep"`) | `null`: moves the current branch |
//! | `git_undo_commit` | | `null`: soft reset of HEAD to its parent; refused when pushed or the first commit |
//! | `git_reword` | `message` | `{summary}`: HEAD's message replaced, its contents and the index kept |
//! | `git_tag` | `name`, `commit`? (HEAD), `message`? (annotated) | `null` |
//! | `git_checkout_revision` | `commit` | `null`: detached HEAD |
//! | `git_patch` | `commits` (oldest first) | `{patch}`: `git format-patch` mailbox (merges have none) |
//! | `git_stash_list` | | `[{index, id, branch, message, date}]`, newest first (`date`: Unix seconds) |
//! | `git_stash_push` | `message`? (empty: git's own), `include_untracked`? (false), `paths`? (default: every change) | `null`; fails when there is nothing to stash |
//! | `git_stash_apply` | `index`? (0), `pop`? (false), `reinstate_index`? (false) | `{result: "done"}` or `{result: "conflicts", paths}` (a conflicting pop keeps the stash) |
//! | `git_stash_drop` | `index` | `null` |
//! | `git_stash_changes` | `index`? (0) | `[{path, orig_path, change, old, new}]`: tracked changes, then untracked files (`change: "untracked"`, `old: "empty"`); `old`/`new` are sides as in `git_diff` |
//! | `git_shelve` | `paths`, `message` (the shelf's name) | `{id, message, date, base, files: [{path, orig_path, change}]}`: written to `.next_code/shelf/<id>/` (`shelf.patch`, `info.json`; the folder ignores itself), then the files go back to HEAD (new files are deleted) |
//! | `git_unshelve` | `shelf` (its `id`; `id` names the request), `remove`? (false) | as `git_stash_apply`; three-way when the patch no longer applies cleanly; conflicts keep the shelf |
//! | `git_shelf_list` | | shelves as `git_shelve` returns them, newest first |
//! | `git_shelf_diff` | `shelf`, `path`, `whole_file`? | as `git_diff`: the shelved file against the version it was shelved on |
//! | `git_shelf_delete` | `shelf` | `null` |
//! | `python_info` | | `null` without `pyproject.toml`, else `{name, version, requires_python, locked_packages, interpreter, environment_python, pinned, uv, in_sync, dependencies: [{name, specifier, kind, locked}], scripts}` |
//! | `python_sync` / `python_lock` | `upgrade`? (lock) | `{code, stdout, stderr}` of `uv sync` / `uv lock` |
//! | `python_add` | `requirement`, `group`? or `optional`? (extra) | `{code, stdout, stderr}` of `uv add` |
//! | `python_remove` | `name`, `group`? or `optional`? | `{code, stdout, stderr}` of `uv remove` |
//! | `python_pin` | `version` | `{code, stdout, stderr}` of `uv python pin` |
//! | `python_interpreters` | | `[{version, implementation, path}]` (`path` null: downloadable) |
//! | `python_test_at` | `path`, `line` (0-based) | pytest node suffix (`TestX::test_y`) or `null` |
//! | `devcontainer_configs` | | `devcontainer.json` paths of the project folder |
//! | `run_configurations` | `file`?, `line`? (0-based; for `${file}`/`${lineNumber}`) | `{configurations: [{name, source, summary, kind, disabled, notes, command}], problems}`: `.next_code/run.json`, `.vscode/launch.json` and automatic ones; `command` is the terminal line Run types (or `{error}`) |
//! | `debug_request` | `name`? (default: the first usable), `file`?, `line`? | the `DebugRequest` (`{name, adapter, request, configuration}`) Debug hands to the debugger; cargo configurations are built first |
//! | `tests_discover` | | `{nodes: [{id, label, kind, tool, parent, depth, children, path, line, run}], roots, errors, results}` (pytest and cargo; `results` holds collection errors) |
//! | `tests_run` | `ids`? (node ids; default: everything) | discovers, runs, `{results: [{id, in_tree, status, message, output, duration}], roots: [{id, summary}], errors, discovery_errors, output}` |
//! | `debug_start` | `request`: `{name, adapter: {type: "python" \| "lldb" \| "custom", command?, args?}, request: "launch" \| "attach", configuration}` | `{state: "starting"}`; one session at a time (a running one is stopped); progress comes as `debug_event`s |
//! | `debug_set_breakpoints` | `path`, `breakpoints`: `[{line (0-based), condition?, hit_condition?, log_message?, enabled?}]` | `null`; replaces the file's breakpoints (sent to a running session; `breakpoints` events report verification) |
//! | `debug_continue` / `debug_step` / `debug_pause` | `kind`? (`over` \| `into` \| `out`, step) | `null` |
//! | `debug_stack` | | `{state, reason, thread_id, threads: [{id, name}], frames: [{index, id, name, path, line, column}], selected_frame}` (0-based lines) |
//! | `debug_select_frame` | `frame` (index) | `null` |
//! | `debug_variables` | `reference`? (a variable's children), else `frame`? (index) and `scope`? (name; first) | `[{name, value, type, reference}]` for `reference`, else `{scope, scopes, variables}` |
//! | `debug_evaluate` | `expression`, `context`? (`repl` \| `watch` \| `hover`) | `{result, type, reference}` in the selected frame |
//! | `debug_output` | | `[{category, text}]` (stdout, stderr, console, important, input, result) |
//! | `debug_stop` | | `null` once the session ended |
//! | `debug_download_codelldb` | | `{path}`: CodeLLDB downloaded where the project lives |
//! | `ports_list` | | `{available, listening: [{port, addresses, pid, command}], forwarded: [{port, host, local_port, address, url, origin, label, connections, process}], error}` after a fresh scan (`origin`: `auto` \| `user` \| `dev_container`; `available` false for local projects) |
//! | `ports_forward` | `port` (number or `"host:port"`), `host`?, `local_port`? (required when given; default: the same port, else the next free one) | `{local_port, url}`; a user port, saved for the project |
//! | `ports_stop` | `port`, `host`? | `null`; an automatically forwarded port stays unforwarded while it keeps listening |
//! | `notebook_open` | `path` (`.ipynb`, or a marimo `.py`; missing: a new notebook), `python`? (the kernel's interpreter; default: the project's, as the editor picks it), `kernel_name`? (a kernelspec) | `{kind: "ipynb" \| "marimo", kernel, cells}`; the notebook stays open (by path) until `notebook_close`; opening it again replaces it |
//! | `notebook_run` | `path`, `cells`? (indices; default: all) | `{kernel, cells}` once nothing runs or is queued: `cells` are those that ran, failed or were skipped by this request (marimo adds stale/never-run ancestors and descendants, in dependency order; Jupyter stops at the first error). The kernel starts on the first run |
//! | `notebook_edit` | `path`, `cell` (index), `source` | the cell (marimo: marks it `stale` if it ran) |
//! | `notebook_insert` | `path`, `index`? (default: the end), `kind`? (`code` \| `markdown` \| `raw`), `source`? | the new cell |
//! | `notebook_delete` | `path`, `cell` | `null` (deleting the last cell leaves an empty code cell) |
//! | `notebook_interrupt` | `path` | `null`: queued cells are dequeued, the running one interrupted |
//! | `notebook_restart` | `path`, `run_all`? | `{kernel, cells}` as `notebook_run` once restarted (and run) |
//! | `notebook_save` | `path` | `null`: written in its format (ipynb with outputs; marimo's layout with signatures recomputed) |
//! | `notebook_close` | `path` | `null` once the kernel process exited |
//! | `notebook_status` | `path` | `{kernel, interpreter, cells}` |
//! | `workflows_list` | | `[{id, name, path, source, error, inputs: [{id, label, type, description, default, required, placeholder, options?: [{value, label}], min?, max?, integer?}], steps: [{id, path, kind: "step", title, depends_on: {"<id>": "ready" \| "completed"}, project, run, check, ready, timeout, stop_grace_period, artifacts: [{label, path or url}], policies: {restart, healthcheck, if, env_file}} or {id, path, kind: "group", title, depends_on, if, project, steps: [...]}], outcome}]`: the user's workflows (`<config>/next_code/workflows/*.yaml`), the project's (`.next_code/workflows/`), those of the settings' `workflows.folders` and `NEXT_CODE_WORKFLOW_FOLDERS`, and files named by an absolute `path` in a workflow command, read again for every workflow command; `path` is the qualified id (`group/step`); `run`/`project` show `${inputs.…}` unexpanded; durations as written in files (`10m`, `3s`); files of the old JSON format are listed with an `error` saying to convert them |
//! | `workflow_schema` | | `{schema, file, modeline}`: the format's JSON Schema (draft-07), the file name next_code keeps a copy under in the user's workflows folder, and the first line that points YAML editors at a copy beside a workflow |
//! | `workflow_convert` | `name`/`path`/`id` of an old-format file (`.json`/`.jsonc`) | `{path, backup, problems}`: writes the YAML file beside it (comments kept; an error if it exists) and renames the old one to `.bak`; `problems` are the converted file's (as the old one had them) |
//! | `workflow_run` | `name`, `path` (or its end) or `id`, `inputs`? (`{"<id>": value}`: JSON types, or strings that parse for numbers and booleans) | `{run, id}`; starts it afresh (a run of it going is stopped first, each command with its step's grace; a step starts once its old command is gone); inputs not given get their defaults (not the GUI form's remembered values); invalid or missing required ones are an error listing each (``input `workers`: must be ≤ 64``); progress comes as `workflow_event`s |
//! | `workflow_status` | `name`/`path`/`id` | `{run, workflow, outcome, inputs, steps: [...]}`, `inputs` the run's values by id; a row per step and group (qualified `id`, `kind`, `parent`): steps `{id, kind, parent, title, status (incl. "skipped"), reason (failed or skipped), detail, blocked, last_line, check_output, exit_code, exited, restarts, health_failures, elapsed_ms, location, terminal, artifacts: [{label, name, path or url, resolved, exists}], artifacts_error}`, groups `{id, kind, parent, title, status, reason, detail, blocked, elapsed_ms, steps}` |
//! | `workflow_stop` | `name`/`path`/`id` | `true` if it had run: Stop All (Ctrl+C, then kill after the step's `stop_grace_period`, default 3s; a second stop kills at once) |
//! | `workflow_continue` | `name`/`path`/`id`, `step` (qualified id) | `null`; an error if the step does not wait for Continue |
//! | `workflow_restart` | `name`/`path`/`id`, `step` | `true`: the step (a group: each of its steps) starts again once its old command is gone (same terminal) |
//! | `workflow_retry` | `name`/`path`/`id`, `step`? | without `step`: `true` if it had run, failed steps start again; with it: whether that step (a group: any of its steps) had failed and starts again |
//! | `workflow_check_artifacts` | `name`/`path`/`id`, `step` | `true` if the step has artifacts and got to where it runs: they are looked up again (see `workflow_status`) |
//! | `shutdown` | | stops language servers, workflow commands and notebook kernels, and exits |
//!
//! Notebook cells are `{index, kind, source, execution_count, status
//! ("idle" \| "queued" \| "running" \| "ok" \| "error"), stale, error (marimo:
//! why it cannot run), outputs: [{output_type, text, name?, mimes?,
//! text_plain?, ename?, evalue?, traceback? (ANSI stripped)}]}`; `kernel` is
//! `{status ("not_started" \| "starting" \| "idle" \| "busy" \| "restarting" \|
//! "dead" \| "missing" \| "failed"), python, message?, missing?}` (`missing`:
//! modules the interpreter lacks, e.g. `ipykernel`).
//!
//! Events: `progress` (while a dev container opens), `ready` (after
//! connecting), `files_changed`, `diagnostics`, `server_status`,
//! `disconnected`, `error` (startup failure, then exit 1), `debug_event`
//! (`type`: `stopped` with `reason`, `thread_id`, `frame`; `running`;
//! `output` with `category`, `text`; `breakpoints` with `path` and
//! `[{line, verified, message}]`; `frame_selected`; `terminated` with
//! `error`, `exit_code`; `adapter_missing` with `message`, `download`),
//! `port_forwarded` (`port`, `host`, `local_port`, `origin`; also automatic
//! and `forwardPorts` ones), `port_released` (`port`, `host`),
//! `port_available` (`port`, `url`, `label`, `action`: what the GUI does
//! as `onAutoForward` says: `notify`, `openBrowser`, `silent`),
//! `workflow_event` (`type`: `step` with `run`, `workflow`, `step`,
//! `status` (`pending`, `waiting`, `starting`, `running`, `stopping`,
//! `ready`, `failed`, `stopped`), `reason`, `detail`; `outcome` with
//! `outcome` (`running`, `ready`, `failed`: finished with failures or a
//! check without `on_fail` failed, `stopped`); `url` with `step`, `url`:
//! an `open_urls` URL, which headless mode does not open). Workflow steps
//! for other projects open them without a window; commands run in
//! terminals nobody sees.
use std::{
    collections::{HashMap, HashSet},
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use futures::{StreamExt, channel::mpsc};
use gpui::{App, AppContext, Application, AsyncApp, Context, Entity, Subscription, Task};
use lsp::types::{
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, GotoDefinitionParams, GotoDefinitionResponse, InlayHint,
    InlayHintKind, InlayHintLabel, InlayHintParams, ParameterLabel, Position, SignatureHelp,
    SignatureHelpContext, SignatureHelpParams, SignatureHelpTriggerKind,
    TextDocumentContentChangeEvent, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, Uri, VersionedTextDocumentIdentifier, notification,
    request::{
        GotoDeclaration, GotoDefinition, GotoImplementation, GotoTypeDefinition, InlayHintRequest,
        SemanticTokensFullRequest, SignatureHelpRequest,
    },
};
use parking_lot::Mutex;
use search::{SearchEvent, SearchLimits, SearchQuery};
use serde_json::{Value, json};
use syntax::Language;
use text::Encoding;
use watcher::ChangeKind;
use worktree::FileKind;

use crate::{
    diff_view::DiffSource,
    editor::{semantic, signature_help},
    lsp_store::{LspStore, LspStoreEvent, RunningServer, ServerStatus, request_retrying},
    project::{Location, Project, WatchHandle},
    settings::Settings,
};

mod debug;
mod edits;
mod files;
mod notebooks;
mod ports;
mod runs;
mod workflows;

const DEFAULT_OPEN_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a server must be idle before waited-for diagnostics count as final.
const SETTLE: Duration = Duration::from_secs(1);
const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(60);
/// Time language servers get to exit on shutdown.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Serializes output lines from any thread.
#[derive(Clone)]
struct Out(Arc<Mutex<io::Stdout>>);

impl Out {
    fn line(&self, value: &Value) {
        let mut stdout = self.0.lock();
        writeln!(stdout, "{value}")
            .and_then(|()| stdout.flush())
            .ok();
    }

    fn reply(&self, id: &Value, result: Result<Value, String>) {
        self.line(&match result {
            Ok(result) => json!({ "id": id, "ok": true, "result": result }),
            Err(error) => json!({ "id": id, "ok": false, "error": error }),
        });
    }

    fn event(&self, name: &str, mut fields: Value) {
        fields["event"] = name.into();
        self.line(&fields);
    }
}

pub fn run(location: Location) {
    // No window to ask in: the folder the user named is trusted.
    crate::trust::trust_all();
    Application::headless().run(move |cx: &mut App| {
        let out = Out(Arc::new(Mutex::new(io::stdout())));
        let (lines_tx, lines) = mpsc::unbounded::<String>();
        let reader = thread::Builder::new()
            .name("headless-stdin".into())
            .spawn(move || {
                for line in io::stdin().lock().lines().map_while(Result::ok) {
                    if lines_tx.unbounded_send(line).is_err() {
                        return;
                    }
                }
            });
        if let Err(err) = reader {
            out.event(
                "error",
                json!({ "message": format!("Cannot read commands: {err}") }),
            );
            std::process::exit(1);
        }
        cx.spawn(async move |cx| serve(location, out, lines, cx).await)
            .detach();
    });
}

async fn serve(
    location: Location,
    out: Out,
    mut lines: mpsc::UnboundedReceiver<String>,
    cx: &mut AsyncApp,
) {
    let connect = location.clone();
    let progress = out.clone();
    let project = cx
        .background_spawn(async move {
            Project::open(&connect, &mut |line| {
                progress.event("progress", json!({ "message": line }));
            })
        })
        .await;
    let project = match project {
        Ok(project) => project,
        Err(err) => {
            out.event(
                "error",
                json!({ "message": format!("Cannot open {location}: {err}") }),
            );
            std::process::exit(1);
        }
    };
    // The user's settings under the workspace file's and the first
    // folder's `.next_code/settings.json` (read where the project lives).
    let layering = project.clone();
    let (settings, problems) = cx
        .background_spawn(async move {
            let (user, user_error) = Settings::load();
            crate::settings::set_current(user);
            let (settings, layered_error) = layering.layered_settings(None);
            let problems: Vec<String> = user_error.into_iter().chain(layered_error).collect();
            (settings, problems)
        })
        .await;
    for problem in problems {
        out.event(
            "warning",
            json!({ "message": format!("Settings ignored: {problem}") }),
        );
    }
    let Ok(session) = cx.new(|cx| Session::new(project, settings, out.clone(), cx)) else {
        return;
    };
    session
        .read_with(cx, |session, _| {
            out.event(
                "ready",
                json!({
                    "root": session.project.root().to_string_lossy(),
                    "remote": session.project.is_remote(),
                }),
            );
        })
        .ok();

    while let Some(line) = lines.next().await {
        if line.trim().is_empty() {
            continue;
        }
        let shutdown = session
            .update(cx, |session, cx| session.handle(&line, cx))
            .unwrap_or(true);
        if shutdown {
            break;
        }
    }

    // Stdin closed or `shutdown`: end workflow commands and notebook
    // kernels, stop language servers, then exit.
    if let Ok(stopping) = session.update(cx, |session, cx| session.close_notebooks(cx)) {
        stopping.await;
    }
    if let Ok(stopping) = session.update(cx, |session, cx| {
        session
            .workflows
            .update(cx, |workflows, cx| workflows.kill_all(cx));
        session.watch = None;
        session
            .store
            .update(cx, |store, cx| store.shutdown_all(SHUTDOWN_GRACE, cx))
    }) {
        stopping.await;
    }
    if let Ok(stopping) = session.update(cx, |session, cx| session.debug_shutdown(cx)) {
        stopping.await;
    }
    cx.update(|cx| cx.quit()).ok();
}

struct Document {
    language: Language,
    uri: Uri,
    version: i32,
    text: String,
}

/// An open document with its running server, for LSP requests.
struct OpenDocument {
    uri: Uri,
    /// The text as last synced to the server.
    text: String,
    server: Arc<RunningServer>,
}

/// The request's `line` and `character` (an LSP position).
fn position(request: &Value) -> Result<Position, String> {
    match (request["line"].as_u64(), request["character"].as_u64()) {
        (Some(line), Some(character)) => Ok(Position::new(line as u32, character as u32)),
        _ => Err("missing `line` and `character`".into()),
    }
}

/// Definition-like results as `[{path, range}]`.
fn locations_json(response: Option<GotoDefinitionResponse>) -> Value {
    let locations: Vec<Value> = response
        .map(language::definition_locations)
        .unwrap_or_default()
        .into_iter()
        .map(|location| json!({ "path": path_json(&location.path), "range": location.range }))
        .collect();
    json!(locations)
}

struct Session {
    project: Project,
    store: Entity<LspStore>,
    out: Out,
    documents: HashMap<PathBuf, Document>,
    /// Files whose diagnostics a server published at least once.
    published: HashSet<PathBuf>,
    watch: Option<(WatchHandle, Task<()>)>,
    _disconnects: Option<Task<()>>,
    /// The last `code_actions` answer, for `apply_code_action`.
    code_actions: Option<(Arc<RunningServer>, Vec<lsp::types::CodeActionOrCommand>)>,
    debug: debug::HeadlessDebug,
    /// Forwarded ports (WSL and container projects).
    ports: Entity<crate::ports::Ports>,
    /// Workflows (the same model as a window's).
    workflows: Entity<crate::workflows::Workflows>,
    /// Open notebooks by path (`notebook_*`).
    notebooks: HashMap<PathBuf, Entity<crate::notebook::NotebookDocument>>,
    _subscription: Subscription,
    _ports_subscription: Subscription,
    _workflows_subscription: Subscription,
}

fn param_str<'a>(request: &'a Value, name: &str) -> Result<&'a str, String> {
    request[name]
        .as_str()
        .ok_or_else(|| format!("missing string parameter `{name}`"))
}

fn millis(request: &Value, name: &str, default: Duration) -> Duration {
    request[name]
        .as_u64()
        .map_or(default, Duration::from_millis)
}

fn path_json(path: &Path) -> Value {
    path.to_string_lossy().into()
}

/// The folder a request's `root` names (its index, or a folder name or
/// path); `None` without `root`.
fn root_param(project: &Project, request: &Value) -> Result<Option<usize>, String> {
    let count = project.roots().len();
    match &request["root"] {
        Value::Null => Ok(None),
        Value::Number(number) => number
            .as_u64()
            .and_then(|index| usize::try_from(index).ok())
            .filter(|&index| index < count)
            .map(Some)
            .ok_or_else(|| format!("no folder {number}: the project has {count}")),
        Value::String(name) => project
            .root_named(name)
            .map(Some)
            .ok_or_else(|| format!("no folder named `{name}`")),
        other => Err(format!(
            "`root` must be a folder index or name, not {other}"
        )),
    }
}

/// A file of a search answer: `{path}`, plus its folder's index `root` and
/// its folder-relative `relative` path in a multi-root workspace.
fn file_json(project: &Project, path: &Path) -> Value {
    let mut file = json!({ "path": path_json(path) });
    if project.is_multi_root()
        && let Some((root, relative)) = project.relative(path)
    {
        file["root"] = root.into();
        file["relative"] = relative.into();
    }
    file
}

/// The repository of a git request: that of the folder holding `path` when
/// it is absolute (`path` becomes repository-relative), else of folder
/// `root`. Blocks.
fn request_repository(
    project: &Project,
    root: usize,
    path: Option<&mut String>,
) -> io::Result<git::Repository> {
    let inside = path
        .as_deref()
        .filter(|path| Path::new(path.as_str()).has_root())
        .and_then(|path| project.relative(Path::new(path.as_str())));
    let index = inside.as_ref().map_or(root, |(index, _)| *index);
    let repository = project.repository_in(index)?.ok_or_else(|| {
        let message = if project.is_multi_root() {
            format!(
                "folder `{}` is not in a git repository",
                project.roots()[index].name
            )
        } else {
            "the project is not in a git repository".to_owned()
        };
        io::Error::new(io::ErrorKind::NotFound, message)
    })?;
    if let (Some(path), Some((_, relative))) = (path, inside) {
        *path = format!("{}{relative}", repository.prefix());
    }
    Ok(repository)
}

/// `git_status`: the status with the repository's `root` and the folder's
/// `prefix` in it. Blocks.
fn repository_status(repository: &git::Repository) -> io::Result<Value> {
    let mut status = json!(repository.status()?);
    status["root"] = repository.root().into();
    status["prefix"] = repository.prefix().into();
    Ok(status)
}

/// `git_status` with `all`: `[{root, repository_root, status}]`, one per
/// repository (folders sharing one report it once, under the first).
/// Blocks.
fn every_repository_status(project: &Project) -> io::Result<Value> {
    let mut seen = HashSet::new();
    let mut list = Vec::new();
    for root in 0..project.roots().len() {
        let Some(repository) = project.repository_in(root)? else {
            continue;
        };
        if seen.insert(repository.root().to_owned()) {
            list.push(json!({
                "root": root,
                "repository_root": repository.root(),
                "status": repository_status(&repository)?,
            }));
        }
    }
    Ok(Value::Array(list))
}

/// A request's string array `name` (missing: empty).
fn strings(request: &Value, name: &str) -> Vec<String> {
    request[name]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str().map(str::to_owned))
        .collect()
}

/// The query of a `search` or `replace` request.
fn search_query(request: &Value) -> SearchQuery {
    SearchQuery {
        pattern: request["pattern"].as_str().unwrap_or_default().to_owned(),
        regex: request["regex"].as_bool().unwrap_or(false),
        case_sensitive: request["case_sensitive"].as_bool().unwrap_or(false),
        whole_word: request["whole_word"].as_bool().unwrap_or(false),
        include: strings(request, "include"),
        exclude: strings(request, "exclude"),
    }
}

fn status_text(status: &ServerStatus) -> String {
    match status {
        ServerStatus::Starting => "starting".into(),
        ServerStatus::Ready => "ready".into(),
        ServerStatus::Busy(work) => format!("busy: {work}"),
        ServerStatus::Disabled => "disabled".into(),
        ServerStatus::NotFound {
            command,
            install_hint,
        } => match install_hint {
            Some(hint) => format!("{command} not found (install: {hint})"),
            None => format!("{command} not found"),
        },
        ServerStatus::Failed(error) => format!("failed: {error}"),
    }
}

impl Session {
    fn new(project: Project, settings: Arc<Settings>, out: Out, cx: &mut Context<Self>) -> Self {
        let ports = cx.new(|cx| crate::ports::Ports::new(project.clone(), &settings.ports, cx));
        let ports_subscription = cx.subscribe(&ports, |this, _, event, _| {
            this.on_ports_event(event);
        });
        let store = cx.new(|_| LspStore::new(project.clone(), settings));
        let workflows = cx.new(|cx| {
            crate::workflows::Workflows::new(
                Box::new(workflows::HeadlessHost::new(project.clone())),
                crate::workflows::user_dir(),
                cx,
            )
        });
        let forward = out.clone();
        let workflows_subscription = cx.subscribe(&workflows, move |_, _, event, _| {
            workflows::forward(&forward, event);
        });
        let subscription = cx.subscribe(&store, |this, store, event, cx| {
            this.on_lsp_event(&store, event, cx)
        });
        let disconnects = project.disconnects().map(|mut reasons| {
            let out = out.clone();
            cx.spawn(async move |_, _| {
                if let Some(reason) = reasons.next().await {
                    out.event("disconnected", json!({ "reason": reason }));
                }
            })
        });
        Self {
            project,
            store,
            out,
            documents: HashMap::new(),
            published: HashSet::new(),
            watch: None,
            _disconnects: disconnects,
            code_actions: None,
            debug: debug::HeadlessDebug::new(cx),
            ports,
            workflows,
            notebooks: HashMap::new(),
            _subscription: subscription,
            _ports_subscription: ports_subscription,
            _workflows_subscription: workflows_subscription,
        }
    }

    fn on_lsp_event(
        &mut self,
        store: &Entity<LspStore>,
        event: &LspStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            LspStoreEvent::DiagnosticsUpdated(path) => {
                self.published.insert(path.clone());
                let diagnostics = store.read(cx).diagnostics(path);
                self.out.event(
                    "diagnostics",
                    json!({ "path": path_json(path), "diagnostics": diagnostics }),
                );
            }
            &LspStoreEvent::StatusChanged(language) => {
                // Servers of other folders report as the first folder's.
                if let Some((name, status)) = store.read(cx).status(language, self.project.root()) {
                    self.out.event(
                        "server_status",
                        json!({
                            "language": language.id(),
                            "server": name.as_ref(),
                            "status": status_text(&status),
                        }),
                    );
                }
            }
            // A restarted server must learn about its open documents again.
            LspStoreEvent::ServerStarted(language, started) => {
                let paths: Vec<PathBuf> = self
                    .documents
                    .iter()
                    .filter(|(_, document)| document.language == *language)
                    .map(|(path, _)| path.clone())
                    .collect();
                for path in paths {
                    let server = store.update(cx, |store, cx| store.server(*language, &path, cx));
                    if server.is_some_and(|server| Arc::ptr_eq(&server, started)) {
                        did_open(started, &self.documents[&path]);
                    }
                }
            }
            LspStoreEvent::ServerStopped(_)
            | LspStoreEvent::SemanticTokensRefresh(_)
            | LspStoreEvent::InlayHintsRefresh(_) => {}
            LspStoreEvent::ApplyEdit(request) => self.apply_edit_request(request, cx),
        }
    }

    /// Handles one request line; returns whether to shut down.
    fn handle(&mut self, line: &str, cx: &mut Context<Self>) -> bool {
        let request: Value = match serde_json::from_str(line) {
            Ok(request) => request,
            Err(err) => {
                self.out
                    .reply(&Value::Null, Err(format!("invalid JSON request: {err}")));
                return false;
            }
        };
        let id = request["id"].clone();
        let Some(cmd) = request["cmd"].as_str() else {
            self.out.reply(&id, Err("missing `cmd`".into()));
            return false;
        };
        if let Err(err) = root_param(&self.project, &request) {
            self.out.reply(&id, Err(err));
            return false;
        }
        match cmd {
            "info" => {
                let roots: Vec<Value> = self
                    .project
                    .roots()
                    .iter()
                    .enumerate()
                    .map(|(index, root)| {
                        json!({ "index": index, "name": root.name, "path": path_json(&root.path) })
                    })
                    .collect();
                self.out.reply(
                    &id,
                    Ok(json!({
                        "root": path_json(self.project.root()),
                        "roots": roots,
                        "workspace_file": self.project.workspace_file(),
                        "remote": self.project.is_remote(),
                        "location": self.project.location().to_string(),
                    })),
                )
            }
            "list_dir" => {
                let path = request["path"].as_str().map_or_else(
                    || self.project.roots()[self.root(&request)].path.clone(),
                    |p| self.path(&request, p),
                );
                self.blocking(id, cx, move |project| {
                    let entries = project.read_dir(&path)?;
                    Ok(json!(entries))
                });
            }
            "list_files" => {
                let root = root_param(&self.project, &request).ok().flatten();
                self.blocking(id, cx, move |project| {
                    if root.is_some() || !project.is_multi_root() {
                        return Ok(json!(project.list_files(root.unwrap_or(0))?));
                    }
                    // Every folder's files, labelled with the folder.
                    let mut files = Vec::new();
                    for root in 0..project.roots().len() {
                        files.extend(
                            project
                                .list_files(root)?
                                .into_iter()
                                .map(|path| json!({ "root": root, "path": path })),
                        );
                    }
                    Ok(Value::Array(files))
                });
            }
            "read" => match param_str(&request, "path") {
                Ok(path) => {
                    let path = self.path(&request, path);
                    self.blocking(id, cx, move |project| {
                        Ok(json!({ "text": project.read_text(&path)? }))
                    });
                }
                Err(err) => self.out.reply(&id, Err(err)),
            },
            "stat" => match param_str(&request, "path") {
                Ok(path) => {
                    let path = self.path(&request, path);
                    self.blocking(id, cx, move |project| {
                        let kind = match project.kind(&path)? {
                            Some(FileKind::File) => json!("file"),
                            Some(FileKind::Dir) => json!("dir"),
                            None => Value::Null,
                        };
                        Ok(json!({ "kind": kind }))
                    });
                }
                Err(err) => self.out.reply(&id, Err(err)),
            },
            "write" => self.write(id, &request, cx),
            "create_file" | "create_dir" | "rename_path" | "copy_path" | "delete_path" => {
                self.files(id, cmd, &request, cx)
            }
            "search" => self.search(id, &request, cx),
            "replace" => self.replace(id, &request, cx),
            "watch" => self.start_watch(id, cx),
            "unwatch" => {
                self.watch = None;
                self.out.reply(&id, Ok(Value::Null));
            }
            "open" => self.open(id, &request, cx),
            "close" => match param_str(&request, "path") {
                Ok(path) => {
                    let path = self.path(&request, path);
                    if let Some(document) = self.documents.remove(&path)
                        && let Some(server) = self.running(document.language, &path, cx)
                    {
                        server
                            .connection
                            .notify::<notification::DidCloseTextDocument>(
                                DidCloseTextDocumentParams {
                                    text_document: TextDocumentIdentifier::new(document.uri),
                                },
                            );
                    }
                    self.out.reply(&id, Ok(Value::Null));
                }
                Err(err) => self.out.reply(&id, Err(err)),
            },
            "diagnostics" => self.diagnostics(id, &request, cx),
            "definition" | "declaration" | "type_definition" | "implementation" => {
                self.goto(id, cmd, &request, cx)
            }
            "incoming_calls" | "outgoing_calls" => self.calls(id, cmd, &request, cx),
            "semantic_tokens" => self.semantic_tokens(id, &request, cx),
            "rename" => self.rename(id, &request, cx),
            "code_actions" => self.code_actions(id, &request, cx),
            "apply_code_action" => self.apply_code_action(id, &request, cx),
            "format" => self.format(id, &request, cx),
            "document_symbols" => self.document_symbols(id, &request, cx),
            "workspace_symbols" => self.workspace_symbols(id, &request, cx),
            "signature_help" => self.signature_help(id, &request, cx),
            "inlay_hints" => self.inlay_hints(id, &request, cx),
            "spell_check" => self.spell_check(id, &request, cx),
            "exec" => match param_str(&request, "command") {
                Ok(command) => {
                    let command = command.to_owned();
                    let timeout = millis(&request, "timeout_ms", DEFAULT_EXEC_TIMEOUT);
                    self.blocking(id, cx, move |project| {
                        let output = project.run(&command, timeout)?;
                        Ok(json!({
                            "code": output.code,
                            "stdout": String::from_utf8_lossy(&output.stdout),
                            "stderr": String::from_utf8_lossy(&output.stderr),
                        }))
                    });
                }
                Err(err) => self.out.reply(&id, Err(err)),
            },
            "git_stash_list" | "git_stash_push" | "git_stash_apply" | "git_stash_drop"
            | "git_stash_changes" | "git_shelve" | "git_unshelve" | "git_shelf_list"
            | "git_shelf_diff" | "git_shelf_delete" => self.git_stash(id, cmd, &request, cx),
            command if command.starts_with("git_") => self.git(id, command, &request, cx),
            command if command.starts_with("python_") => self.python(id, command, &request, cx),
            "devcontainer_configs" => {
                self.blocking(id, cx, |project| Ok(json!(project.devcontainer_configs()?)))
            }
            "run_configurations" | "debug_request" | "tests_discover" | "tests_run" => {
                self.run_command(id, cmd, &request, cx)
            }
            command if command.starts_with("debug_") => self.debug(id, command, &request, cx),
            command if command.starts_with("ports_") => self.ports(id, command, &request, cx),
            command if command.starts_with("workflow") => self.workflows(id, command, &request, cx),
            command if command.starts_with("notebook_") => {
                self.notebooks(id, command, &request, cx)
            }
            "shutdown" => {
                self.out.reply(&id, Ok(Value::Null));
                return true;
            }
            other => self
                .out
                .reply(&id, Err(format!("unknown command `{other}`"))),
        }
        false
    }

    /// `git_*` commands, run against the repository of the request's
    /// folder (see [`request_repository`]).
    fn git(&self, id: Value, command: &str, request: &Value, cx: &mut Context<Self>) {
        let paths: Vec<String> = request["paths"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|path| path.as_str().map(str::to_owned))
            .collect();
        let message = request["message"].as_str().unwrap_or_default().to_owned();
        let mut path = request["path"].as_str().map(str::to_owned);
        let root = self.root(request);
        let every_repository = command == "git_status" && request["all"].as_bool().unwrap_or(false);
        // Commands whose `path` is repository-relative (else a folder).
        let repository_path = matches!(
            command,
            "git_diff"
                | "git_apply_hunk"
                | "git_line_changes"
                | "git_stage_line_change"
                | "git_blame"
        );
        let staged = request["staged"].as_bool().unwrap_or(false);
        let context = if request["whole_file"].as_bool().unwrap_or(false) {
            git::FULL_FILE
        } else {
            git::DIFF_CONTEXT
        };
        // `"worktree"` names the working tree; anything else is a revision.
        let side = |name: &str| -> Option<git::Side> {
            request[name].as_str().map(|value| match value {
                "worktree" => git::Side::Worktree,
                revision => git::Side::Revision(revision.to_owned()),
            })
        };
        let (old, new) = (side("old"), side("new"));
        let base = request["base"].as_str().map(str::to_owned);
        let target = side("target").unwrap_or(git::Side::Worktree);
        let commit = request["commit"].as_str().map(str::to_owned);
        let limit = request["limit"].as_u64().unwrap_or(200) as usize;
        let skip = request["skip"].as_u64().unwrap_or(0) as usize;
        let strings = |name: &str| -> Vec<String> {
            request[name]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect()
        };
        let mut filter = git::LogFilter {
            revisions: strings("branches"),
            text: request["text"].as_str().unwrap_or_default().to_owned(),
            authors: strings("authors"),
            since: request["since"].as_i64(),
            until: request["until"].as_i64(),
            paths: paths.clone(),
        };
        if filter.revisions.is_empty() && !request["all"].as_bool().unwrap_or(true) {
            filter.revisions.push("HEAD".into());
        }
        let commits = strings("commits");
        let newest = request["newest"].as_str().map(str::to_owned);
        let oldest = request["oldest"].as_str().map(str::to_owned);
        let mode = request["mode"].as_str().map(str::to_owned);
        let amend = request["amend"].as_bool().unwrap_or(false);
        let force = request["force"].as_bool().unwrap_or(false);
        let name = request["name"].as_str().map(str::to_owned);
        let start = request["start"].as_str().map(str::to_owned);
        let switch = request["switch"].as_bool().unwrap_or(true);
        let create = request["create"].as_bool().unwrap_or(false);
        let hunk = request["hunk"].as_u64().map(|hunk| hunk as usize);
        let action = request["action"].as_str().map(str::to_owned);
        let onto = request["onto"].as_str().map(str::to_owned);
        let branch = request["branch"].as_str().map(str::to_owned);
        let new_name = request["new_name"].as_str().map(str::to_owned);
        let rebase = request["rebase"].as_bool().unwrap_or(false);
        let editor_text = request["contents"].as_str().map(str::to_owned);
        let against_index = request["index"].as_bool().unwrap_or(false);
        let line_change = request["change"].as_u64().map(|change| change as usize);
        let command = command.to_owned();
        self.blocking(id, cx, move |project| {
            if every_repository {
                return every_repository_status(project);
            }
            let repository =
                request_repository(project, root, path.as_mut().filter(|_| repository_path))?;
            let missing = |name: &str| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("missing parameter `{name}`"),
                )
            };
            let changed_entry = |path: &str| {
                repository
                    .status()?
                    .entries
                    .into_iter()
                    .find(|entry| entry.path == path)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, format!("{path} has no changes"))
                    })
            };
            match command.as_str() {
                "git_status" => repository_status(&repository),
                "git_stage" => repository.stage(&paths).map(|()| Value::Null),
                "git_unstage" => repository.unstage(&paths).map(|()| Value::Null),
                "git_discard" => {
                    let entries: Vec<git::Entry> = repository
                        .status()?
                        .entries
                        .into_iter()
                        .filter(|entry| paths.contains(&entry.path))
                        .collect();
                    repository.discard(&entries).map(|()| Value::Null)
                }
                "git_commit" => Ok(json!({ "summary": repository.commit(&message, amend)? })),
                "git_diff" => {
                    let path = path.ok_or_else(|| missing("path"))?;
                    let source = match (old, new) {
                        (None, None) => DiffSource::Index {
                            entry: changed_entry(&path)?,
                            staged,
                        },
                        (old, new) => {
                            let old = old.ok_or_else(|| missing("old"))?;
                            let new = new.unwrap_or(git::Side::Worktree);
                            let git::Side::Revision(base) = &old else {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidInput,
                                    "`old` must be a revision",
                                ));
                            };
                            let change = repository
                                .changes(base, &new)?
                                .into_iter()
                                .find(|change| change.path == path)
                                .ok_or_else(|| {
                                    io::Error::new(
                                        io::ErrorKind::NotFound,
                                        format!("{path} does not differ"),
                                    )
                                })?;
                            DiffSource::Revisions { old, new, change }
                        }
                    };
                    Ok(json!(source.compute(project, &repository, context)?.0))
                }
                "git_blame" => {
                    let path = path.ok_or_else(|| missing("path"))?;
                    Ok(json!(
                        repository.blame(&path, editor_text.as_deref().map(str::as_bytes))?
                    ))
                }
                "git_line_changes" | "git_stage_line_change" => {
                    let path = path.ok_or_else(|| missing("path"))?;
                    // The editor's text, else the file in the working tree.
                    let current = match editor_text {
                        Some(text) => text,
                        None => {
                            let bytes = project.worktree_file(&repository, &path)?;
                            String::from_utf8_lossy(&bytes.unwrap_or_default()).into_owned()
                        }
                    };
                    let object = repository
                        .base_objects(std::slice::from_ref(&path), against_index)?
                        .pop()
                        .unwrap_or(git::BaseObject::None);
                    let (kind, base, changes) = match &object {
                        git::BaseObject::Blob(id) => {
                            let bytes = repository.blob(id)?.unwrap_or_default();
                            let base = String::from_utf8_lossy(&bytes).into_owned();
                            let changes = git::line_changes(&base, &current, None);
                            ("blob", base, changes)
                        }
                        git::BaseObject::NewFile => {
                            ("new_file", String::new(), git::all_added(&current))
                        }
                        git::BaseObject::None => ("none", String::new(), Vec::new()),
                    };
                    if command == "git_line_changes" {
                        return Ok(json!({ "base": kind, "changes": changes }));
                    }
                    if !against_index {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "staging a change needs `index: true` (changes against the index)",
                        ));
                    }
                    let change = line_change.ok_or_else(|| missing("change"))?;
                    let change = changes.get(change).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("{path} has {} changes", changes.len()),
                        )
                    })?;
                    let staged = git::apply_change(&base, &current, change);
                    repository.stage_contents(&path, staged.as_bytes())?;
                    Ok(Value::Null)
                }
                "git_apply_hunk" => {
                    let path = path.ok_or_else(|| missing("path"))?;
                    let hunk = hunk.ok_or_else(|| missing("hunk"))?;
                    let action = match action.as_deref() {
                        Some("stage") => git::HunkAction::Stage,
                        Some("unstage") => git::HunkAction::Unstage,
                        Some("discard") => git::HunkAction::Discard,
                        _ => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "`action` must be \"stage\", \"unstage\" or \"discard\"",
                            ));
                        }
                    };
                    let entry = changed_entry(&path)?;
                    let patch = repository
                        .index_patch(&entry, staged, context)?
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("{path} has no textual hunks on that side"),
                            )
                        })?;
                    repository.apply_hunk(&patch, hunk, action)?;
                    Ok(Value::Null)
                }
                "git_branch_create" => {
                    let name = name.ok_or_else(|| missing("name"))?;
                    repository.create_branch(&name, start.as_deref(), switch)?;
                    Ok(Value::Null)
                }
                "git_switch" => {
                    let name = name.ok_or_else(|| missing("name"))?;
                    let mut refs = repository.refs()?;
                    refs.retain(|r| r.name == name);
                    // A local branch wins over a remote branch or tag of that name.
                    refs.sort_by_key(|r| r.kind as u8);
                    let target = refs.into_iter().next().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, format!("no branch or tag {name}"))
                    })?;
                    repository.switch_branch(&target)?;
                    Ok(Value::Null)
                }
                "git_branch_delete" => {
                    let name = name.ok_or_else(|| missing("name"))?;
                    repository.delete_branch(&name, force)?;
                    Ok(Value::Null)
                }
                "git_fetch" => repository.fetch().map(|()| Value::Null),
                "git_pull" => repository.pull().map(|()| Value::Null),
                "git_push" => repository.push(&repository.status()?).map(|()| Value::Null),
                // PyCharm's workflow (Commit window, branches popup).
                "git_commit_paths" => {
                    let entries: Vec<git::Entry> = repository
                        .status()?
                        .entries
                        .into_iter()
                        .filter(|entry| paths.contains(&entry.path))
                        .collect();
                    let summary = repository.commit_entries(&message, &entries, amend)?;
                    Ok(json!({ "summary": summary }))
                }
                "git_rollback" => {
                    let entries: Vec<git::Entry> = repository
                        .status()?
                        .entries
                        .into_iter()
                        .filter(|entry| paths.contains(&entry.path))
                        .collect();
                    repository.rollback(&entries).map(|()| Value::Null)
                }
                "git_add" => repository.add(&paths).map(|()| Value::Null),
                "git_ignore" => {
                    let file = project.repository_path(&repository, ".gitignore");
                    let existing = match project.read_text(&file) {
                        Ok(text) => text,
                        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
                        Err(err) => return Err(err),
                    };
                    project
                        .write(&file, git::ignore_entries(&existing, &paths).into_bytes())
                        .map(|()| Value::Null)
                }
                "git_operation_state" => Ok(json!(repository.operation_state()?)),
                "git_merge" => {
                    let name = name.ok_or_else(|| missing("name"))?;
                    Ok(json!(repository.merge(&name)?))
                }
                "git_rebase" => {
                    let onto = onto.ok_or_else(|| missing("onto"))?;
                    Ok(json!(repository.rebase(&onto, branch.as_deref())?))
                }
                "git_continue_rebase" => Ok(json!(repository.continue_rebase()?)),
                "git_abort" => repository
                    .abort(repository.operation_state()?)
                    .map(|()| Value::Null),
                "git_branch_rename" => {
                    let name = name.ok_or_else(|| missing("name"))?;
                    let new_name = new_name.ok_or_else(|| missing("new_name"))?;
                    repository
                        .rename_branch(&name, &new_name)
                        .map(|()| Value::Null)
                }
                "git_remote_branch_delete" => {
                    let name = name.ok_or_else(|| missing("name"))?;
                    repository.delete_remote_branch(&name).map(|()| Value::Null)
                }
                "git_update" => Ok(json!(repository.update(rebase)?)),
                "git_update_branch" => {
                    let name = name.ok_or_else(|| missing("name"))?;
                    repository.update_branch(&name).map(|()| Value::Null)
                }
                "git_branches" => Ok(json!({
                    "tracking": repository.branch_tracking()?,
                    "recent": repository.recent_branches(5)?,
                })),
                "git_push_target" | "git_push_branch" => {
                    let branch = match name {
                        Some(name) => name,
                        None => repository.status()?.branch.ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidInput, "HEAD is detached")
                        })?,
                    };
                    let target = repository.push_target(&branch)?;
                    if command == "git_push_branch" {
                        repository.push_branch(&branch, &target)?;
                        return Ok(json!({ "target": target }));
                    }
                    let outgoing = repository.outgoing(&branch, &target)?;
                    Ok(json!({ "target": target, "outgoing": outgoing }))
                }
                "git_worktrees" => Ok(json!(repository.worktrees()?)),
                "git_worktree_add" => {
                    let branch = name.ok_or_else(|| missing("name"))?;
                    let path = path.unwrap_or_else(|| repository.default_worktree_path(&branch));
                    repository.add_worktree(&path, &branch, create, start.as_deref())?;
                    Ok(json!({ "path": path }))
                }
                "git_worktree_remove" => {
                    let path = path.ok_or_else(|| missing("path"))?;
                    repository.remove_worktree(&path, force)?;
                    Ok(Value::Null)
                }
                "git_log" => {
                    let page = repository.log_page(&filter, skip, limit)?;
                    let graph = filter.graph(&page.commits);
                    Ok(json!({ "commits": page.commits, "graph": graph, "next": page.next }))
                }
                "git_refs" => Ok(json!(repository.refs()?)),
                "git_changes" => {
                    let base = base.ok_or_else(|| missing("base"))?;
                    Ok(json!(repository.changes(&base, &target)?))
                }
                "git_commit_changes" => {
                    let commit = commit.ok_or_else(|| missing("commit"))?;
                    let commit = repository.show(&commit)?;
                    Ok(json!({
                        "commit": commit,
                        "changes": repository.commit_changes(&commit)?,
                    }))
                }
                "git_selection_changes" => {
                    let newest = repository.show(&newest.ok_or_else(|| missing("newest"))?)?;
                    let oldest = match oldest {
                        Some(oldest) => repository.show(&oldest)?,
                        None => newest.clone(),
                    };
                    let span = repository.selection_changes(&newest, &oldest)?;
                    Ok(json!({ "base": span.base, "target": span.target, "changes": span.changes }))
                }
                "git_commit_details" => {
                    let commit = repository.resolve(&commit.ok_or_else(|| missing("commit"))?)?;
                    Ok(json!({
                        "message": repository.commit_message(&commit)?,
                        "branches": repository.branches_containing(&commit)?,
                        "pushed": repository.is_pushed(&commit)?,
                    }))
                }
                "git_cherry_pick" => Ok(json!(repository.cherry_pick(&commits)?)),
                "git_revert" => Ok(json!(repository.revert(&commits)?)),
                "git_reset" => {
                    let commit = commit.ok_or_else(|| missing("commit"))?;
                    let mode = match mode.as_deref() {
                        Some("soft") => git::ResetMode::Soft,
                        Some("mixed") | None => git::ResetMode::Mixed,
                        Some("hard") => git::ResetMode::Hard,
                        Some("keep") => git::ResetMode::Keep,
                        Some(other) => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("unknown reset mode `{other}`"),
                            ));
                        }
                    };
                    repository.reset(&commit, mode)?;
                    Ok(Value::Null)
                }
                "git_undo_commit" => repository.undo_last_commit().map(|()| Value::Null),
                "git_reword" => Ok(json!({ "summary": repository.reword_head(&message)? })),
                "git_tag" => {
                    let name = name.ok_or_else(|| missing("name"))?;
                    let at = commit.unwrap_or_else(|| "HEAD".into());
                    let message = (!message.is_empty()).then_some(message.as_str());
                    repository.create_tag(&name, &at, message)?;
                    Ok(Value::Null)
                }
                "git_checkout_revision" => {
                    let commit = commit.ok_or_else(|| missing("commit"))?;
                    repository.checkout_revision(&commit).map(|()| Value::Null)
                }
                "git_patch" => {
                    let patch = repository.create_patch(&commits)?;
                    Ok(json!({ "patch": String::from_utf8_lossy(&patch) }))
                }
                other => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown command `{other}`"),
                )),
            }
        });
    }

    /// `git_stash_*` and shelf commands (the UI's Stash/Unstash dialogs and
    /// Shelf tab run the same `git` functions, with the same project files).
    fn git_stash(&self, id: Value, command: &str, request: &Value, cx: &mut Context<Self>) {
        let paths: Option<Vec<String>> = request["paths"].as_array().map(|paths| {
            paths
                .iter()
                .filter_map(|path| path.as_str().map(str::to_owned))
                .collect()
        });
        let message = request["message"].as_str().unwrap_or_default().to_owned();
        let flag = |name: &str| request[name].as_bool().unwrap_or(false);
        let (include_untracked, pop, reinstate_index, remove) = (
            flag("include_untracked"),
            flag("pop"),
            flag("reinstate_index"),
            flag("remove"),
        );
        let index = request["index"].as_u64().map(|index| index as usize);
        let shelf = request["shelf"].as_str().map(str::to_owned);
        let mut path = request["path"].as_str().map(str::to_owned);
        let root = self.root(request);
        let context = if flag("whole_file") {
            git::FULL_FILE
        } else {
            git::DIFF_CONTEXT
        };
        let command = command.to_owned();
        self.blocking(id, cx, move |project| {
            let repository = request_repository(project, root, path.as_mut())?;
            let missing = |name: &str| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("missing parameter `{name}`"),
                )
            };
            let files = crate::shelf_view::ProjectFiles::new(project, &repository);
            match command.as_str() {
                "git_stash_list" => Ok(json!(repository.stash_list()?)),
                "git_stash_push" => repository
                    .stash_push(&message, include_untracked, paths.as_deref())
                    .map(|()| Value::Null),
                "git_stash_apply" => Ok(json!(repository.stash_apply(
                    index.unwrap_or(0),
                    pop,
                    reinstate_index
                )?)),
                "git_stash_drop" => repository
                    .stash_drop(index.ok_or_else(|| missing("index"))?)
                    .map(|()| Value::Null),
                "git_stash_changes" => Ok(json!(repository.stash_changes(index.unwrap_or(0))?)),
                "git_shelve" => {
                    let paths = paths.ok_or_else(|| missing("paths"))?;
                    Ok(json!(repository.shelve(&files, &paths, &message)?))
                }
                "git_unshelve" => {
                    let shelf = shelf.ok_or_else(|| missing("shelf"))?;
                    Ok(json!(repository.unshelve(&files, &shelf, remove)?))
                }
                "git_shelf_list" => Ok(json!(repository.shelves(&files)?)),
                "git_shelf_diff" => {
                    let shelf = shelf.ok_or_else(|| missing("shelf"))?;
                    let path = path.ok_or_else(|| missing("path"))?;
                    Ok(json!(
                        repository.shelved_file_diff(&files, &shelf, &path, context)?
                    ))
                }
                "git_shelf_delete" => {
                    let shelf = shelf.ok_or_else(|| missing("shelf"))?;
                    repository.delete_shelf(&shelf).map(|()| Value::Null)
                }
                other => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown command `{other}`"),
                )),
            }
        });
    }

    /// `python_*` commands: the uv project in the project root.
    fn python(&self, id: Value, command: &str, request: &Value, cx: &mut Context<Self>) {
        let text = |name: &str| request[name].as_str().map(str::to_owned);
        let (name, requirement, version, path) = (
            text("name"),
            text("requirement"),
            text("version"),
            text("path").map(|path| self.path(request, &path)),
        );
        let kind = match (text("group"), text("optional")) {
            (Some(group), _) => python::DependencyKind::Group(group),
            (None, Some(extra)) => python::DependencyKind::Optional(extra),
            (None, None) => python::DependencyKind::Main,
        };
        let upgrade = request["upgrade"].as_bool().unwrap_or(false);
        let line = request["line"].as_u64().map(|line| line as usize);
        let command = command.to_owned();
        self.blocking(id, cx, move |project| {
            let missing = |name: &str| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("missing parameter `{name}`"),
                )
            };
            if command == "python_test_at" {
                let path = path.ok_or_else(|| missing("path"))?;
                let line = line.ok_or_else(|| missing("line"))?;
                let text = project.read_text(&path)?;
                return Ok(json!(python::test_at(&text, line)));
            }
            let info = crate::python_panel::load(project)?;
            if command == "python_info" {
                return Ok(match info {
                    None => Value::Null,
                    Some(info) => {
                        let mut value = json!(info);
                        value["dependencies"] = info
                            .dependencies
                            .iter()
                            .map(|dependency| {
                                json!({
                                    "name": dependency.requirement.name,
                                    "specifier": dependency.requirement.specifier,
                                    "kind": dependency.kind.label(),
                                    "locked": dependency.locked,
                                })
                            })
                            .collect();
                        value["scripts"] = json!(info.project.scripts);
                        value
                    }
                });
            }
            let uv = info.and_then(|info| info.uv).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "no pyproject.toml, or uv is not installed where the project lives",
                )
            })?;
            let args = match command.as_str() {
                "python_sync" => python::uv::sync(),
                "python_lock" => python::uv::lock(upgrade),
                "python_add" => {
                    python::uv::add(&requirement.ok_or_else(|| missing("requirement"))?, &kind)
                }
                "python_remove" => python::uv::remove(&name.ok_or_else(|| missing("name"))?, &kind),
                "python_pin" => python::uv::pin(&version.ok_or_else(|| missing("version"))?),
                "python_interpreters" => {
                    let output =
                        crate::python_panel::run_uv(project, &uv, &python::uv::python_list())?;
                    let interpreters =
                        python::parse_python_list(&String::from_utf8_lossy(&output.stdout))?;
                    return Ok(interpreters
                        .iter()
                        .map(|interpreter| {
                            json!({
                                "version": interpreter.version,
                                "implementation": interpreter.implementation,
                                "path": interpreter.path,
                            })
                        })
                        .collect());
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown command `{other}`"),
                    ));
                }
            };
            let output = crate::python_panel::run_uv(project, &uv, &args)?;
            Ok(json!({
                "code": output.code,
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr),
            }))
        });
    }

    /// Runs a blocking project operation off the main thread and replies.
    fn blocking(
        &self,
        id: Value,
        cx: &mut Context<Self>,
        operation: impl FnOnce(&Project) -> io::Result<Value> + Send + 'static,
    ) {
        let project = self.project.clone();
        let out = self.out.clone();
        cx.background_spawn(async move {
            out.reply(&id, operation(&project).map_err(|err| err.to_string()));
        })
        .detach();
    }

    /// The folder the request's `root` names (checked in `handle`), else
    /// the first.
    fn root(&self, request: &Value) -> usize {
        root_param(&self.project, request)
            .ok()
            .flatten()
            .unwrap_or(0)
    }

    /// A path parameter of the request: absolute as is, else relative to
    /// the request's folder (see [`Session::root`]).
    fn path(&self, request: &Value, input: &str) -> PathBuf {
        self.project.path_in(self.root(request), input)
    }

    /// The server serving the open document `path`.
    fn running(
        &self,
        language: Language,
        path: &Path,
        cx: &mut Context<Self>,
    ) -> Option<Arc<RunningServer>> {
        self.store
            .update(cx, |store, cx| store.server(language, path, cx))
    }

    /// The open document the request's `path` names and its running
    /// server; replies with the error and returns `None` otherwise.
    fn document(
        &mut self,
        id: &Value,
        request: &Value,
        cx: &mut Context<Self>,
    ) -> Option<OpenDocument> {
        let path = match param_str(request, "path") {
            Ok(path) => self.path(request, path),
            Err(err) => {
                self.out.reply(id, Err(err));
                return None;
            }
        };
        let Some(document) = self.documents.get(&path) else {
            self.out.reply(
                id,
                Err(format!("{} is not open; send `open` first", path.display())),
            );
            return None;
        };
        let (uri, text, language) = (
            document.uri.clone(),
            document.text.clone(),
            document.language,
        );
        let Some(server) = self.running(language, &path, cx) else {
            self.out
                .reply(id, Err("the language server is not running".into()));
            return None;
        };
        Some(OpenDocument { uri, text, server })
    }

    /// Sends `R` (retrying while the server is still analyzing, which
    /// rust-analyzer reports as "content modified"), then replies with
    /// `map` of the result, off the main thread.
    fn reply_request<R: lsp::types::request::Request>(
        &self,
        id: Value,
        server: Arc<RunningServer>,
        params: R::Params,
        map: impl FnOnce(R::Result) -> Value + Send + 'static,
        cx: &mut Context<Self>,
    ) where
        R::Params: Clone,
    {
        let out = self.out.clone();
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let result =
                request_retrying::<R>(&server, params, &executor, 10, Duration::from_millis(500))
                    .await;
            out.reply(&id, result.map(map).map_err(|err| err.to_string()));
        })
        .detach();
    }

    fn write(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let (path, text) = match (param_str(request, "path"), param_str(request, "text")) {
            (Ok(path), Ok(text)) => (self.path(request, path), text.to_owned()),
            (Err(err), _) | (_, Err(err)) => return self.out.reply(&id, Err(err)),
        };
        let project = self.project.clone();
        let data = text.clone().into_bytes();
        cx.spawn(async move |this, cx| {
            let written = cx
                .background_spawn(async move { project.write(&path, data).map(|()| path) })
                .await;
            this.update(cx, |this, cx| match written {
                Ok(path) => {
                    this.sync_document(&path, text, cx);
                    this.out.reply(&id, Ok(Value::Null));
                }
                Err(err) => this.out.reply(&id, Err(err.to_string())),
            })
            .ok();
        })
        .detach();
    }

    /// Sends an open document's new saved text to its server.
    fn sync_document(&mut self, path: &Path, text: String, cx: &mut Context<Self>) {
        let Some(language) = self.documents.get(path).map(|document| document.language) else {
            return;
        };
        let server = self.running(language, path, cx);
        let Some(document) = self.documents.get_mut(path) else {
            return;
        };
        document.version += 1;
        document.text = text;
        let Some(server) = server else {
            return;
        };
        server
            .connection
            .notify::<notification::DidChangeTextDocument>(DidChangeTextDocumentParams {
                text_document: VersionedTextDocumentIdentifier::new(
                    document.uri.clone(),
                    document.version,
                ),
                content_changes: vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: document.text.clone(),
                }],
            });
        server
            .connection
            .notify::<notification::DidSaveTextDocument>(DidSaveTextDocumentParams {
                text_document: TextDocumentIdentifier::new(document.uri.clone()),
                text: server.save_includes_text.then(|| document.text.clone()),
            });
    }

    fn search(&self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let query = search_query(request);
        let project = self.project.clone();
        let out = self.out.clone();
        cx.background_spawn(async move {
            let (_handle, mut events) = match project.search(&query, SearchLimits::default()) {
                Ok(started) => started,
                Err(err) => return out.reply(&id, Err(err)),
            };
            let mut files = Vec::new();
            while let Some(event) = events.next().await {
                match event {
                    SearchEvent::File(file) => {
                        let mut found = file_json(&project, &file.path);
                        found["lines"] = file
                            .lines
                            .iter()
                            .map(|line| {
                                json!({
                                    "line": line.line,
                                    "column": line.column,
                                    "text": line.text,
                                })
                            })
                            .collect();
                        files.push(found);
                    }
                    SearchEvent::Done {
                        files_searched,
                        matches,
                        limit_hit,
                    } => {
                        return out.reply(
                            &id,
                            Ok(json!({
                                "files": files,
                                "matches": matches,
                                "files_searched": files_searched,
                                "limit_hit": limit_hit,
                            })),
                        );
                    }
                }
            }
            out.reply(
                &id,
                Err("the search ended early (agent disconnected?)".into()),
            );
        })
        .detach();
    }

    fn start_watch(&mut self, id: Value, cx: &mut Context<Self>) {
        let (handle, mut batches) = match self.project.watch(Duration::from_millis(150)) {
            Ok(started) => started,
            Err(err) => return self.out.reply(&id, Err(err.to_string())),
        };
        let task = cx.spawn(async move |this, cx| {
            while let Some(batch) = batches.next().await {
                let alive = this.update(cx, |this, cx| {
                    this.store.read(cx).notify_file_changes(&batch.events);
                    // `tests_*` read a folder's settings again once it changed.
                    this.project.folder_settings_changed(
                        batch.events.iter().map(|event| event.path.as_path()),
                    );
                    let changes: Vec<Value> = batch
                        .events
                        .iter()
                        .map(|event| {
                            let kind = match event.kind {
                                ChangeKind::Created => "created",
                                ChangeKind::Modified => "modified",
                                ChangeKind::Removed => "removed",
                            };
                            json!({ "path": path_json(&event.path), "kind": kind })
                        })
                        .collect();
                    this.out.event(
                        "files_changed",
                        json!({
                            "changes": changes,
                            "rescan": batch.rescan,
                            "repository": batch.repository,
                        }),
                    );
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        self.watch = Some((handle, task));
        self.out.reply(&id, Ok(Value::Null));
    }

    fn open(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let path = match param_str(request, "path") {
            Ok(path) => self.path(request, path),
            Err(err) => return self.out.reply(&id, Err(err)),
        };
        let Some(language) = Language::from_path(&path) else {
            return self
                .out
                .reply(&id, Err(format!("no language for {}", path.display())));
        };
        let Some(uri) = lsp::path_to_uri(&path) else {
            return self
                .out
                .reply(&id, Err(format!("{} has no file URI", path.display())));
        };
        let timeout = millis(request, "timeout_ms", DEFAULT_OPEN_TIMEOUT);
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            let read_path = path.clone();
            let text = match cx
                .background_spawn(async move { project.read_text(&read_path) })
                .await
            {
                Ok(text) => text,
                Err(err) => {
                    this.update(cx, |this, _| this.out.reply(&id, Err(err.to_string())))
                        .ok();
                    return;
                }
            };
            let deadline = Instant::now() + timeout;
            loop {
                let Ok(step) = this.update(cx, |this, cx| {
                    let server = this.running(language, &path, cx);
                    let status = this.store.read(cx).status(language, &path);
                    (server, status)
                }) else {
                    return;
                };
                let failure = match step {
                    (Some(server), status) => {
                        this.update(cx, |this, _| {
                            let document = Document {
                                language,
                                uri,
                                version: 1,
                                text,
                            };
                            did_open(&server, &document);
                            this.documents.insert(path, document);
                            this.out.reply(
                                &id,
                                Ok(json!({
                                    "language": language.id(),
                                    "server": status.map(|(name, _)| name.to_string()),
                                    "encoding": match server.encoding {
                                        Encoding::Utf8 => "utf-8",
                                        Encoding::Utf16 => "utf-16",
                                    },
                                })),
                            );
                        })
                        .ok();
                        return;
                    }
                    (None, None) => Some(format!("no language server for {}", language.id())),
                    (
                        None,
                        Some((
                            _,
                            status @ (ServerStatus::Starting
                            | ServerStatus::Ready
                            | ServerStatus::Busy(_)),
                        )),
                    ) => (Instant::now() >= deadline)
                        .then(|| format!("the server is still {}", status_text(&status))),
                    (None, Some((_, status))) => Some(status_text(&status)),
                };
                if let Some(error) = failure {
                    this.update(cx, |this, _| this.out.reply(&id, Err(error)))
                        .ok();
                    return;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
        })
        .detach();
    }

    /// Replies with diagnostics. With `wait_ms`, first waits (up to that
    /// long) until the file's server published for it and has been idle for
    /// [`SETTLE`]: servers often publish an empty list before checking.
    fn diagnostics(&self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let path = request["path"]
            .as_str()
            .map(|path| self.path(request, path));
        let wait = millis(request, "wait_ms", Duration::ZERO);
        cx.spawn(async move |this, cx| {
            let deadline = Instant::now() + wait;
            let mut settled_since: Option<Instant> = None;
            loop {
                let Ok(settled) = this.update(cx, |this, cx| {
                    let documents: Vec<(Language, &Path)> = match &path {
                        Some(path) => Language::from_path(path)
                            .map(|language| (language, path.as_path()))
                            .into_iter()
                            .collect(),
                        None => this
                            .documents
                            .iter()
                            .map(|(path, d)| (d.language, path.as_path()))
                            .collect(),
                    };
                    let store = this.store.read(cx);
                    let idle = documents.iter().all(|&(language, path)| {
                        !matches!(
                            store.status(language, path),
                            Some((_, ServerStatus::Starting | ServerStatus::Busy(_)))
                        )
                    });
                    let published = path
                        .as_ref()
                        .is_none_or(|path| this.published.contains(path));
                    idle && published
                }) else {
                    return;
                };
                settled_since = settled.then(|| settled_since.unwrap_or_else(Instant::now));
                let now = Instant::now();
                if now >= deadline || settled_since.is_some_and(|since| now - since >= SETTLE) {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
            }
            this.update(cx, |this, cx| {
                let store = this.store.read(cx);
                let result: Vec<Value> = match &path {
                    Some(path) => vec![json!({
                        "path": path_json(path),
                        "diagnostics": store.diagnostics(path),
                    })],
                    None => store
                        .all_diagnostics()
                        .map(|(path, diagnostics)| {
                            json!({ "path": path_json(path), "diagnostics": diagnostics })
                        })
                        .collect(),
                };
                this.out.reply(&id, Ok(json!(result)));
            })
            .ok();
        })
        .detach();
    }

    /// `definition`, `declaration`, `type_definition`, `implementation`.
    fn goto(&mut self, id: Value, cmd: &str, request: &Value, cx: &mut Context<Self>) {
        let position = match position(request) {
            Ok(position) => position,
            Err(err) => return self.out.reply(&id, Err(err)),
        };
        let Some(document) = self.document(&id, request, cx) else {
            return;
        };
        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier::new(document.uri),
                position,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let server = document.server;
        match cmd {
            "declaration" => {
                self.reply_request::<GotoDeclaration>(id, server, params, locations_json, cx)
            }
            "type_definition" => {
                self.reply_request::<GotoTypeDefinition>(id, server, params, locations_json, cx)
            }
            "implementation" => {
                self.reply_request::<GotoImplementation>(id, server, params, locations_json, cx)
            }
            _ => self.reply_request::<GotoDefinition>(id, server, params, locations_json, cx),
        }
    }

    /// `incoming_calls` / `outgoing_calls`: prepares the call hierarchy at
    /// the position, then asks for the first item's calls.
    fn calls(&mut self, id: Value, cmd: &str, request: &Value, cx: &mut Context<Self>) {
        use crate::call_hierarchy::{self, Direction};
        use lsp::types::{CallHierarchyPrepareParams, request::CallHierarchyPrepare};

        let position = match position(request) {
            Ok(position) => position,
            Err(err) => return self.out.reply(&id, Err(err)),
        };
        let Some(document) = self.document(&id, request, cx) else {
            return;
        };
        let direction = if cmd == "outgoing_calls" {
            Direction::Outgoing
        } else {
            Direction::Incoming
        };
        let params = CallHierarchyPrepareParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier::new(document.uri),
                position,
            },
            work_done_progress_params: Default::default(),
        };
        let server = document.server;
        let out = self.out.clone();
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let delay = Duration::from_millis(500);
            let result = async {
                let items =
                    request_retrying::<CallHierarchyPrepare>(&server, params, &executor, 10, delay)
                        .await?;
                let Some(item) = items.and_then(|items| items.into_iter().next()) else {
                    return Ok(Vec::new());
                };
                call_hierarchy::calls(&server, direction, item, &executor, 10, delay).await
            }
            .await;
            let result = result.map_err(|err| err.to_string()).map(|calls| {
                let calls: Vec<Value> = calls
                    .into_iter()
                    .map(|call| {
                        json!({
                            "name": call.item.name,
                            "kind": call.item.kind,
                            "detail": call.item.detail,
                            "path": lsp::uri_to_path(&call.item.uri).as_deref().map(path_json),
                            "range": call.item.range,
                            "selection_range": call.item.selection_range,
                            "from_ranges": call.from_ranges,
                        })
                    })
                    .collect();
                json!(calls)
            });
            out.reply(&id, result);
        })
        .detach();
    }

    /// The signature help at a position, with each signature's highlighted
    /// parameter resolved as the editor's popover does.
    fn signature_help(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let position = match position(request) {
            Ok(position) => position,
            Err(err) => return self.out.reply(&id, Err(err)),
        };
        let Some(document) = self.document(&id, request, cx) else {
            return;
        };
        let encoding = document.server.encoding;
        let params = SignatureHelpParams {
            context: Some(SignatureHelpContext {
                trigger_kind: SignatureHelpTriggerKind::INVOKED,
                trigger_character: None,
                is_retrigger: false,
                active_signature_help: None,
            }),
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier::new(document.uri),
                position,
            },
            work_done_progress_params: Default::default(),
        };
        let map = move |help: Option<SignatureHelp>| {
            let Some(help) = help.filter(|help| !help.signatures.is_empty()) else {
                return Value::Null;
            };
            let signatures: Vec<Value> = help
                .signatures
                .iter()
                .map(|signature| {
                    let active =
                        signature_help::active_parameter(&help, signature).and_then(|index| {
                            signature_help::parameter_range(signature, index, encoding)
                        });
                    let parameters: Vec<String> = signature
                        .parameters
                        .iter()
                        .flatten()
                        .enumerate()
                        .map(|(index, parameter)| match &parameter.label {
                            ParameterLabel::Simple(text) => text.clone(),
                            ParameterLabel::LabelOffsets(_) => {
                                signature_help::parameter_range(signature, index, encoding)
                                    .map(|range| signature.label[range].to_owned())
                                    .unwrap_or_default()
                            }
                        })
                        .collect();
                    json!({
                        "label": signature.label,
                        "parameters": parameters,
                        "active": active.map(|range| [range.start, range.end]),
                    })
                })
                .collect();
            json!({
                "active_signature": help.active_signature,
                "active_parameter": help.active_parameter,
                "signatures": signatures,
            })
        };
        self.reply_request::<SignatureHelpRequest>(id, document.server, params, map, cx);
    }

    /// Unknown words of `text` or of the file at `path` (the text is
    /// checked as that file: Markdown, prose or code by its language;
    /// `language` overrides it), with corrections.
    fn spell_check(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let path = request["path"]
            .as_str()
            .map(|path| self.path(request, path));
        let text = request["text"].as_str().map(str::to_owned);
        let language = match request["language"].as_str() {
            Some(name) => match Language::ALL
                .into_iter()
                .find(|language| language.id() == name)
            {
                Some(language) => Some(language),
                None => {
                    return self
                        .out
                        .reply(&id, Err(format!("unknown language `{name}`")));
                }
            },
            None => path.as_deref().and_then(Language::from_path),
        };
        self.blocking(id, cx, move |project| {
            let text = match (text, &path) {
                (Some(text), _) => text,
                (None, Some(path)) => project.read_text(path)?,
                (None, None) => {
                    return Err(io::Error::other(
                        "missing string parameter `path` or `text`",
                    ));
                }
            };
            let settings = project.settings_for(path.as_deref());
            let misspellings = crate::editor::spell::check_document(
                Some(project),
                path.as_deref(),
                &text,
                language,
                &settings,
            )
            .map_err(io::Error::other)?;
            Ok(json!({ "misspellings": misspellings }))
        });
    }

    /// Inlay hints of a line range (default: the whole document).
    fn inlay_hints(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let Some(document) = self.document(&id, request, cx) else {
            return;
        };
        let buffer = text::Buffer::new(&document.text);
        let last_line = buffer.line_count() - 1;
        let line = |name: &str, default: usize| {
            request[name]
                .as_u64()
                .map_or(default, |line| (line as usize).min(last_line))
        };
        let (start, end) = (line("start_line", 0), line("end_line", last_line));
        let encoding = document.server.encoding;
        let range = lsp::types::Range::new(
            language::to_position(&buffer, buffer.line_start(start), encoding),
            language::to_position(&buffer, buffer.line_end(end.max(start)), encoding),
        );
        let params = InlayHintParams {
            text_document: TextDocumentIdentifier::new(document.uri),
            range,
            work_done_progress_params: Default::default(),
        };
        let map = |hints: Option<Vec<InlayHint>>| {
            let hints: Vec<Value> = hints
                .unwrap_or_default()
                .into_iter()
                .map(|hint| {
                    let label: String = match hint.label {
                        InlayHintLabel::String(label) => label,
                        InlayHintLabel::LabelParts(parts) => {
                            parts.into_iter().map(|part| part.value).collect()
                        }
                    };
                    json!({
                        "line": hint.position.line,
                        "character": hint.position.character,
                        "label": label,
                        "kind": match hint.kind {
                            Some(InlayHintKind::TYPE) => Some("type"),
                            Some(InlayHintKind::PARAMETER) => Some("parameter"),
                            _ => None,
                        },
                        "padding_left": hint.padding_left.unwrap_or(false),
                        "padding_right": hint.padding_right.unwrap_or(false),
                    })
                })
                .collect();
            json!(hints)
        };
        self.reply_request::<InlayHintRequest>(id, document.server, params, map, cx);
    }

    fn document_symbols(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        use language::symbols;
        use lsp::types::{DocumentSymbolParams, request::DocumentSymbolRequest};
        let Some(document) = self.document(&id, request, cx) else {
            return;
        };
        let params = DocumentSymbolParams {
            text_document: TextDocumentIdentifier::new(document.uri),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let map = |response: Option<lsp::types::DocumentSymbolResponse>| {
            let list: Vec<Value> = response
                .map(symbols::flatten)
                .unwrap_or_default()
                .into_iter()
                .map(|symbol| {
                    let range = |range: std::ops::Range<Position>| {
                        lsp::types::Range::new(range.start, range.end)
                    };
                    json!({
                        "kind_name": symbols::kind_name(symbol.kind),
                        "name": symbol.name,
                        "detail": symbol.detail,
                        "kind": symbol.kind,
                        "depth": symbol.depth,
                        "parent": symbol.parent,
                        "range": range(symbol.range),
                        "selection_range": range(symbol.selection_range),
                    })
                })
                .collect();
            json!(list)
        };
        self.reply_request::<DocumentSymbolRequest>(id, document.server, params, map, cx);
    }

    /// `workspace_symbols`: asks every running server that serves
    /// `workspace/symbol` and merges their answers.
    fn workspace_symbols(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        use language::symbols;
        use lsp::types::{OneOf, WorkspaceSymbolParams, request::WorkspaceSymbolRequest};
        let query = match param_str(request, "query") {
            Ok(query) => query.to_owned(),
            Err(err) => return self.out.reply(&id, Err(err)),
        };
        let servers: Vec<Arc<RunningServer>> = self
            .store
            .read(cx)
            .running_servers()
            .into_iter()
            .map(|(_, _, server)| server)
            .filter(|server| {
                server
                    .capabilities
                    .workspace_symbol_provider
                    .as_ref()
                    .is_some_and(|provider| !matches!(provider, OneOf::Left(false)))
            })
            .collect();
        if servers.is_empty() {
            return self.out.reply(
                &id,
                Err(
                    "no running language server lists workspace symbols; `open` a file first"
                        .into(),
                ),
            );
        }
        let params = WorkspaceSymbolParams {
            query,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let out = self.out.clone();
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let requests = servers.iter().map(|server| {
                request_retrying::<WorkspaceSymbolRequest>(
                    server,
                    params.clone(),
                    &executor,
                    10,
                    Duration::from_millis(500),
                )
            });
            let mut items = Vec::new();
            let mut error = None;
            for result in futures::future::join_all(requests).await {
                match result {
                    Ok(response) => items.extend(
                        response
                            .map(symbols::workspace_symbols)
                            .unwrap_or_default()
                            .into_iter()
                            .map(|symbol| {
                                json!({
                                    "name": symbol.name,
                                    "kind": symbol.kind,
                                    "container": symbol.container,
                                    "path": path_json(&symbol.path),
                                    "range": symbol.range,
                                })
                            }),
                    ),
                    Err(err) => error = Some(err.to_string()),
                }
            }
            out.reply(
                &id,
                match error {
                    Some(error) if items.is_empty() => Err(error),
                    _ => Ok(json!(items)),
                },
            );
        })
        .detach();
    }

    fn semantic_tokens(&mut self, id: Value, request: &Value, cx: &mut Context<Self>) {
        let path = match param_str(request, "path") {
            Ok(path) => self.path(request, path),
            Err(err) => return self.out.reply(&id, Err(err)),
        };
        let Some(document) = self.documents.get(&path) else {
            return self.out.reply(
                &id,
                Err(format!("{} is not open; send `open` first", path.display())),
            );
        };
        let params = semantic::params(document.uri.clone());
        let buffer = text::Buffer::new(&document.text);
        let Some(server) = self.running(document.language, &path, cx) else {
            return self
                .out
                .reply(&id, Err("the language server is not running".into()));
        };
        if server.semantic_legend.is_none() {
            return self.out.reply(
                &id,
                Err("the language server does not serve semantic tokens".into()),
            );
        }
        let out = self.out.clone();
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let mut attempts = 0;
            let response = loop {
                let result = server
                    .connection
                    .request::<SemanticTokensFullRequest>(params.clone())
                    .await;
                match result {
                    Err(lsp::Error::Response { code: -32801, .. }) if attempts < 10 => {
                        attempts += 1;
                        executor.timer(Duration::from_millis(500)).await;
                    }
                    other => break other,
                }
            };
            let Some(legend) = &server.semantic_legend else {
                return;
            };
            let result = response.map_err(|err| err.to_string()).map(|response| {
                let data = semantic::response_data(response);
                let tokens: Vec<Value> = semantic::decode(&data, legend, &buffer, server.encoding)
                    .into_iter()
                    .map(|token| {
                        let start = buffer.point(token.range.start);
                        json!({
                            "line": start.row,
                            "start": start.column,
                            "end": start.column + token.range.len(),
                            "text": buffer.slice(token.range.clone()),
                            "scope": token.highlight.map(|highlight| highlight.scope()),
                            "modifiers": legend.modifier_names(token.modifiers).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                json!(tokens)
            });
            out.reply(&id, result);
        })
        .detach();
    }
}

fn did_open(server: &RunningServer, document: &Document) {
    server
        .connection
        .notify::<notification::DidOpenTextDocument>(DidOpenTextDocumentParams {
            text_document: TextDocumentItem::new(
                document.uri.clone(),
                document.language.id().into(),
                document.version,
                document.text.clone(),
            ),
        });
}
