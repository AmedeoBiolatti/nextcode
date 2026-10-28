# Build and run

Windows (MSVC Build Tools required), macOS, or Linux:

```sh
cargo run --release -p next_code -- C:\path\to\your\project
```

Replace the example path with an existing project folder (`.` is the
current directory). Without an argument, the windows and project tabs that
were open when the editor last closed come back (see
[Window, menus and commands](window-and-commands.md)); the first time,
or with nothing to reopen, a window with the welcome screen opens.

To create a project, choose **File > New Project…**, use the welcome screen's
**New Project…** link, or press **Ctrl+Alt+N** (**Cmd+Alt+N** on macOS).
Enter a folder name, type or browse for its parent folder, and choose an
Empty, Python, Rust, C, or C++ starter. **Create Project** creates the local folder
and opens it in a new project tab; existing folders are never overwritten.
Python and Rust starters include a small application, a package manifest,
and `.gitignore`. For the Python starter, optionally check **Initialize with uv**
to run [`uv init`](https://docs.astral.sh/uv/reference/cli/#uv-init), including
its Python version pin and README. This requires uv and a local Python
interpreter; the editor also finds uv in `~/.local/bin`. Initialization creates
a standalone project without changing a parent uv workspace or initializing Git.
Creation does not install dependencies or a toolchain; `uv.lock` and `.venv`
are created later when you sync or run the uv project.
Tab/Shift+Tab move between fields and buttons; Escape cancels.

C and C++ starters include a C17 or C++20 application, CMake files, `.clangd`,
and `.next_code/run.json`. Select **Build and Run** in the toolbar to configure,
compile and start the application, or **Build** to compile only. This requires
CMake 3.20+ and a C/C++ compiler. After building, **Debug (build first)** uses
the editor's existing CodeLLDB integration. CMake exports
`build/compile_commands.json` for Makefile/Ninja generators; clangd uses it for
completion and diagnostics when installed. Each starter includes a README
with terminal commands and setup instructions.

For example, from PowerShell:

```powershell
cargo run --release -p next_code -- C:\dev\next_code
```

Linux additionally needs the gpui system libraries (Wayland/X11, xkbcommon, fontconfig, Vulkan).

On Ubuntu, install Rust using [rustup](https://rustup.rs), then install the
native build dependencies:

```sh
sudo apt-get update
sudo apt-get install --no-install-recommends build-essential pkg-config cmake clang \
  libclang-dev libfontconfig-dev libfreetype-dev libxkbcommon-dev \
  libxkbcommon-x11-dev libwayland-dev libx11-dev libxcb1-dev libssl-dev \
  libvulkan-dev libzstd-dev
```

Build the launcher, editor, agent and all grammar libraries together, then
open the current folder:

```sh
cargo build --release --workspace --locked -j 2
./target/release/next_code .
```

The two build jobs limit memory use on machines with 16 GB of RAM. The
first release build downloads and compiles all dependencies and can take
some time. The desktop also needs a working Vulkan driver.

Install the build for your user account and register the NextCode icon in
the desktop's applications menu and dock:

```sh
./scripts/install-linux.sh
```

The default destination is `~/.local/share/next_code/bin` (or under
`XDG_DATA_HOME` when set). The installer also writes `next_code.desktop`
and the icon under the user's data directory. Re-run it after rebuilding;
restart existing editor windows to pick up updated window controls and
desktop identity. Linux uses the menu bar for minimize, maximize/restore
and close when the compositor requires client decorations.

On Windows the editor is a GUI-subsystem program: no console window opens
with it. Started from a terminal, it returns to the prompt right away;
argument errors and `--headless` output still print to that terminal (or to
redirected streams), and closing the terminal does not close the editor.

## Startup

The build makes two programs: `next_code` (`crates/launcher`), a small
launcher without gpui, and `next_code_editor` (`crates/ui`), the editor.
Run `next_code`. It hands its command line to the editor already running
(over a named pipe on Windows, a Unix socket in the state directory
elsewhere), which opens the window in a fraction of the time a process
start takes, and starts `next_code_editor` next to it when none runs. Like
VS Code, `next_code <dir>` from a second shell opens in the same process;
a project already open comes forward. `cargo run -p next_code` runs the
editor directly, which joins a running editor the same way.

- `--new-instance` starts a separate editor process that neither joins
  nor serves launches (tests and `--automation` use it).
- `--quit` asks the running editor to exit.
- `"keep_running"` in `settings.json`: `true` keeps the editor running
  after its last window closes, ready for the next launch; `false` exits;
  `"auto"` (the default) keeps it except for builds run from a cargo
  `target` directory, which cargo must be able to rewrite. A resident
  editor exits by itself when its executable has been replaced (the next
  launch starts the new build), on `--quit`, and on **File > Exit**.
- `scripts\install.ps1` copies the release build to
  `%LOCALAPPDATA%\next_code\bin` (asking a resident editor to exit first),
  where launches do not hold the cargo output; add that folder to `PATH`.
- The tree-sitter grammars (about 14 MB of parse tables) are not in the
  executable: each is a library, `nc_grammar_<name>.dll` on Windows
  (`libnc_grammar_<name>.so` on Linux), built with the editor and loaded
  the first time a file of its language opens. The editor looks in the
  `NEXT_CODE_GRAMMARS` directory, next to its executable, then in its
  `deps` directory (where cargo puts libraries built as dependencies, so
  `cargo run` finds them). A missing or stale library (the ABI is checked)
  leaves that language as plain text, with the reason on stderr;
  `cargo build --release --workspace` puts the libraries next to the
  executable, and `scripts\install.ps1` copies them.
- Size, Windows release build: 55 MB at the start of this work, 38 MB
  with `panic = "abort"`, thin LTO and gpui's `image` trimmed, 25 MB with
  the grammars in libraries, 24 MB with cold dependencies (LSP types,
  notebooks, workflows, git, SVG and image decoding, ...) at `opt-level =
  "s"`. `cargo build --profile dist` (fat LTO, one codegen unit) takes
  another 2 MB off at about four times the build time; `scripts\install.ps1
  -Profile dist` installs it.

Set `NEXT_CODE_TRACE_STARTUP=1` to print startup milestones (time since the
process was created) to stderr, e.g. from PowerShell:
`$env:NEXT_CODE_TRACE_STARTUP=1; .\next_code_editor.exe C:\dev\p 2> trace.txt`.
The WSL connection (or, for local projects, git status) starts on a thread
before the window, alongside gpui's platform setup. gpui is a patched copy
(`vendor/gpui`, see `NEXT_CODE_PATCHES.md` there): its Windows platform
creates the Direct3D device once (upstream probed with a throwaway device
first) and on a thread of its own, loads DirectWrite's font list on
another, and creates the shell's drag-and-drop helper on the first drag
instead of at startup. The release profile is `panic = "abort"` with thin
LTO and gpui's `image` crate trimmed to the formats shown, for a smaller
executable: on machines whose endpoint security scans every image load,
each MB of executable costs milliseconds per launch, and a rebuilt
executable's first launch pays a full scan.
