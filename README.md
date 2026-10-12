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
| `crates/git` | Git through its CLI: porcelain v2 status, stage/unstage/discard/commit, line diffs |
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
Enter in the message box commits what is staged. Git runs where the project
lives (inside WSL for WSL projects), and the panel refreshes on file changes
and on index/ref changes (commits or checkouts from a terminal).

Diffs show unified or side by side, and changed hunks only or the whole
file; the last choice sticks for new diffs.

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
the project lives), `git_status`, `git_diff`, `git_stage`, `git_unstage`,
`git_discard`, `git_commit`, `git_log`, `git_refs`, `git_changes`,
`git_commit_changes`, `shutdown`. Parameters and results are documented in
`crates/ui/src/headless.rs`.

## Roadmap

- **M0** editor core: window, file tree, tabs, editing, save, syntax highlighting, fuzzy file finder — *done*
- **M1** LSP (completion, go-to-definition, diagnostics), resizable docks, terminal, project search, Problems panel, external file watching — *done*
- **M2** host agent protocol, WSL projects — *done*
- **M3** git panel and diff view, multiple workspaces — *done*
- **M4** git review: commit graph, compare with branches/commits, compact change trees, side-by-side and whole-file diffs — *done*
- **M5** AI inline completion, chat, agent
- **M6** devcontainers, debugger (DAP)
- **M7** WASM extension API
