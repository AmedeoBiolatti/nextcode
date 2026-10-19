//! Hover popover, go to definition/declaration/type definition/
//! implementation, find references, and the Ctrl+hover link (underlined
//! where the server has a definition; Ctrl+click follows it).

use std::collections::HashMap;

use gpui::WeakEntity;
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
/// Wait after Ctrl+hover reaches a word before asking the server whether
/// it has a definition there: sweeping across words asks only where the
/// mouse rests.
const LINK_DELAY: Duration = Duration::from_millis(60);

/// The Ctrl+hover link: the identifier under the mouse while Ctrl (Cmd on
/// macOS) is held, underlined once the server reports a definition for it.
#[derive(Default)]
pub(super) struct LinkState {
    /// The identifier under the mouse while Ctrl is held.
    hovered: Option<Range<usize>>,
    /// The underlined identifier: `hovered`, once it has a definition.
    pub(super) shown: Option<Range<usize>>,
    /// The server's answers by word for edit `version`: whether it has a
    /// definition there.
    checked: HashMap<Range<usize>, bool>,
    version: u64,
    _check: Option<Task<()>>,
}

/// The editor under the mouse: a Ctrl press or release reaches only the
/// focused editor, which passes it on (another notebook cell or pane).
#[derive(Default)]
struct HoveredEditor(Option<WeakEntity<Editor>>);

impl Global for HoveredEditor {}

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
        // On the definition itself, Go to Definition finds its usages
        // instead, as in VS Code and JetBrains IDEs.
        let here = (goto == Goto::Definition)
            .then(|| self.lsp.as_ref().and_then(|doc| lsp::uri_to_path(&doc.uri)))
            .flatten()
            .map(|path| (path, position.position))
            .zip(self.references_params(offset));
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
            let locations = match result {
                Ok(response) => response
                    .map(language::definition_locations)
                    .unwrap_or_default(),
                Err(err) => {
                    this.update(cx, |this, cx| {
                        this.show_notice(format!("Go to {} failed: {err}", goto.noun()), cx)
                    })
                    .ok();
                    return;
                }
            };
            if let Some(((path, at), references)) = here
                && !locations.is_empty()
                && locations.iter().all(|location| {
                    location.path == path && location.range.start <= at && at <= location.range.end
                })
            {
                let result =
                    request_retrying::<References>(server, references, &executor, 1, RETRY_DELAY)
                        .await;
                this.update(cx, |this, cx| this.show_references(result, cx))
                    .ok();
                return;
            }
            this.update(cx, |this, cx| {
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
        let Some(params) = self.references_params(self.primary().head) else {
            return self.show_notice("The language server cannot find references", cx);
        };
        self._navigation_task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let result =
                request_retrying::<References>(&server, params, &executor, 1, RETRY_DELAY).await;
            this.update(cx, |this, cx| this.show_references(result, cx))
                .ok();
        }));
    }

    /// A references request at `offset`, if the server answers those.
    fn references_params(&self, offset: usize) -> Option<ReferenceParams> {
        let server = self.server()?;
        if !matches!(
            server.capabilities.references_provider,
            Some(OneOf::Left(true) | OneOf::Right(_))
        ) {
            return None;
        }
        Some(ReferenceParams {
            text_document_position: self.position_params(offset)?,
            context: ReferenceContext {
                include_declaration: true,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
    }

    fn show_references(
        &mut self,
        result: Result<Option<Vec<lsp::types::Location>>, lsp::Error>,
        cx: &mut Context<Self>,
    ) {
        let locations: Vec<language::Location> = match result {
            Ok(found) => found
                .unwrap_or_default()
                .iter()
                .filter_map(language::Location::from_lsp)
                .collect(),
            Err(err) => {
                return self.show_notice(format!("Find references failed: {err}"), cx);
            }
        };
        if locations.is_empty() {
            self.show_notice("No references found", cx);
        } else {
            cx.emit(EditorEvent::Navigate {
                locations,
                title: "References".into(),
            });
        }
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
    /// the mouse once the server reports a definition for it: asked after
    /// `LINK_DELAY`, remembered per word until the next edit.
    fn update_link(&mut self, position: Option<Point<Pixels>>, held: bool, cx: &mut Context<Self>) {
        let server = self.server().filter(|server| {
            held && !self.single_line && Goto::Definition.supported(&server.capabilities)
        });
        let word = position
            .filter(|_| server.is_some())
            .and_then(|position| self.character_at(position))
            .map(|(_, offset)| self.word_at(offset))
            .filter(|word| {
                self.buffer
                    .slice(word.clone())
                    .chars()
                    .next()
                    .is_some_and(is_word_char)
            });
        if word == self.link.hovered {
            return;
        }
        if self.link.version != self.version {
            self.link.checked.clear();
            self.link.version = self.version;
        }
        self.link.hovered = word.clone();
        self.link._check = None;
        let known = word
            .as_ref()
            .and_then(|word| self.link.checked.get(word).copied());
        self.show_link(word.clone().filter(|_| known == Some(true)), cx);
        if let (Some(word), Some(server), None) = (word, server, known) {
            self.check_link(word, server, cx);
        }
    }

    /// Asks whether the server has a definition for `word`; underlines it
    /// if it does and the mouse is still on it.
    fn check_link(
        &mut self,
        word: Range<usize>,
        server: Arc<RunningServer>,
        cx: &mut Context<Self>,
    ) {
        let Some(position) = self.position_params(word.start) else {
            return;
        };
        let version = self.version;
        self.link._check = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            executor.timer(LINK_DELAY).await;
            let params = GotoDefinitionParams {
                text_document_position_params: position,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            let Ok(response) =
                request_retrying::<GotoDefinition>(&server, params, &executor, 1, RETRY_DELAY)
                    .await
            else {
                return;
            };
            let found = response
                .is_some_and(|response| !language::definition_locations(response).is_empty());
            this.update(cx, |this, cx| {
                if this.version != version {
                    return;
                }
                this.link.checked.insert(word.clone(), found);
                if found && this.link.hovered.as_ref() == Some(&word) {
                    this.show_link(Some(word), cx);
                }
            })
            .ok();
        }));
    }

    fn show_link(&mut self, link: Option<Range<usize>>, cx: &mut Context<Self>) {
        if link != self.link.shown {
            self.link.shown = link;
            cx.notify();
        }
    }

    /// Drops the Ctrl+hover link (the mouse left, the view scrolled, the
    /// text changed); returns whether one was underlined.
    pub(super) fn clear_link(&mut self) -> bool {
        self.link.hovered = None;
        self.link._check = None;
        self.link.shown.take().is_some()
    }

    /// The mouse moved over this editor's text.
    pub(super) fn link_mouse_moved(
        &mut self,
        position: Point<Pixels>,
        held: bool,
        cx: &mut Context<Self>,
    ) {
        let id = cx.entity_id();
        let hovered = cx
            .try_global::<HoveredEditor>()
            .and_then(|hovered| hovered.0.as_ref())
            .is_some_and(|editor| editor.entity_id() == id);
        if !hovered {
            let editor = cx.weak_entity();
            cx.set_global(HoveredEditor(Some(editor)));
        }
        self.update_link(Some(position), held, cx);
    }

    /// The mouse left this editor.
    pub(super) fn link_mouse_left(&mut self, cx: &mut Context<Self>) {
        let id = cx.entity_id();
        let hovered = cx
            .try_global::<HoveredEditor>()
            .and_then(|hovered| hovered.0.as_ref())
            .is_some_and(|editor| editor.entity_id() == id);
        if hovered {
            cx.set_global(HoveredEditor(None));
        }
        if self.clear_link() {
            cx.notify();
        }
    }

    /// Ctrl went down or up while this editor had focus: the link follows
    /// here and in the editor under the mouse, if that is another one.
    pub(super) fn link_modifiers_changed(&mut self, held: bool, cx: &mut Context<Self>) {
        self.update_link(self.mouse_position, held, cx);
        let id = cx.entity_id();
        let hovered = cx
            .try_global::<HoveredEditor>()
            .and_then(|hovered| hovered.0.as_ref()?.upgrade())
            .filter(|editor| editor.entity_id() != id);
        if let Some(editor) = hovered {
            editor.update(cx, |editor, cx| {
                editor.update_link(editor.mouse_position, held, cx)
            });
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

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        io::{self, BufRead, BufReader, Write},
        rc::Rc,
        thread,
    };

    use gpui::{Modifiers, ModifiersChangedEvent, TestAppContext, WindowHandle};
    use lsp::{LanguageServer, types::TextDocumentSyncKind};
    use serde_json::{Value, json};

    use super::super::tests::{editor, state, update};
    use super::*;

    const SOURCE: &str = "def greet():\n    pass\n\ngreet()\n";
    /// Where the fake server says `greet` is defined.
    const DEFINITION: (u32, u32, u32) = (0, 4, 9);

    fn read_frame(reader: &mut impl BufRead) -> Option<Value> {
        let mut length = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).ok()? == 0 {
                return None;
            }
            let line = line.trim();
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length:") {
                length = value.trim().parse().ok();
            }
        }
        let mut body = vec![0; length?];
        io::Read::read_exact(reader, &mut body).ok()?;
        serde_json::from_slice(&body).ok()
    }

    /// A server knowing one function, `greet` (`DEFINITION`), used on line
    /// 3; it reports definition requests to `asked`.
    fn fake_server(asked: Arc<parking_lot::Mutex<Vec<(u32, u32)>>>) -> Arc<RunningServer> {
        let (client_reads, mut server_writes) = io::pipe().unwrap();
        let (server_reads, client_writes) = io::pipe().unwrap();
        let (connection, incoming) = LanguageServer::new(client_reads, client_writes, None);
        thread::spawn(move || {
            let _incoming = incoming;
            let mut reader = BufReader::new(server_reads);
            while let Some(message) = read_frame(&mut reader) {
                let (Some(id), Some(method)) = (message.get("id"), message["method"].as_str())
                else {
                    continue;
                };
                let params = &message["params"];
                let uri = params["textDocument"]["uri"].clone();
                let line = params["position"]["line"].as_u64().unwrap_or(0) as u32;
                let character = params["position"]["character"].as_u64().unwrap_or(0) as u32;
                let (def_line, start, end) = DEFINITION;
                let location = |line: u32| {
                    json!({
                        "uri": uri,
                        "range": {
                            "start": { "line": line, "character": start },
                            "end": { "line": line, "character": end },
                        },
                    })
                };
                let on_greet = (line == def_line && (start..=end).contains(&character))
                    || (line == 3 && character <= 5);
                let result = match method {
                    "textDocument/definition" => {
                        asked.lock().push((line, character));
                        if on_greet {
                            location(def_line)
                        } else {
                            Value::Null
                        }
                    }
                    "textDocument/references" if on_greet => {
                        json!([location(def_line), {
                            "uri": uri,
                            "range": {
                                "start": { "line": 3, "character": 0 },
                                "end": { "line": 3, "character": 5 },
                            },
                        }])
                    }
                    _ => Value::Null,
                };
                let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string();
                write!(
                    server_writes,
                    "Content-Length: {}\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
                server_writes.flush().unwrap();
            }
        });
        Arc::new(RunningServer {
            connection: Arc::new(connection),
            encoding: Encoding::Utf16,
            sync: TextDocumentSyncKind::FULL,
            save_includes_text: false,
            capabilities: ServerCapabilities {
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                ..ServerCapabilities::default()
            },
            semantic_legend: None,
        })
    }

    /// The `Navigate` events: title and locations.
    type Navigations = Rc<RefCell<Vec<(SharedString, Vec<language::Location>)>>>;

    struct Fixture {
        window: WindowHandle<Editor>,
        navigated: Navigations,
        asked: Arc<parking_lot::Mutex<Vec<(u32, u32)>>>,
    }

    /// `SOURCE` in an editor attached to the fake server, recording where
    /// it navigates.
    fn fixture(cx: &mut TestAppContext) -> Fixture {
        let path = std::env::temp_dir()
            .join("next_code_navigation")
            .join("a.py");
        let window = editor(&path.to_string_lossy(), &format!("|{SOURCE}"), cx);
        let asked = Arc::default();
        let server = fake_server(Arc::clone(&asked));
        update(window, cx, |e, _, cx| e.attach_server(server, cx));
        let navigated = Rc::new(RefCell::new(Vec::new()));
        let sink = navigated.clone();
        cx.update(|cx| {
            cx.subscribe(&window.entity(cx).unwrap(), move |_, event, _| {
                if let EditorEvent::Navigate { locations, title } = event {
                    sink.borrow_mut().push((title.clone(), locations.clone()));
                }
            })
            .detach();
        });
        cx.run_until_parked();
        Fixture {
            window,
            navigated,
            asked,
        }
    }

    /// Runs the app until `done` holds (the server answers on its thread).
    fn wait_for(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) {
        for _ in 0..500 {
            cx.executor().advance_clock(LINK_DELAY);
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out");
    }

    /// The middle of the character at `offset`.
    fn position_of(e: &Editor, offset: usize) -> Point<Pixels> {
        let layout = e.layout.as_ref().expect("laid out");
        let line = e.buffer.line_of(offset);
        let row = &layout.rows[line];
        let x = layout.text_left(row, e.scroll.x) + row.x_for(offset) + layout.em_width * 0.5;
        point(
            x,
            layout.row_top(line, e.scroll.y) + layout.line_height / 2.,
        )
    }

    fn ctrl() -> Modifiers {
        Modifiers::secondary_key()
    }

    fn mouse_down(
        e: &mut Editor,
        offset: usize,
        modifiers: Modifiers,
        w: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        let position = position_of(e, offset);
        e.on_mouse_down(
            &MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers,
                click_count: 1,
                first_mouse: false,
            },
            w,
            cx,
        );
    }

    fn mouse_move(
        e: &mut Editor,
        offset: usize,
        modifiers: Modifiers,
        w: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        let position = position_of(e, offset);
        e.on_mouse_move(
            &MouseMoveEvent {
                position,
                pressed_button: None,
                modifiers,
            },
            w,
            cx,
        );
    }

    fn modifiers_changed(
        e: &mut Editor,
        modifiers: Modifiers,
        w: &mut Window,
        cx: &mut Context<Editor>,
    ) {
        e.on_modifiers_changed(
            &ModifiersChangedEvent {
                modifiers,
                capslock: Default::default(),
            },
            w,
            cx,
        );
    }

    #[gpui::test]
    fn ctrl_click_on_a_use_goes_to_the_definition(cx: &mut TestAppContext) {
        let Fixture {
            window, navigated, ..
        } = fixture(cx);
        let call = SOURCE.rfind("greet").unwrap() + 2;
        // A plain click only moves the cursor.
        update(window, cx, |e, w, cx| {
            mouse_down(e, call, Modifiers::default(), w, cx);
            e.on_mouse_up(
                &MouseUpEvent {
                    button: MouseButton::Left,
                    position: position_of(e, call),
                    modifiers: Modifiers::default(),
                    click_count: 1,
                },
                w,
                cx,
            );
        });
        for _ in 0..20 {
            cx.executor().advance_clock(LINK_DELAY);
            cx.run_until_parked();
        }
        assert!(navigated.borrow().is_empty(), "a plain click navigated");
        assert_eq!(state(window, cx), "def greet():\n    pass\n\ngre|et()\n");
        update(window, cx, |e, _, cx| e.set_cursor(0, cx));

        // Ctrl+click: the cursor goes there (the nearer side of the clicked
        // character's middle) and the definition opens.
        update(window, cx, |e, w, cx| mouse_down(e, call, ctrl(), w, cx));
        assert_eq!(state(window, cx), "def greet():\n    pass\n\ngre|et()\n");
        // No selection drag: moving with the button down selects nothing.
        assert!(window.read_with(cx, |e, _| e.drag.is_none()).unwrap());
        update(window, cx, |e, w, cx| {
            e.on_mouse_move(
                &MouseMoveEvent {
                    position: position_of(e, call + 3),
                    pressed_button: Some(MouseButton::Left),
                    modifiers: ctrl(),
                },
                w,
                cx,
            )
        });
        assert_eq!(state(window, cx), "def greet():\n    pass\n\ngre|et()\n");
        wait_for(cx, |_| !navigated.borrow().is_empty());
        let (title, locations) = navigated.borrow()[0].clone();
        assert_eq!(title.as_ref(), Goto::Definition.title());
        assert_eq!(locations.len(), 1);
        let (line, start, end) = DEFINITION;
        assert_eq!(
            locations[0].range.start,
            lsp::types::Position::new(line, start)
        );
        assert_eq!(locations[0].range.end, lsp::types::Position::new(line, end));
    }

    #[gpui::test]
    fn ctrl_click_on_the_definition_finds_its_usages(cx: &mut TestAppContext) {
        let Fixture {
            window, navigated, ..
        } = fixture(cx);
        let name = SOURCE.find("greet").unwrap() + 1;
        update(window, cx, |e, w, cx| mouse_down(e, name, ctrl(), w, cx));
        wait_for(cx, |_| !navigated.borrow().is_empty());
        let (title, locations) = navigated.borrow()[0].clone();
        assert_eq!(title.as_ref(), "References");
        assert_eq!(locations.len(), 2);
    }

    #[gpui::test]
    fn ctrl_hover_underlines_identifiers_with_a_definition(cx: &mut TestAppContext) {
        let Fixture { window, asked, .. } = fixture(cx);
        let shown =
            |cx: &mut TestAppContext| window.read_with(cx, |e, _| e.link.shown.clone()).unwrap();
        let call = SOURCE.rfind("greet").unwrap();
        // Without Ctrl, nothing is asked or underlined.
        update(window, cx, |e, w, cx| {
            mouse_move(e, call + 1, Modifiers::default(), w, cx)
        });
        cx.executor().advance_clock(LINK_DELAY * 2);
        cx.run_until_parked();
        assert_eq!(shown(cx), None);
        assert!(asked.lock().is_empty());

        // Pressing Ctrl over `greet` underlines it once the server answers,
        // with a pointing hand.
        update(window, cx, |e, w, cx| modifiers_changed(e, ctrl(), w, cx));
        wait_for(cx, |cx| shown(cx).is_some());
        assert_eq!(shown(cx), Some(call..call + 5));
        // Releasing Ctrl removes it.
        update(window, cx, |e, w, cx| {
            modifiers_changed(e, Modifiers::default(), w, cx)
        });
        assert_eq!(shown(cx), None);

        // `pass` has no definition: never underlined.
        let pass = SOURCE.find("pass").unwrap();
        update(window, cx, |e, w, cx| {
            mouse_move(e, pass + 1, ctrl(), w, cx)
        });
        wait_for(cx, |_| asked.lock().len() == 2);
        cx.run_until_parked();
        assert_eq!(shown(cx), None);

        // Back on `greet`: the cached answer underlines it at once.
        update(window, cx, |e, w, cx| {
            mouse_move(e, call + 3, ctrl(), w, cx)
        });
        assert_eq!(shown(cx), Some(call..call + 5));
        assert_eq!(asked.lock().len(), 2, "answers are cached per word");
        // Sweeping across words asks only where the mouse rests.
        update(window, cx, |e, w, cx| {
            mouse_move(e, pass, ctrl(), w, cx);
            mouse_move(e, SOURCE.find("def").unwrap(), ctrl(), w, cx);
            mouse_move(e, call + 2, ctrl(), w, cx);
        });
        cx.executor().advance_clock(LINK_DELAY * 2);
        cx.run_until_parked();
        assert_eq!(asked.lock().len(), 2);
        assert_eq!(shown(cx), Some(call..call + 5));
    }
}
