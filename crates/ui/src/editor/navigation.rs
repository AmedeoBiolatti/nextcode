//! Hover popover, go-to-definition and find-references.

use language::markdown::{self, Block};
use lsp::types::{
    GotoDefinitionParams, HoverParams, HoverProviderCapability, OneOf, ReferenceContext,
    ReferenceParams, TextDocumentIdentifier, TextDocumentPositionParams,
    request::{GotoDefinition, HoverRequest, References},
};

use super::*;

actions!(editor, [GoToDefinition, FindReferences]);

/// How long the mouse must rest before the hover popover appears.
const HOVER_DELAY: Duration = Duration::from_millis(350);
/// rust-analyzer answers "content modified" while it is still indexing.
const CONTENT_MODIFIED: i64 = -32801;

impl Editor {
    pub(super) fn server(&self) -> Option<Arc<RunningServer>> {
        self.lsp.as_ref().map(|doc| doc.server.clone())
    }

    pub(super) fn position_params(&self, offset: usize) -> Option<TextDocumentPositionParams> {
        let doc = self.lsp.as_ref()?;
        Some(TextDocumentPositionParams::new(
            TextDocumentIdentifier::new(doc.uri.clone()),
            language::to_position(&self.buffer, offset, doc.server.encoding),
        ))
    }

    fn show_notice(&mut self, notice: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.notice = Some(notice.into());
        cx.notify();
    }

    /// The primary cursor.
    pub fn cursor(&self) -> usize {
        self.primary().head
    }

    /// Moves the cursor to `offset` (clamped to a character boundary) and
    /// centers it.
    pub fn set_cursor(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = offset.min(self.buffer.len());
        let offset = self
            .buffer
            .offset_of(self.buffer.point(offset), Encoding::Utf8);
        self.selections = SelectionSet::single(Selection::cursor(offset));
        self.auto_closed.clear();
        self.completion = None;
        self.center_cursor = true;
        cx.notify();
    }

    /// Moves to `line` and UTF-8 byte `column` (e.g. a search match).
    pub fn go_to_line(&mut self, line: usize, column: usize, cx: &mut Context<Self>) {
        let offset = self
            .buffer
            .offset_of(text::Point { row: line, column }, Encoding::Utf8);
        self.set_cursor(offset, cx);
    }

    /// Moves to a server position (in this document's server encoding).
    pub fn go_to(&mut self, position: lsp::types::Position, cx: &mut Context<Self>) {
        let encoding = self
            .lsp
            .as_ref()
            .map_or(Encoding::Utf16, |doc| doc.server.encoding);
        let offset = language::to_offset(&self.buffer, position, encoding);
        self.set_cursor(offset, cx);
    }

    pub(super) fn go_to_definition(
        &mut self,
        _: &GoToDefinition,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_definition(self.primary().head, cx);
    }

    pub(super) fn request_definition(&mut self, offset: usize, cx: &mut Context<Self>) {
        let Some(server) = self.server() else {
            return self.show_notice("No language server for this file", cx);
        };
        if !matches!(
            server.capabilities.definition_provider,
            Some(OneOf::Left(true) | OneOf::Right(_))
        ) {
            return self.show_notice("The language server cannot go to definitions", cx);
        }
        let Some(position) = self.position_params(offset) else {
            return;
        };
        let params = GotoDefinitionParams {
            text_document_position_params: position,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        self._navigation_task = Some(cx.spawn(async move |this, cx| {
            let mut result = server
                .connection
                .request::<GotoDefinition>(params.clone())
                .await;
            if is_content_modified(&result) {
                cx.background_executor()
                    .timer(Duration::from_millis(400))
                    .await;
                result = server.connection.request::<GotoDefinition>(params).await;
            }
            this.update(cx, |this, cx| {
                let locations = match result {
                    Ok(response) => response
                        .map(language::definition_locations)
                        .unwrap_or_default(),
                    Err(err) => {
                        return this.show_notice(format!("Go to definition failed: {err}"), cx);
                    }
                };
                if locations.is_empty() {
                    this.show_notice("No definition found", cx);
                } else {
                    cx.emit(EditorEvent::Navigate {
                        locations,
                        title: "Definitions".into(),
                    });
                }
            })
            .ok();
        }));
    }

    pub(super) fn find_references(
        &mut self,
        _: &FindReferences,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(server) = self.server() else {
            return self.show_notice("No language server for this file", cx);
        };
        if !matches!(
            server.capabilities.references_provider,
            Some(OneOf::Left(true) | OneOf::Right(_))
        ) {
            return self.show_notice("The language server cannot find references", cx);
        }
        let Some(position) = self.position_params(self.primary().head) else {
            return;
        };
        let params = ReferenceParams {
            text_document_position: position,
            context: ReferenceContext {
                include_declaration: true,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        self._navigation_task = Some(cx.spawn(async move |this, cx| {
            let mut result = server
                .connection
                .request::<References>(params.clone())
                .await;
            if is_content_modified(&result) {
                cx.background_executor()
                    .timer(Duration::from_millis(400))
                    .await;
                result = server.connection.request::<References>(params).await;
            }
            this.update(cx, |this, cx| {
                let locations: Vec<language::Location> = match result {
                    Ok(found) => found
                        .unwrap_or_default()
                        .iter()
                        .filter_map(language::Location::from_lsp)
                        .collect(),
                    Err(err) => {
                        return this.show_notice(format!("Find references failed: {err}"), cx);
                    }
                };
                if locations.is_empty() {
                    this.show_notice("No references found", cx);
                } else {
                    cx.emit(EditorEvent::Navigate {
                        locations,
                        title: "References".into(),
                    });
                }
            })
            .ok();
        }));
    }

    /// The identifier at `offset`, or the single character there.
    fn word_at(&self, offset: usize) -> Range<usize> {
        let line_end = self.buffer.line_end(self.buffer.line_of(offset));
        let rest = self.buffer.slice(offset..line_end);
        let word_len: usize = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .map(char::len_utf8)
            .sum();
        if word_len == 0 {
            return offset..self.buffer.next_grapheme(offset);
        }
        let end = offset + word_len;
        language::completion::word_start(&self.buffer, end)..end
    }

    /// After the mouse rests on a word, shows its diagnostics and the
    /// server's hover documentation.
    pub(super) fn update_hover(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some((line, offset)) = self.character_at(position) else {
            if self.hover_word.take().is_some() {
                self.hover = None;
                self._hover_delay = None;
                cx.notify();
            }
            return;
        };
        let word = self.word_at(offset);
        if self.hover_word.as_ref() == Some(&word) {
            return;
        }
        let had_hover = self.hover.take().is_some();
        self.hover_word = Some(word.clone());
        self._hover_delay = None;

        let messages: Vec<(Severity, SharedString)> = self
            .diagnostics
            .iter()
            .filter(|d| d.range.start < word.end && word.start < d.range.end)
            .map(|d| {
                let message = match &d.source {
                    Some(source) => format!("{} ({source})", d.message).into(),
                    None => d.message.clone(),
                };
                (d.severity, message)
            })
            .collect();
        let hover_request = self
            .server()
            .filter(|server| {
                matches!(
                    server.capabilities.hover_provider,
                    Some(
                        HoverProviderCapability::Simple(true) | HoverProviderCapability::Options(_)
                    )
                )
            })
            .zip(self.position_params(offset));
        if !messages.is_empty() || hover_request.is_some() {
            self._hover_delay = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(HOVER_DELAY).await;
                let docs = match hover_request {
                    Some((server, position)) => server
                        .connection
                        .request::<HoverRequest>(HoverParams {
                            text_document_position_params: position,
                            work_done_progress_params: Default::default(),
                        })
                        .await
                        .ok()
                        .flatten()
                        .map(|hover| markdown::parse(&language::hover_markdown(hover)))
                        .unwrap_or_default(),
                    None => Vec::new(),
                };
                this.update(cx, |this, cx| {
                    if (messages.is_empty() && docs.is_empty())
                        || this.hover_word.as_ref() != Some(&word)
                    {
                        return;
                    }
                    this.hover = Some(HoverPopover {
                        line,
                        x: position.x,
                        messages,
                        docs,
                    });
                    cx.notify();
                })
                .ok();
            }));
        }
        if had_hover {
            cx.notify();
        }
    }

    /// The hover popover, positioned in the text area below (or, near the
    /// bottom, above) the hovered line.
    pub(super) fn render_hover(&self) -> Option<impl IntoElement + use<>> {
        let hover = self.hover.as_ref()?;
        let layout = self.layout.as_ref()?;
        let bounds = layout.bounds;
        let line_top = layout.line_height * hover.line as f32 - self.scroll.y;
        let below = line_top < bounds.size.height * 0.6;
        let diagnostics = hover.messages.iter().map(|(severity, message)| {
            div()
                .flex()
                .gap_2()
                .font_family(theme::UI_FONT)
                .child(div().text_color(severity.color()).child(severity.label()))
                .child(message.clone())
                .into_any_element()
        });
        let separator = (!hover.messages.is_empty() && !hover.docs.is_empty())
            .then(|| div().h(px(1.)).bg(theme::border()).into_any_element());
        Some(
            div()
                .id("hover-popover")
                .absolute()
                .left((hover.x - bounds.left()).max(px(0.)))
                .when(below, |popover| popover.top(line_top + layout.line_height))
                .when(!below, |popover| {
                    popover.bottom(bounds.size.height - line_top)
                })
                .max_w(px(640.))
                .max_h(px(360.))
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
                .cursor_default()
                // Moving over the popover must not re-target the hover.
                .on_mouse_move(|_, _, cx| cx.stop_propagation())
                .children(diagnostics)
                .children(separator)
                .children(markdown_blocks(&hover.docs)),
        )
    }
}

/// Renders parsed Markdown: code in the editor font, prose in the UI font.
pub(super) fn markdown_blocks(blocks: &[Block]) -> impl Iterator<Item = AnyElement> + '_ {
    blocks.iter().map(|block| match block {
        Block::Code { text, .. } => div()
            .font_family(theme::MONO_FONT)
            .text_color(theme::text())
            .child(SharedString::from(text.clone()))
            .into_any_element(),
        Block::Paragraph(text) => div()
            .font_family(theme::UI_FONT)
            .text_color(theme::text())
            .child(SharedString::from(text.clone()))
            .into_any_element(),
        Block::Rule => div().h(px(1.)).bg(theme::border()).into_any_element(),
    })
}

pub(super) fn is_content_modified<T>(result: &Result<T, lsp::Error>) -> bool {
    matches!(result, Err(lsp::Error::Response { code, .. }) if *code == CONTENT_MODIFIED)
}
