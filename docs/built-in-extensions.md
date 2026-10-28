# Built-in extensions

Some features are built-in extensions: part of next_code, turned on and off
in **File > Manage Extensions…** under **Built-in**. One that is off has no
menu items (also when `menus.json` lists them), commands, panels or
suggestions, and turning it off closes its tabs (a tab with unsaved
changes stays until closed; a notebook tab shows its file as text). Its
panels keep their place in the dock layout for when it is turned on again. The choice is
saved in `<config>/next_code/extension-state.json`, beside the grants of
installed extensions; until then each keeps its default. An invalid file
turns them all back to their defaults.

| Extension | Default | Contributes |
|---|---|---|
| [Assembly Inspection](assembly-inspection.md) | off | Run > Inspect Assembly, assembly tabs |
| [Workflows](workflows.md) | on | workflow tabs, Run > Open Workflow… / Open Workflow File… / Add Workflow Folder… / New Workflow…, the palette's *Run Workflow:* entries |
| [Notebooks](notebooks.md) | on | notebook tabs for `.ipynb` and marimo files, the `notebook::*` commands, Run > Run Cell / Run All Cells / Interrupt / Restart, the *Open as Notebook* suggestion |
| [Python](python.md) | on | the Python panel, the `python::*` commands (Sync, Lock, Run File, Run Tests…) and the *out of date* Sync suggestion |
| [Sticky Notes](sticky-notes.md) | on | note cards and markers in editors and notebook cells, Add Note, the Notes panel and its count, the *left you a note* suggestion |
| [Remote - SSH](remote-development.md#ssh-and-tailscale-machines) | on | `ssh://` project locations, File > Connect to SSH Host…, SSH hosts and Tailscale machines in Open Recent |

With Python off, Python files still get their language server, run
configurations, debugging, notebooks and the Tests panel's pytest tests.
The panel's command was `workspace::TogglePython`; that name still works
(for example in `menus.json`) and runs `python::TogglePanel`. Likewise
`workspace::ToggleNotes` runs `notes::TogglePanel`. With Sticky Notes off,
the notes files and `next_code notes` work as before; the person just does
not see the notes in the app. With Workflows off, running workflows stop,
and windows reopen without their workflow tabs; workflow files still get
their language server and New File's *NextCode Workflow* type. With
Notebooks off, `.ipynb` and marimo files open as text. Headless mode's
`workflow_*` and `notebook_*` commands work either way.

For contributors: a built-in extension is a module under
`crates/ui/src/builtins/` with a `Builtin` in `builtins::ALL`. It declares
its commands' labels, the commands the palette leaves out, and menu items
(after a named command, separators included). In each window it may
install command handlers, keep a model, add palette entries, hear that a
project tab opened, and open window tabs (`WindowTabKind`: a file of a
project or of this machine beside the project tabs, saved with the window
as `{"<kind>": file, "project": …}`, its view a `WindowTabView`). In each
workspace it may install command handlers, keep a model, add dock panels
(`Panel`: a key that is also the saved-layout name, a default dock and
neighbour, an optional count), open files in its own tabs instead of a
text editor (`FileTabKind`, the view a `FileTabView`: dirty, saved,
restored with the session and released like an editor's file), show
language-server locations in documents of its own, and hook every open
file's view (`attach`/`detach`), tab changes, file changes and suggestions
(`OfferKind::Builtin`) with their accept hook. Anywhere, it may add kinds
of machines projects open on (`Connector`: a scheme, so
`<scheme>://host/path` is a `Location::Remote`; a function that starts the
agent there and returns its connection; the machines Open Recent suggests);
the connector's functions run on threads, and opening checks the saved
on/off choice. It reaches the window and the workspace through the methods
in `shell/builtins.rs` and `workspace/builtins.rs`; nothing else names it.
