# Python (uv)

The **Python** panel (left dock) reads `pyproject.toml` and `uv.lock`:
project, interpreter (`.venv`), pinned version (`.python-version`) and the
dependencies of each section with their locked versions. It runs uv where
the project lives (Windows, WSL or a dev container):

- **Sync**, **Lock**, **Upgrade all**, per-package **↑** (upgrade in the
  lock) and **×** (remove); a requirement typed in *Add dependency* goes to
  the section shown next to it (click to cycle: dependencies, `dev`, other
  groups, extras).
- **Python…** lists interpreters (`uv python list`) and pins one.
- When `.venv` is out of date with `uv.lock`/`pyproject.toml`
  (`uv sync --check`), the editor offers **Sync**. After a sync the
  language server re-reads the interpreter.
- **Run**: `[project.scripts]` entry points and the current file run as
  configurations (`uv run …`) in a terminal tab named after them (running
  again replaces the tab); all tests, the current file's tests, or the test
  under the cursor (pytest node id from the enclosing `test_*` function /
  `Test*` class) run in the **Tests** panel (see *Run configurations and
  tests*). Ctrl+click on a traceback line (`File "…", line N` or pytest's
  `path:N:`) opens it.

New terminals activate `.venv` (`VIRTUAL_ENV`/`PATH` locally;
`. .venv/bin/activate` in WSL and containers).

Links in terminal output are underlined under the mouse; Ctrl+click opens
them (the pointer turns into a hand while Ctrl is held). URLs (`http`,
`https`, `file`, `ftp`) open in the browser; file locations open in the
editor at their line and column: compiler-style `path:line:column`,
`path:line`, Python's `File "path", line N`, and paths with a separator
(`src/lib.rs`). Relative paths are resolved against the project root. OSC 8
hyperlinks (`ls --hyperlink`, `gcc`, `cargo` with hyperlinks on) open their
target. A link wrapped over several rows is one link.

Terminal lines are not wrapped at the panel's edge: a line wider than the
view (a long `printf`, output printed while the panel was wider, a long
command being typed) stays one row, and a left-right bar appears over the
bottom edge while any row on screen is wider. Drag its thumb, press the
track (the thumb centres there and drags), turn the wheel over it, or use
Shift+wheel or a trackpad's sideways scroll anywhere in the terminal. The
offset stays where you put it while output arrives (following the end of
the output as before); typing brings the cursor back into view, and it
returns to the start when nothing on screen is wider (`clear`,
full-screen programs). Programs still see the panel's width (the pty
follows the view, so `vim`/`htop` fit it and shells wrap their input as
they expect): the emulator wraps there and only the display joins the
rows, so a line keeps its full width when the panel narrows or widens.
Selection, copy, Ctrl+click on links and Pinpoint use the cells shown.
Lines over 2048 columns are cut into rows of that width.
