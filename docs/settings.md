# Settings

**File > Preferences > Settings…** (`Ctrl+,`, `Cmd+,` on macOS) opens the Settings page:
every user setting of `settings.json` with a control, in sections
(Appearance, Editor, Languages, Vim, Spelling, Git, Tests, Remote,
Notebooks, Workflows, System), or found with the search box. A change is
written to `settings.json` at once and applies everywhere, like an edit of
the file; a value equal to its default is removed instead, so the file
holds only what differs from the defaults.

The theme is the first setting under **Appearance**. **File > Preferences > Select
Theme…** and **File > Preferences > Customize Theme…** work as before (see
[Themes and highlighting](themes.md)).

Settings the page has no control for (language servers' commands,
formatters, debug adapters) stay in the file: **File > Preferences > Open Settings
(JSON)** opens it.
