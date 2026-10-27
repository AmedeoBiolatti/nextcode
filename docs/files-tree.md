# Files tree

The Files panel works like PyCharm's Project view (VS Code's Explorer where
PyCharm has no equivalent), for local, WSL and dev container projects alike.

- **Selection**: click selects and opens a file (focus stays in the tree;
  double-click or `Enter` focuses the editor), `Ctrl`+click toggles a row,
  `Shift`+click / `Shift+Up/Down` select a range, `Up/Down/Home/End` move,
  `Right/Left` open/close folders, `Ctrl+A` selects every visible row.
- **Context menu** (right-click a row, or empty space for the project
  root): New File / Folder / Python File (New File with Python chosen) /
  Python Package
  (folder with `__init__.py`), Cut, Copy, Paste, Duplicate, Copy Path,
  Copy Relative Path, Rename, Delete, Reveal in File Explorer (local
  projects), Open in Terminal (a shell in that folder). `Alt+Insert` shows
  the New items under the selected row (`Up/Down/Enter` choose).
- **New File…** is also in the File menu and the command palette wherever
  focus is: it shows the Files panel and starts in the active file's
  folder.
- **Names** are typed in place (VS Code style): New puts an input at the top
  of the folder (the selected folder, or the selected file's folder); a
  path such as `pkg/sub/mod.py` creates the folders too. Rename (`F2` or
  `Shift+F6`) selects the name without its extension. Problems show under
  the input while typing: empty name, `/`, `.`/`..`, on local Windows
  projects `\ < > : " | ? *`, control characters, trailing dot/space and
  reserved names (`CON`, `NUL`, `COM1`…), and names already taken.
  `Enter` applies, `Escape` cancels, clicking elsewhere applies a valid
  name. A new file opens in an editor (a notebook in the notebook view).
- **File types**: New File's row has a type selector beside the name,
  **Auto** by default (the name is used as typed; the type its extension
  implies shows faintly). `Tab` (or `Alt+Down`, or a click) opens the type
  list; typing filters it (by name or extension), `Up/Down` and `Enter`
  (or `Tab`) choose, `Escape` or `Shift+Tab` go back to the name (a second
  `Escape` cancels). Types: Text, Python, Jupyter Notebook, marimo
  Notebook, Markdown, JSON, YAML, TOML, Rust, JavaScript, TypeScript,
  TypeScript React, HTML, CSS, Shell script, PowerShell, C, C++, Go, SQL,
  Dockerfile, Dev Container config, NextCode Workflow, .gitignore. A
  chosen type adds its extension when the name has none of its own
  (`analysis` → `analysis.ipynb`, `tool.py` stays) and writes its starter
  text: an empty notebook with one code cell (Python kernel), a marimo
  app with one cell, the commented workflow template (and its schema
  beside it), a dev container `name` + `image`, a `#!/usr/bin/env bash`
  line; the others start empty. The name may stay empty for the types
  that name their file: Dockerfile (`Dockerfile`, or `<name>.Dockerfile`),
  .gitignore (a typed name is its folder), Dev Container config
  (`.devcontainer/devcontainer.json`, or `.devcontainer/<name>/…`, at the
  workspace folder's root) and NextCode Workflow
  (`.next_code/workflows/<name>.yaml` at the project root; no name:
  `workflow.yaml`, `workflow-2.yaml`, …).
- **Cut/Copy/Paste** (`Ctrl+X/C/V`) paste into the selected folder (or the
  selected file's folder); copies whose name is taken become `name
  copy.ext`, `name copy 2.ext`…; a cut entry moves (refused if the name is
  taken there). `Ctrl+D` duplicates next to the original. The paths are
  also put on the clipboard as text.
- **Drag and drop** rows onto a folder (onto a file: its folder; onto empty
  space: the root) to move them; hold `Ctrl` at the drop to copy; more than
  five items ask first; `Escape` cancels the drag; a folder dropped into
  itself does nothing.
- **Delete** (`Delete` key) asks first, naming the item (or listing up to
  ten) and any unsaved files among them. Local Windows projects move to the
  Recycle Bin; Linux, WSL and dev container projects delete permanently
  (the dialog says so). Editor tabs of deleted files close.
- **Open editors follow** renames and moves (path, title, unsaved edits,
  language server document, breakpoints); the watcher's events for the
  tree's own changes are not treated as external edits (no reload prompt,
  no "deleted on disk" notice, no second tree refresh).
- **Language servers**: before a rename or move, running servers that
  registered `workspace/willRenameFiles` are asked for edits (5 s each),
  which are applied first; `didRenameFiles` follows. rust-analyzer updates
  `mod` lines and paths; basedpyright updates Python imports
  (`from helpers import x` → `from tools import x`). Servers without it are
  skipped; failures and timeouts are reported with the operation's notice.
- Errors (taken names, permissions, agent failures) appear as selectable
  notices above the editor.
