//! Spell checking (VS Code's Code Spell Checker, PyCharm's typo
//! inspection): unknown words in comments, docstrings and strings of code,
//! and in all of Markdown, plain text and commit messages, get a dotted
//! underline. The words come from `spell::scope` and `spell::tokenize`,
//! and are checked by `spell::Checker` against the `spell.languages`
//! dictionaries, the user's and the folder's `words.txt`, and the words
//! ignored in this file.
//!
//! Only the lines on screen and `MARGIN_LINES` around them are checked,
//! on a background thread, a moment after edits (`EDIT_DELAY`) or
//! scrolling (`SCROLL_DELAY`); results are kept per buffer version and
//! each line's verdict is cached by its text (`Cache`). Between an edit
//! and the next check, found words move with the text; edited ones drop.
//!
//! Quick fixes (Alt+Enter / Ctrl+.) on an unknown word, in the code-action
//! menu: the first `SUGGESTIONS` corrections, add to the user dictionary
//! (`<config>/next_code/words.txt`), add to the folder's dictionary
//! (`.next_code/words.txt`), ignore in this file. Editing either file
//! reloads it (the user's when the editor saves or regains focus, the
//! folder's through the file watcher).
//!
//! With `spell.in_problems`, file editors also check their whole text a
//! second after it settles and report the words to the Problems panel
//! (`EditorEvent::Spelling`).

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        LazyLock,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use ::spell::{Checker, Scope, WordList};
use parking_lot::{Mutex, RwLock};

use super::*;
use crate::settings::Settings;

/// Lines checked above and below the visible ones.
const MARGIN_LINES: usize = 100;
/// Quiet time after an edit before checking again.
const EDIT_DELAY: Duration = Duration::from_millis(300);
/// Delay before checking lines scrolled into view.
const SCROLL_DELAY: Duration = Duration::from_millis(30);
/// Quiet time after an edit before checking the whole file for Problems.
const PROBLEMS_DELAY: Duration = Duration::from_millis(1000);
/// Corrections offered in the quick-fix menu.
pub(super) const SUGGESTIONS: usize = 5;
/// Lines whose verdicts the cache keeps before starting over.
const CACHE_LIMIT: usize = 20_000;
/// Where a project folder keeps its dictionary (`words.txt`).
const FOLDER_WORDS_DIR: &str = ".next_code";
const FOLDER_WORDS: &str = ".next_code/words.txt";

/// Bumped whenever a word list changes; part of every check's inputs.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The user's and the folders' word lists, shared by every editor.
#[derive(Default)]
struct WordLists {
    user: Arc<WordList>,
    /// Modification time of the user's file when it was read (`None`
    /// before the first read).
    user_stamp: Option<Option<SystemTime>>,
    /// Folder dictionaries by file path.
    folders: HashMap<PathBuf, Arc<WordList>>,
}

static LISTS: LazyLock<RwLock<WordLists>> = LazyLock::new(Default::default);

#[cfg(test)]
pub(super) static TEST_USER_WORDS: Mutex<Option<PathBuf>> = Mutex::new(None);

/// `<config>/next_code/words.txt` (in unit tests only when one set it).
fn user_words_path() -> Option<PathBuf> {
    #[cfg(test)]
    return TEST_USER_WORDS.lock().clone();
    #[cfg(not(test))]
    Some(dirs::config_dir()?.join("next_code").join("words.txt"))
}

/// Extra Hunspell dictionaries (none in unit tests).
fn dictionaries_dir() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    Some(dirs::config_dir()?.join("next_code").join("dictionaries"))
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

fn bump_generation() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// The user's word list, read on first use.
fn user_words() -> Arc<WordList> {
    {
        let lists = LISTS.read();
        if lists.user_stamp.is_some() {
            return lists.user.clone();
        }
    }
    reload_user_words();
    LISTS.read().user.clone()
}

/// Reads the user's `words.txt` again; `true` when it changed.
fn reload_user_words() -> bool {
    let path = user_words_path();
    let stamp = path.as_deref().and_then(modified);
    let text = path
        .as_deref()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    let list = WordList::parse(&text);
    let mut lists = LISTS.write();
    let changed = *lists.user != list;
    lists.user = Arc::new(list);
    lists.user_stamp = Some(stamp);
    drop(lists);
    if changed {
        bump_generation();
    }
    changed
}

/// Re-reads the user's `words.txt` if it changed on disk since it was
/// read, then redraws (run with `theme::refresh`: after saves and when a
/// window activates).
pub fn refresh_user_words(cx: &mut App) {
    let stamp = LISTS.read().user_stamp;
    let Some(stamp) = stamp else {
        return;
    };
    if user_words_path().as_deref().and_then(modified) != stamp && reload_user_words() {
        cx.refresh_windows();
    }
}

/// A folder's word list, read (where the project lives) on first use.
fn folder_words(project: &Project, file: &Path) -> Arc<WordList> {
    if let Some(list) = LISTS.read().folders.get(file) {
        return list.clone();
    }
    let list = Arc::new(
        project
            .read_text(file)
            .map(|text| WordList::parse(&text))
            .unwrap_or_default(),
    );
    LISTS
        .write()
        .folders
        .insert(file.to_path_buf(), list.clone());
    list
}

/// The watcher reported `paths` (or lost track, `rescan`): when a folder
/// dictionary is among them, folder dictionaries are read again on next
/// use (they rarely change; this avoids matching path spellings).
pub fn files_changed<'a>(paths: impl IntoIterator<Item = &'a Path>, rescan: bool, cx: &mut App) {
    let touched = rescan
        || paths
            .into_iter()
            .any(|path| path.ends_with(FOLDER_WORDS_DIR) || path.ends_with(FOLDER_WORDS));
    let mut lists = LISTS.write();
    if !touched || lists.folders.is_empty() {
        return;
    }
    lists.folders.clear();
    drop(lists);
    bump_generation();
    cx.refresh_windows();
}

/// The folder dictionary for a file at `path` of `project`.
fn folder_words_file(project: &Project, path: &Path) -> PathBuf {
    let dir = project.join(&project.root_for(path).path, FOLDER_WORDS_DIR);
    project.join(&dir, "words.txt")
}

/// Builds a checker for `languages` with the word lists; also returns
/// dictionaries that failed to load. Blocks while dictionaries load: call
/// on a background thread.
fn checker(
    languages: &[String],
    folder: Option<(&Project, &Path)>,
    ignored: Option<Arc<WordList>>,
) -> (Checker, Vec<String>) {
    let dir = dictionaries_dir();
    let mut dictionaries = Vec::new();
    let mut errors = Vec::new();
    for language in languages {
        match ::spell::dictionary(language, dir.as_deref()) {
            Ok(dictionary) => dictionaries.push(dictionary),
            Err(err) => errors.push(err),
        }
    }
    let mut lists = vec![user_words()];
    if let Some((project, file)) = folder {
        lists.push(folder_words(project, file));
    }
    lists.extend(ignored);
    (Checker::new(dictionaries, lists), errors)
}

/// What a check depends on besides the text.
#[derive(Debug, Clone, PartialEq)]
struct Inputs {
    scope: Scope,
    languages: Vec<String>,
    generation: u64,
    /// Bumped by "Ignore in this file".
    ignored: u64,
}

/// What a set of found words covers.
#[derive(Debug, Clone, PartialEq)]
struct Checked {
    version: u64,
    lines: Range<usize>,
    inputs: Inputs,
}

/// Misspellings (relative ranges) by piece text, for one set of inputs.
#[derive(Default)]
struct Cache {
    inputs: Option<Inputs>,
    pieces: HashMap<String, Vec<Range<usize>>>,
}

#[derive(Default)]
pub(super) struct SpellState {
    /// Unknown words, sorted, at current offsets.
    words: Vec<Range<usize>>,
    checked: Option<Checked>,
    /// The check scheduled or running.
    pending: Option<Checked>,
    cache: Arc<Mutex<Cache>>,
    /// Words of "Ignore in this file".
    ignored: Arc<WordList>,
    ignored_generation: u64,
    /// Dictionary errors last reported, so they show once.
    reported: Vec<String>,
    _task: Option<Task<()>>,
    /// Whole-file check for the Problems panel: the version and inputs
    /// reported or being checked.
    problems: Option<(u64, Inputs)>,
    _problems_task: Option<Task<()>>,
}

/// A quick fix for an unknown word.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum SpellFix {
    Replace { range: Range<usize>, with: String },
    AddToUser(String),
    AddToFolder(String),
    IgnoreInFile(String),
}

impl SpellFix {
    pub(super) fn title(&self) -> String {
        match self {
            Self::Replace { with, .. } => format!("Change to \"{with}\""),
            Self::AddToUser(word) => format!("Add \"{word}\" to user dictionary"),
            Self::AddToFolder(word) => format!("Add \"{word}\" to project dictionary"),
            Self::IgnoreInFile(word) => format!("Ignore \"{word}\" in this file"),
        }
    }
}

/// An unknown word for the Problems panel.
#[derive(Debug, Clone, PartialEq)]
pub struct SpellingProblem {
    pub range: lsp::types::Range,
    pub word: String,
}

/// An unknown word found by [`check_document`].
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Misspelling {
    pub word: String,
    /// Byte offsets in the text.
    pub start: usize,
    pub end: usize,
    /// 0-based line and UTF-16 column (LSP positions).
    pub line: usize,
    pub column: usize,
    pub suggestions: Vec<String>,
}

/// The checkable pieces of `lines` with their text.
fn pieces(
    buffer: &Buffer,
    syntax: Option<&Syntax>,
    scope: Scope,
    lines: Range<usize>,
) -> Vec<(usize, String)> {
    let mut ranges = Vec::new();
    ::spell::segments(buffer, syntax, scope, lines, &mut ranges);
    ranges
        .into_iter()
        .map(|range| (range.start, buffer.slice(range)))
        .collect()
}

/// Unknown words of `pieces` (absolute ranges, sorted), through `cache`.
fn check_pieces(
    checker: &Checker,
    pieces: &[(usize, String)],
    cache: &Mutex<Cache>,
    inputs: &Inputs,
) -> Vec<Range<usize>> {
    let mut cache = cache.lock();
    if cache.inputs.as_ref() != Some(inputs) || cache.pieces.len() > CACHE_LIMIT {
        cache.pieces.clear();
        cache.inputs = Some(inputs.clone());
    }
    let mut words = Vec::new();
    for (start, text) in pieces {
        let found = match cache.pieces.get(text) {
            Some(found) => found,
            None => {
                let mut found = Vec::new();
                checker.check(text, &mut found);
                cache.pieces.entry(text.clone()).or_insert(found)
            }
        };
        words.extend(
            found
                .iter()
                .map(|word| start + word.start..start + word.end),
        );
    }
    words
}

/// Spell checks `text` as a file at `path` (Markdown, prose or code by its
/// language) with the settings' dictionaries, the user's words and the
/// folder's when `project` is given, with up to [`SUGGESTIONS`]
/// corrections each. Blocking (headless `spell_check`).
pub fn check_document(
    project: Option<&Project>,
    path: Option<&Path>,
    text: &str,
    language: Option<Language>,
    settings: &Settings,
) -> Result<Vec<Misspelling>, String> {
    let Some(scope) = Scope::of(language, path, settings.spell.check_code)
        .or_else(|| path.is_none().then_some(Scope::Prose))
    else {
        return Ok(Vec::new());
    };
    let buffer = Buffer::new(text);
    let syntax = language.map(|language| Syntax::new(language, &buffer));
    let folder = project.zip(path).map(|(project, path)| {
        let file = folder_words_file(project, path);
        (project, file)
    });
    let (checker, errors) = checker(
        &settings.spell.languages,
        folder
            .as_ref()
            .map(|(project, file)| (*project, file.as_path())),
        None,
    );
    if let Some(error) = errors.first() {
        return Err(error.clone());
    }
    let mut out = Vec::new();
    let mut found = Vec::new();
    for (start, piece) in pieces(&buffer, syntax.as_ref(), scope, 0..buffer.line_count()) {
        found.clear();
        checker.check(&piece, &mut found);
        for range in &found {
            let word = piece[range.clone()].to_owned();
            let point = buffer.point_in(start + range.start, text::Encoding::Utf16);
            out.push(Misspelling {
                suggestions: checker.suggest(&word, SUGGESTIONS),
                word,
                start: start + range.start,
                end: start + range.end,
                line: point.row,
                column: point.column,
            });
        }
    }
    Ok(out)
}

impl Editor {
    /// The settings and scope spelling uses here, `None` when off.
    fn spell_config(&self) -> Option<(Arc<Settings>, Scope)> {
        if self.single_line {
            return None;
        }
        let settings = self.file_settings();
        if !settings.spell(self.language()) {
            return None;
        }
        let scope = if self.text_area {
            Scope::Prose
        } else {
            Scope::of(
                self.language(),
                self.buffer.path(),
                settings.spell.check_code,
            )?
        };
        Some((settings, scope))
    }

    fn spell_inputs(&self, settings: &Settings, scope: Scope) -> Inputs {
        Inputs {
            scope,
            languages: settings.spell.languages.clone(),
            generation: GENERATION.load(Ordering::Relaxed),
            ignored: self.spell.ignored_generation,
        }
    }

    /// Before each frame: checks the lines on screen (and around them)
    /// unless the current results or the pending check cover them.
    pub(super) fn follow_spelling(&mut self, cx: &mut Context<Self>) {
        let Some((settings, scope)) = self.spell_config() else {
            if self.spell.checked.is_some() || !self.spell.words.is_empty() {
                self.spell.words.clear();
                self.spell.checked = None;
                self.spell.pending = None;
                self.spell._task = None;
            }
            self.follow_spelling_problems(None, cx);
            return;
        };
        let inputs = self.spell_inputs(&settings, scope);
        self.follow_spelling_problems(Some((&settings, &inputs)), cx);
        let visible = match self.layout.as_ref().map(|layout| layout.rows.as_slice()) {
            Some([first, .., last]) => first.row.line..last.row.line + 1,
            Some([only]) => only.row.line..only.row.line + 1,
            _ => {
                let first = self.pending_scroll_line.unwrap_or(0);
                first..first + 1
            }
        };
        let covers = |checked: &Option<Checked>| {
            checked.as_ref().is_some_and(|checked| {
                checked.version == self.version
                    && checked.inputs == inputs
                    && checked.lines.start <= visible.start
                    && visible.end <= checked.lines.end
            })
        };
        if covers(&self.spell.checked) || covers(&self.spell.pending) {
            return;
        }
        let lines = visible.start.saturating_sub(MARGIN_LINES)
            ..(visible.end + MARGIN_LINES).min(self.buffer.line_count());
        let delay = match &self.spell.checked {
            Some(checked) if checked.version == self.version => SCROLL_DELAY,
            None => SCROLL_DELAY,
            Some(_) => EDIT_DELAY,
        };
        let pending = Checked {
            version: self.version,
            lines: lines.clone(),
            inputs: inputs.clone(),
        };
        self.spell.pending = Some(pending.clone());
        let cache = self.spell.cache.clone();
        let ignored = self.spell.ignored.clone();
        let folder = self.spell_folder();
        self.spell._task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let Ok(pieces) = this.update(cx, |this, _| {
                pieces(&this.buffer, this.syntax.as_ref(), scope, lines.clone())
            }) else {
                return;
            };
            let job_inputs = inputs.clone();
            let (words, errors) = cx
                .background_spawn(async move {
                    let folder = folder
                        .as_ref()
                        .map(|(project, file)| (project, file.as_path()));
                    let (checker, errors) = checker(&job_inputs.languages, folder, Some(ignored));
                    (check_pieces(&checker, &pieces, &cache, &job_inputs), errors)
                })
                .await;
            this.update(cx, |this, cx| {
                if this.spell.pending.as_ref() != Some(&pending) {
                    return;
                }
                this.spell.pending = None;
                if this.version != pending.version {
                    return cx.notify();
                }
                if !errors.is_empty() && errors != this.spell.reported {
                    this.notice = Some(format!("Spelling: {}", errors.join("; ")).into());
                }
                this.spell.reported = errors;
                this.spell.words = words;
                this.spell.checked = Some(pending);
                cx.notify();
            })
            .ok();
        }));
    }

    /// With `spell.in_problems`, reports the whole file's unknown words
    /// once its text settles; turning it off reports none.
    fn follow_spelling_problems(
        &mut self,
        config: Option<(&Settings, &Inputs)>,
        cx: &mut Context<Self>,
    ) {
        let wanted = config.filter(|(settings, _)| {
            settings.spell.in_problems && self.project.is_some() && !self.text_area
        });
        let Some((_, inputs)) = wanted else {
            if self.spell.problems.take().is_some() {
                self.spell._problems_task = None;
                cx.emit(EditorEvent::Spelling(Vec::new()));
            }
            return;
        };
        let current = (self.version, inputs.clone());
        if self.spell.problems.as_ref() == Some(&current) {
            return;
        }
        let delay = if self.spell.problems.is_some() {
            PROBLEMS_DELAY
        } else {
            Duration::ZERO
        };
        self.spell.problems = Some(current);
        let language = self.language();
        let inputs = inputs.clone();
        let ignored = self.spell.ignored.clone();
        let folder = self.spell_folder();
        self.spell._problems_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let Ok(text) = this.update(cx, |this, _| this.text()) else {
                return;
            };
            let problems = cx
                .background_spawn(async move {
                    let buffer = Buffer::new(&text);
                    let syntax = language.map(|language| Syntax::new(language, &buffer));
                    let folder = folder
                        .as_ref()
                        .map(|(project, file)| (project, file.as_path()));
                    let (checker, _) = checker(&inputs.languages, folder, Some(ignored));
                    let lines = 0..buffer.line_count();
                    let position =
                        |offset| language::to_position(&buffer, offset, text::Encoding::Utf16);
                    let mut found = Vec::new();
                    let mut problems = Vec::new();
                    for (start, piece) in pieces(&buffer, syntax.as_ref(), inputs.scope, lines) {
                        found.clear();
                        checker.check(&piece, &mut found);
                        problems.extend(found.iter().map(|word| SpellingProblem {
                            range: lsp::types::Range::new(
                                position(start + word.start),
                                position(start + word.end),
                            ),
                            word: piece[word.clone()].to_owned(),
                        }));
                    }
                    problems
                })
                .await;
            this.update(cx, |_, cx| cx.emit(EditorEvent::Spelling(problems)))
                .ok();
        }));
    }

    /// The project and the folder dictionary of this file, if it has them.
    fn spell_folder(&self) -> Option<(Project, PathBuf)> {
        let project = self.project.as_ref()?;
        let file = folder_words_file(project, self.buffer.path()?);
        Some((project.clone(), file))
    }

    /// After an edit: words move with the text; touched ones drop until the
    /// next check.
    pub(super) fn spell_edited(&mut self) {
        let words = &mut self.spell.words;
        for change in &self.changes {
            words.retain_mut(|word| {
                let touched = change.start <= word.end && word.start <= change.old_end;
                word.start = change.map_offset(word.start, Bias::Right);
                word.end = change.map_offset(word.end, Bias::Left);
                !touched
            });
        }
    }

    /// The unknown word at or touching `offset`.
    pub(super) fn misspelled_at(&self, offset: usize) -> Option<Range<usize>> {
        let words = &self.spell.words;
        let index = words.partition_point(|word| word.end < offset);
        words
            .get(index)
            .filter(|word| word.start <= offset)
            .cloned()
    }

    /// The unknown word the quick fixes are for: at the primary cursor (or
    /// selected).
    pub(super) fn spell_target(&self) -> Option<Range<usize>> {
        let primary = self.primary();
        let word = self.misspelled_at(primary.head)?;
        let range = primary.range();
        (range.is_empty() || range == word).then_some(word)
    }

    /// Quick fixes for the unknown word at `range`: the corrections
    /// (computed in the background), then the dictionaries.
    pub(super) fn spell_fixes(
        &self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Task<Vec<SpellFix>> {
        let word = self.buffer.slice(range.clone());
        let languages = self
            .spell_config()
            .map(|(settings, _)| settings.spell.languages.clone())
            .unwrap_or_default();
        let folder = self.project.is_some() && self.buffer.path().is_some();
        cx.background_spawn(async move {
            let (checker, _) = checker(&languages, None, None);
            let mut fixes: Vec<SpellFix> = checker
                .suggest(&word, SUGGESTIONS)
                .into_iter()
                .map(|with| SpellFix::Replace {
                    range: range.clone(),
                    with,
                })
                .collect();
            fixes.push(SpellFix::AddToUser(word.clone()));
            if folder {
                fixes.push(SpellFix::AddToFolder(word.clone()));
            }
            fixes.push(SpellFix::IgnoreInFile(word));
            fixes
        })
    }

    pub(super) fn apply_spell_fix(&mut self, fix: SpellFix, cx: &mut Context<Self>) {
        match fix {
            SpellFix::Replace { range, with } => {
                if self.buffer.len() < range.end {
                    return;
                }
                let end = range.start + with.len();
                self.commit(
                    vec![(range, with)],
                    |_| SelectionSet::single(Selection::cursor(end)),
                    cx,
                );
            }
            SpellFix::AddToUser(word) => match add_user_word(&word) {
                Ok(()) => self.spell_words_changed(cx),
                Err(err) => {
                    self.notice = Some(format!("Cannot add \"{word}\": {err}").into());
                    cx.notify();
                }
            },
            SpellFix::AddToFolder(word) => {
                let (Some(project), Some(path)) = (self.project.clone(), self.buffer.path()) else {
                    return;
                };
                let file = folder_words_file(&project, path);
                // Accepted at once; the file is written where the project lives.
                {
                    let mut lists = LISTS.write();
                    let mut list = lists
                        .folders
                        .get(&file)
                        .map(|list| (**list).clone())
                        .unwrap_or_default();
                    list.insert(&word);
                    lists.folders.insert(file.clone(), Arc::new(list));
                }
                self.spell_words_changed(cx);
                let task =
                    cx.background_spawn(async move { add_folder_word(&project, &file, &word) });
                cx.spawn(async move |this, cx| {
                    if let Err(err) = task.await {
                        this.update(cx, |this, cx| {
                            this.notice =
                                Some(format!("Cannot update the project dictionary: {err}").into());
                            cx.notify();
                        })
                        .ok();
                    }
                })
                .detach();
            }
            SpellFix::IgnoreInFile(word) => {
                let mut ignored = (*self.spell.ignored).clone();
                ignored.insert(&word);
                self.spell.ignored = Arc::new(ignored);
                self.spell.ignored_generation += 1;
                self.spell
                    .words
                    .retain(|range| !self.buffer.slice(range.clone()).eq_ignore_ascii_case(&word));
                cx.notify();
            }
        }
    }

    /// A dictionary gained a word: hides it here at once, and every editor
    /// checks again.
    fn spell_words_changed(&mut self, cx: &mut Context<Self>) {
        bump_generation();
        let mut lists = vec![LISTS.read().user.clone()];
        if let (Some(project), Some(path)) = (&self.project, self.buffer.path()) {
            let file = folder_words_file(project, path);
            lists.extend(LISTS.read().folders.get(&file).cloned());
        }
        let buffer = &self.buffer;
        self.spell.words.retain(|range| {
            let word = buffer.slice(range.clone());
            !lists.iter().any(|list| list.contains(&word))
        });
        cx.refresh_windows();
    }

    /// Paints the dotted underlines of unknown words on `row`: square dots
    /// snapped to device pixels so every dot looks the same.
    pub(super) fn paint_spelling(
        &self,
        layout: &EditorLayout,
        row: &RowLayout,
        text_left: Pixels,
        top: Pixels,
        window: &mut Window,
    ) {
        let (start, end) = (row.row.range.start, row.row.range.end);
        let words = &self.spell.words;
        let first = words.partition_point(|word| word.end <= start);
        if words.get(first).is_none_or(|word| word.start >= end) {
            return;
        }
        let scale = window.scale_factor();
        let snap = |value: Pixels| (f32::from(value) * scale).round() / scale;
        let dot = (1.5 * scale).round().max(1.) / scale;
        let step = 2. * dot;
        let y = px(snap(top + layout.line_height - px(2.)) - dot);
        let color = theme::accent_text().opacity(0.9);
        for word in words[first..].iter().take_while(|word| word.start < end) {
            let mut x = snap(text_left + row.x_for(word.start.max(start)));
            let x1 = f32::from(text_left + row.x_for(word.end.min(end)));
            while x + dot <= x1 {
                window.paint_quad(fill(
                    Bounds::new(point(px(x), y), size(px(dot), px(dot))),
                    color,
                ));
                x += step;
            }
        }
    }
}

/// Appends `word` to the user's `words.txt` and accepts it at once.
fn add_user_word(word: &str) -> Result<(), String> {
    let path = user_words_path().ok_or("no config directory")?;
    let fail = |err: std::io::Error| format!("{}: {err}", path.display());
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(fail(err)),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(fail)?;
    }
    std::fs::write(&path, ::spell::add_word(&text, word)).map_err(fail)?;
    reload_user_words();
    Ok(())
}

/// Appends `word` to a folder's `words.txt` (creating `.next_code/`).
fn add_folder_word(project: &Project, file: &Path, word: &str) -> std::io::Result<()> {
    let text = match project.read_text(file) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };
    if let Some(dir) = file.parent() {
        project.create_dir_all(dir)?;
    }
    project.write(file, ::spell::add_word(&text, word).into_bytes())
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, WindowHandle};

    use super::super::tests::{editor, state, type_text, update};
    use super::*;

    /// Lets a scheduled check run (dictionaries load on first use).
    fn settle(cx: &mut TestAppContext) {
        cx.run_until_parked();
        cx.executor().advance_clock(EDIT_DELAY + SCROLL_DELAY);
        cx.run_until_parked();
    }

    fn unknown(window: WindowHandle<Editor>, cx: &mut TestAppContext) -> Vec<String> {
        window
            .read_with(cx, |editor, _| {
                editor
                    .spell
                    .words
                    .iter()
                    .map(|range| editor.buffer.slice(range.clone()))
                    .collect()
            })
            .unwrap()
    }

    fn quick_fix(window: WindowHandle<Editor>, cx: &mut TestAppContext) -> Vec<String> {
        update(window, cx, |editor, window, cx| {
            editor.quick_fix(&code_actions::QuickFix, window, cx)
        });
        cx.run_until_parked();
        window
            .read_with(cx, |editor, _| editor.code_action_titles())
            .unwrap()
            .unwrap_or_default()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nc-spell-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[gpui::test]
    fn python_comments_and_docstrings_are_checked_but_not_identifiers(cx: &mut TestAppContext) {
        let window = editor(
            "a.py",
            "\"\"\"Modul helpers.\"\"\"\n\
             def fetch_usr(argumnt):|\n    \
             # Retreive the user\n    \
             return argumnt\n",
            cx,
        );
        settle(cx);
        assert_eq!(unknown(window, cx), ["Modul", "Retreive"]);
        // Markdown is prose.
        let markdown = editor("notes.md", "Thiss tpyo|\n", cx);
        settle(cx);
        assert_eq!(unknown(markdown, cx), ["Thiss", "tpyo"]);
    }

    #[gpui::test]
    fn found_words_move_with_edits_and_typed_ones_wait_for_a_pause(cx: &mut TestAppContext) {
        let window = editor("a.rs", "|// Teh end\n", cx);
        settle(cx);
        assert_eq!(unknown(window, cx), ["Teh"]);
        type_text(window, "// Recieve\n", cx);
        cx.run_until_parked();
        // The old word moved with the text; the new one is not flagged yet.
        assert_eq!(unknown(window, cx), ["Teh"]);
        cx.executor().advance_clock(EDIT_DELAY / 2);
        cx.run_until_parked();
        assert_eq!(unknown(window, cx), ["Teh"]);
        settle(cx);
        assert_eq!(unknown(window, cx), ["Recieve", "Teh"]);
        // Editing inside a word drops it until the next check.
        update(window, cx, |editor, _, cx| {
            let start = editor.text().find("Teh").unwrap();
            editor.commit(
                vec![(start + 1..start + 3, "he".into())],
                |_| SelectionSet::single(Selection::cursor(0)),
                cx,
            )
        });
        assert_eq!(unknown(window, cx), ["Recieve"]);
        settle(cx);
        assert_eq!(unknown(window, cx), ["Recieve"]);
    }

    #[gpui::test]
    fn quick_fixes_offer_corrections_then_dictionaries(cx: &mut TestAppContext) {
        let window = editor("a.py", "# We recie|ve it\n", cx);
        settle(cx);
        assert_eq!(unknown(window, cx), ["recieve"]);
        let titles = quick_fix(window, cx);
        assert_eq!(
            titles.first().map(String::as_str),
            Some("Change to \"receive\"")
        );
        assert!(titles.len() <= SUGGESTIONS + 2, "{titles:?}");
        assert_eq!(
            &titles[titles.len() - 2..],
            [
                "Add \"recieve\" to user dictionary",
                "Ignore \"recieve\" in this file"
            ],
            "no project dictionary without a project"
        );
        update(window, cx, |editor, _, cx| editor.run_code_action(0, cx));
        assert_eq!(state(window, cx), "# We receive| it\n");
        settle(cx);
        assert!(unknown(window, cx).is_empty());
        // Off an unknown word, without a language server: nothing to offer.
        assert!(quick_fix(window, cx).is_empty());
    }

    #[gpui::test]
    fn the_no_server_notice_shows_only_without_spelling_fixes(cx: &mut TestAppContext) {
        let notice = |window: WindowHandle<Editor>, cx: &mut TestAppContext| {
            window
                .read_with(cx, |editor, _| {
                    editor.notice.as_ref().map(|n| n.to_string())
                })
                .unwrap()
        };
        let window = editor("a.py", "x = 1|  # recieve\n", cx);
        settle(cx);
        // Off the word: nothing to show, so the notice explains why.
        assert!(quick_fix(window, cx).is_empty());
        assert_eq!(
            notice(window, cx).as_deref(),
            Some("No language server for this file")
        );
        // On the word: the spelling menu, and no stale notice beside it.
        update(window, cx, |editor, _, cx| {
            let at = editor.text().find("recieve").unwrap() + 2;
            editor.set_selections(SelectionSet::single(Selection::cursor(at)), cx)
        });
        let titles = quick_fix(window, cx);
        assert_eq!(
            titles.first().map(String::as_str),
            Some("Change to \"receive\"")
        );
        assert_eq!(notice(window, cx), None);
    }

    #[gpui::test]
    fn ignoring_a_word_hides_it_in_this_file_only(cx: &mut TestAppContext) {
        let window = editor("a.md", "Zorgle and zorgle|\n", cx);
        let other = editor("b.md", "zorgle|\n", cx);
        settle(cx);
        let titles = quick_fix(window, cx);
        let ignore = titles.iter().position(|t| t.starts_with("Ignore")).unwrap();
        update(window, cx, |editor, _, cx| {
            editor.run_code_action(ignore, cx)
        });
        assert!(unknown(window, cx).is_empty(), "both cases, at once");
        settle(cx);
        assert!(unknown(window, cx).is_empty());
        assert_eq!(unknown(other, cx), ["zorgle"]);
    }

    #[gpui::test]
    fn dictionaries_gain_words_and_reload_when_edited(cx: &mut TestAppContext) {
        let dir = temp_dir("dictionaries");
        let user = dir.join("config").join("words.txt");
        *TEST_USER_WORDS.lock() = Some(user.clone());
        reload_user_words();
        let root = dir.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let project = Project::local(root.clone());
        let open = |name: &str, text: &str, cx: &mut TestAppContext| {
            let path = root.join(name);
            let project = project.clone();
            let text = text.to_owned();
            let window = cx.add_window(|_, cx| {
                let mut editor = Editor::for_file(Buffer::with_path(&text, path), project, cx);
                editor.selections = SelectionSet::single(Selection::cursor(3));
                editor
            });
            settle(cx);
            window
        };
        let a = open("a.md", "Blorptastic and Snarfwidget\n", cx);
        let b = open("b.md", "Blorptastic\n", cx);
        assert_eq!(unknown(a, cx), ["Blorptastic", "Snarfwidget"]);

        // User dictionary: written, and every editor accepts the word.
        let titles = quick_fix(a, cx);
        let add = titles
            .iter()
            .position(|t| t.contains("user dictionary"))
            .unwrap();
        update(a, cx, |editor, _, cx| editor.run_code_action(add, cx));
        assert_eq!(std::fs::read_to_string(&user).unwrap(), "Blorptastic\n");
        assert_eq!(unknown(a, cx), ["Snarfwidget"], "hidden at once");
        settle(cx);
        assert_eq!(unknown(a, cx), ["Snarfwidget"]);
        assert!(unknown(b, cx).is_empty());

        // Editing the file (outside the editor) reloads it.
        std::fs::write(&user, "# mine\nsnarfwidget\n").unwrap();
        let later = SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&user)
            .unwrap()
            .set_modified(later)
            .unwrap();
        cx.update(refresh_user_words);
        settle(cx);
        assert_eq!(unknown(a, cx), ["Blorptastic"]);
        assert_eq!(unknown(b, cx), ["Blorptastic"]);

        // Project dictionary: `.next_code/words.txt` in the folder.
        let titles = quick_fix(a, cx);
        let add = titles
            .iter()
            .position(|t| t.contains("project dictionary"))
            .unwrap();
        update(a, cx, |editor, _, cx| editor.run_code_action(add, cx));
        cx.run_until_parked();
        let folder = root.join(".next_code").join("words.txt");
        assert_eq!(std::fs::read_to_string(&folder).unwrap(), "Blorptastic\n");
        settle(cx);
        assert!(unknown(a, cx).is_empty());
        assert!(unknown(b, cx).is_empty());

        // The watcher reports an edit of it: read again.
        std::fs::write(&folder, "other\n").unwrap();
        cx.update(|cx| files_changed([folder.as_path()], false, cx));
        settle(cx);
        assert_eq!(unknown(a, cx), ["Blorptastic"]);

        *TEST_USER_WORDS.lock() = None;
        reload_user_words();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[gpui::test]
    fn text_areas_check_every_line(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, cx| {
            let mut editor = Editor::text_area(cx);
            editor.set_text("Fix teh parser\n\nIt chrashed on emtpy input.", cx);
            editor
        });
        settle(cx);
        assert_eq!(unknown(window, cx), ["teh", "chrashed", "emtpy"]);
    }

    #[gpui::test]
    fn large_files_check_only_lines_around_the_view(cx: &mut TestAppContext) {
        let text: String = (0..100_000)
            .map(|i| format!("x = {i}  # line numbr {i}\n"))
            .collect();
        let window = editor("big.py", &format!("|{text}"), cx);
        settle(cx);
        let (words, lines) = window
            .read_with(cx, |editor, _| {
                let lines = editor.spell.checked.as_ref().unwrap().lines.clone();
                (editor.spell.words.len(), lines)
            })
            .unwrap();
        assert_eq!(lines.start, 0);
        assert!(lines.end < 300, "{lines:?}");
        assert_eq!(words, lines.len());
    }
}
