# Multi-root workspaces

A project tab can hold several folders, like a VS Code `.code-workspace`
window. The folders live on one machine: all local, or all in one WSL
distro (a dev container project has a single folder; adding one, saving it
as a workspace or opening a workspace in a container is refused with a
message).

- **File > Add Folder to Workspace…** adds folders (the system folder
  dialog for local projects; a typed path for WSL projects, or where there
  is no dialog). A folder on another machine is refused. The Files tree's
  folder rows have **Add Folder to Workspace…** and **Remove Folder from
  Workspace** (disabled for the last folder; also in the command palette
  while the tree has focus).
- **File > Save Workspace As…** writes a `*.next_code-workspace` file
  (the extension is added when missing) and reopens the tab from it; the
  tab is then named after the file (`all (Workspace)`). Adding or removing
  folders later rewrites that file.
- **File > Open Workspace from File…**, `Ctrl+R` with a typed path, a
  Recent entry or the command line open `*.next_code-workspace` and VS Code
  `*.code-workspace` files (`Ubuntu:/home/me/all.code-workspace` for one in
  WSL). VS Code files are read, never written: changing the folders of one
  turns the tab into an untitled workspace (save it as a next_code file to
  keep it).
- Unsaved workspaces are kept too: Recent and window restore reopen them
  with their folders (and each folder's session).

The project's folders are fixed for the life of its workspace: adding or
removing one saves the session (open editors, splits, layout) for the new
set and reopens the project in the same tab, as VS Code reloads its window
when a folder becomes a workspace. Files with unsaved changes must be saved
or closed first (a notice says so).

```jsonc
{
  "folders": [
    { "path": "." },                          // relative to the file
    { "path": "../api", "name": "API" },      // shown as "API"
    { "path": "/srv/shared/lib" }             // absolute
  ],
  "settings": { "format_on_save": true, "languages": { "python": { "word_wrap": true } } }
}
```

Comments and trailing commas are allowed. VS Code files may list folders
as `path` or `file:` `uri` (other URIs are skipped); of their `settings`,
`editor.wordWrap`, `editor.formatOnSave`, `editor.inlayHints.enabled`,
`editor.autoClosingBrackets`, `editor.minimap.enabled`,
`editor.stickyScroll.enabled`, `git.blame.editorDecoration.enabled`,
`python.analysis.typeCheckingMode` and per-language blocks
(`"[python]": { "editor.formatOnSave": … }`) are translated; the rest is
ignored.

**Settings layering.** A file's settings are the user's `settings.json`,
under the workspace file's `settings`, under its folder's own
`.next_code/settings.json` (any folder, also single-folder projects; same
schema as `settings.json`). They apply to word wrap, inlay hints, format on
save and formatters, language servers and tests; app-wide settings (theme,
window restore, menus) stay the user's. A broken workspace or folder file
shows a notice and is skipped; changes to a folder file apply when it is
saved.

**Across folders.** With more than one folder, lists label paths with their
folder, `api › src/main.rs`:

- **Files tree**: one bold row per folder, in the workspace's order (the
  first expanded). Folder rows cannot be renamed, moved or deleted; files
  can be dragged or pasted between folders. Copy Relative Path is relative
  to the item's folder.
- **Go to File** (`Ctrl+P`) lists every folder's files; typing a folder's
  name narrows to it (`web main`). **Find/Replace in Files** searches every
  folder (a file inside two nested folders counts once); include/exclude
  globs apply relative to each folder. Replace notices, references, Go to
  Symbol in Workspace and the breadcrumb file browser (whose `..` at a
  folder's top lists the folders) use the same labels.
- **Git** (PyCharm's multi-repository mode): each folder's repository is
  found (folders in one repository share it; folders outside git are
  skipped; a refused repository gets its own Trust banner). The Commit
  window shows one collapsible header per repository (`name`, branch, file
  count, checkbox); **Commit** commits each repository that has checked
  files with the same message and reports per repository; Commit and Push
  opens a Push dialog per repository; Fetch, Pull and Update Project act
  on all of them. The Shelf tab and the Git log have a repository
  selector; the log follows the active editor's repository until one is
  picked. The title-bar branch widget shows the active file's repository
  (`api: main`), and the branches popup has a row of repositories to
  switch between. Change markers, blame, Stage Hunk and Show Diff use each
  file's own repository.
- **Language servers** start with every folder in `workspaceFolders`
  (`rootUri` is the first) and answer `workspace/workspaceFolders` with all
  of them; folders never change while a server runs, so
  `didChangeWorkspaceFolders` is not needed. A server that does not
  advertise workspace folder support gets one instance per folder (in the
  folder, with its settings and interpreter; logs `lsp-<lang>-<n>.log`),
  and each document goes to its folder's server. **Problems** are grouped
  under a header per folder.
- **Terminal**: New Terminal starts in the active file's folder and
  activates that folder's `.venv`; tab titles end with `(folder)`.
- **Run configurations** come from every folder (`.next_code/run.json`,
  `.vscode/launch.json`, pyproject/Cargo) as `name (folder)`, resolve
  `${workspaceFolder}` to their folder and run there; Edit Configurations
  opens the active file's folder's `run.json`. The **Tests** panel has a
  node per folder (test ids `folder › id`), each run in its folder.

Limits: the Python panel shows the first folder's uv project; a folder
nested in another shows both under its parent and as its own row;
`exec` and most `python_*` headless commands run in the first folder.
