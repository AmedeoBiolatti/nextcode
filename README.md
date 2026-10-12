# next_code

Native, AI-first IDE written in Rust on [gpui](https://crates.io/crates/gpui).

## Layout

| Crate | Purpose |
|---|---|
| `crates/text` | UI-agnostic rope buffer: edits, undo/redo, grapheme/word motion, UTF-16 mapping, line endings, change log |
| `crates/syntax` | Incremental tree-sitter parsing and highlight spans for C, C++, Python, Rust, SQL |
| `crates/search` | Gitignore-aware project text search with regex, globs and streaming results |
| `crates/watcher` | Debounced project file watching with `.gitignore` filtering |
| `crates/terminal` | Shell process and terminal emulation, local or attached to a remote pty |
| `crates/worktree` | Project file operations shared by the editor and the agent: listing, ignore rules, tool lookup |
| `crates/proto` | Versioned editor ↔ agent wire protocol (framing, requests, events) |
| `crates/agent` | `next_code_agent`: headless Linux process serving files, watching, search, language servers and shells |
| `crates/remote` | Agent client and the WSL bootstrap (install, start, version check) |
| `crates/git` | Git through its CLI: porcelain v2 status, stage/unstage/discard/commit, hunk patches, branches, worktrees, fetch/pull/push, history, diffs |
| `crates/ui` | `next_code` binary: editor, docks, LSP, terminal, search, problems, git panel and diffs, fuzzy file finder (`Ctrl+P`) |

## Build and run

Windows (MSVC Build Tools required), macOS, or Linux:

```sh
cargo run --release -p next_code -- C:\path\to\your\project
```

Replace the example path with an existing project folder. Without an argument, the current directory is opened.

For example, from PowerShell:

```powershell
cargo run --release -p next_code -- C:\dev\next_code
```

Linux additionally needs the gpui system libraries (Wayland/X11, xkbcommon, fontconfig, Vulkan).

On Windows the editor is a GUI-subsystem program: no console window opens
with it. Started from a terminal, it returns to the prompt right away;
argument errors and `--headless` output still print to that terminal (or to
redirected streams), and closing the terminal does not close the editor.

Set `NEXT_CODE_TRACE_STARTUP=1` to print startup milestones (time since the
process was created) to stderr, e.g. from PowerShell:
`$env:NEXT_CODE_TRACE_STARTUP=1; .\next_code.exe C:\dev\p 2> trace.txt`.
The WSL connection (or, for local projects, git status) starts on a thread
before the window, alongside gpui's platform setup; DirectWrite's font list
and the shell's drag-and-drop helper are loaded on threads of their own
while gpui creates its Direct3D devices.

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
binary's location.

The real-WSL tests are ignored by default:

```powershell
$env:NEXT_CODE_WSL_AGENT = "$PWD\target\release\next_code_agent"
$env:NEXT_CODE_TEST_WSL_DISTRO = "Ubuntu"
cargo test -p remote --test wsl -- --ignored
```

## Git and multiple projects

The Git panel (`Ctrl+Shift+G`, left dock) shows the branch, ahead/behind
counts and changes in Merge Conflicts / Staged Changes / Changes sections,
as a compact folder tree (single-folder chains like `src/editor` collapse
into one row; folders fold) or as a flat list. Rows stage (`+`), unstage
(`−`) or discard (`↺`, asks first; untracked files are deleted); clicking a
row opens its diff in a tab, and clicking a diff line opens the file there.
Enter in the message box commits what is staged; "Amend" rewrites the last
commit instead (with what is staged, or just its message). Git runs where the
project lives (inside WSL for WSL projects), and the panel refreshes on file
changes and on index/ref changes (commits or checkouts from a terminal).

Diffs show unified or side by side, and changed hunks only or the whole
file; the last choice sticks for new diffs. Diffs of changes in the index
or working tree come from `git diff` itself and act per hunk: "Stage hunk"
and "Discard" (asks first) on unstaged changes, "Unstage hunk" on staged
ones.

**Branches**: clicking the branch name opens the branch picker: switch to a
branch, a remote branch (checks out a tracking branch) or a tag (detached),
type a new name to create a branch from HEAD and switch to it, or `×` to
delete a branch (asks; "Force Delete" for unmerged ones). In History,
"Create branch here…" starts a branch at the selected commit.

**Fetch / Pull / Push** sit under the branch name with the behind/ahead
counts. Fetch is `--all --prune`, pull is fast-forward only (diverged
branches are left to a terminal), and the first push of a branch sets its
upstream on `origin` (or the only remote). Git never prompts: credentials
come from your credential helper or SSH agent, and failures are shown.

**Worktrees…** lists the repository's worktrees; choosing one opens it in
its own window. Typing a branch name adds a worktree for it (creating the
branch if needed) next to the repository as `<repository>-<branch>` and
opens it; `×` removes one (asks; its branch is kept).

**History** (panel toolbar) opens the commit graph of all branches (or the
current one): lanes, branch/tag labels, author and age. Selecting a commit
shows its details and changed files; each opens its diff against the first
parent, and "Compare with working tree" starts a comparison from it.

**Compare…** compares the working tree with a branch, tag or any revision
(`HEAD~3`, a commit id). A comparison tab lists the changed files as a tree
or list; its base and target can be changed (the target may be another
revision, and then swapped), and each file opens its diff.

Each project opens in its own window: `Ctrl+O` picks a folder (a
`\\wsl.localhost\...` folder opens through WSL), `Ctrl+R` lists recent
projects and also accepts a typed location such as `Ubuntu:~/project`.
Opening a project that already has a window brings that window forward.

## Headless mode

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
`search`, `watch`/`unwatch`, `open`/`close` (language server documents),
`diagnostics`, `definition`, `exec` (shell command in the project root, where
the project lives), `git_status`, `git_diff`, `git_apply_hunk`, `git_stage`,
`git_unstage`, `git_discard`, `git_commit`, `git_branch_create`,
`git_switch`, `git_branch_delete`, `git_fetch`, `git_pull`, `git_push`,
`git_worktrees`, `git_worktree_add`, `git_worktree_remove`, `git_log`,
`git_refs`, `git_changes`, `git_commit_changes`, `shutdown`. Parameters and
results are documented in
`crates/ui/src/headless.rs`.

## Roadmap

- **M0** editor core: window, file tree, tabs, editing, save, syntax highlighting, fuzzy file finder — *done*
- **M1** LSP (completion, go-to-definition, diagnostics), resizable docks, terminal, project search, Problems panel, external file watching — *done*
- **M2** host agent protocol, WSL projects — *done*
- **M3** git panel and diff view, multiple workspaces — *done*
- **M4** git review: commit graph, compare with branches/commits, compact change trees, side-by-side and whole-file diffs — *done*
- **M5** git operations: branches, worktrees (one window each), fetch/pull/push, amend, hunk staging — *done*
- **M6** AI inline completion, chat, agent
- **M7** devcontainers, debugger (DAP)
- **M8** WASM extension API
