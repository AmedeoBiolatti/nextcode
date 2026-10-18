//! Hover popover, go to definition/declaration/type definition/
//! implementation, find references, and the Ctrl+hover link.

use language::markdown::{self, Block};
use lsp::types::{
    DeclarationCapability, GotoDefinitionParams, HoverParams, HoverProviderCapability,
    ImplementationProviderCapability, OneOf, ReferenceContext, ReferenceParams, ServerCapabilities,
    TextDocumentIdentifier, TextDocumentPositionParams, TypeDefinitionProviderCapability,
    request::{
        GotoDeclaration, GotoDefinition, GotoImplementation, GotoTypeDefinition, HoverRequest,
        References,
    },
};

use super::*;
use crate::lsp_store::{CONTENT_MODIFIED, request_retrying};

actions!(
    editor,
    [
        GoToDefinition,
        GoToDeclaration,
        GoToTypeDefinition,
        GoToImplementation,
        FindReferences
    ]
);

/// How long the mouse must rest before the hover popover appears.
const HOVER_DELAY: Duration = Duration::from_millis(350);
/// Wait before retrying a request the server answered "content modified".
const RETRY_DELAY: Duration = Duration::from_millis(400);

/// What a go-to command looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Goto {
    Definition,
    Declaration,
    TypeDefinition,
    Implementation,
}

impl Goto {
    fn noun(self) -> &'static str {
        match self {
            Self::Definition => "definition",
            Self::Declaration => "declaration",
            Self::TypeDefinition => "type definition",
            Self::Implementation => "implementation",
        }
    }

    /// The locations picker's title.
    fn title(self) -> &'static str {
        match self {
            Self::Definition => "Definitions",
            Self::Declaration => "Declarations",
            Self::TypeDefinition => "Type Definitions",
            Self::Implementation => "Implementations",
        }
    }

    fn supported(self, capabilities: &ServerCapabilities) -> bool {
        match self {
            Self::Definition => matches!(
                capabilities.definition_provider,
                Some(OneOf::Left(true) | OneOf::Right(_))
            ),
            Self::Declaration => matches!(
                capabilities.declaration_provider,
                Some(
                    DeclarationCapability::Simple(true)
                        | DeclarationCapability::RegistrationOptions(_)
                        | DeclarationCapability::Options(_)
                )
            ),
            Self::TypeDefinition => matches!(
                capabilities.type_definition_provider,
                Some(
                    TypeDefinitionProviderCapability::Simple(true)
                        | TypeDefinitionProviderCapability::Options(_)
                )
            ),
            Self::Implementation => matches!(
                capabilities.implementation_provider,
                Some(
                    ImplementationProviderCapability::Simple(true)
                        | ImplementationProviderCapability::Options(_)
                )
            ),
        }
    }
}

impl Editor {
    pub(crate) fn server(&self) -> Option<Arc<RunningServer>> {
        self.lsp.as_ref().map(|doc| doc.server.clone())
    }

    pub(super) fn position_params(&self, offset: usize) -> Option<TextDocumentPositionParams> {
        let doc = self.lsp.as_ref()?;
        Some(TextDocumentPositionParams::new(
            TextDocumentIdentifier::new(doc.uri.clone()),
            self.server_position(offset, doc.server.encoding),
        ))
    }

    pub(crate) fn show_notice(&mut self, notice: impl Into<SharedString>, cx: &mut Context<Self>) {
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
        self.reveal_selections();
        self.auto_closed.clear();
        self.completion = None;
        self.dismiss_signature_help();
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
        self.request_goto(Goto::Definition, self.primary().head, cx);
    }

    pub(super) fn go_to_declaration(
        &mut self,
        _: &GoToDeclaration,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_goto(Goto::Declaration, self.primary().head, cx);
    }

    pub(super) fn go_to_type_definition(
        &mut self,
        _: &GoToTypeDefinition,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_goto(Goto::TypeDefinition, self.primary().head, cx);
    }

    pub(super) fn go_to_implementation(
        &mut self,
        _: &GoToImplementation,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_goto(Goto::Implementation, self.primary().head, cx);
    }

    pub(super) fn request_definition(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.request_goto(Goto::Definition, offset, cx);
    }

    /// Asks for `goto`'s targets at `offset`: one jumps, several open the
    /// locations picker.
    pub(super) fn request_goto(&mut self, goto: Goto, offset: usize, cx: &mut Context<Self>) {
        let Some(server) = self.server() else {
            return self.show_notice("No language server for this file", cx);
        };
        if !goto.supported(&server.capabilities) {
            return self.show_notice(
                format!("The language server cannot go to {}s", goto.noun()),
                cx,
            );
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
            let executor = cx.background_executor().clone();
            let (server, retries) = (&server, 1);
            let result = match goto {
                Goto::Definition => {
                    request_retrying::<GotoDefinition>(
                        server,
                        params,
                        &executor,
                        retries,
                        RETRY_DELAY,
                    )
                    .await
                }
                Goto::Declaration => {
                    request_retrying::<GotoDeclaration>(
                        server,
                        params,
                        &executor,
                        retries,
                        RETRY_DELAY,
                    )
                    .await
                }
                Goto::TypeDefinition => {
                    request_retrying::<GotoTypeDefinition>(
                        server,
                        params,
                        &executor,
                        retries,
                        RETRY_DELAY,
                    )
                    .await
                }
                Goto::Implementation => {
                    request_retrying::<GotoImplementation>(
                        server,
                        params,
                        &executor,
                        retries,
                        RETRY_DELAY,
                    )
                    .await
                }
            };
            this.update(cx, |this, cx| {
                let locations = match result {
                    Ok(response) => response
                        .map(language::definition_locations)
                        .unwrap_or_default(),
                    Err(err) => {
                        return this
                            .show_notice(format!("Go to {} failed: {err}", goto.noun()), cx);
                    }
                };
                if locations.is_empty() {
                    this.show_notice(format!("No {} found", goto.noun()), cx);
                } else {
                    cx.emit(EditorEvent::Navigate {
                        locations,
                        title: goto.title().into(),
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

    /// While Ctrl (Cmd on macOS) is held, underlines the identifier under
    /// the mouse as a link to its definition (Ctrl+click follows it).
    pub(super) fn update_link(
        &mut self,
        position: Option<Point<Pixels>>,
        held: bool,
        cx: &mut Context<Self>,
    ) {
        let can_follow = held
            && !self.single_line
            && self
                .server()
                .is_some_and(|server| Goto::Definition.supported(&server.capabilities));
        let link = position
            .filter(|_| can_follow)
            .and_then(|position| self.character_at(position))
            .map(|(_, offset)| self.word_at(offset))
            .filter(|word| {
                self.buffer
                    .slice(word.clone())
                    .chars()
                    .next()
                    .is_some_and(is_word_char)
            });
        if link != self.link {
            self.link = link;
            cx.notify();
        }
    }

    /// After the mouse rests on a word, shows its diagnostics and the
    /// server's hover documentation.
    pub(super) fn update_hover(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some((row, offset)) = self.character_at(position) else {
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
                let message = match crate::lsp_store::diagnostic_origin(&d.lsp) {
                    Some(origin) => format!("{} ({origin})", d.message).into(),
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
        // While a debug session is paused: the value under the mouse.
        let debug_value = self
            .debug
            .evaluator
            .clone()
            .zip(self.hover_expression(&word));
        // Under the word's start, so the pointer can move straight down
        // from anywhere on the word into the popover.
        let x = self
            .screen_position(word.start)
            .map(|(x, _)| x)
            .filter(|&x| x <= position.x)
            .unwrap_or(position.x);
        if !messages.is_empty() || hover_request.is_some() || debug_value.is_some() {
            self._hover_delay = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(HOVER_DELAY).await;
                let value = match debug_value {
                    Some((evaluate, expression)) => {
                        match cx.update(|cx| evaluate(expression, cx)) {
                            Ok(task) => task.await,
                            Err(_) => None,
                        }
                    }
                    None => None,
                };
                let mut docs = match hover_request {
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
                if let Some(value) = value {
                    if !docs.is_empty() {
                        docs.insert(0, Block::Rule);
                    }
                    docs.insert(
                        0,
                        Block::Code {
                            language: None,
                            text: value,
                        },
                    );
                }
                this.update(cx, |this, cx| {
                    if (messages.is_empty() && docs.is_empty())
                        || this.hover_word.as_ref() != Some(&word)
                    {
                        return;
                    }
                    this.hover = Some(HoverPopover {
                        row,
                        x,
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
        let line_top = layout.line_height * hover.row as f32 - self.scroll.y;
        let below = line_top < bounds.size.height * 0.6;
        let diagnostics = hover
            .messages
            .iter()
            .enumerate()
            .map(|(index, (severity, message))| {
                div()
                    .flex()
                    .gap_2()
                    .font_family(theme::UI_FONT)
                    .child(div().text_color(severity.color()).child(severity.label()))
                    .child(selectable(("diagnostic", index), message.clone()))
                    .pin(format!("diagnostic:{}", index + 1))
                    .label(message.clone())
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
                // Moving over the popover must not re-target the hover, and
                // pressing in it (to select text) must not close it.
                .on_mouse_move(|_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .children(diagnostics)
                .children(separator)
                .children(markdown_blocks("doc", &hover.docs))
                .pin("hover")
                .label("Hover"),
        )
    }
}

/// Renders parsed Markdown: code in the editor font, prose in the UI font;
/// selectable, with ids `(id, index)`.
pub(super) fn markdown_blocks<'a>(
    id: &'static str,
    blocks: &'a [Block],
) -> impl Iterator<Item = AnyElement> + 'a {
    blocks
        .iter()
        .enumerate()
        .map(move |(index, block)| match block {
            Block::Code { text, .. } => div()
                .font_family(theme::MONO_FONT)
                .text_color(theme::text())
                .child(selectable((id, index), text.clone()))
                .into_any_element(),
            Block::Paragraph(text) => div()
                .font_family(theme::UI_FONT)
                .text_color(theme::text())
                .child(selectable((id, index), text.clone()))
                .into_any_element(),
            Block::Rule => div().h(px(1.)).bg(theme::border()).into_any_element(),
        })
}

pub(super) fn is_content_modified<T>(result: &Result<T, lsp::Error>) -> bool {
    matches!(result, Err(lsp::Error::Response { code, .. }) if *code == CONTENT_MODIFIED)
}
