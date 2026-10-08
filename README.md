# next_code

Native, AI-first IDE written in Rust on [gpui](https://crates.io/crates/gpui).

## Layout

| Crate | Purpose |
|---|---|
| `crates/text` | UI-agnostic rope buffer: edits, undo/redo, grapheme/word motion, UTF-16 mapping, line endings |
| `crates/ui` | `next_code` binary: window, file tree, tabs, editor view |

## Build and run

Windows (MSVC Build Tools required), macOS, or Linux:

```sh
cargo run -p next_code -- <project-folder>
```

Without an argument the current directory is opened.

Linux additionally needs the gpui system libraries (Wayland/X11, xkbcommon, fontconfig, Vulkan).

## Roadmap

- **M0** editor core: window, file tree, tabs, editing, save — *in progress*; next: tree-sitter highlighting, fuzzy file finder
- **M1** LSP (completion, go-to-definition, diagnostics), terminal, project search
- **M2** host agent protocol, WSL
- **M3** git panel and diff view, multiple workspaces
- **M4** AI inline completion, chat, agent
- **M5** devcontainers, debugger (DAP)
- **M6** WASM extension API
