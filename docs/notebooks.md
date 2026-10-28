# Notebooks

Jupyter notebooks (`.ipynb`, nbformat 4) and [marimo](https://marimo.io)
notebooks (a `.py` file with `import marimo`, `app = marimo.App(…)` and
`@app.cell` functions) open in a notebook tab (a
[built-in extension](built-in-extensions.md), on by default); other `.py`
files open as text. A marimo tab has **Open as Text**; a marimo file shown as text offers
**Open as Notebook** (also in the command palette).

- **Cells**: code cells are editors (highlighting, brackets, undo, Vim,
  spell checking, the language server below, completion from the kernel); Markdown cells render
  (headings, emphasis, lists, tables, code, links, images from attachments,
  `data:` URIs or files next to the notebook) and switch to an editor on
  Enter or double-click. The gutter shows `[n]` / `[*]`, a run button and the
  elapsed time; marimo cells also show *stale* (edited since they ran) and
  *error*. Only the cells on screen get editors, so long notebooks scroll
  smoothly.
- **Toolbar**: Run All, Run Above, Run Below, Interrupt, Restart, Restart &
  Run All, Clear Outputs, Save, the kernel status (idle, busy, starting,
  dead) and the interpreter; clicking it picks another one (the project's
  virtualenvs, uv's Pythons, Jupyter kernelspecs, or a typed path).
- **Keys** (Jupyter's): edit mode — Shift+Enter run and select below (a new
  cell after the last), Ctrl+Enter run, Alt+Enter run and insert below, Esc
  command mode (with Vim, from Normal mode), Ctrl+Shift+- split at the
  cursor, Up/Down across cell edges, Ctrl+K Ctrl+I the kernel's help for the
  name at the cursor. Command mode — Enter edit, A/B insert above/below,
  D D delete, Z / Shift+Z undo / redo cell operations, M/Y/R Markdown /
  code / raw, Up/Down/J/K select, Shift+Up/Down/J/K extend, Home/End,
  C/X/V copy/cut/paste cells (Shift+V above), Ctrl+Shift+Up/Down move, O
  collapse outputs, I I interrupt, 0 0 restart, Ctrl+S save. Text edits undo
  per cell. **Collapse/Expand Cell Source** (palette) folds a cell's code to
  one line; clicking it or Enter shows it again.
- **Language server** (Python notebooks, as VS Code does): the code cells,
  in order, are one module for the project's Python server (basedpyright by
  default; locally, in WSL or in the dev container like any file), so a
  name defined in cell 1 is known in cell 3. In cells: diagnostics
  (squiggles, hover, Problems entries named `notebook.ipynb [cell 3]` with
  the line in the cell; clicking one opens the notebook at that cell),
  hover, completion (the server's items first, then names only the kernel
  knows, such as a DataFrame's columns), parameter hints, go to
  definition/declaration/type definition/implementation and find
  references (targets in the notebook open that cell, others their file;
  the picker names cells), rename and code actions (edits land in the
  cells, one undo step per cell), semantic highlighting and inlay hints.
  **Format Cell** (`Shift+Alt+F` in a cell) and **Format
  Notebook** (`Shift+Alt+F` in command mode) run the Python formatter
  (`ruff format` by default, see Formatting) on each code cell; cells with
  IPython magics go to ruff as a one-cell notebook so the magics stay.
  The server sees `<notebook>.nc-notebook.py` next to the notebook (never
  written): each cell keeps its lines, IPython syntax (`%magic`, `!cmd`,
  `x = !cmd`, `name?`) is replaced on its line by a placeholder, `%%bash`
  and other non-Python cell magics blank their cell, and Jupyter notebooks
  start with stubs for `display`, `get_ipython`, `In` and `Out`. Diagnostics
  that only come from joining cells are dropped: "expression value is
  unused" on a cell's last expression (Jupyter shows it), redeclarations
  of a name redefined in another cell, top-level `await`, anything on an
  IPython line. An edit sends only the lines it changed (one change per
  keystroke or structural change). Limits: the server reads the cells as
  one file in notebook order, not in run order; document highlights and
  document symbols (outline, breadcrumbs) are not offered in cells;
  notebooks whose kernelspec names another language get no server;
  formatting with a formatter other than ruff skips cells with magics;
  a cell's inlay hints refresh when it is edited or its diagnostics change;
  cells have no status bar, so editor notices ("No definition found")
  are not shown there.
- **Outputs** scroll with the cells: streams (ANSI colours, `\r` progress
  lines), errors with coloured tracebacks, PNG/JPEG/SVG images, Markdown,
  JSON (pretty), LaTeX as text, plain text. `clear_output(wait=True)` and
  display updates work. Text outputs are selectable; text longer than
  `"notebook": { "output_max_lines": 200 }` (`settings.json`) shows **Show
  more**.
- **HTML** outputs render natively when they stay within a safe subset:
  headings, paragraphs, `div`/`span`, bold/italic/code, links (open in the
  browser), lists, quotes, rules, `<pre>`, `data:` and inline SVG images
  (remote images show as links), and tables with `colspan`/`rowspan`. Inline
  `style` honours only `color`, `background(-color)`, `font-weight` and
  `text-align` (also `#id` rules of `<style>` blocks, which is how pandas'
  Styler colours cells); colours that would not read on the theme are
  dropped. pandas DataFrames: bold header and index, numeric columns
  right-aligned, alternating rows, the header sticks to the top while a long
  table scrolls by, wide tables scroll sideways (Shift+wheel), and pandas'
  `...` truncation row/column show as such; past 500 rows a table shows its
  first 500. HTML with `<script>`, `<iframe>`, `<canvas>`, form controls or
  other elements (Plotly, Bokeh, Altair, xarray…) shows its text version with
  a note saying why; Plotly figures (`application/vnd.plotly.v1+json`) get
  the same note. Every HTML output keeps **Open in browser** (Plotly's opens
  a page drawing the figure with plotly.js).
- **ipywidgets** work through the Jupyter comm protocol (the bridge forwards
  `comm_open`/`comm_msg`/`comm_close` both ways, with binary buffers):
  Int/Float/FloatLog/IntRange/FloatRange sliders (dragging sends updates
  while `continuous_update`, else at the end), Int/Float/Bounded text,
  Text, Textarea (Enter or typing sends, per `continuous_update`), Checkbox,
  ToggleButton(s), Dropdown, RadioButtons, Select, SelectMultiple, Button
  (`on_click`), Label, HTML/HTMLMath (through the HTML renderer), Image,
  Int/FloatProgress, HBox/VBox/Box/GridBox (as flowing rows/columns),
  Accordion, Tab and Stack, and Output widgets (what they capture renders as
  outputs), so `interact`/`interactive` work. `description`, `disabled`,
  `layout.width`/`display` and the readout format are honoured. The widget
  state is saved in the notebook's `metadata.widgets` as Jupyter does, so a
  reopened notebook (or one whose kernel ended) shows the last values,
  disabled until the cell runs again. Widgets of other libraries
  (anywidget, ipyleaflet, Plotly's FigureWidget, bqplot…) show their text
  and a note.
- **Files**: saving keeps everything the file had (metadata, cell ids,
  attachments, `nbformat_minor`) and writes Jupyter's own JSON layout
  (sorted keys, the file's indentation, a final newline), so `git diff`
  shows only what changed; outputs, execution counts and collapsed
  outputs/sources are saved (`metadata.jupyter.outputs_hidden` with the
  older `collapsed`, and `source_hidden`, as JupyterLab and VS Code read
  them). marimo files are written as marimo writes them (header,
  `__generated_with`, cell names and options kept; each cell's parameters
  and returns recomputed from its code); they have no place for collapsed
  state, which then lasts while the notebook is open. Changes on disk
  reload a clean notebook (a modified one asks): cells are matched by text,
  marimo cell name and position, so unchanged cells keep their outputs
  (also when a tool stripped the outputs from the file); in a marimo
  notebook an edited cell and its descendants turn *stale*, as after an
  edit in the editor.
- **Kernel**: an ipykernel in the project's interpreter (the folder's
  `.venv`, else `uv run python` in a uv project, else `python3`/`python`),
  started on the first run, in the notebook's folder, where the project
  lives (also in WSL and dev containers, where its ports are not
  forwarded). A small Python bridge drives it
  with `jupyter_client` (no ZeroMQ in the editor); it is sent to the
  interpreter on stdin, so nothing is installed. Without ipykernel a banner
  offers **Install ipykernel** (`uv add --dev ipykernel` in uv projects,
  else `python -m pip install ipykernel`, in a terminal) and **Choose
  Interpreter**. Closing the tab or the editor stops the kernel; a kernel
  whose editor died exits by itself.
- **marimo**: cells run in one kernel with marimo's rules: running a cell
  also runs its stale or never-run ancestors and then every cell that uses
  what it defines, in dependency order; a name defined by two cells, or a
  cycle, is an error on those cells; cells after a failure are skipped.
  Each cell runs with only the names it uses and publishes the names it
  defines (`_private` names stay in the cell); top-level `await` works
  (`async def` cells, on the kernel's event loop); `mo.md(…)` and other
  marimo outputs render. `mo.ui` sliders, numbers, text fields,
  checkboxes, switches, dropdowns, radio groups and buttons are drawn
  natively, also inside `mo.md(f"… {slider} …")`: changing one sets its
  value in the kernel as marimo does (conversion, `on_change`) and re-runs
  the cells that use it. Other elements (tables, forms, arrays, dates,
  files…) show as text: **Open in marimo** runs `marimo edit` in a terminal
  and opens its URL in the browser (forwarding the port for WSL and
  containers).
