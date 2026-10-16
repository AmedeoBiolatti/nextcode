//! Completion menu: requesting, filtering, accepting and rendering items.

use gpui::{
    FontWeight, HighlightStyle, ScrollStrategy, StyledText, UniformListScrollHandle, uniform_list,
};
use language::completion::{self as complete, Candidate};
use lsp::types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionParams, CompletionResponse,
    CompletionTriggerKind, Documentation, InsertTextFormat, SymbolKind,
    request::{Completion, ResolveCompletionItem},
};

use super::*;

/// Rows visible at once.
const MAX_ROWS: usize = 10;
const ROW_HEIGHT: f32 = 22.;

pub(crate) struct CompletionMenu {
    /// Start of the word being completed; the query runs from here to the cursor.
    word_start: usize,
    items: Vec<CompletionItem>,
    candidates: Vec<Candidate>,
    selected: usize,
    /// The server's list is partial; re-request as the query changes.
    incomplete: bool,
    scroll: UniformListScrollHandle,
    server: Arc<RunningServer>,
}

impl Editor {
    /// Reacts to typed text: trigger characters (e.g. `.`) start a request,
    /// identifier characters start one when no menu is open.
    pub(super) fn on_typed(&mut self, text: &str, cx: &mut Context<Self>) {
        let mut chars = text.chars();
        let (Some(typed), None) = (chars.next(), chars.next()) else {
            return;
        };
        if self.marked.is_some() {
            return;
        }
        let Some(server) = self.server() else {
            return;
        };
        let Some(provider) = &server.capabilities.completion_provider else {
            return;
        };
        let is_trigger = provider
            .trigger_characters
            .iter()
            .flatten()
            .any(|trigger| trigger == text);
        if is_trigger {
            self.request_completions(Some(text.to_owned()), cx);
        } else if (typed.is_alphanumeric() || typed == '_') && self.completion.is_none() {
            self.request_completions(None, cx);
        }
    }

    pub(super) fn show_completions(
        &mut self,
        _: &ShowCompletions,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_completions(None, cx);
    }

    fn request_completions(&mut self, trigger: Option<String>, cx: &mut Context<Self>) {
        let Some(server) = self.server() else {
            return;
        };
        let Some(position) = self.position_params(self.primary().head) else {
            return;
        };
        let trigger_kind = if trigger.is_some() {
            CompletionTriggerKind::TRIGGER_CHARACTER
        } else if self.completion.as_ref().is_some_and(|menu| menu.incomplete) {
            CompletionTriggerKind::TRIGGER_FOR_INCOMPLETE_COMPLETIONS
        } else {
            CompletionTriggerKind::INVOKED
        };
        let params = CompletionParams {
            text_document_position: position,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: Some(CompletionContext {
                trigger_kind,
                trigger_character: trigger,
            }),
        };
        self.completion_generation += 1;
        let generation = self.completion_generation;
        let requested_at = self.primary().head;
        let request = server.connection.request::<Completion>(params);
        self._completion_task = Some(cx.spawn(async move |this, cx| {
            let response = request.await;
            this.update(cx, |this, cx| {
                if this.completion_generation != generation {
                    return;
                }
                let (items, incomplete) = match response {
                    Ok(Some(CompletionResponse::Array(items))) => (items, false),
                    Ok(Some(CompletionResponse::List(list))) => (list.items, list.is_incomplete),
                    Ok(None) | Err(_) => return,
                };
                let head = this.primary().head;
                let word_start = complete::word_start(&this.buffer, head);
                let same_line = this.buffer.line_of(head) == this.buffer.line_of(requested_at);
                if items.is_empty() || !same_line || head < word_start.min(requested_at) {
                    return;
                }
                this.completion = Some(CompletionMenu {
                    word_start,
                    items,
                    candidates: Vec::new(),
                    selected: 0,
                    incomplete,
                    scroll: UniformListScrollHandle::new(),
                    server,
                });
                this.refilter_completions(cx);
            })
            .ok();
        }));
    }

    /// Re-validates the open menu after an edit or cursor move: closes it
    /// when the cursor left the word, otherwise refilters.
    pub(super) fn update_completion_menu(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = &self.completion else {
            return;
        };
        let primary = self.primary();
        let still_in_word = primary.is_empty()
            && primary.head >= menu.word_start
            && self.buffer.line_of(primary.head) == self.buffer.line_of(menu.word_start)
            && self
                .buffer
                .slice(menu.word_start..primary.head)
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_');
        if !still_in_word {
            self.close_menu(cx);
        } else if menu.incomplete {
            self.request_completions(None, cx);
        } else {
            self.refilter_completions(cx);
        }
    }

    fn refilter_completions(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = &mut self.completion else {
            return;
        };
        let query = self
            .buffer
            .slice(menu.word_start..self.selections.primary().head);
        menu.candidates = complete::filter(&menu.items, &query);
        // Nothing left, or the only match is exactly what was typed.
        let exhausted = menu.candidates.is_empty()
            || (menu.candidates.len() == 1 && menu.items[menu.candidates[0].index].label == query);
        if exhausted {
            return self.close_menu(cx);
        }
        menu.selected = 0;
        menu.scroll.scroll_to_item(0, ScrollStrategy::Top);
        self.resolve_selected(cx);
        cx.notify();
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        self.completion = None;
        self._resolve_task = None;
        self.completion_generation += 1;
        cx.notify();
    }

    pub(super) fn close_completions(
        &mut self,
        _: &CloseCompletions,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_menu(cx);
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(menu) = &mut self.completion else {
            return;
        };
        let len = menu.candidates.len() as isize;
        menu.selected = (menu.selected as isize + delta).rem_euclid(len) as usize;
        menu.scroll
            .scroll_to_item(menu.selected, ScrollStrategy::Center);
        self.resolve_selected(cx);
        cx.notify();
    }

    pub(super) fn select_prev_completion(
        &mut self,
        _: &SelectPrevCompletion,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_selection(-1, cx);
    }

    pub(super) fn select_next_completion(
        &mut self,
        _: &SelectNextCompletion,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_selection(1, cx);
    }

    /// Fetches documentation for the selected item, if the server defers it.
    fn resolve_selected(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = &self.completion else {
            return;
        };
        let Some(candidate) = menu.candidates.get(menu.selected) else {
            return;
        };
        let item = &menu.items[candidate.index];
        let resolvable = menu
            .server
            .capabilities
            .completion_provider
            .as_ref()
            .and_then(|provider| provider.resolve_provider)
            == Some(true);
        if item.documentation.is_some() || !resolvable {
            self._resolve_task = None;
            return;
        }
        let index = candidate.index;
        let request = menu
            .server
            .connection
            .request::<ResolveCompletionItem>(item.clone());
        let label = item.label.clone();
        self._resolve_task = Some(cx.spawn(async move |this, cx| {
            let Ok(resolved) = request.await else {
                return;
            };
            this.update(cx, |this, cx| {
                if let Some(menu) = &mut this.completion
                    && menu
                        .items
                        .get(index)
                        .is_some_and(|item| item.label == label)
                {
                    menu.items[index] = resolved;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    pub(super) fn confirm_completion(
        &mut self,
        _: &ConfirmCompletion,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.accept_completion(None, cx);
    }

    /// Accepts the selected (or `index`-th) candidate. Auto-import edits may
    /// only arrive via `completionItem/resolve`, so unresolved items are
    /// resolved first.
    fn accept_completion(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        let Some(menu) = self.completion.take() else {
            return;
        };
        self.completion_generation += 1;
        self._resolve_task = None;
        cx.notify();
        let Some(candidate) = menu.candidates.get(index.unwrap_or(menu.selected)) else {
            return;
        };
        let item = menu.items[candidate.index].clone();
        let resolvable = menu
            .server
            .capabilities
            .completion_provider
            .as_ref()
            .and_then(|provider| provider.resolve_provider)
            == Some(true);
        let (word_start, encoding) = (menu.word_start, menu.server.encoding);
        if item.additional_text_edits.is_some() || !resolvable {
            return self.apply_completion(&item, word_start, encoding, cx);
        }
        let request = menu
            .server
            .connection
            .request::<ResolveCompletionItem>(item.clone());
        self._completion_task = Some(cx.spawn(async move |this, cx| {
            let resolved = request.await.unwrap_or(item);
            this.update(cx, |this, cx| {
                this.apply_completion(&resolved, word_start, encoding, cx)
            })
            .ok();
        }));
    }

    fn apply_completion(
        &mut self,
        item: &CompletionItem,
        word_start: usize,
        encoding: Encoding,
        cx: &mut Context<Self>,
    ) {
        // The cursor may have moved off the word while resolving.
        let head = self.primary().head;
        if head < word_start || self.buffer.line_of(head) != self.buffer.line_of(word_start) {
            return;
        }
        let accepted = complete::accept(&self.buffer, item, word_start, head, encoding);
        // Accepted at the primary cursor; other cursors keep their text.
        let mut edits = accepted.edits;
        let mut cursor = accepted.cursor;
        // A snippet expands in the main edit (the first); its tab stops
        // start where that edit's text lands.
        let snippet = (item.insert_text_format == Some(InsertTextFormat::SNIPPET)).then(|| {
            let source = std::mem::take(&mut edits[0].1);
            let snippet = self.expand_snippet(&source, edits[0].0.start);
            let base = cursor.saturating_sub(source.len());
            edits[0].1.clone_from(&snippet.text);
            cursor = base + snippet.text.len();
            (snippet, base)
        });
        edits.sort_by_key(|(range, _)| range.start);
        let primary = self.selections.primary_index();
        let others: Vec<Selection> = self.selections.all().to_vec();
        let mapped: Vec<Selection> = others
            .iter()
            .enumerate()
            .map(|(i, s)| {
                if i == primary {
                    Selection::cursor(cursor)
                } else {
                    Selection::new(
                        lines::map_through(&edits, s.anchor, Bias::Right),
                        lines::map_through(&edits, s.head, Bias::Right),
                    )
                }
            })
            .collect();
        let transaction = self.buffer.transaction_id();
        let before = self.selections.clone();
        let was_dirty = self.buffer.is_dirty();
        if self.buffer.edit_batch(edits).is_ok() {
            self.selections = SelectionSet::new(mapped, primary).clamped(&self.buffer);
            self.record_history(transaction, before);
            self.autoscroll = true;
            self.finish_change(was_dirty, cx);
            if let Some((snippet, base)) = snippet {
                let others = self
                    .selections
                    .all()
                    .iter()
                    .enumerate()
                    .filter(|&(i, _)| i != self.selections.primary_index())
                    .map(|(_, s)| *s)
                    .collect();
                self.start_snippet(snippet, base, others, cx);
            }
            // Accepting must not reopen the menu on the inserted word.
            self.close_menu(cx);
        }
    }

    pub(super) fn render_completions(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let menu = self.completion.as_ref()?;
        let layout = self.layout.as_ref()?;
        let (word_x, line_top) = self.screen_position(menu.word_start)?;
        let bounds = layout.bounds;
        // Align the label column (after the kind badge) with the word.
        let left = (word_x - bounds.left() - px(30.)).max(px(0.));
        let below = line_top < bounds.size.height * 0.6;
        let rows = menu.candidates.len().min(MAX_ROWS);

        let selected_item = menu
            .candidates
            .get(menu.selected)
            .map(|c| &menu.items[c.index]);
        let docs = selected_item
            .map(completion_docs)
            .filter(|docs| !docs.is_empty());

        let list = div()
            .w(px(420.))
            .h(px(ROW_HEIGHT * rows as f32 + 2.))
            .bg(theme::panel())
            .border_1()
            .border_color(theme::border())
            .rounded_md()
            .shadow_md()
            .child(
                uniform_list(
                    "completions",
                    menu.candidates.len(),
                    cx.processor(|this, range: Range<usize>, _, cx| {
                        let language = this.language();
                        let Some(menu) = &this.completion else {
                            return Vec::new();
                        };
                        range
                            .map(|row| completion_row(menu, language, row, cx).into_any_element())
                            .collect()
                    }),
                )
                .track_scroll(menu.scroll.clone())
                .size_full(),
            );
        let docs_panel = docs.map(|docs| {
            div()
                .id("completion-docs")
                .w(px(420.))
                .max_h(px(280.))
                .overflow_y_scroll()
                .px_2()
                .py_1()
                .flex()
                .flex_col()
                .gap_1()
                .bg(theme::panel())
                .border_1()
                .border_color(theme::border())
                .rounded_md()
                .shadow_md()
                .text_sm()
                .children(super::navigation::markdown_blocks("doc", &docs).collect::<Vec<_>>())
        });
        Some(
            div()
                .absolute()
                .left(left)
                .when(below, |menu| menu.top(line_top + layout.line_height))
                .when(!below, |menu| menu.bottom(bounds.size.height - line_top))
                .flex()
                .items_start()
                .gap_1()
                .cursor_default()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_move(|_, _, cx| cx.stop_propagation())
                .child(list)
                .children(docs_panel),
        )
    }
}

fn completion_row(
    menu: &CompletionMenu,
    language: Option<Language>,
    row: usize,
    cx: &mut Context<Editor>,
) -> impl IntoElement + use<> {
    let candidate = &menu.candidates[row];
    let item = &menu.items[candidate.index];
    let (badge, badge_color) = kind_badge(item.kind, language);
    let highlight = HighlightStyle {
        color: Some(theme::accent_text()),
        font_weight: Some(FontWeight::BOLD),
        ..Default::default()
    };
    let highlights: Vec<_> = candidate
        .positions
        .iter()
        .map(|&pos| {
            let len = item.label[pos..].chars().next().map_or(1, char::len_utf8);
            (pos..pos + len, highlight)
        })
        .collect();
    let detail = item
        .label_details
        .as_ref()
        .and_then(|details| {
            details
                .description
                .clone()
                .or_else(|| details.detail.clone())
        })
        .or_else(|| item.detail.clone())
        .map(|detail| detail.lines().next().unwrap_or_default().to_owned());
    let deprecated = item.deprecated == Some(true)
        || item
            .tags
            .as_ref()
            .is_some_and(|tags| tags.contains(&lsp::types::CompletionItemTag::DEPRECATED));
    // Deprecated items: the theme's `deprecated` modifier over muted text.
    let label_style = if deprecated {
        let muted = SyntaxStyle {
            color: Some(theme::muted()),
            ..SyntaxStyle::default()
        };
        theme::current()
            .modifier_style("deprecated")
            .map_or(muted, |modifier| modifier.over(muted))
    } else {
        SyntaxStyle::default()
    };

    div()
        .id(row)
        .h(px(ROW_HEIGHT))
        .w_full()
        .px_2()
        .flex()
        .items_center()
        .gap_2()
        .overflow_hidden()
        .when(row == menu.selected, |row| row.bg(theme::active_row()))
        .when(row != menu.selected, |row| {
            row.hover(|style| style.bg(theme::hover_row()))
        })
        .on_click(cx.listener(move |this, _, _, cx| this.accept_completion(Some(row), cx)))
        .child(
            div()
                .w(px(16.))
                .flex_none()
                .text_xs()
                .text_color(badge_color)
                .child(badge),
        )
        .child(
            div()
                .flex_none()
                .text_color(label_style.color.unwrap_or_else(theme::text))
                .when_some(label_style.background, |label, background| {
                    label.bg(background)
                })
                .when(label_style.bold == Some(true), |label| {
                    label.font_weight(FontWeight::BOLD)
                })
                .when(label_style.italic == Some(true), |label| label.italic())
                .when(label_style.underline == Some(true), |label| {
                    label.underline()
                })
                .when(label_style.strikethrough == Some(true), |label| {
                    label.line_through()
                })
                .child(StyledText::new(item.label.clone()).with_highlights(highlights)),
        )
        .child(div().flex_1())
        .children(detail.map(|detail| {
            div()
                .flex_shrink()
                .overflow_hidden()
                .text_xs()
                .text_color(theme::muted())
                .child(detail)
        }))
}

/// Documentation for the docs pane: the detail line as code, then docs.
fn completion_docs(item: &CompletionItem) -> Vec<language::markdown::Block> {
    let mut blocks = Vec::new();
    if let Some(detail) = item.detail.as_ref().filter(|detail| !detail.is_empty()) {
        blocks.push(language::markdown::Block::Code {
            language: None,
            text: detail.clone(),
        });
    }
    match &item.documentation {
        Some(Documentation::String(text)) => {
            blocks.push(language::markdown::Block::Paragraph(text.clone()))
        }
        Some(Documentation::MarkupContent(markup)) => {
            blocks.extend(language::markdown::parse(&markup.value))
        }
        None => {}
    }
    blocks
}

/// A one-letter kind badge and its color: the theme's color for the
/// matching highlight scope in `language`.
fn kind_badge(
    kind: Option<CompletionItemKind>,
    language: Option<Language>,
) -> (&'static str, Hsla) {
    let scope_color = |scope| {
        let theme = theme::current();
        syntax::Highlight::from_scope(scope).map_or(theme.ui.text, |highlight| {
            theme.syntax_color(language, highlight)
        })
    };
    match kind {
        Some(
            CompletionItemKind::FUNCTION
            | CompletionItemKind::METHOD
            | CompletionItemKind::CONSTRUCTOR,
        ) => ("ƒ", scope_color("function")),
        Some(CompletionItemKind::VARIABLE) => ("v", theme::accent_text()),
        Some(CompletionItemKind::FIELD | CompletionItemKind::PROPERTY) => {
            ("p", scope_color("property"))
        }
        Some(
            CompletionItemKind::CLASS
            | CompletionItemKind::STRUCT
            | CompletionItemKind::INTERFACE
            | CompletionItemKind::TYPE_PARAMETER,
        ) => ("C", scope_color("type")),
        Some(CompletionItemKind::ENUM | CompletionItemKind::ENUM_MEMBER) => {
            ("E", scope_color("type"))
        }
        Some(CompletionItemKind::MODULE) => ("M", scope_color("constant")),
        Some(CompletionItemKind::KEYWORD) => ("k", scope_color("keyword")),
        Some(CompletionItemKind::CONSTANT) => ("c", scope_color("constant")),
        Some(CompletionItemKind::SNIPPET) => ("s", theme::muted()),
        _ => ("·", theme::muted()),
    }
}

/// The badge of a document/workspace/call-hierarchy symbol: the same letters
/// and colors as the completion kinds.
pub(crate) fn symbol_kind_badge(
    kind: SymbolKind,
    language: Option<Language>,
) -> (&'static str, Hsla) {
    let completion_kind = match kind {
        SymbolKind::FUNCTION => CompletionItemKind::FUNCTION,
        SymbolKind::METHOD => CompletionItemKind::METHOD,
        SymbolKind::CONSTRUCTOR => CompletionItemKind::CONSTRUCTOR,
        SymbolKind::VARIABLE => CompletionItemKind::VARIABLE,
        SymbolKind::FIELD => CompletionItemKind::FIELD,
        SymbolKind::PROPERTY => CompletionItemKind::PROPERTY,
        SymbolKind::CLASS => CompletionItemKind::CLASS,
        SymbolKind::STRUCT => CompletionItemKind::STRUCT,
        SymbolKind::INTERFACE => CompletionItemKind::INTERFACE,
        SymbolKind::TYPE_PARAMETER => CompletionItemKind::TYPE_PARAMETER,
        SymbolKind::ENUM => CompletionItemKind::ENUM,
        SymbolKind::ENUM_MEMBER => CompletionItemKind::ENUM_MEMBER,
        SymbolKind::MODULE | SymbolKind::NAMESPACE | SymbolKind::PACKAGE => {
            CompletionItemKind::MODULE
        }
        SymbolKind::CONSTANT => CompletionItemKind::CONSTANT,
        _ => return ("·", theme::muted()),
    };
    kind_badge(Some(completion_kind), language)
}
