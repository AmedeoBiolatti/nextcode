# Repository layout

| Crate | Purpose |
|---|---|
| `crates/text` | UI-agnostic rope buffer: edits, undo/redo, grapheme/word motion, UTF-16 mapping, line endings, change log |
| `crates/syntax` | Incremental tree-sitter parsing and highlight spans for Bash, C, C++, CSS, Dockerfile, Go, HTML, JavaScript, JSON, Markdown, Python, Rust, SQL, TOML, TypeScript/TSX, YAML; loads the grammars from `grammars/` libraries on first use |
| `crates/grammar_abi` | The contract between the editor and its grammar libraries (entry table, ABI version, `export!`) |
| `grammars/*` | One shared library per tree-sitter grammar (`nc_grammar_<name>.dll`/`.so`), built with the editor and loaded when a file of that language opens |
| `crates/search` | Gitignore-aware project text search with regex, globs and streaming results |
| `crates/watcher` | Debounced project file watching with `.gitignore` filtering |
| `crates/terminal` | Shell process and terminal emulation, local or attached to a remote pty; links (URLs, file locations, OSC 8) in its output |
| `crates/worktree` | Project file operations shared by the editor and the agent: listing, ignore rules, tool lookup |
| `crates/proto` | Versioned editor ↔ agent wire protocol (framing, requests, events) |
| `crates/agent` | `next_code_agent`: headless Linux process serving files, watching, search, language servers and shells |
| `vendor/gpui` | gpui 0.2.2 with startup patches to its Windows platform (`NEXT_CODE_PATCHES.md` there) |
| `crates/instance` | One editor process per user: the launch request channel (named pipe / Unix socket), its JSON protocol, client and server |
| `crates/launcher` | `next_code`: the small launcher users run; hands the command line to the running editor or starts `next_code_editor` |
| `crates/remote` | Agent client and the bootstraps that install and start it in WSL or a container (`docker exec`) |
| `crates/devcontainer` | `devcontainer.json` (JSONC) and bringing dev containers up with the docker CLI |
| `crates/python` | `pyproject.toml` / `uv.lock`, uv command lines, pytest node ids |
| `crates/notebook` | Notebooks: `.ipynb` and marimo file formats (round-trip exact), marimo's static analysis and cell graph, kernel output folding, ANSI colours, the kernel bridge (`assets/bridge.py`) and its JSON-lines protocol |
| `crates/git` | Git through its CLI: porcelain v2 status, stage/unstage/discard/commit, hunk patches, branches, worktrees, fetch/pull/push, history, diffs |
| `crates/spell` | Spell checking: Hunspell dictionaries through `spellbook` (bundled en_US), word lists, word splitting, which parts of a document are prose |
| `crates/ui` | `next_code_editor` binary: editor, docks, LSP, terminal, search, problems, git and Python panels, diffs, fuzzy file finder (`Ctrl+P`) |
