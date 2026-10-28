# Spell checking

Like VS Code's Code Spell Checker and PyCharm's typo inspection, unknown
words get a dotted underline (not the wavy one of diagnostics):

- in code, only comments, docstrings and strings (from the syntax tree;
  identifiers are never checked);
- in Markdown, everything except code blocks, link targets and fence info
  strings; in plain text (`.txt`, `.rst`, `.adoc`, `.org`, git's
  `COMMIT_EDITMSG`…) and the Commit window's message, everything.

Words are split at camelCase humps, `_`, `-` and `.` (`parseHTTPRequest` →
parse, HTTP, Request). Skipped: URLs, e-mail addresses, paths, `@tags`,
`$variables`, `%formats`, `#hex` colors, hashes, text in backticks, words
shorter than 3 letters, all-caps words up to 4 letters (`TODO`, `JSON`),
words with digits (`utf8`, `x86`) and non-Latin scripts. A built-in list of
programming words (`async`, `regex`, `stdout`, `kwargs`, `json`, …) is
accepted.

**Quick fixes.** On an unknown word, `Alt+Enter` / `Ctrl+.` (or the
lightbulb) lists, before any language-server actions: up to 5 corrections,
*Add to user dictionary* (`<config>/next_code/words.txt`), *Add to project
dictionary* (`.next_code/words.txt` in the file's project folder, written
where the project lives), *Ignore in this file* (in this editor tab, until
it closes). Dictionary files have one word per line (`#` comments; any case
matches). Editing them reloads them: the user's file when the editor saves
a file or a window regains focus, the project's through the file watcher.

Only the visible lines and 100 lines around them are checked, on a
background thread, 300 ms after typing stops (scrolling to unchecked lines
checks at once); results are cached per line text. Dictionaries load on
first use, off the UI thread (en_US: ~10 ms in a release build; checking
300 lines of comments takes about 1.5 ms).

Settings (`settings.json`, workspace or folder settings):

```json
{ "spell": {
    "enabled": true,
    "languages": ["en_US"],
    "check_code": true,
    "in_problems": false },
  "languages": { "json": { "spell": false } } }
```

`languages` lists Hunspell dictionaries: `en_US` is built in (SCOWL, see
`crates/spell/dictionaries/README_en_US.txt` for its licence); others are
read from `<config>/next_code/dictionaries/<language>.aff` and `.dic`
(also replacing the built-in one). A word passes if any dictionary knows
it. `check_code: false` limits checking to Markdown, plain text and commit
messages. `languages.<id>.spell` turns checking on or off for one language.
`in_problems: true` also lists the unknown words of open files in the
Problems panel (information level, source `spelling`; the whole file is
checked a second after it settles). The checker is
[spellbook](https://github.com/helix-editor/spellbook) (MPL-2.0).
