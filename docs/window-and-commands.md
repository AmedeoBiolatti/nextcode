# Window, menus and commands

Top to bottom, a window has the menu bar (File, Edit, Selection, View, Go,
Run, Git, Help) with the command bar in it, the project tabs, and the active
project. On Windows the menu bar is also the title bar: the system one is
hidden, empty space in the bar moves the window (double click maximizes),
and minimize/maximize/close sit at its right. Each project tab is a whole
workspace (docks, editor tabs, language servers, terminals); closing it
asks about unsaved files first.

- **File > Open Project…** (`Ctrl+O`) picks a folder (a
  `\\wsl.localhost\...` folder opens through WSL) and opens it in a new
  tab; **Open Project in New Window…** does the same in its own window.
- **File > Recent Projects** lists recent projects and workspaces; **Open
  Recent…** (`Ctrl+R`) also accepts a typed location such as
  `Ubuntu:~/project`, `devcontainer:C:\src\app` or a workspace file.
- **File > Open Workspace from File…**, **Add Folder to Workspace…** and
  **Save Workspace As…**: see [multi-root workspaces](multi-root-workspaces.md).
- Opening a project that is already open (in any window) switches to its
  tab. `Ctrl+Alt+]` / `Ctrl+Alt+[` cycle the tabs.
- Drag project or workflow tabs left/right to reorder them, just like file
  tabs. Drop on a tab to insert before it, or on `+`/empty strip space to
  append. The current project stays active, and the order survives a restart.
- The command bar (`Ctrl+Shift+P` or `F1`) lists every command available
  where focus is, with its shortcut; typed words filter it (in any order),
  `Enter` runs the command where focus was.

Like VS Code (`window.restoreWindows: all`), a launch without a project
reopens the windows that were open when the editor closed: each with its
project tabs, active tab, size, position (moved onto a connected display
if its monitor is gone) and maximized state, focusing the window that had
focus. Each project then reopens its editor tabs (see
[session restore](split-editor-and-sessions.md)); WSL and container
projects reconnect as when opened from the recent list, and local folders
that no longer exist are skipped. Closing a project tab, or a window while
others remain, removes it; quitting (**File > Exit**, a termination
signal on Linux, or closing the last window) keeps every window open at
that moment. A window on the welcome screen is kept only when it is the
only one. Naming a project on the command line (`next_code .`,
`next_code <dir>`, `--wsl`, `--devcontainer`) opens just that project,
in the editor already running if there is one (see
[Startup](building.md#startup)); a launch without a project while the editor runs
brings its windows forward, or reopens the saved ones when it stayed
running with none.
The state is `windows.json` next to `recent.json`
(`%LOCALAPPDATA%\next_code\` on Windows, `~/.local/share/next_code/` on
Linux); `"restore_windows": false` in `settings.json` shows the welcome
screen instead.

Every menu item is a command, the same ones the palette runs. Menus are
customizable: **File > Customize Menus…** writes the built-in menus to
`menus.json` in the config dir (`%APPDATA%\next_code` on Windows,
`~/.config/next_code` on Linux) and opens it; the file then replaces the
built-in menus, and edits apply when a menu is next opened:

```json
{ "menus": [
    { "label": "File", "items": [
        "app::OpenProject",
        { "label": "Recent Projects", "list": "recent_projects" },
        "-",
        { "command": "editor::Save", "label": "Save File" },
        { "label": "More", "items": ["app::About"] }
    ] }
] }
```

An item is a command name, `"-"` (separator), a command with its own
label, a submenu, or a list. Command names are `namespace::Name` (`app`,
`workspace`, `editor`, `git`, `python`); unknown ones show disabled with a
notice, and an invalid file falls back to the built-in menus.

**Copying text from the UI.** Messages, errors, notices, banners, hover
and parameter-hint docs, Problems entries (with their `source(code)`, e.g.
`rustc(E0308)`), workflow details and step output, test failures and the
status bar can be selected like web-page text: drag (also past the text's
end), double-click a word, triple-click the whole message; `Ctrl+C` (or
`Ctrl+Insert`, Edit > Copy) copies, `Ctrl+A` selects the whole message,
`Escape` clears and returns focus where it was. Right-click shows **Copy**
and **Copy All**. A plain click still does what it did (a Problems row or
the workflow banner still jumps to the location), and in rows where a
double-click does something (Commit window and Git log files, branches,
Tests, Call Hierarchy, debugger values) a double-click on the text still
does it instead of selecting a word; dragging still selects. A selection
stays within one message.
