# Code navigation

## Go to definition and friends

| Command (Go menu) | Keys |
|---|---|
| Go to Definition | `F12`, `Ctrl+B`, `Ctrl+click` (`Cmd` on macOS) |
| Go to Declaration | Go menu / command bar |
| Go to Type Definition (of the variable or expression's type) | `Ctrl+Shift+B` |
| Go to Implementation (of a trait/interface or its method) | `Ctrl+F12`, `Ctrl+Alt+B` |
| Find References | `Shift+F12`, `Alt+F7` |

One result opens directly, several open the locations picker; `Alt+Left`
goes back. On the definition itself, Go to Definition (and Ctrl+click)
finds its usages instead, as in VS Code and JetBrains IDEs. While `Ctrl`
is held, an identifier under the mouse that the language server has a
definition for is underlined and the pointer becomes a hand (asked once
the mouse rests on a word for a moment, remembered per word until the
next edit); a click goes to its definition without starting a selection.
This works in notebook cells, read-only files and remote (WSL, dev
container) projects alike, and in the editor under the mouse even when
another one (another cell or pane) has focus. Alt+click (add a cursor),
Shift+Alt+drag and middle-drag (column selection) are unchanged.
Commands the server does not support say so in the status bar.

## Parameter hints and inlay hints

**Parameter hints** (signature help) appear above the cursor when typing
one of the server's trigger characters (`(` and `,` for most servers) or
with `Ctrl+Shift+Space`: the signature with the current parameter in
bold, then that parameter's and the function's documentation. They follow
the cursor and close when it leaves the call, on `Escape`, or on a jump.
With several signatures (overloads), `Up`/`Down` (or `Alt+Up`/`Alt+Down`,
or the arrows in the popover) switch between them. The completion menu
can show at the same time; its keys win while it is open.

**Inlay hints** show what the server infers inside the text, muted: types
after `let` names and closure parameters, parameter names before
arguments, chained types, and so on (whatever the server sends). They
are not text: the cursor steps over them, clicks on one land beside it,
and they are never selected, copied or found. Word wrap counts their
width. The visible lines (and a screen above and below) are asked for
250 ms after edits and when scrolling. `"inlay_hints": false` in
`settings.json` (or `"languages": { "<id>": { "inlay_hints": … } }`) turns
them off, live; View > Inlay Hints (`Ctrl+Alt+I`) toggles them for every
editor until the app exits. Theme keys: `inlay_hint`, `inlay_hint_background`.

## Call hierarchy

**Show Call Hierarchy** (`Shift+Alt+H`, JetBrains `Ctrl+Alt+H`; Go menu)
asks the language server about the symbol at the cursor and opens the
**Call Hierarchy** panel (bottom dock; View > Call Hierarchy toggles it). The
header names the symbol and switches between **Incoming** (who calls it, the
default) and **Outgoing** (what it calls); switching starts over from the
symbol. Each level is fetched when it is expanded, so recursive calls can be
followed as deep as wanted. Rows show the kind, name, detail and `file:line`.
`Up`/`Down` select, `Right`/`Left` expand/collapse (or step into the first
child / out to the parent), `Enter` or a click opens the location (an
incoming caller opens at its call site, an outgoing callee at its name; Back
returns), double-click or the chevron expands/collapses.

## Symbols: outline, breadcrumbs, Go to Symbol

Each file editor asks its language server for the document's symbols when it
opens and 300 ms after edits (until then the previous ones move with the
text).

- **Outline** panel (right dock; `Alt+7`, View > Outline): the active editor's
  symbol tree with kind badges, follows tab and pane switches, highlights the
  symbol around the cursor as it moves, and says when there is no language
  server or no symbols. Click a row (or `Enter`) to jump there; the chevron
  collapses/expands. With the panel focused (`Alt+7` focuses it),
  `Up`/`Down` select and `Right`/`Left` expand/collapse or step in/out.
- **Breadcrumbs** above the text: the project-relative path, then the symbols
  containing the cursor. A folder segment lists its entries (choosing a folder
  enters it, `..` goes up; through the project, so WSL and containers work),
  the file segment lists its folder, and a symbol segment lists its siblings.
- **Go to Symbol in File…** (`Ctrl+Shift+O`; Go menu): fuzzy picker over the
  file's symbols (tree order until you type; kind, name, detail or container,
  line). `Enter` jumps (Back returns), `Escape` closes. Opened before the
  symbols arrive, it fills in when they do.
- **Go to Symbol in Workspace…** (`Ctrl+T`, JetBrains `Ctrl+Alt+Shift+N`; Go
  menu): `workspace/symbol` 150 ms after each keystroke (older answers are
  dropped) to the active file's language server, or every running one when
  the active tab has none; rows show kind, name, container and
  `path:line`, and `Enter` opens the file there.
