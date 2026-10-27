# Refactoring and formatting

Through the file's language server (the server runs where the project
lives: locally, in WSL or in the dev container).

| Command (Refactor menu) | Keys |
|---|---|
| Rename Symbol… | `F2`, `Shift+F6` |
| Quick Fix… (code actions at the cursor or selection) | `Ctrl+.`, `Alt+Enter` |
| Format Document | `Shift+Alt+F` |
| Format Selection | `Ctrl+K Ctrl+F` |

**Rename** opens an input over the symbol with its name selected (the range
and placeholder come from the server's `prepareRename`; its refusal, e.g.
"cannot rename a keyword", shows in the status bar). `Enter` renames it in
every file, `Escape` (or a click elsewhere) cancels. Open files change in
their editor (one undo step per file, unsaved like any edit); other files
change on disk. Servers' file renames (e.g. renaming a Rust module) move the
file and the open tab with it. The status bar reports how many files
changed, or which one failed.

**Code actions.** `Ctrl+.` / `Alt+Enter` (while the find bar is open,
`Alt+Enter` keeps selecting all matches) lists the server's actions for the
selection and the diagnostics under it: quick fixes first, the server's
preferred fix in bold. `Up`/`Down`, `Enter` (or a click) applies, `Escape`
closes. A lightbulb in the gutter marks the cursor line when actions exist
(amber when one is a quick fix); clicking it opens the menu. Actions are
resolved when the server defers their edits, and their commands run after
the edit; edits a command makes come back as `workspace/applyEdit` and are
applied the same way.

**Formatting.** Format Document and Format Selection use the language
server (Format Selection needs its range formatting). Settings choose
another formatter per language; it runs where the project lives, in the
project root, with the text on stdin and `{file}` replaced by the file's
path:

```json
{ "format_on_save": true,
  "languages": {
    "python": { "formatter": { "command": "ruff", "args": ["format", "--stdin-filename", "{file}", "-"] } },
    "rust": { "format_on_save": false },
    "json": { "formatter": "language_server" } } }
```

Python uses `ruff format` by default when `ruff` is installed (the
project's `.venv` first, then `PATH`), else the language server. The result
replaces the text as one undo step through the smallest edits, so cursors
and selections stay where they were. The editor's indentation (tabs or N
spaces, from the status bar) is sent as the server's formatting options.
`"format_on_save"` (global, or per language) formats on `Ctrl+S`; if the
formatter takes longer than 2 seconds (or fails), the file is saved
unformatted with a note in the status bar. Save All and saving from the
close prompt don't format.

**Snippets.** Completions that insert snippets (e.g. rust-analyzer's
`fn name($1) {$0}`, basedpyright's call snippets) place the cursor on the
first placeholder with its text selected; every copy of a placeholder gets a
cursor, so typing changes them together. `Tab` / `Shift+Tab` move to the
next / previous placeholder, the last `Tab` goes to the snippet's end
position; `Escape` or moving the cursor out of the snippet ends it. Choices
(`${1|a,b|}`) insert the first option; snippet variables (`$TM_FILENAME`,
`$TM_SELECTED_TEXT`, …) are filled where known.
