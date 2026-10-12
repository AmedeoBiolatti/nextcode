# Handoff: next_code

## Project state

- Repository: the `next_code` checkout on the Windows desktop (`C:\Users\AmedeoBiolatti\OneDrive - Powerfleet\Desktop\next_code`; from WSL `/mnt/c/Users/AmedeoBiolatti/OneDrive - Powerfleet/Desktop/next_code`). `C:\dev\next_code` is an empty directory.
- Branch `main`: `61e5f35` holds M2, headless mode, M3 and M4; the M5 git operations commit follows it.
- Product: a native Rust IDE on GPUI. The UI runs on Windows; WSL projects are served by a headless agent inside the distro.

## Completed

- M0, M1 (see README).
- M2, WSL host agent:
  - `crates/worktree`: file operations shared by the editor and the agent (listing with ignore rules, file list, text decoding, writes, stat, executable/venv lookup; skips `PATH` entries on Windows drives inside WSL).
  - `crates/proto`: versioned protocol (`PROTOCOL_VERSION = 3`: M3 added `Batch::repository`, M5 added `CloseInput` to end a process's stdin). Banner line, then length-prefixed postcard frames; request ids; streams (watch, search, spawn) with final events; `Cancel`/`Input`/`CloseInput`/`Resize`; shutdown is EOF on the agent's stdin.
  - `crates/agent` (`next_code_agent serve`): runs requests on threads, watchers, searches, piped processes (own process group, killed with the group) and ptys (via alacritty's tty). Kills everything on EOF.
  - `crates/remote`: `AgentClient` (sync requests with 30 s timeout, streams, disconnect reasons with exit status and stderr tail, language-server and terminal adapters, `run`), and `wsl::connect` (checks `wsl.exe`/distro/arch, installs the agent by piping it to `~/.local/share/next_code/next_code_agent-<protocol>-<hash>`, starts it through `sh -l`, 60 s start timeout).
  - UI `Project` (local or WSL) routes tree, finder, editor open/save/reload, watcher, search, LSP launch, and terminal. WSL paths are POSIX strings in `PathBuf`s; `lsp::uri` maps driveless paths to `file:///home/...` on Windows. WSL language servers get no Windows `processId`.
  - Launch: `next_code --wsl <distro> <linux path>` or a `\\wsl.localhost\<distro>\...` argument; a "Connecting" window shows failures with Retry. A disconnect shows a persistent workspace notice.
- Headless mode (`next_code --headless [--wsl D P | DIR]`): JSON lines on stdin/stdout over the same `Project` and `LspStore`; commands and events are listed in `crates/ui/src/headless.rs` and README.
- M3, git and multiple projects:
  - `crates/git`: `Repository` over a `Runner` (git CLI, so it runs locally or through the agent): discovery (`--show-toplevel --show-prefix`), porcelain v2 status, stage (`add -A`), unstage (`reset`, or `rm --cached` before the first commit), discard (`checkout` / `clean`), commit (`-m`), blobs via `cat-file`, and unified line diffs (`similar`, CRLF-insensitive).
  - Watcher batches carry `repository: true` when `.git/index`, `HEAD`, `packed-refs`, `MERGE_HEAD` or entries under `refs/` change, so external commits refresh the panel. Status uses `--no-optional-locks`, so it does not trigger itself.
  - UI: Git panel (`Ctrl+Shift+G`), diff tabs (workspace tabs are now editor or diff), `Ctrl+O` open folder and `Ctrl+R` recent/typed location (`Distro:/path`) in new windows, reusing a window already showing the project. Recents are stored in the local data dir (`next_code/recent.json`).
  - Headless: `git_status`, `git_diff`, `git_stage`, `git_unstage`, `git_discard`, `git_commit`; `files_changed` events include `repository`.
- M4, git review (the user asked to prioritize review over AI, which moved to M5):
  - `crates/git`: `log` (topo order, all branches or HEAD, `\x1e`/`\x1f`-separated records), `graph` lane layout (per row: node column, top/bottom edge segments), `refs` (branches, remotes, tags; skips `*/HEAD`), `resolve`/`show`, `changes` (`diff --name-status -z -M` between revisions or against the working tree), `commit_changes` (first parent, `diff-tree --root` for root commits), `revision_diff` between `Side`s (`Empty`/`Revision`/`Worktree`), `side_by_side` pairing and `FULL_FILE` context. Revisions starting with `-` are rejected.
  - UI: `change_tree` (compact folder tree / list, collapsible), Git panel tree/list toggle and History/Compare buttons, `DiffView` over `DiffSource` (index or revisions) with Unified/Side-by-side and Changes-only/Whole-file toggles (remembered in a gpui global), `HistoryView` tab (canvas-drawn graph, refs badges, details pane with changed files and "Compare with working tree"), `CompareView` tab (base/target buttons, swap, tree/list), `RevisionPicker` modal (refs + typed revision validated with `resolve`, optional working tree).
  - Headless: `git_log`, `git_refs`, `git_changes`, `git_commit_changes`; `git_diff` also takes `old`/`new`/`whole_file`.
- M5, git operations (requested by the user before AI, which moved to M6):
  - Input for git: `Runner::run_git(args, input)`; `AgentClient::run` takes stdin bytes and closes it (`CloseInput`); `Project::run_program` writes stdin locally on a thread.
  - `crates/git/src/operations.rs`: `create_branch` (validated with `check-ref-format --branch`), `switch_branch` (local; remote → `switch --track` or the existing local branch; tag → `--detach`), `delete_branch` (`-d`/`-D`), `fetch --all --prune`, `pull --ff-only`, `push` (upstream, else `-u origin|only remote`), `worktrees` (porcelain, current one marked, Windows paths compared case-insensitively), `default_worktree_path` (`<root>-<branch>` sibling), `add_worktree`, `remove_worktree`; `commit(message, amend)`. All git runs have `GIT_TERMINAL_PROMPT=0`.
  - `crates/git/src/patch.rs`: index/worktree diffs from `git diff` (no color/ext-diff/textconv, fixed prefixes, `-M` for staged) parsed into hunks; `Patch::to_file_diff` adds `Row::Hunk {index, header}` rows; `apply_hunk` builds a one-hunk patch and pipes it to `git apply [--cached] [-R]` (stage, unstage, discard). Untracked and binary files fall back to the `similar` diff without hunk actions.
  - UI: diff tabs of the index/working tree show hunk headers with Stage hunk/Discard (asks) or Unstage hunk; `BranchPicker` (switch, create from HEAD or a History commit, delete/force delete), `WorktreePicker` (open in a new window via `Project::location_of`, add for a new/existing branch then open, remove/force remove), Git panel Fetch/Pull ↓n/Push ↑n row, Amend toggle, clickable branch name, `Worktrees…` tool. Operations refresh the panel, history and diffs, and report in the panel.
  - Headless: `git_apply_hunk`, `git_branch_create`, `git_switch`, `git_branch_delete`, `git_fetch`, `git_pull`, `git_push`, `git_worktrees`, `git_worktree_add`, `git_worktree_remove`; `git_commit` takes `amend`.

## Verification done

- Windows: `cargo test --workspace` and `cargo clippy --workspace --all-targets` clean (only the existing `proc-macro-error2` future-incompat note). Includes `crates/ui/tests/headless.rs` (end-to-end headless on a local project: files/search/watch/exec, and git status/diff/stage/commit/discard with the repository event, log/refs/commit changes/compare/revision diffs, hunk stage/discard, amend, branch create/switch/delete, worktree add/list/remove, push failure without a remote) and git crate tests on real repositories (branches, merge, rename, tags, CRLF hunk stage/unstage/discard, bare remote push -u/fetch/pull, worktrees).
- WSL/Linux (target dir `~/.cache/next_code_target`): agent/remote/proto/worktree/terminal tests pass, including real pipes, ptys, process-group shutdown and disconnect. The `ui` crate does not build on Linux (`xattr` + libc `ENOATTR`), unchanged from before.
- Real WSL (`cargo test -p remote --test wsl -- --ignored`, see README): install, start, open, error messages.
- GUI smoke on Windows against `/home/.../next_code_smoke`: tree via agent with ignore rules, open, edit and save, rust-analyzer running in WSL (child of the agent) with diagnostics, clean reload on external edit, the Keep/Reload prompt for a dirty buffer, create/delete refresh, search. The terminal tab was not checked: the user asked to stop on-screen tests.
- Headless smoke against WSL and local projects: every command including `open`, `diagnostics` (E0308 reported), `definition`, `exec`, watch events and shutdown, with no processes left behind. M3: headless git flow inside WSL (status, diff, stage, commit, repository event) against a repo in the distro, with the v2 agent installed automatically. M4: headless log/graph (branch + merge), refs, compare and revision diffs inside WSL. M5: inside WSL with the v3 agent installed automatically: push -u to a bare remote, two-hunk diff, stage hunk 1 / discard hunk 0 (stdin through the agent), commit + amend, branch create/push/switch/delete, fetch, pull, worktree add/list/remove.
- Not checked on screen (the user asked not to run on-screen tests without asking): Git panel (tree/list, sync row, amend), diff toggles and hunk buttons, History and Compare tabs, revision/branch/worktree pickers, opening a worktree window, Open Folder/Open Recent windows, and the WSL terminal tab.
- Headless commands run concurrently (each on a background task): scripts that depend on order must wait for each reply.

## Build

```powershell
scripts\build-wsl-agent.ps1        # Linux agent (musl, static) -> target\release\next_code_agent
cargo build --release -p next_code
cargo run --release -p next_code -- --wsl Ubuntu ~/project
```

Debug builds look for `target\debug\next_code_agent` (`-Profile debug`), or set `NEXT_CODE_WSL_AGENT`.

## Open points

- Check on screen (with the user's go-ahead): Git panel tree/list, sync row and amend, diff toggles and hunk buttons, History graph and Compare tabs, revision/branch/worktree pickers, worktree windows, `Ctrl+O`/`Ctrl+R` windows, and the WSL terminal tab (`pwd` should be the project root).
- Do not test on screen without asking the user; use `--headless` for automation.
- Git: no merge/rebase/stash, blame or editor gutter markers yet; pull is fast-forward only; hunk actions are whole hunks (no line selection); history loads the latest 2000 commits; comparisons against the working tree omit untracked files.
- Agent requests are synchronous on the UI thread for tree/open/save (30 s timeout). Move them to background tasks if latency shows.
- A disconnected agent is reported but not reconnected; restarting the editor reconnects.
- On Windows, local language servers left running at window close are pre-existing behavior (the GUI does not call `LspStore::shutdown_all`).

## Environment note

The PowerShell profile on this machine prints an execution-policy error at startup. Leave execution policy unchanged. `rust-analyzer` was added to the WSL toolchain (`rustup component add rust-analyzer`), and the `x86_64-unknown-linux-musl` target was installed in WSL for the agent build.
