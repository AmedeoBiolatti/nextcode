# Themes and highlighting

Colors come from a theme: the whole UI, the terminal's palette and syntax
highlighting. `Dark` (the default) and `Light` are built in; user themes are
JSON files in `themes\` in the config dir (`%APPDATA%\next_code\themes` on
Windows, `~/.config/next_code/themes` on Linux), named by their `"name"`
(else the file name; built-in names are reserved). `"theme": "<name>"` in
`settings.json` picks one.

- **File > Select Theme…** lists the built-in and user themes; choosing one
  writes `"theme"` into `settings.json` (keeping the other settings) and
  applies it.
- **File > Customize Theme…** creates `themes/<Name> (custom).json`
  extending the current built-in theme, with one example per section,
  selects it and opens it; on a user theme it just opens its file.

Edits to `settings.json` or to a theme file (or one it extends) apply
without a restart: when the file is saved in the editor, when a window is
activated, and when a menu or the command bar opens. Terminals opened
before a theme change keep their palette.

```json
{ "name": "Mine", "extends": "Dark",
  "ui": { "accent": "#3574f0", "selection": "#21428380" },
  "terminal": { "cursor": "#ced0d6", "ansi": ["#1e1f22", "…16 colors…"] },
  "syntax": {
    "keyword": "#cf8e6d",
    "keyword.control": { "bold": true },
    "comment.doc": { "color": "#5f826b", "italic": true }
  },
  "languages": { "python": { "syntax": { "function.builtin": "#56a8f5" } } },
  "semantic_modifiers": { "deprecated": { "strikethrough": true } } }
```

- Colors are `#rrggbb` or `#rrggbbaa`. A style is a color or an object
  with `color`, `background`, `bold`, `italic`, `underline`,
  `strikethrough`.
- `extends` names any theme; the child wins key by key, and a style set in
  both merges field by field. A theme without `extends` must set every
  `ui` color and the whole `terminal` palette.
- `ui` keys: `background`, `panel`, `border`, `text`, `muted`, `accent`,
  `accent_text`, `on_accent`, `warning`, `error`, `active_row`,
  `hover_row`, `selection`, `cursor`, `diff_added`, `diff_removed`,
  `git_added`, `git_modified`, `git_renamed`, `git_deleted`, `git_unversioned`,
  `git_conflicted`, `graph_lanes` (a list, cycled), `branch`, `tag`,
  `lightbulb`, `breakpoint`, `debug_current_line`, `debug_frame_line`,
  `debug_arrow`, `run_marker`. An
  unknown key is an error naming it; the theme then falls back to `Dark`
  with a notice under the menu bar.
- `syntax` keys are highlight scopes, most general part first (`keyword`,
  `keyword.control`, `function.method`, `variable.parameter`,
  `markup.heading`, …). A scope takes the style of every prefix the theme
  defines, the most specific last: `function.method` is drawn with
  `function`'s style unless `function.method` sets its own; scopes the theme
  does not cover use the text color. Unknown scopes are reported in the
  notice but do not stop the theme.
- `languages` overrides layer over `syntax` for one language, keyed by its
  id (`c`, `cpp`, `python`, `rust`, `sql`, …).
- `semantic_modifiers` style language-server token modifiers
  (`deprecated`, `mutable`, …) over the token's style; today `deprecated`
  also styles deprecated completion items. The built-in themes strike
  `deprecated` through and underline `mutable` (as RustRover does).

**Semantic highlighting.** When the file's language server provides
semantic tokens (rust-analyzer, basedpyright, …), the editor layers them
over tree-sitter highlighting: a run gets its tree-sitter style, then the
token type's scope (`parameter` → `variable.parameter`, `struct` →
`type.struct`, `enumMember` → `variant`, `namespace` → `module`,
`selfParameter` → `variable.builtin`, …; the full table is `TOKEN_TYPES` in
`crates/ui/src/editor/semantic.rs`) unless tree-sitter already found a more
specific scope of the same family (a doc comment stays `comment.doc`), then
each modifier's `semantic_modifiers` style. Diagnostics underlines, faded
unused code and IME underlines still take precedence. Tokens are requested
when a file opens on the server, 200 ms after typing stops, and when the
server asks for a refresh (e.g. rust-analyzer after indexing); until a new
answer arrives, the previous tokens follow the edits and those an edit
touches are dropped. `"semantic_highlighting": false` in `settings.json`
turns this off (default `true`); the change applies live like a theme
change.
