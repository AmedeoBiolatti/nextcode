# Sticky notes

Notes between the person and agents, pinned to lines of a file like a sticky
note on a page. They live in `<project>/.next_code/notes.json` (local: the
first note adds `/.next_code/notes.json*` to `.git/info/exclude`, never to
`.gitignore`), one per project root, and follow the code: a note found at
its lines is exact; else the nearest copy of its text (lines inserted above),
else the most similar lines with their context (the code was edited), else it
is orphaned and kept in the list.

In the editor a note is a card in the free space right of the code, at its
first line (author, age, `→ agent`/`→ user`, reply count, the start of the
text), with a marker in the gutter beside its lines. Cards stack downward so
they never overlap; where the code leaves no room (a long line, a narrow
pane) a card becomes a small `✎` icon at the right edge, and `–` collapses
one by hand. Clicking a card (or its gutter marker) opens the thread: every
message, a reply box (`Ctrl+Enter` sends, `Escape` closes), **Resolve** /
**Reopen**, **Delete**, **Edit** on your messages and the `→` button that
changes who the note is for. **Add Note** (`Ctrl+K Ctrl+N`, the text's or
the gutter's right-click menu, Edit menu, palette) notes the selected lines
or the cursor's line, addressed to the agent by default. Resolved notes are
hidden; **Show/Hide Resolved Notes** (palette, Notes panel) shows them
faded. As you type, notes move with their lines (edits above shift them,
edits inside keep them, deleting their lines orphans them); the new anchors
are written when you save, and discarding the edits puts the notes back.

The **Notes** panel (View menu, bottom dock) lists every note of the project
by file with filters (Open / Resolved / All, Anyone / To me / To agents),
its current line, and "lines deleted" for orphaned ones; a click opens the
file with the thread. Its tab shows how many open notes wait for you
(addressed to you, or to anyone by an agent). When an agent adds a note for
you or replies while NextCode is open, a banner quotes it with **Show
Note**. Notes reach pinpoint as `…/editor:<file>/note:<id>` (the card, or the
open thread with `reply`, `send`, `resolve`, `reopen`, `delete`, `to`,
`close`, `message:<n>/edit`), `…/editor:<file>/new-note/{text,to,add,cancel}`
and `…/panel:notes/note:<id>`, so an agent can point at one. The editor reads
and writes the notes file through the project, so WSL and dev-container
projects work the same. Notes on Jupyter notebook cells (`cell`) show as `✎`
icons in their cells; the panel opens the notebook scrolled to the cell with
the thread open.

Agents use the command line; it edits the file directly, so NextCode need not
be running (the editor watches the file):

```sh
next_code notes list                                 # open notes, current lines, threads
next_code notes add src/main.rs:12-14 "Why clone here?" --as claude
next_code notes reply n7f3a "Fixed, have a look"
next_code notes resolve n7f3a
next_code notes wait --timeout 600                   # until the person writes
```

Inside WSL or a dev container the same commands are
`~/.local/share/next_code/next_code_agent notes …`. Writers (the editor, the
Windows CLI, agents through `/mnt` or a bind mount) take a lock file,
re-read, apply their change and rename a temporary file over `notes.json`,
so concurrent replies are never lost. Commands, the file format and the
`wait` loop are in [docs/notes-for-agents.md](notes-for-agents.md).
