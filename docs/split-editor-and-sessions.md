# Split editor and session restore

The editor area splits into panes (VS Code's editor groups), each with its
own tabs and active tab. A file open in two panes is two views of one
buffer: edits, undo and the unsaved marker are shared, and closing one view
of an edited file does not ask (closing the last one does). Files opened
from the tree, the finder, search, Problems, the terminal, diffs or Back /
Forward (`Alt+Left` / `Alt+Right`) go to the active pane (the one last
clicked or focused).

| Command (View menu, palette) | Shortcut |
|---|---|
| Split Editor Right (the active file, at the same position) | `Ctrl+\` |
| Split Editor Down | `Ctrl+K Ctrl+\` |
| Focus First / Second / Third Editor Pane (one past the last splits right) | `Ctrl+1` / `Ctrl+2` / `Ctrl+3` |
| Focus Next / Previous Editor Pane | `Ctrl+K Ctrl+Right` / `Ctrl+K Ctrl+Left` |
| Move Tab to Next / Previous Pane (splits one off at the edge) | `Ctrl+Alt+Right` / `Ctrl+Alt+Left` |
| Close Editor Pane (asks once about unsaved files) | `Ctrl+K W` |

Tabs can be dragged onto another pane's tab bar (before a tab or at the
end), into a pane's middle, or onto its left/right/top/bottom quarter to
split it there. The bar between panes resizes them. Closing a pane's last
tab removes the pane. Middle click closes a tab.

Each project remembers its editor tabs per pane, the active tab and pane,
the splits and their sizes, and every editor's selections and scroll, and
reopens them the next time the project opens (local, WSL and container
projects alike). Files that no longer exist are skipped; diff, history and
comparison tabs are not kept. The state is `session.json` next to the
project's saved layout, in `%LOCALAPPDATA%\next_code\projects\<key>\` on
Windows or `~/.local/share/next_code/projects/<key>/` on Linux (the key is
the project's location with other characters than letters and digits as
`_`). `"restore_session": false` in `settings.json` turns it off.
