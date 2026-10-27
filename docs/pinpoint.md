# Pinpoint

Every part of the interface has a stable path (`menu-bar/run`,
`workspace/left-dock/panel:commit/commit-area/commit`,
`workspace/editor-area/pane:1/editor:src/main.rs/gutter/line:12`) that a
person and an agent can both name, and text is addressable down to single
characters (`src/main.rs:12:5-12:18`). An agent asked "how do I …?" can show
the answer in the running window: outlines with a soft glow, numbered badges
and short notes next to buttons, tints on code; the highlights follow their
targets as panels scroll and resize, and go away with `clear`, Escape (when
no popup uses it) or a note's ×.

```sh
next_code pinpoint list workspace/left-dock --depth 2
next_code pinpoint highlight --clear workspace/left-dock/dock-tabs/tab:commit --step 1 \
  --note "Open the Commit panel" src/main.rs:12 --reveal --note "Here"
next_code pinpoint wait-pin --pick
```

`next_code pinpoint` is a client of the editor already running (it never
opens a window): the editor serves JSON lines on a random loopback port with
a token, announced in `<data dir>/next_code/pinpoint/<pid>.json`; the client
picks the editor whose project holds the current directory. Agents inside
WSL or a dev container use `next_code_agent pinpoint …`, and `--automation`
has a `pinpoint` passthrough. The commands, targets and conventions are in
[docs/pinpoint-for-agents.md](pinpoint-for-agents.md).

The other way round, **Pinpoint: Pick Element** (`Ctrl+Alt+Shift+P`) outlines
the element under the pointer with its path; a click copies a reference
(`nextcode://ui/<path>`, or the characters under the pointer in code and
text) and adds it to the pins agents read (`pins`, `wait_pin`); Shift+click
keeps picking, Escape leaves. **Pinpoint: Clear Highlights** removes the
window's highlights.

**What I'm looking at.** `next_code context` (`next_code_agent context` in
WSL or a dev container) answers an agent's "what are you looking at?" without
the person pasting anything: the focused area, the active file with cursor,
selection, the lines around the cursor, its diagnostics and sticky notes, the
open tabs, the terminal's last lines and exit code, Problems, the Tests
panel's failures, the debugger's stack and locals, the notebook cell with its
error, or the workflow run's failed step. Markdown by default, `--format json`
raw; everything capped (`--max-lines`, `--around`). **Pinpoint: Copy Context
for Agent** (command palette) copies the same Markdown for pasting into a
chat. See [docs/pinpoint-for-agents.md](pinpoint-for-agents.md#what-the-user-is-looking-at-context).
