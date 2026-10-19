//! Find and replace in the file: the find bar above the text, matches
//! cached per buffer version and query, highlighted while the bar is open.
//! A big file is searched on a background thread from a snapshot; until
//! that search is in, the previous matches follow the edits.

use gpui::Subscription;
use regex::{Regex, RegexBuilder};

use super::*;

/// Files larger than this are searched in the background.
const FIND_IN_BACKGROUND_BYTES: usize = 512 * 1024;

/// What a search is for: buffer version, query and options.
type FindKey = (u64, String, FindOptions);

actions!(
    editor,
    [
        Find,
        FindReplace,
        FindNext,
        FindPrevious,
        SelectAllMatches,
        ReplaceNext,
        ReplaceAll,
        CloseFind,
        ToggleMatchCase,
        ToggleWholeWord,
        ToggleRegex,
    ]
);

/// Key context of the find bar (around both inputs).
pub(super) const FIND_CONTEXT: &str = "FindBar";
/// Key contexts of the query and replacement inputs.
pub(super) const FIND_INPUT: &str = "FindInput";
pub(super) const REPLACE_INPUT: &str = "ReplaceInput";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct FindOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

/// The search for `query`, or `None` when it is empty.
pub(super) fn compile(query: &str, options: FindOptions) -> Result<Option<Regex>, String> {
    if query.is_empty() {
        return Ok(None);
    }
    let pattern = if options.regex {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(!options.case_sensitive)
        .multi_line(true)
        .build()
        .map(Some)
        .map_err(|err| match err {
            regex::Error::Syntax(message) => message
                .lines()
                .last()
                .unwrap_or("invalid regex")
                .trim_start_matches("error: ")
                .to_owned(),
            other => other.to_string(),
        })
}

/// Non-empty matches of `regex` in `text`, sorted.
pub(super) fn find_matches(text: &str, regex: &Regex, whole_word: bool) -> Vec<Range<usize>> {
    regex
        .find_iter(text)
        .map(|m| m.range())
        .filter(|range| !range.is_empty() && (!whole_word || is_whole_word(text, range)))
        .collect()
}

/// The text replacing `range` (a match of `regex`): `$1`-style groups
/// expand in regex mode, the replacement is literal otherwise.
pub(super) fn replacement_for(
    text: &str,
    range: &Range<usize>,
    regex: &Regex,
    options: FindOptions,
    replacement: &str,
) -> String {
    if !options.regex {
        return replacement.to_owned();
    }
    let mut expanded = String::new();
    if let Some(captures) = regex.captures_at(text, range.start) {
        captures.expand(replacement, &mut expanded);
    }
    expanded
}

pub(super) struct FindBar {
    pub query: Entity<Editor>,
    pub replacement: Entity<Editor>,
    pub open: bool,
    pub replace_open: bool,
    pub options: FindOptions,
    /// Matches for `computed`, sorted (mapped through later edits while a
    /// background search runs).
    pub matches: Vec<Range<usize>>,
    pub error: Option<SharedString>,
    /// Buffer version, query and options `matches` belong to.
    computed: Option<FindKey>,
    /// The search running in the background, if any.
    requested: Option<FindKey>,
    _task: Option<Task<()>>,
    /// Where incremental search starts: the selection when the bar opened.
    origin: usize,
    _subscriptions: [Subscription; 2],
}

impl Editor {
    pub(super) fn find_open(&self) -> bool {
        self.find.as_ref().is_some_and(|find| find.open)
    }

    pub(super) fn ensure_find_bar(&mut self, cx: &mut Context<Self>) {
        if self.find.is_some() {
            return;
        }
        let query = cx.new(Editor::single_line);
        let replacement = cx.new(Editor::single_line);
        let on_query = cx.subscribe(&query, |this, _, event, cx| {
            if let EditorEvent::Edited = event {
                this.find_query_changed(cx);
            }
        });
        let on_replacement = cx.subscribe(&replacement, |_, _, _: &EditorEvent, cx| cx.notify());
        self.find = Some(FindBar {
            query,
            replacement,
            replace_open: false,
            options: FindOptions::default(),
            matches: Vec::new(),
            error: None,
            computed: None,
            requested: None,
            _task: None,
            origin: 0,
            open: false,
            _subscriptions: [on_query, on_replacement],
        });
    }

    /// Ctrl+F / Ctrl+H: opens the bar seeded with the selection (or the
    /// word at the cursor) and focuses it.
    fn open_find(&mut self, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.single_line {
            return;
        }
        self.ensure_find_bar(cx);
        let primary = self.primary();
        let seed = if primary.is_empty() {
            self.word_around(primary.head)
        } else {
            let range = primary.range();
            (!self.buffer.slice(range.clone()).contains('\n')).then_some(range)
        };
        let origin = seed.as_ref().map_or(primary.start(), |range| range.start);
        let seed = seed.map(|range| self.buffer.slice(range));
        let Some(find) = &mut self.find else {
            return;
        };
        find.open = true;
        find.replace_open = replace;
        // Incremental search starts at the seeded word, so it stays selected.
        find.origin = origin;
        let (query, replacement) = (find.query.clone(), find.replacement.clone());
        if let Some(seed) = seed {
            let seed = if find.options.regex {
                regex::escape(&seed)
            } else {
                seed
            };
            query.update(cx, |query, cx| query.set_text(&seed, cx));
        }
        query.update(cx, |query, cx| query.select_all_text(cx));
        let has_query = !query.read(cx).buffer().is_empty();
        let target = if replace && has_query {
            &replacement
        } else {
            &query
        };
        window.focus(&target.focus_handle(cx));
        cx.notify();
    }

    pub(super) fn find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        self.open_find(false, window, cx);
    }

    pub(super) fn find_replace(
        &mut self,
        _: &FindReplace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_find(true, window, cx);
    }

    pub(super) fn close_find(
        &mut self,
        _: &CloseFind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(find) = &mut self.find {
            find.open = false;
        }
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Recomputes the matches if the buffer, query or options changed: at
    /// once for a small file, on a background thread for a big one (the
    /// matches from before follow the edits until it is in, see
    /// `find_edited`).
    pub(super) fn refresh_find(&mut self, cx: &mut Context<Self>) {
        let Some(find) = &self.find else {
            return;
        };
        let query = find.query.read(cx).text();
        let key: FindKey = (self.version, query, find.options);
        if find.computed.as_ref() == Some(&key) {
            return;
        }
        let regex = match compile(&key.1, key.2) {
            Ok(Some(regex)) => regex,
            Ok(None) => return self.set_find_matches(Vec::new(), None, key),
            Err(err) => return self.set_find_matches(Vec::new(), Some(err.into()), key),
        };
        if self.buffer.len() <= FIND_IN_BACKGROUND_BYTES {
            let matches = find_matches(&self.text(), &regex, key.2.whole_word);
            return self.set_find_matches(matches, None, key);
        }
        if find.requested.as_ref() == Some(&key) {
            return;
        }
        let snapshot = self.buffer.snapshot();
        let whole_word = key.2.whole_word;
        let Some(find) = &mut self.find else {
            return;
        };
        find.requested = Some(key.clone());
        find._task = Some(cx.spawn(async move |this, cx| {
            let matches = cx
                .background_spawn(
                    async move { find_matches(&snapshot.to_string(), &regex, whole_word) },
                )
                .await;
            this.update(cx, |this, cx| {
                let Some(find) = &mut this.find else {
                    return;
                };
                if find.requested.as_ref() != Some(&key) {
                    return;
                }
                find.requested = None;
                this.set_find_matches(matches, None, key);
                cx.notify();
            })
            .ok();
        }));
    }

    fn set_find_matches(
        &mut self,
        matches: Vec<Range<usize>>,
        error: Option<SharedString>,
        key: FindKey,
    ) {
        if let Some(find) = &mut self.find {
            find.matches = matches;
            find.error = error;
            find.computed = Some(key);
            find.requested = None;
            find._task = None;
        }
    }

    /// After an edit (`changes`): the matches follow the text, ones the
    /// edit touched go, until the search for the new version replaces
    /// them.
    pub(super) fn find_edited(&mut self) {
        let Some(find) = &mut self.find else {
            return;
        };
        if find.matches.is_empty() {
            return;
        }
        for change in &self.changes {
            find.matches.retain_mut(|m| {
                let touched = change.start < m.end && m.start < change.old_end;
                m.start = change.map_offset(m.start, Bias::Right);
                m.end = change.map_offset(m.end, Bias::Left);
                !touched && m.start < m.end
            });
        }
    }

    /// Index of the match the primary selection covers exactly.
    pub(super) fn active_match(&self) -> Option<usize> {
        let find = self.find.as_ref()?;
        let range = self.primary().range();
        find.matches
            .binary_search_by_key(&range.start, |m| m.start)
            .ok()
            .filter(|&i| find.matches[i] == range)
    }

    fn select_match(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(range) = self
            .find
            .as_ref()
            .and_then(|f| f.matches.get(index))
            .cloned()
        else {
            return;
        };
        self.set_selections(
            SelectionSet::single(Selection::new(range.start, range.end)),
            cx,
        );
    }

    /// Typing in the query selects the first match from where the search
    /// started.
    fn find_query_changed(&mut self, cx: &mut Context<Self>) {
        self.refresh_find(cx);
        let Some(find) = &self.find else {
            return;
        };
        let origin = find.origin;
        let next = find
            .matches
            .iter()
            .position(|m| m.start >= origin)
            .or((!find.matches.is_empty()).then_some(0));
        if let Some(index) = next {
            self.select_match(index, cx);
        }
        cx.notify();
    }

    fn find_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.refresh_find(cx);
        let Some(find) = &self.find else {
            return;
        };
        if find.matches.is_empty() {
            return;
        }
        let primary = self.primary().range();
        let matches = &find.matches;
        let index = if forward {
            matches
                .iter()
                .position(|m| m.start >= primary.end && *m != primary)
                .unwrap_or(0)
        } else {
            matches
                .iter()
                .rposition(|m| m.start < primary.start)
                .unwrap_or(matches.len() - 1)
        };
        self.select_match(index, cx);
    }

    pub(super) fn find_next(&mut self, _: &FindNext, _: &mut Window, cx: &mut Context<Self>) {
        if self.find.is_some() {
            self.find_step(true, cx);
        }
    }

    pub(super) fn find_previous(
        &mut self,
        _: &FindPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.find.is_some() {
            self.find_step(false, cx);
        }
    }

    /// Alt+Enter: a selection on every match; focus returns to the text.
    pub(super) fn select_all_matches(
        &mut self,
        _: &SelectAllMatches,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.refresh_find(cx);
        let Some(find) = &self.find else {
            return;
        };
        if find.matches.is_empty() {
            return;
        }
        let primary = self.active_match().unwrap_or(0);
        let items = find
            .matches
            .iter()
            .map(|m| Selection::new(m.start, m.end))
            .collect();
        self.set_selections(SelectionSet::new(items, primary), cx);
        window.focus(&self.focus_handle);
    }

    /// The compiled query and replacement text, if the query is valid.
    fn replace_inputs(&self, cx: &App) -> Option<(Regex, FindOptions, String)> {
        let find = self.find.as_ref()?;
        let query = find.query.read(cx).text();
        let regex = compile(&query, find.options).ok()??;
        Some((regex, find.options, find.replacement.read(cx).text()))
    }

    /// Enter in the replacement: replaces the selected match and selects
    /// the next one (or just goes to the next match).
    pub(super) fn replace_next(&mut self, _: &ReplaceNext, _: &mut Window, cx: &mut Context<Self>) {
        self.refresh_find(cx);
        let Some(index) = self.active_match() else {
            return self.find_step(true, cx);
        };
        let Some((regex, options, replacement)) = self.replace_inputs(cx) else {
            return;
        };
        let range = self.find.as_ref().expect("has a match").matches[index].clone();
        // Groups are read from the match's lines, not a copy of the file:
        // anchors and word boundaries see the same neighbours there.
        let new = if options.regex {
            let window_start = self.buffer.line_start(self.buffer.line_of(range.start));
            let window_end = self.buffer.line_end(self.buffer.line_of(range.end));
            let window = self.buffer.slice(window_start..window_end.max(range.end));
            let local = range.start - window_start..range.end - window_start;
            replacement_for(&window, &local, &regex, options, &replacement)
        } else {
            replacement
        };
        let end = range.start + new.len();
        self.commit(
            vec![(range, new)],
            |_| SelectionSet::single(Selection::cursor(end)),
            cx,
        );
        self.find_step(true, cx);
    }

    /// Ctrl+Alt+Enter: replaces every match as one undo step.
    pub(super) fn replace_all(&mut self, _: &ReplaceAll, _: &mut Window, cx: &mut Context<Self>) {
        self.refresh_find(cx);
        let Some((regex, options, replacement)) = self.replace_inputs(cx) else {
            return;
        };
        let Some(find) = &self.find else {
            return;
        };
        let text = self.text();
        let edits: Vec<(Range<usize>, String)> = find
            .matches
            .iter()
            .map(|m| {
                (
                    m.clone(),
                    replacement_for(&text, m, &regex, options, &replacement),
                )
            })
            .collect();
        let count = edits.len();
        if count == 0 {
            return;
        }
        let selections = self.map_selections(&edits);
        self.commit(edits, |_| selections, cx);
        self.notice = Some(
            format!(
                "Replaced {count} occurrence{}",
                if count == 1 { "" } else { "s" }
            )
            .into(),
        );
    }

    fn toggle_option(&mut self, toggle: fn(&mut FindOptions), cx: &mut Context<Self>) {
        if let Some(find) = &mut self.find {
            toggle(&mut find.options);
            self.find_query_changed(cx);
        }
    }

    pub(super) fn toggle_match_case(
        &mut self,
        _: &ToggleMatchCase,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_option(|o| o.case_sensitive = !o.case_sensitive, cx);
    }

    pub(super) fn toggle_whole_word(
        &mut self,
        _: &ToggleWholeWord,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_option(|o| o.whole_word = !o.whole_word, cx);
    }

    pub(super) fn toggle_regex(&mut self, _: &ToggleRegex, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_option(|o| o.regex = !o.regex, cx);
    }

    /// `n of m` (or `m results` when no match is selected).
    fn find_count(&self) -> SharedString {
        let Some(find) = &self.find else {
            return SharedString::default();
        };
        if let Some(error) = &find.error {
            return error.clone();
        }
        let total = find.matches.len();
        match self.active_match() {
            _ if total == 0 => "No results".into(),
            Some(index) => format!("{} of {total}", index + 1).into(),
            None => format!("{total} result{}", if total == 1 { "" } else { "s" }).into(),
        }
    }

    pub(super) fn render_find_bar(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let find = self.find.as_ref().filter(|find| find.open)?;
        let input = |editor: &Entity<Editor>, context: &'static str, placeholder: &'static str| {
            let empty = editor.read(cx).buffer().is_empty();
            div()
                .key_context(context)
                .relative()
                .w(px(260.))
                // Border, padding and one editor line (`EDITOR_LINE_HEIGHT`).
                .h(px(28.))
                .px_2()
                .py(px(3.))
                .bg(theme::background())
                .border_1()
                .border_color(theme::border())
                .rounded_sm()
                .text_sm()
                .child(editor.clone())
                .when(empty, |input| {
                    input.child(
                        div()
                            .absolute()
                            .top(px(4.))
                            .left(px(8.))
                            .text_color(theme::muted())
                            .child(placeholder),
                    )
                })
        };
        let toggle =
            |id: &'static str, label: &'static str, on: bool, flip: fn(&mut FindOptions)| {
                div()
                    .id(id)
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
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_option(flip, cx)))
            };
        let button = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .px_1()
                .rounded_sm()
                .text_xs()
                .cursor_pointer()
                .text_color(theme::muted())
                .hover(|style| style.text_color(theme::text()).bg(theme::hover_row()))
                .child(label)
        };
        let count_color = if find.error.is_some() || find.matches.is_empty() {
            theme::error()
        } else {
            theme::muted()
        };
        let empty_query = find.query.read(cx).buffer().is_empty();
        let options = find.options;
        Some(
            div()
                .key_context(FIND_CONTEXT)
                .flex()
                .flex_col()
                .gap_1()
                .px_2()
                .py_1()
                .bg(theme::panel())
                .border_b_1()
                .border_color(theme::border())
                .text_color(theme::text())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            input(&find.query, FIND_INPUT, "Find")
                                .pin("find-input")
                                .label("Find"),
                        )
                        .child(
                            toggle("find-case", "Aa", options.case_sensitive, |o| {
                                o.case_sensitive = !o.case_sensitive
                            })
                            .pin("match-case")
                            .label("Match Case"),
                        )
                        .child(
                            toggle("find-word", "ab", options.whole_word, |o| {
                                o.whole_word = !o.whole_word
                            })
                            .pin("whole-word")
                            .label("Match Whole Word"),
                        )
                        .child(
                            toggle("find-regex", ".*", options.regex, |o| o.regex = !o.regex)
                                .pin("regex")
                                .label("Use Regular Expression"),
                        )
                        .child(
                            div()
                                .min_w(px(80.))
                                .px_2()
                                .text_xs()
                                .text_color(if empty_query {
                                    theme::muted()
                                } else {
                                    count_color
                                })
                                .when(!empty_query, |count| {
                                    count.child(selectable("find-count", self.find_count()))
                                })
                                .pin("count"),
                        )
                        .child(
                            button("find-prev", "↑")
                                .on_click(cx.listener(|this, _, _, cx| this.find_step(false, cx)))
                                .pin("previous")
                                .label("Previous Match"),
                        )
                        .child(
                            button("find-next", "↓")
                                .on_click(cx.listener(|this, _, _, cx| this.find_step(true, cx)))
                                .pin("next")
                                .label("Next Match"),
                        )
                        .child(
                            button("find-all", "All")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.select_all_matches(&SelectAllMatches, window, cx)
                                }))
                                .pin("select-all")
                                .label("Select All Matches"),
                        )
                        .child(
                            button("find-close", "✕")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.close_find(&CloseFind, window, cx)
                                }))
                                .pin("close")
                                .label("Close"),
                        ),
                )
                .when(find.replace_open, |bar| {
                    bar.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                input(&find.replacement, REPLACE_INPUT, "Replace")
                                    .pin("replace-input")
                                    .label("Replace"),
                            )
                            .child(
                                button("replace-next", "Replace")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.replace_next(&ReplaceNext, window, cx)
                                    }))
                                    .pin("replace")
                                    .label("Replace"),
                            )
                            .child(
                                button("replace-all", "Replace All")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.replace_all(&ReplaceAll, window, cx)
                                    }))
                                    .pin("replace-all")
                                    .label("Replace All"),
                            ),
                    )
                })
                .pin("find-bar")
                .label("Find"),
        )
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;
    use crate::editor::tests::{editor, type_text, update};

    #[gpui::test]
    fn big_files_are_searched_in_the_background_and_matches_follow_edits(cx: &mut TestAppContext) {
        let filler = "x".repeat(FIND_IN_BACKGROUND_BYTES);
        let window = editor("a.txt", &format!("|foo {filler} foo\nfoo"), cx);
        let bar = |e: &Editor| {
            let find = e.find.as_ref().unwrap();
            (find.matches.clone(), find.requested.is_some())
        };
        update(window, cx, |e, w, cx| {
            e.find(&Find, w, cx);
            let input = e.find.as_ref().unwrap().query.clone();
            input.update(cx, |input, cx| input.set_text("foo", cx));
        });
        // The query asked for a search; nothing is in before it ran.
        assert_eq!(
            window.read_with(cx, |e, _| bar(e)).unwrap(),
            (Vec::new(), true)
        );
        cx.run_until_parked();
        let (matches, pending) = window.read_with(cx, |e, _| bar(e)).unwrap();
        assert_eq!(matches.len(), 3);
        assert!(!pending);
        let second = matches[1].clone();

        // Typing before the matches moves them at once (the file is not
        // copied for that); the search then runs again for the new text.
        update(window, cx, |e, _, cx| e.set_cursor(0, cx));
        type_text(window, "yy", cx);
        let (matches, _) = window.read_with(cx, |e, _| bar(e)).unwrap();
        assert_eq!(matches[0], 2..5);
        assert_eq!(matches[1], second.start + 2..second.end + 2);
        cx.run_until_parked();
        let version = window
            .read_with(cx, |e, _| {
                assert_eq!(bar(e).0.len(), 3);
                (
                    e.version,
                    e.find.as_ref().unwrap().computed.clone().unwrap().0,
                )
            })
            .unwrap();
        assert_eq!(version.0, version.1, "searched for the latest text");

        // Editing inside a match drops it at once.
        update(window, cx, |e, _, cx| {
            e.commit(
                vec![(3..4, String::new())],
                |_| SelectionSet::single(Selection::cursor(3)),
                cx,
            )
        });
        let (matches, _) = window.read_with(cx, |e, _| bar(e)).unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0], second.start + 1..second.end + 1);
    }

    fn matches<'a>(text: &'a str, query: &str, options: FindOptions) -> Vec<&'a str> {
        let regex = compile(query, options).unwrap().unwrap();
        find_matches(text, &regex, options.whole_word)
            .into_iter()
            .map(|range| &text[range])
            .collect()
    }

    #[test]
    fn finds_with_case_whole_word_and_regex_options() {
        let text = "Foo foo food (foo) f.o";
        let plain = FindOptions::default();
        assert_eq!(matches(text, "foo", plain), ["Foo", "foo", "foo", "foo"]);
        let case = FindOptions {
            case_sensitive: true,
            ..plain
        };
        assert_eq!(matches(text, "Foo", case), ["Foo"]);
        let word = FindOptions {
            whole_word: true,
            ..plain
        };
        assert_eq!(matches(text, "foo", word), ["Foo", "foo", "foo"]);
        assert_eq!(matches(text, "f.o", plain), ["f.o"], "literal by default");
        let regex = FindOptions {
            regex: true,
            ..plain
        };
        assert_eq!(matches(text, "f.o", regex).len(), 5);
        assert_eq!(
            matches("a\nb\n", "^\\w$", regex),
            ["a", "b"],
            "per line anchors"
        );
        assert!(compile("(", regex).is_err());
        assert!(compile("", regex).unwrap().is_none());
    }

    #[test]
    fn regex_replacements_expand_groups_and_literal_ones_do_not() {
        let text = "let a = 1; let b = 2;";
        let regex_options = FindOptions {
            regex: true,
            ..FindOptions::default()
        };
        let regex = compile(r"let (\w) = (\d)", regex_options).unwrap().unwrap();
        let replaced: Vec<String> = find_matches(text, &regex, false)
            .iter()
            .map(|m| replacement_for(text, m, &regex, regex_options, "const $1: i32 = ${2}0"))
            .collect();
        assert_eq!(replaced, ["const a: i32 = 10", "const b: i32 = 20"]);
        let literal = compile("let", FindOptions::default()).unwrap().unwrap();
        let range = find_matches(text, &literal, false)[0].clone();
        assert_eq!(
            replacement_for(text, &range, &literal, FindOptions::default(), "$1"),
            "$1"
        );
    }
}
