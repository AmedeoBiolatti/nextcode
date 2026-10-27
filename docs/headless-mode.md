# Headless mode

## UI automation (Linux / WSL)

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

`{"cmd":"pinpoint","request":{…}}` forwards a [Pinpoint](pinpoint.md) request
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
as the project) opens a [multi-root workspace](multi-root-workspaces.md):
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
