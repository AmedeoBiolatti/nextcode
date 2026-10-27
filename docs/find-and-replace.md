# Find and replace in files

| Command | Keys |
|---|---|
| Find in Files (Search panel) | `Ctrl+Shift+F` |
| Replace in Files (Edit menu) | `Ctrl+Shift+R` (PyCharm), `Ctrl+Shift+H` (VS Code) |
| Replace All (in the panel) | `Ctrl+Alt+Enter` |

The Search panel searches the project where it lives (locally, in WSL or in
the dev container) with case (`Aa`), whole-word (`ab`) and regex (`.*`)
toggles and include/exclude globs. Open files with unsaved changes are
searched as the editor shows them, not as saved. **Replace in Files** (or
the `▸` before the query) shows a replacement field: in regex mode `$1`,
`${name}` expand to capture groups and `$$` is a `$` (the find bar's rules);
otherwise the text is literal. With a replacement typed, every result row is
one match showing the old text struck through and the new text next to it.
On hover, a match row offers **Replace** and **×** (exclude it from the
results), a file row **Replace All** (that file) and **×**; **Replace All**
next to the field replaces every listed match after a confirmation (*Replace
N matches in M files?*). In search-only mode a line row's **×** excludes the
line.

Replacing goes through the same path as language-server edits: files open in
an editor change there (one undo step per file, left unsaved); other files
are written through the project. Each file is checked first: if any of its
listed matches is no longer at the same place with the same text (the file
changed since the search), the file is skipped. A line under the status
reports the counts, skipped files and failures (*Replaced 4 matches in 2
files; skipped a.py (changed since the search)*), and the results refresh
(exclusions are kept until the query changes). Limits: lines longer than
~400 bytes are listed around their first match, so matches outside that
window are neither shown nor replaced; at most 10 000 matches are listed;
matches never span lines.

Headless: `replace` (see [Headless mode](headless-mode.md)).
