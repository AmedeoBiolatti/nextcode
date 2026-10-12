//! Project search panel: query, options, and streamed results grouped by file.

use std::{ops::Range, path::PathBuf, time::Duration};

use futures::StreamExt;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle,
    SharedString, StyledText, Subscription, Task, Window, div, prelude::*, px, rgb, uniform_list,
};
use search::{FileMatches, SearchEvent, SearchLimits, SearchQuery};

use crate::{
    editor::{self, Editor, EditorEvent},
    project::{Project, SearchHandle},
    theme,
};

const ROW_HEIGHT: f32 = 22.;
/// Wait after the last keystroke before searching.
const DEBOUNCE: Duration = Duration::from_millis(250);

pub enum SearchPanelEvent {
    /// Open `path` at a 0-based line and UTF-8 byte column.
    Open {
        path: PathBuf,
        line: usize,
        column: usize,
    },
}

impl EventEmitter<SearchPanelEvent> for SearchPanel {}

enum Row {
    File { file: usize },
    Line { file: usize, line: usize },
}

#[derive(Clone, Copy)]
enum Toggle {
    Regex,
    CaseSensitive,
    WholeWord,
}

pub struct SearchPanel {
    project: Project,
    query: Entity<Editor>,
    include: Entity<Editor>,
    exclude: Entity<Editor>,
    regex: bool,
    case_sensitive: bool,
    whole_word: bool,
    results: Vec<FileMatches>,
    rows: Vec<Row>,
    status: Option<(SharedString, u32)>,
    _search: Option<(SearchHandle, Task<()>)>,
    _debounce: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl SearchPanel {
    pub fn new(project: Project, cx: &mut Context<Self>) -> Self {
        let query = cx.new(Editor::single_line);
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
        ];
        Self {
            project,
            query,
            include,
            exclude,
            regex: false,
            case_sensitive: false,
            whole_word: false,
            results: Vec::new(),
            rows: Vec::new(),
            status: None,
            _search: None,
            _debounce: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &App) {
        window.focus(&self.query.focus_handle(cx));
    }

    fn schedule(&mut self, cx: &mut Context<Self>) {
        self._debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            this.update(cx, |this, cx| this.run(cx)).ok();
        }));
    }

    /// Starts a new search, cancelling the previous one.
    fn run(&mut self, cx: &mut Context<Self>) {
        self._debounce = None;
        self._search = None;
        self.results.clear();
        self.rows.clear();
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
        let query = SearchQuery {
            pattern: self.query.read(cx).text(),
            regex: self.regex,
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            include: globs(&self.include),
            exclude: globs(&self.exclude),
        };
        if query.pattern.is_empty() {
            self.status = None;
            cx.notify();
            return;
        }
        let (handle, mut events) = match self.project.search(&query, SearchLimits::default()) {
            Ok(started) => started,
            Err(err) => {
                self.status = Some((err.into(), theme::ERROR));
                cx.notify();
                return;
            }
        };
        self.status = Some(("Searching…".into(), theme::MUTED));
        let task = cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                let alive = this.update(cx, |this, cx| {
                    match event {
                        SearchEvent::File(file) => this.add_file(file),
                        SearchEvent::Done {
                            files_searched,
                            matches,
                            limit_hit,
                        } => {
                            let files = this.results.len();
                            let more = if limit_hit { " (limit reached)" } else { "" };
                            this.status = Some((
                                format!("{matches} matches in {files} files{more} · {files_searched} files searched").into(),
                                theme::MUTED,
                            ));
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

    fn add_file(&mut self, file: FileMatches) {
        let index = self.results.len();
        self.rows.push(Row::File { file: index });
        self.rows
            .extend((0..file.lines.len()).map(|line| Row::Line { file: index, line }));
        self.results.push(file);
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

    /// Enter in the query: search now instead of waiting for the debounce.
    fn search_now(&mut self, _: &editor::Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.run(cx);
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
                button
                    .bg(rgb(theme::ACTIVE_ROW))
                    .text_color(rgb(theme::TEXT))
            })
            .when(!on, |button| {
                button
                    .text_color(rgb(theme::MUTED))
                    .hover(|style| style.text_color(rgb(theme::TEXT)))
            })
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| this.toggle(toggle, cx)))
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        match self.rows[index] {
            Row::File { file } => {
                let matches = &self.results[file];
                let shown = matches
                    .path
                    .strip_prefix(self.project.root())
                    .unwrap_or(&matches.path);
                let name = shown
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let dir = shown
                    .parent()
                    .map(|dir| dir.display().to_string())
                    .unwrap_or_default();
                div()
                    .id(index)
                    .h(px(ROW_HEIGHT))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_sm()
                    .child(div().flex_none().text_color(rgb(theme::TEXT)).child(name))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(rgb(theme::MUTED))
                            .child(dir),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(rgb(theme::MUTED))
                            .child(matches.lines.len().to_string()),
                    )
            }
            Row::Line { file, line } => {
                let matches = &self.results[file];
                let found = &matches.lines[line];
                // Leading indentation wastes the narrow panel.
                let trimmed = found.text.trim_start();
                let shift = found.text.len() - trimmed.len();
                let highlight = HighlightStyle {
                    background_color: Some(rgb(theme::SELECTION).into()),
                    font_weight: Some(FontWeight::BOLD),
                    ..Default::default()
                };
                let highlights: Vec<(Range<usize>, HighlightStyle)> = found
                    .ranges
                    .iter()
                    .filter(|range| range.start >= shift)
                    .map(|range| (range.start - shift..range.end - shift, highlight))
                    .collect();
                let (path, line_number, column) = (matches.path.clone(), found.line, found.column);
                div()
                    .id(index)
                    .h(px(ROW_HEIGHT))
                    .pl(px(20.))
                    .pr_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(theme::HOVER_ROW)))
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
                            .text_color(rgb(theme::MUTED))
                            .child((found.line + 1).to_string()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(theme::MONO_FONT)
                            .text_sm()
                            .text_color(rgb(theme::TEXT))
                            .child(StyledText::new(trimmed.to_owned()).with_highlights(highlights)),
                    )
            }
        }
    }
}

impl Focusable for SearchPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.focus_handle(cx)
    }
}

impl Render for SearchPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let input = |editor: &Entity<Editor>, placeholder: &'static str, empty: bool| {
            div()
                .relative()
                .h(px(26.))
                .px_2()
                .py_1()
                .bg(rgb(theme::BACKGROUND))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .rounded_sm()
                .child(editor.clone())
                .when(empty, |input| {
                    input.child(
                        div()
                            .absolute()
                            .top(px(4.))
                            .left(px(8.))
                            .text_sm()
                            .text_color(rgb(theme::MUTED))
                            .child(placeholder),
                    )
                })
        };
        let query_empty = self.query.read(cx).buffer().is_empty();
        let include_empty = self.include.read(cx).buffer().is_empty();
        let exclude_empty = self.exclude.read(cx).buffer().is_empty();

        div()
            .size_full()
            .flex()
            .flex_col()
            .on_action(cx.listener(Self::search_now))
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
                                    .flex_1()
                                    .child(input(&self.query, "Search", query_empty)),
                            )
                            .child(self.render_toggle(
                                Toggle::CaseSensitive,
                                "Aa",
                                self.case_sensitive,
                                cx,
                            ))
                            .child(self.render_toggle(Toggle::WholeWord, "ab", self.whole_word, cx))
                            .child(self.render_toggle(Toggle::Regex, ".*", self.regex, cx)),
                    )
                    .child(input(
                        &self.include,
                        "Files to include (e.g. *.py, src/**)",
                        include_empty,
                    ))
                    .child(input(&self.exclude, "Files to exclude", exclude_empty))
                    .children(self.status.clone().map(|(status, color)| {
                        div().text_xs().text_color(rgb(color)).child(status)
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
