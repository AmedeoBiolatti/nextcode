# Vim mode

Optional Vim emulation in file editors, like VS Code Vim / IdeaVim: turn it
on with `"vim_mode": true` in `settings.json`, or **Toggle Vim Mode**
(command palette, View > Vim Mode; until the editor exits). The status bar
shows the mode (`NORMAL`, `INSERT`, `VISUAL`, `V-LINE`, `V-BLOCK`,
`REPLACE`), keys typed so far (`2d`) and `recording @q`; the cursor is a
block in normal and visual mode, a half block while an operator waits for
its motion, an underline in replace mode and the usual bar in insert mode.

Keys reach Vim before the IDE's shortcuts, but Vim only takes what it uses:
insert mode leaves everything but Escape to the editor (completion,
snippets, auto-closing, multi-cursor typing work as usual), and in normal
and visual mode Alt and function keys and Ctrl keys Vim has no meaning for
(`Ctrl+S`, `Ctrl+P`, `Ctrl+Shift+…`) stay the IDE's. Escape first closes an
open completion, parameter-hint, code-action or change popup; Escape in
normal mode with nothing pending goes to the IDE (collapse cursors, close
the find bar). Input fields, the terminal and diff tabs never use Vim; the
Commit message does with `"vim": { "in_commit_message": true }`.

| | |
|---|---|
| Modes | `i a I A o O gi gI`, `v V Ctrl+V` (`Ctrl+Q`), `gv`, `R`, Escape / `Ctrl+[` |
| Motions (with counts) | `h j k l` (arrows, Backspace, Space), `w W b B e E ge gE`, `0 ^ $ g_ \|`, `gg G {n}G`, `+ - Enter`, `f F t T ; ,`, `%` (`{n}%`), `{ }`, `H M L`, `Ctrl+D/U/F/B` (PageUp/Down), `gj gk`, `* #`, `/ ? n N`, marks `'a` `` `a `` `''` `'<` `'>` `'.` `'^` |
| Operators | `d c y > < = g~ gu gU` with a motion, a text object or doubled for lines (`dd`, `>>`, `gUU`); counts multiply (`2d3w`) |
| Text objects | `iw aw iW aW`, `i" a" i' a'`, ``i` a` ``, `i( a( ib ab i[ a[ i{ a{ iB aB i< a<`, `it at`, `ip ap` (with counts) |
| Commands | `x X s S D C Y p P`, `u Ctrl+R`, `.`, `J gJ`, `r{c}`, `~`, `Ctrl+A / Ctrl+X` (decimal and `0x` hex), `m{a-z}`, `q{a-z}` … `q`, `@{a-z}`, `@@`, `zz zt zb`, `Ctrl+E / Ctrl+Y`, `zc zo za zM zR` (folds), `gd` (go to definition) |
| Visual | motions and objects extend; `d x X D c s S C R y Y > < = ~ u U J r{c} p P o`; block `I`/`A` type on every line (`$A` appends to each line's end) |
| Registers | unnamed, `"a`-`"z` (`"A` appends), `"0` last yank, `"1`-`"9` deletes of lines, `"-` small deletes, `"_`, `"+`/`"*` system clipboard, `".` `":` `"/` read-only |

**Command line.** `:` opens it in the status bar (with `'<,'>` from visual
mode, `.,.+n` after a count); `/` and `?` search (also as operator motions,
`d/foo`). Up/Down browse the history, `Ctrl+U` clears, `Ctrl+W` deletes a
word, `Ctrl+V` pastes, Backspace on an empty line or Escape cancels.

| Ex command | |
|---|---|
| `:w`, `:wa`, `:q`, `:q!`, `:wq`, `:x` | save (formats on save like `Ctrl+S`), save all, close the tab (asks about unsaved changes; `!` discards), save and close |
| `:e {file}`, `:e`, `:e!` | open a file (relative to the project folder), reload from disk (`!` drops edits) |
| `:{n}`, `:$`, `:.+3` | go to line |
| `:[range]s/pat/rep/[giI]`, `:%s/…` | substitute; `&` and `\1`…`\9` in the replacement, empty pattern = last search |
| `:[range]d [x]`, `:[range]y [x]`, `:[range]>`, `:[range]<`, `:[range]j` | delete, yank, shift, join lines |
| `:noh`, `:set hls` / `:set nohls` | hide / show search highlights |
| `:sp`, `:vs` | split the editor down / right |

Ranges: `%`, `n`, `.`, `$`, `'x` marks, `+n`/`-n` offsets, `a,b`.
Search patterns use Rust regex syntax plus Vim's `\<` `\>`, with smartcase
(case-sensitive only with an uppercase letter; `\c` / `\C` force it); `*` and
`#` match whole words, case-sensitively. Matches of the last search are
highlighted until `:noh` (only the lines on screen are searched for that).

**Undo and repeat.** A command and the insert mode it opens are one undo
step (`ciwfoo<Esc>` undoes with one `u`), and `.` repeats it with the typed
text (`3.` replaces the count); visual-mode commands repeat on the same
amount of text. Insert counts repeat the text (`3ix<Esc>`, `2oline<Esc>`).

**Several cursors.** Normal-mode commands act on the primary cursor and
drop the others, except `i` and `a`, which insert at every cursor. A
selection made outside Vim (mouse drag, `Ctrl+D`, Select All) is visual
mode, and visual operators act on every selection (`Ctrl+D Ctrl+D c` changes
all occurrences).

```json
{ "vim_mode": true,
  "vim": { "ctrl_keys": true, "in_commit_message": false } }
```

`"ctrl_keys": false` gives `Ctrl+D/U/F/B/E/Y/R/A/X/V/Q` (and `Ctrl+[`) back to
the IDE. Not supported: sentence objects (`is`/`as`), `:s` confirm (`c`),
`Ctrl+O`/`Ctrl+I` jumps, `Ctrl+O` in insert mode, uppercase (file) marks,
`:w {file}`, Vim regex syntax beyond `\<` `\>` `\c` `\C`, `:normal`,
key remapping and `.vimrc`.
