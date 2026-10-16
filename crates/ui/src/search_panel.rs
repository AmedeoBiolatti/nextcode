//! Project search panel: query, options, and streamed results grouped by
//! file; Replace in Files (PyCharm's Ctrl+Shift+R, VS Code's Ctrl+Shift+H):
//! a replacement field, a preview per match, Replace for one match, a file
//! or everything (after a confirmation), and × to exclude matches or files.
//!
//! Open files with unsaved changes are searched in memory, so results and
//! replacements follow what the editor shows. Replacing goes through the
//! workspace ([`SearchPanelEvent::Replace`] → [`SearchPanel::replaced`]):
//! open editors take the edits as one undo step per file, other files are
//! written, files changed since the search are skipped.

use std::{collections::HashSet, ops::Range, path::PathBuf, rc::Rc, time::Duration};

use futures::StreamExt;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle, Hsla,
    KeyBinding, PromptLevel, SharedString, StrikethroughStyle, Subscription, Task, Window, actions,
    div, prelude::*, px, uniform_list,
};
use search::{FileMatches, PathFilter, Replacer, SearchEvent, SearchLimits, SearchQuery, Target};

use crate::{
    debugger::panel::Tooltip,
    editor::{self, Editor, EditorEvent},
    project::{Project, SearchHandle},
    selectable::selectable,
    theme,
    workspace_edit::{FileTargets, ReplaceReport},
};

actions!(search, [ReplaceAll, NextField, PreviousField]);

/// Key context around the whole panel.
const CONTEXT: &str = "ProjectSearch";

pub fn bind_keys(cx: &mut App) {
    let fields = Some("ProjectSearch > Editor");
    cx.bind_keys([
        // VS Code's Replace All in the search view.
        KeyBinding::new("secondary-alt-enter", ReplaceAll, Some(CONTEXT)),
        // After the editor's own keys, which these replace in the inputs.
        KeyBinding::new("tab", NextField, fields),
        KeyBinding::new("shift-tab", PreviousField, fields),
    ]);
}

const ROW_HEIGHT: f32 = 22.;
/// Wait after the last keystroke before searching.
const DEBOUNCE: Duration = Duration::from_millis(250);
/// Bytes of a match row's line kept before the match (the panel is narrow).
const PREVIEW_CONTEXT: usize = 12;

pub enum SearchPanelEvent {
    /// Open `path` at a 0-based line and UTF-8 byte column.
    Open {
        path: PathBuf,
        line: usize,
        column: usize,
    },
    /// Replace these matches; the workspace answers with
    /// [`SearchPanel::replaced`].
    Replace {
        replacer: Replacer,
        files: Vec<FileTargets>,
    },
}

impl EventEmitter<SearchPanelEvent> for SearchPanel {}

/// Open files with unsaved changes and their text.
pub type UnsavedDocuments = Rc<dyn Fn(&App) -> Vec<(PathBuf, String)>>;

struct FileResult {
    path: PathBuf,
    lines: Vec<LineResult>,
}

struct LineResult {
    line: usize,
    /// As the search reported it (long lines are windows of the line).
    text: String,
    /// Non-empty matches: range in `text`, and what replacing it checks.
    matches: Vec<(Range<usize>, Target)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    File {
        file: usize,
    },
    /// A line with all its matches (search mode).
    Line {
        file: usize,
        line: usize,
    },
    /// One match (replace mode).
    Match {
        file: usize,
        line: usize,
        index: usize,
    },
}

/// What an action applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    All,
    File(usize),
    Line(usize, usize),
    Match(usize, usize, usize),
}

impl Row {
    fn scope(self) -> Scope {
        match self {
            Row::File { file } => Scope::File(file),
            Row::Line { file, line } => Scope::Line(file, line),
            Row::Match { file, line, index } => Scope::Match(file, line, index),
        }
    }
}

#[derive(Clone, Copy)]
enum Toggle {
    Regex,
    CaseSensitive,
    WholeWord,
}

enum Status {
    Idle,
    Searching,
    Done {
        files_searched: usize,
        limit_hit: bool,
    },
    Error(SharedString),
}

/// How a part of a match row's preview is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    Match,
    Removed,
    Inserted,
}

/// A match row's text: the line without its indentation (and with a long
/// prefix elided to keep the match visible), the match struck through and
/// followed by its replacement when there is one.
fn preview(
    text: &str,
    range: Range<usize>,
    replacement: Option<&str>,
) -> (String, Vec<(Range<usize>, Mark)>) {
    let indent = text.len() - text.trim_start().len();
    let mut start = indent.min(range.start);
    let mut shown = String::new();
    if range.start - start > PREVIEW_CONTEXT + 4 {
        start = range.start - PREVIEW_CONTEXT;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        shown.push('…');
    }
    shown.push_str(&text[start..range.start]);
    let old = shown.len()..shown.len() + range.len();
    shown.push_str(&text[range.clone()]);
    let marks = match replacement {
        Some(new) => {
            let inserted = shown.len()..shown.len() + new.len();
            shown.push_str(new);
            vec![(old, Mark::Removed), (inserted, Mark::Inserted)]
        }
        None => vec![(old, Mark::Match)],
    };
    shown.push_str(&text[range.end..]);
    (shown, marks)
}

fn highlight(mark: Mark) -> HighlightStyle {
    match mark {
        Mark::Match => HighlightStyle {
            background_color: Some(theme::selection()),
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        },
        Mark::Removed => HighlightStyle {
            background_color: Some(theme::diff_removed()),
            strikethrough: Some(StrikethroughStyle {
                thickness: px(1.),
                color: Some(theme::text()),
            }),
            ..Default::default()
        },
        Mark::Inserted => HighlightStyle {
            background_color: Some(theme::diff_added()),
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        },
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

pub struct SearchPanel {
    project: Project,
    query: Entity<Editor>,
    replacement: Entity<Editor>,
    include: Entity<Editor>,
    exclude: Entity<Editor>,
    regex: bool,
    case_sensitive: bool,
    whole_word: bool,
    replace_open: bool,
    /// The query the results belong to.
    searched: Option<SearchQuery>,
    /// `searched` with the replacement text, if both are valid.
    replacer: Option<Replacer>,
    results: Vec<FileResult>,
    rows: Vec<Row>,
    /// Excluded by the user; kept when results refresh after a replace,
    /// cleared by a new search.
    excluded_files: HashSet<PathBuf>,
    excluded: HashSet<(PathBuf, Target)>,
    status: Status,
    /// The last replace's report.
    notice: Option<(SharedString, fn() -> Hsla)>,
    unsaved: UnsavedDocuments,
    _search: Option<(SearchHandle, Task<()>)>,
    _debounce: Option<Task<()>>,
    _confirm: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl SearchPanel {
    pub fn new(project: Project, cx: &mut Context<Self>) -> Self {
        let query = cx.new(Editor::single_line);
        let replacement = cx.new(Editor::single_line);
        let include = cx.new(Editor::single_line);
        let exclude = cx.new(Editor::single_line);
        let on_edit =
            |this: &mut Self, _: Entity<Editor>, event: &EditorEvent, cx: &mut Context<Self>| {
                if let EditorEvent::Edited = event {
                    this.schedule(cx);
                }
            };
        let subscriptions = vec![
            cx.subscribe(&query, on_edit),
            cx.subscribe(&include, on_edit),
            cx.subscribe(&exclude, on_edit),
            cx.subscribe(&replacement, |this, _, event: &EditorEvent, cx| {
                if let EditorEvent::Edited = event {
                    this.update_replacer(cx);
                    cx.notify();
                }
            }),
        ];
        Self {
            project,
            query,
            replacement,
            include,
            exclude,
            regex: false,
            case_sensitive: false,
            whole_word: false,
            replace_open: false,
            searched: None,
            replacer: None,
            results: Vec::new(),
            rows: Vec::new(),
            excluded_files: HashSet::new(),
            excluded: HashSet::new(),
            status: Status::Idle,
            notice: None,
            unsaved: Rc::new(|_| Vec::new()),
            _search: None,
            _debounce: None,
            _confirm: None,
            _subscriptions: subscriptions,
        }
    }

    /// Where unsaved editor text comes from (the workspace's open editors).
    pub fn set_unsaved_documents(&mut self, unsaved: UnsavedDocuments) {
        self.unsaved = unsaved;
    }

    pub fn focus(&self, window: &mut Window, cx: &App) {
        window.focus(&self.query.focus_handle(cx));
    }

    /// Ctrl+Shift+R / Ctrl+Shift+H: shows the replace field and focuses the
    /// query.
    pub fn open_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.replace_open {
            self.toggle_replace(cx);
        }
        self.focus(window, cx);
    }

    fn toggle_replace(&mut self, cx: &mut Context<Self>) {
        self.replace_open = !self.replace_open;
        self.rebuild_rows();
        cx.notify();
    }

    fn schedule(&mut self, cx: &mut Context<Self>) {
        self._debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            this.update(cx, |this, cx| this.run(cx)).ok();
        }));
    }

    fn current_query(&self, cx: &App) -> SearchQuery {
        let globs = |editor: &Entity<Editor>| -> Vec<String> {
            editor
                .read(cx)
                .text()
                .split(',')
                .map(str::trim)
                .filter(|glob| !glob.is_empty())
                .map(str::to_owned)
                .collect()
        };
        SearchQuery {
            pattern: self.query.read(cx).text(),
            regex: self.regex,
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            include: globs(&self.include),
            exclude: globs(&self.exclude),
        }
    }

    /// Starts a new search, cancelling the previous one.
    fn run(&mut self, cx: &mut Context<Self>) {
        self.start_search(false, cx);
    }

    /// Searches with the current query; `refresh` (after a replace) keeps
    /// the exclusions and the replace report.
    fn start_search(&mut self, refresh: bool, cx: &mut Context<Self>) {
        self._debounce = None;
        self._search = None;
        self.results.clear();
        self.rows.clear();
        if !refresh {
            self.excluded_files.clear();
            self.excluded.clear();
            self.notice = None;
        }
        let query = self.current_query(cx);
        self.searched = None;
        self.replacer = None;
        if query.pattern.is_empty() {
            self.status = Status::Idle;
            cx.notify();
            return;
        }
        let (handle, mut events) = match self.project.search(&query, SearchLimits::default()) {
            Ok(started) => started,
            Err(err) => {
                self.status = Status::Error(err.into());
                cx.notify();
                return;
            }
        };
        // Unsaved editors are searched as shown; their files' disk results
        // are dropped.
        let mut in_memory = HashSet::new();
        if let Ok(filter) = PathFilter::new(self.project.root(), &query) {
            for (path, text) in (self.unsaved)(cx) {
                if !filter.matches(&path) {
                    continue;
                }
                if let Ok(lines) = search::search_text(&query, &text)
                    && !lines.is_empty()
                {
                    self.add_file(FileMatches {
                        path: path.clone(),
                        lines,
                    });
                }
                in_memory.insert(path);
            }
        }
        self.searched = Some(query);
        self.update_replacer(cx);
        self.status = Status::Searching;
        let task = cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                let alive = this.update(cx, |this, cx| {
                    match event {
                        SearchEvent::File(file) => {
                            if !in_memory.contains(&file.path) {
                                this.add_file(file);
                            }
                        }
                        SearchEvent::Done {
                            files_searched,
                            limit_hit,
                            ..
                        } => {
                            this.status = Status::Done {
                                files_searched,
                                limit_hit,
                            };
                        }
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        self._search = Some((handle, task));
        cx.notify();
    }

    fn update_replacer(&mut self, cx: &App) {
        self.replacer = self
            .searched
            .as_ref()
            .and_then(|query| Replacer::new(query, &self.replacement.read(cx).text()).ok());
    }

    fn add_file(&mut self, file: FileMatches) {
        if self.excluded_files.contains(&file.path) {
            return;
        }
        let lines: Vec<LineResult> = file
            .lines
            .into_iter()
            .filter_map(|line| {
                let matches: Vec<(Range<usize>, Target)> = Target::of_line(&line)
                    .into_iter()
                    .filter(|(_, target)| {
                        !self.excluded.contains(&(file.path.clone(), target.clone()))
                    })
                    .collect();
                (!matches.is_empty()).then_some(LineResult {
                    line: line.line,
                    text: line.text,
                    matches,
                })
            })
            .collect();
        if lines.is_empty() {
            return;
        }
        self.results.push(FileResult {
            path: file.path,
            lines,
        });
        self.push_rows(self.results.len() - 1);
    }

    fn push_rows(&mut self, file: usize) {
        self.rows.push(Row::File { file });
        for (line, found) in self.results[file].lines.iter().enumerate() {
            if self.replace_open {
                self.rows
                    .extend((0..found.matches.len()).map(|index| Row::Match { file, line, index }));
            } else {
                self.rows.push(Row::Line { file, line });
            }
        }
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        for file in 0..self.results.len() {
            self.push_rows(file);
        }
    }

    fn match_count(&self) -> usize {
        self.results
            .iter()
            .flat_map(|file| &file.lines)
            .map(|line| line.matches.len())
            .sum()
    }

    /// Removes `scope` from the results and remembers it as excluded.
    fn exclude(&mut self, scope: Scope, cx: &mut Context<Self>) {
        let (file, line, index) = match scope {
            Scope::All => return,
            Scope::File(file) => (file, None, None),
            Scope::Line(file, line) => (file, Some(line), None),
            Scope::Match(file, line, index) => (file, Some(line), Some(index)),
        };
        let Some(result) = self.results.get_mut(file) else {
            return;
        };
        let path = result.path.clone();
        match (line, index) {
            (None, _) => {
                self.results.remove(file);
                self.excluded_files.insert(path);
            }
            (Some(line), index) => {
                let Some(found) = result.lines.get_mut(line) else {
                    return;
                };
                let removed: Vec<Target> = match index {
                    Some(index) if index < found.matches.len() => {
                        vec![found.matches.remove(index).1]
                    }
                    Some(_) => return,
                    None => found.matches.drain(..).map(|(_, target)| target).collect(),
                };
                self.excluded
                    .extend(removed.into_iter().map(|target| (path.clone(), target)));
                if found.matches.is_empty() {
                    result.lines.remove(line);
                }
                if result.lines.is_empty() {
                    self.results.remove(file);
                }
            }
        }
        self.rebuild_rows();
        cx.notify();
    }

    /// The matches `scope` covers, by file.
    fn targets(&self, scope: Scope) -> Vec<FileTargets> {
        let file_targets =
            |file: &FileResult, lines: &mut dyn Iterator<Item = &LineResult>| FileTargets {
                path: file.path.clone(),
                targets: lines
                    .flat_map(|line| line.matches.iter().map(|(_, target)| target.clone()))
                    .collect(),
            };
        match scope {
            Scope::All => self
                .results
                .iter()
                .map(|file| file_targets(file, &mut file.lines.iter()))
                .collect(),
            Scope::File(file) => self
                .results
                .get(file)
                .map(|result| file_targets(result, &mut result.lines.iter()))
                .into_iter()
                .collect(),
            Scope::Line(file, line) => self
                .results
                .get(file)
                .map(|result| file_targets(result, &mut result.lines.get(line).into_iter()))
                .into_iter()
                .collect(),
            Scope::Match(file, line, index) => self
                .results
                .get(file)
                .and_then(|result| {
                    let target = result.lines.get(line)?.matches.get(index)?.1.clone();
                    Some(FileTargets {
                        path: result.path.clone(),
                        targets: vec![target],
                    })
                })
                .into_iter()
                .collect(),
        }
    }

    /// Replaces the matches of `scope`; everything only after confirming.
    fn replace(&mut self, scope: Scope, window: &mut Window, cx: &mut Context<Self>) {
        let Some(replacer) = self.replacer.clone() else {
            return;
        };
        let files = self.targets(scope);
        let matches: usize = files.iter().map(|file| file.targets.len()).sum();
        if matches == 0 {
            return;
        }
        if scope != Scope::All {
            cx.emit(SearchPanelEvent::Replace { replacer, files });
            return;
        }
        let pattern = self
            .searched
            .as_ref()
            .map(|query| query.pattern.clone())
            .unwrap_or_default();
        let message = format!(
            "Replace {} in {}?",
            plural(matches, "match", "matches"),
            plural(files.len(), "file", "files")
        );
        let detail = format!(
            "“{pattern}” → “{}”\nOpen files change in their editor (unsaved).\n\
             Other files are written to disk.",
            self.replacement.read(cx).text()
        );
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            Some(&detail),
            &["Replace", "Cancel"],
            cx,
        );
        self._confirm = Some(cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update(cx, |_, cx| {
                cx.emit(SearchPanelEvent::Replace { replacer, files })
            })
            .ok();
        }));
    }

    fn replace_all(&mut self, _: &ReplaceAll, window: &mut Window, cx: &mut Context<Self>) {
        if self.replace_open {
            self.replace(Scope::All, window, cx);
        }
    }

    /// A replace finished: shows its report and searches again (after the
    /// workspace's update, which may be running now).
    pub fn replaced(&mut self, report: ReplaceReport, cx: &mut Context<Self>) {
        let color: fn() -> Hsla = if !report.failed.is_empty() {
            theme::error
        } else if !report.skipped.is_empty() {
            theme::warning
        } else {
            theme::muted
        };
        self.notice = Some((report.summary(self.project.root()).into(), color));
        let this = cx.entity().downgrade();
        cx.defer(move |cx| {
            this.update(cx, |this, cx| this.start_search(true, cx)).ok();
        });
        cx.notify();
    }

    fn toggle(&mut self, toggle: Toggle, cx: &mut Context<Self>) {
        let flag = match toggle {
            Toggle::Regex => &mut self.regex,
            Toggle::CaseSensitive => &mut self.case_sensitive,
            Toggle::WholeWord => &mut self.whole_word,
        };
        *flag = !*flag;
        self.run(cx);
    }

    /// Enter in an input: search now instead of waiting for the debounce.
    fn search_now(&mut self, _: &editor::Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.run(cx);
    }

    /// Tab / Shift+Tab: the next or previous input (query, replacement
    /// when shown, include, exclude), wrapping around.
    fn move_field(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let mut fields = vec![&self.query];
        if self.replace_open {
            fields.push(&self.replacement);
        }
        fields.extend([&self.include, &self.exclude]);
        let handles: Vec<FocusHandle> = fields.iter().map(|f| f.focus_handle(cx)).collect();
        let at = handles
            .iter()
            .position(|handle| handle.contains_focused(window, cx))
            .unwrap_or(0);
        let next = if forward {
            (at + 1) % handles.len()
        } else {
            (at + handles.len() - 1) % handles.len()
        };
        window.focus(&handles[next]);
    }

    fn next_field(&mut self, _: &NextField, window: &mut Window, cx: &mut Context<Self>) {
        self.move_field(true, window, cx);
    }

    fn previous_field(&mut self, _: &PreviousField, window: &mut Window, cx: &mut Context<Self>) {
        self.move_field(false, window, cx);
    }

    fn status_text(&self) -> Option<(SharedString, fn() -> Hsla)> {
        let counts = || {
            format!(
                "{} in {}",
                plural(self.match_count(), "match", "matches"),
                plural(self.results.len(), "file", "files")
            )
        };
        match &self.status {
            Status::Idle => None,
            Status::Searching => Some((format!("Searching… {}", counts()).into(), theme::muted)),
            Status::Done {
                files_searched,
                limit_hit,
            } => {
                let more = if *limit_hit { " (limit reached)" } else { "" };
                Some((
                    format!("{}{more} · {files_searched} files searched", counts()).into(),
                    theme::muted,
                ))
            }
            Status::Error(err) => Some((err.clone(), theme::error)),
        }
    }

    fn render_toggle(
        &self,
        toggle: Toggle,
        label: &'static str,
        on: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        div()
            .id(label)
            .px_1()
            .rounded_sm()
            .text_xs()
            .font_family(theme::MONO_FONT)
            .cursor_pointer()
            .when(on, |button| {
                button.bg(theme::active_row()).text_color(theme::text())
            })
            .when(!on, |button| {
                button
                    .text_color(theme::muted())
                    .hover(|style| style.text_color(theme::text()))
            })
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| this.toggle(toggle, cx)))
    }

    /// The hover actions of a result row: Replace (replace mode) and ×.
    fn render_actions(
        &self,
        index: usize,
        row: Row,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let button = |id: &'static str, label: &'static str, tooltip: &'static str| {
            div()
                .id((id, index))
                .px_1()
                .rounded_sm()
                .text_xs()
                .text_color(theme::muted())
                .cursor_pointer()
                .hover(|style| style.bg(theme::active_row()).text_color(theme::text()))
                .tooltip(move |window, cx| Tooltip::view(tooltip, window, cx))
                .child(label)
        };
        let scope = row.scope();
        let replace = match row {
            Row::File { .. } => Some(("Replace All", "Replace all matches in this file")),
            Row::Match { .. } => Some(("Replace", "Replace this match")),
            Row::Line { .. } => None,
        }
        .filter(|_| self.replace_open && self.replacer.is_some());
        let exclude = match row {
            Row::File { .. } => "Exclude this file from the results",
            Row::Line { .. } => "Exclude this line from the results",
            Row::Match { .. } => "Exclude this match from the results",
        };
        div()
            .absolute()
            .top_0()
            .right_0()
            .h_full()
            .pl_1()
            .pr_1()
            .flex()
            .items_center()
            .gap_1()
            .bg(theme::hover_row())
            .invisible()
            .group_hover("search-row", |style| style.visible())
            .children(replace.map(|(label, tooltip)| {
                button("search-replace", label, tooltip).on_click(cx.listener(
                    move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.replace(scope, window, cx);
                    },
                ))
            }))
            .child(button("search-exclude", "×", exclude).on_click(cx.listener(
                move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.exclude(scope, cx);
                },
            )))
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let row = self.rows[index];
        let base = div()
            .id(index)
            .group("search-row")
            .relative()
            .h(px(ROW_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .overflow_hidden()
            .hover(|style| style.bg(theme::hover_row()));
        match row {
            Row::File { file } => {
                let result = &self.results[file];
                let shown = result
                    .path
                    .strip_prefix(self.project.root())
                    .unwrap_or(&result.path);
                let name = shown
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let dir = shown
                    .parent()
                    .map(|dir| dir.display().to_string())
                    .unwrap_or_default();
                let count: usize = result.lines.iter().map(|line| line.matches.len()).sum();
                base.px_2()
                    .text_sm()
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme::text())
                            .child(selectable("name", name)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(selectable("dir", dir)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(selectable("count", count.to_string())),
                    )
                    .child(self.render_actions(index, row, cx))
            }
            Row::Line { file, line } | Row::Match { file, line, .. } => {
                let result = &self.results[file];
                let found = &result.lines[line];
                let (text, marks, column) = match row {
                    Row::Match { index: at, .. } => {
                        let (range, target) = &found.matches[at];
                        let replacement = self
                            .replacer
                            .as_ref()
                            .filter(|_| !self.replacement.read(cx).buffer().is_empty())
                            .and_then(|replacer| replacer.replacement(&found.text, range.clone()));
                        let (text, marks) =
                            preview(&found.text, range.clone(), replacement.as_deref());
                        (text, marks, target.column)
                    }
                    _ => {
                        // Leading indentation wastes the narrow panel.
                        let trimmed = found.text.trim_start();
                        let shift = found.text.len() - trimmed.len();
                        let marks = found
                            .matches
                            .iter()
                            .filter(|(range, _)| range.start >= shift)
                            .map(|(range, _)| (range.start - shift..range.end - shift, Mark::Match))
                            .collect();
                        let column = found.matches[0].1.column;
                        (trimmed.to_owned(), marks, column)
                    }
                };
                let highlights: Vec<(Range<usize>, HighlightStyle)> = marks
                    .into_iter()
                    .map(|(range, mark)| (range, highlight(mark)))
                    .collect();
                let (path, line_number) = (result.path.clone(), found.line);
                base.pl(px(20.))
                    .pr_2()
                    .cursor_pointer()
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(SearchPanelEvent::Open {
                            path: path.clone(),
                            line: line_number,
                            column,
                        });
                    }))
                    .child(
                        div()
                            .flex_none()
                            .w(px(36.))
                            .text_xs()
                            .text_color(theme::muted())
                            .child(selectable("line", (found.line + 1).to_string())),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(theme::MONO_FONT)
                            .text_sm()
                            .text_color(theme::text())
                            .child(selectable("text", text).with_highlights(highlights)),
                    )
                    .child(self.render_actions(index, row, cx))
            }
        }
    }
}

#[cfg(test)]
impl SearchPanel {
    pub(crate) fn replacement_input(&self) -> Entity<Editor> {
        self.replacement.clone()
    }

    /// The status line and the replace report, as shown.
    pub(crate) fn shown_status(&self) -> (Option<String>, Option<String>) {
        (
            self.status_text().map(|(text, _)| text.to_string()),
            self.notice.as_ref().map(|(text, _)| text.to_string()),
        )
    }
}

impl Focusable for SearchPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for SearchPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let input = |editor: &Entity<Editor>, placeholder: &'static str, cx: &App| {
            let empty = editor.read(cx).buffer().is_empty();
            div()
                .relative()
                .h(px(26.))
                .px_2()
                // Room for the line height, so `_` is not clipped.
                .py(px(2.))
                .bg(theme::background())
                .border_1()
                .border_color(theme::border())
                .rounded_sm()
                .child(editor.clone())
                .when(empty, |input| {
                    input.child(
                        div()
                            .absolute()
                            .top(px(4.))
                            .left(px(8.))
                            .text_sm()
                            .text_color(theme::muted())
                            .child(placeholder),
                    )
                })
        };
        let can_replace = self.replacer.is_some() && !self.results.is_empty();
        let chevron = if self.replace_open { "▾" } else { "▸" };

        div()
            .size_full()
            .flex()
            .flex_col()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::search_now))
            .on_action(cx.listener(Self::replace_all))
            .on_action(cx.listener(Self::next_field))
            .on_action(cx.listener(Self::previous_field))
            .child(
                div()
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .id("search-toggle-replace")
                                    .flex_none()
                                    .w(px(14.))
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .cursor_pointer()
                                    .hover(|style| style.text_color(theme::text()))
                                    .tooltip(|window, cx| {
                                        Tooltip::view("Toggle Replace (Ctrl+Shift+R)", window, cx)
                                    })
                                    .child(chevron)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.toggle_replace(cx)),
                                    ),
                            )
                            .child(div().flex_1().child(input(&self.query, "Search", cx)))
                            .child(self.render_toggle(
                                Toggle::CaseSensitive,
                                "Aa",
                                self.case_sensitive,
                                cx,
                            ))
                            .child(self.render_toggle(Toggle::WholeWord, "ab", self.whole_word, cx))
                            .child(self.render_toggle(Toggle::Regex, ".*", self.regex, cx)),
                    )
                    .when(self.replace_open, |panel| {
                        panel.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .pl(px(18.))
                                .child(div().flex_1().child(input(
                                    &self.replacement,
                                    if self.regex {
                                        "Replace ($1, ${name})"
                                    } else {
                                        "Replace"
                                    },
                                    cx,
                                )))
                                .child(
                                    div()
                                        .id("search-replace-all")
                                        .flex_none()
                                        .px_2()
                                        .py_1()
                                        .rounded_sm()
                                        .text_xs()
                                        .border_1()
                                        .border_color(theme::border())
                                        .when(can_replace, |button| {
                                            button
                                                .text_color(theme::text())
                                                .cursor_pointer()
                                                .hover(|style| style.bg(theme::hover_row()))
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.replace(Scope::All, window, cx)
                                                }))
                                        })
                                        .when(!can_replace, |button| {
                                            button.text_color(theme::muted())
                                        })
                                        .tooltip(|window, cx| {
                                            Tooltip::view(
                                                "Replace All (Ctrl+Alt+Enter)",
                                                window,
                                                cx,
                                            )
                                        })
                                        .child("Replace All"),
                                ),
                        )
                    })
                    .child(input(
                        &self.include,
                        "Files to include (e.g. *.py, src/**)",
                        cx,
                    ))
                    .child(input(&self.exclude, "Files to exclude", cx))
                    .children(self.status_text().map(|(status, color)| {
                        div()
                            .text_xs()
                            .text_color(color())
                            .child(selectable("status", status))
                    }))
                    .children(self.notice.clone().map(|(notice, color)| {
                        div()
                            .text_xs()
                            .text_color(color())
                            .child(selectable("replace-notice", notice))
                    })),
            )
            .child(
                uniform_list(
                    "search-results",
                    self.rows.len(),
                    cx.processor(|this, range: Range<usize>, _, cx| {
                        range.map(|index| this.render_row(index, cx)).collect()
                    }),
                )
                .flex_1(),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;

    #[test]
    fn previews_strike_the_match_and_show_the_replacement() {
        let (text, marks) = preview("    let old = 1;", 8..11, Some("new"));
        assert_eq!(text, "let oldnew = 1;");
        assert_eq!(marks, [(4..7, Mark::Removed), (7..10, Mark::Inserted)]);
        let (text, marks) = preview("  é old", 5..8, None);
        assert_eq!(text, "é old");
        assert_eq!(marks, [(3..6, Mark::Match)]);
        // A match far into a long line keeps some context before it.
        let line = format!("{}ü {}x", "a".repeat(100), "é".repeat(20));
        let start = line.len() - 1;
        let (text, marks) = preview(&line, start..line.len(), Some("y"));
        assert!(text.starts_with('…') && text.ends_with("xy"), "{text}");
        let (old, _) = &marks[0];
        assert_eq!(&text[old.clone()], "x");
        assert!(text.len() < 50, "{text}");
    }

    fn file(path: &str, text: &str, query: &SearchQuery) -> FileMatches {
        FileMatches {
            path: PathBuf::from(path),
            lines: search::search_text(query, text).unwrap(),
        }
    }

    #[gpui::test]
    fn exclusions_drop_matches_lines_and_files_and_survive_a_refresh(cx: &mut TestAppContext) {
        let query = SearchQuery {
            pattern: "x".into(),
            case_sensitive: true,
            ..Default::default()
        };
        let panel = cx.new(|cx| SearchPanel::new(Project::local(PathBuf::from("/p")), cx));
        let a = file("/p/a.txt", "x x\nx\n", &query);
        let b = file("/p/b.txt", "x\n", &query);
        panel.update(cx, |panel, cx| {
            panel.searched = Some(query.clone());
            panel.add_file(a.clone());
            panel.add_file(b.clone());
            assert_eq!(panel.rows.len(), 2 + 1 + 2, "file rows and line rows");
            panel.toggle_replace(cx);
            assert_eq!(panel.rows.len(), 2 + 3 + 1, "one row per match");
            assert_eq!(panel.match_count(), 4);

            // × on the second match of line 1, then on file b.
            panel.exclude(Scope::Match(0, 0, 1), cx);
            panel.exclude(Scope::File(1), cx);
            let targets = panel.targets(Scope::All);
            assert_eq!(targets.len(), 1);
            let columns: Vec<(usize, usize)> = targets[0]
                .targets
                .iter()
                .map(|target| (target.line, target.column))
                .collect();
            assert_eq!(columns, [(0, 0), (1, 0)]);
            assert_eq!(panel.targets(Scope::Match(0, 1, 0))[0].targets[0].line, 1);

            // Results of a refresh skip what was excluded.
            panel.results.clear();
            panel.add_file(a.clone());
            panel.add_file(b.clone());
            assert_eq!(panel.results.len(), 1);
            assert_eq!(panel.match_count(), 2);

            // Excluding the last match of a line drops the line, the last
            // line drops the file.
            panel.exclude(Scope::Line(0, 0), cx);
            assert_eq!(panel.match_count(), 1);
            panel.exclude(Scope::Match(0, 0, 0), cx);
            assert!(panel.results.is_empty() && panel.rows.is_empty());
        });
    }
}
