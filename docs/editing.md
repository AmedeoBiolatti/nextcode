# Editing

Shortcuts follow VS Code (JetBrains where VS Code has none); the menus and
the command bar show each command's keys.

**Multiple cursors.** Every edit, motion, deletion, copy/cut/paste and
undo works at every cursor; selections that overlap (or a cursor touching
one) merge. Pasting N lines into N cursors puts one line at each. Undo
restores the cursors of the undone edit.

| Command | Keys |
|---|---|
| Add / remove a cursor | `Alt+click` |
| Column selection | `Shift+Alt+drag` or middle-drag |
| Add Next Occurrence (first: select the word) | `Ctrl+D` |
| Select All Occurrences | `Ctrl+Shift+L` |
| Add Cursor Above / Below | `Ctrl+Alt+Up` / `Ctrl+Alt+Down` |
| Back to one cursor | `Escape` |

Occurrences started from a cursor on a word match whole words; started from a
selection they match the text anywhere. Both are case-sensitive.

**Lines, comments, indentation.** All work on every selection.

| Command | Keys |
|---|---|
| Move Line Up / Down | `Alt+Up` / `Alt+Down` |
| Copy Line Up / Down | `Shift+Alt+Up` / `Shift+Alt+Down` |
| Delete Line | `Ctrl+Shift+K` |
| Insert Line Below / Above | `Ctrl+Enter` / `Ctrl+Shift+Enter` |
| Toggle Line Comment (block comment in HTML, CSS, Markdown) | `Ctrl+/` |
| Toggle Block Comment | `Shift+Alt+A` or `Ctrl+Shift+/` |
| Indent / Outdent Lines | `Tab` on a multi-line selection, `Ctrl+]` / `Shift+Tab`, `Ctrl+[` |
| Go to Matching Bracket | `Ctrl+Shift+\` |
| Go to Line (`line` or `line:column`, previews while typing, `Escape` returns) | `Ctrl+G` |

Line comments go at the smallest indentation of the selected lines and are
removed only when every non-blank line is commented. The indentation style
(tabs or N spaces) is detected from the file; files without indentation get
4 spaces (2 for JSON, YAML, JavaScript, TypeScript, HTML, CSS). The status
bar shows it.

**Typing helpers.** Typing `(`, `[`, `{`, `"`, `'` (and `` ` `` in
JavaScript/TypeScript, Go, shell, SQL, Markdown; no `'` in Rust or
Markdown) inserts the closer; quotes don't pair right after a letter,
digit or backslash, and nothing pairs before a word. Typing the closer
steps over an inserted one, Backspace between an empty pair deletes both,
and an opener typed over a selection wraps it. `"auto_close_brackets":
false` in `settings.json` turns all of this off. Enter keeps the line's
indentation, adds a level after `{`, `(`, `[` (and Python's `:`), and puts
the closer of `{|}` on its own line. The bracket pair at the cursor is
highlighted (from the syntax tree; brackets in strings and comments don't
match).

**Find and replace.** `Ctrl+F` opens the find bar above the text, seeded
with the selection or the word at the cursor; `Ctrl+H` also shows the
replace field. Matches are highlighted, the current one stands out, and the
bar shows `n of m` (updated as the text changes).

| In the find bar | Keys |
|---|---|
| Next / previous match | `Enter` / `Shift+Enter`, `F3` / `Shift+F3` |
| Match case / whole word / regular expression | `Alt+C` / `Alt+W` / `Alt+R` |
| Select all matches as cursors | `Alt+Enter` |
| Replace (in the replace field) / Replace All | `Enter` / `Ctrl+Alt+Enter` |
| Close and return to the text | `Escape` |

Regular expressions use Rust's `regex` syntax (`^`/`$` match at lines);
replacements expand `$1`, `${name}`. Without regex the replacement is
literal.

**Word wrap.** `Alt+Z` (View > Word Wrap) wraps long lines at the editor's
width for that editor; `"word_wrap": true` in `settings.json` makes it the
default, and `"languages": { "<id>": { "word_wrap": … } }` overrides it per
language (Markdown wraps by default). Rows break after spaces (long words
break anywhere, East Asian wide characters count two columns), continuation
rows keep the line's indentation, and the line number shows on the first
row only. Up/Down move by rows; `Home`/`End` go to the row's start/end
first, then the line's. There is no horizontal scrolling while wrapping.
Shift+Alt+drag column selections count buffer columns, so they take the
same characters of every line.

**Folding.** Foldable regions come from the syntax tree: bracketed blocks,
objects, arrays and argument lists (the closing line stays visible),
multi-line comments, Python blocks (from the `def`/`class`/`if` line),
HTML/JSX elements, Markdown sections and code fences, TOML tables; YAML,
shell, SQL, Dockerfiles and plain text fold by indentation. Chevrons appear
in the gutter while the mouse is over it (folded lines always show `▸`);
click one to fold or unfold. A folded line ends with `⋯`, which unfolds on
click. Folds belong to the editor pane, follow edits, and open when an
edit, a cursor, a jump or a find match lands inside; motions step over them.
Folds are saved with the session.

| Folding | Keys |
|---|---|
| Fold / unfold at the cursor | `Ctrl+Shift+[` / `Ctrl+Shift+]` |
| Toggle fold at the cursor | `Ctrl+K Ctrl+L` |
| Fold / unfold recursively | `Ctrl+K Ctrl+[` / `Ctrl+K Ctrl+]` |
| Fold all / unfold all | `Ctrl+K Ctrl+0` / `Ctrl+K Ctrl+J` |

**Minimap.** File editors show the whole document scaled down in a column
beside the scrollbar (VS Code): one 2 px row per screen row (wrapped rows
and folds follow the text), each run of same-coloured characters a block
in its syntax colour, up to `max_columns` columns. A translucent slider
marks the visible part; a click centres the text on the clicked row and
dragging the slider scrolls. When the document is taller than the column,
the minimap scrolls with the text so their tops and ends meet. Git change
markers (left edge), errors and warnings, selections and find matches show
over it. It is hidden in editors narrower than 500 px, input fields, text
areas and diff tabs.

**Sticky scroll.** The opening lines of the scopes around the first visible
line (the foldable regions above: definitions, blocks, bracketed and
indented regions; a multi-line signature shows its `def`/`fn` line, never
a decorator, comment or docstring) stay pinned at the top of the text, outermost first,
with their line numbers and syntax colours over a subtle shadow; each
pinned header covers the line below it, so nested headers stack up to
`max_lines` (and at most half the view). The innermost one is pushed up as
its scope's last row scrolls under it. Clicking one scrolls its line back
to where it was pinned and puts the cursor there; the cursor never stays
hidden under the pinned lines when it moves. Folded scopes and wrapped
rows are followed.

```json
{ "editor": {
    "minimap": { "enabled": true, "max_columns": 120, "side": "right" },
    "sticky_scroll": { "enabled": true, "max_lines": 5 } } }
```

`"side": "left"` puts the minimap before the gutter. Both apply live and per
workspace/folder like other settings.
