//! Signature help: the popover above the cursor while typing a call's
//! arguments, with the active parameter highlighted.
//!
//! Typing one of the server's trigger characters (e.g. `(` or `,`) or
//! `Ctrl+Shift+Space` asks for it; while it shows, every edit and cursor
//! move asks again (as a retrigger), and the popover closes when the
//! server says the cursor left the call, on Escape, or on a click outside
//! it (its text is selectable).
//! Several signatures (overloads) cycle with Up/Down or the popover's
//! arrows.

use gpui::{FontWeight, HighlightStyle};
use language::markdown::Block;
use lsp::types::{
    Documentation, ParameterLabel, SignatureHelp, SignatureHelpContext, SignatureHelpParams,
    SignatureHelpTriggerKind, SignatureInformation, request::SignatureHelpRequest,
};

use super::*;
use crate::lsp_store::request_retrying;

actions!(
    editor,
    [
        ShowSignatureHelp,
        CloseSignatureHelp,
        NextSignature,
        PreviousSignature
    ]
);

/// Pause before re-asking after an edit or cursor move while it shows.
const RETRIGGER_DELAY: Duration = Duration::from_millis(50);
const RETRY_DELAY: Duration = Duration::from_millis(400);

/// The open popover.
pub(super) struct SignaturePopover {
    help: SignatureHelp,
    /// The signature shown (starts at the server's active one).
    active: usize,
}

impl SignaturePopover {
    pub(super) fn has_several(&self) -> bool {
        self.help.signatures.len() > 1
    }
}

#[derive(Default)]
pub(super) struct SignatureHelpState {
    pub(super) popover: Option<SignaturePopover>,
    /// Bumped per request; older responses are dropped.
    generation: usize,
    /// The primary cursor the popover was last asked for.
    head: Option<usize>,
    task: Option<Task<()>>,
}

impl Editor {
    pub(super) fn show_signature_help(
        &mut self,
        _: &ShowSignatureHelp,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_signature_help(SignatureHelpTriggerKind::INVOKED, None, Duration::ZERO, cx);
    }

    pub(super) fn close_signature_help(
        &mut self,
        _: &CloseSignatureHelp,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_signature_help();
        cx.notify();
    }

    pub(super) fn dismiss_signature_help(&mut self) {
        self.signature.popover = None;
        self.signature.task = None;
        self.signature.head = None;
        self.signature.generation += 1;
    }

    pub(super) fn next_signature(
        &mut self,
        _: &NextSignature,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_signature(1, cx);
    }

    pub(super) fn previous_signature(
        &mut self,
        _: &PreviousSignature,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_signature(-1, cx);
    }

    fn cycle_signature(&mut self, step: isize, cx: &mut Context<Self>) {
        if let Some(popover) = &mut self.signature.popover {
            let count = popover.help.signatures.len() as isize;
            popover.active = (popover.active as isize + step).rem_euclid(count.max(1)) as usize;
            cx.notify();
        }
    }

    /// Typed text: a trigger character opens the popover; while it shows,
    /// a retrigger character asks again at once.
    pub(super) fn signature_help_typed(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.marked.is_some() {
            return;
        }
        let Some(options) = self
            .server()
            .and_then(|server| server.capabilities.signature_help_provider.clone())
        else {
            return;
        };
        let open = self.signature.popover.is_some();
        let is = |characters: &Option<Vec<String>>| {
            characters
                .iter()
                .flatten()
                .any(|character| character == text)
        };
        if is(&options.trigger_characters) || (open && is(&options.retrigger_characters)) {
            self.request_signature_help(
                SignatureHelpTriggerKind::TRIGGER_CHARACTER,
                Some(text.to_owned()),
                Duration::ZERO,
                cx,
            );
        }
    }

    /// After an edit or a cursor move: re-asks while the popover shows (or
    /// is being asked for) and the primary cursor moved.
    pub(super) fn signature_help_moved(&mut self, cx: &mut Context<Self>) {
        let head = self.primary().head;
        if self.signature.head.is_none_or(|asked| asked == head) {
            return;
        }
        if self.selections.len() > 1 || !self.primary().is_empty() {
            self.dismiss_signature_help();
            return;
        }
        self.request_signature_help(
            SignatureHelpTriggerKind::CONTENT_CHANGE,
            None,
            RETRIGGER_DELAY,
            cx,
        );
    }

    fn request_signature_help(
        &mut self,
        trigger_kind: SignatureHelpTriggerKind,
        trigger_character: Option<String>,
        delay: Duration,
        cx: &mut Context<Self>,
    ) {
        let Some(server) = self.server() else {
            return;
        };
        if server.capabilities.signature_help_provider.is_none() {
            return;
        }
        let head = self.primary().head;
        let Some(position) = self.position_params(head) else {
            return;
        };
        let params = SignatureHelpParams {
            context: Some(SignatureHelpContext {
                trigger_kind,
                trigger_character,
                is_retrigger: self.signature.popover.is_some(),
                active_signature_help: self.signature.popover.as_ref().map(|popover| {
                    SignatureHelp {
                        active_signature: Some(popover.active as u32),
                        ..popover.help.clone()
                    }
                }),
            }),
            text_document_position_params: position,
            work_done_progress_params: Default::default(),
        };
        self.signature.generation += 1;
        self.signature.head = Some(head);
        let generation = self.signature.generation;
        self.signature.task = Some(cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            executor.timer(delay).await;
            let result = request_retrying::<SignatureHelpRequest>(
                &server,
                params,
                &executor,
                1,
                RETRY_DELAY,
            )
            .await;
            this.update(cx, |this, cx| {
                if this.signature.generation != generation {
                    return;
                }
                match result {
                    Ok(Some(help)) if !help.signatures.is_empty() => {
                        // Keep the overload the user picked if the server
                        // still offers as many.
                        let picked = this
                            .signature
                            .popover
                            .as_ref()
                            .filter(|popover| {
                                popover.help.signatures.len() == help.signatures.len()
                            })
                            .map(|popover| popover.active);
                        let active = picked
                            .or(help.active_signature.map(|active| active as usize))
                            .filter(|&active| active < help.signatures.len())
                            .unwrap_or(0);
                        this.signature.popover = Some(SignaturePopover { help, active });
                    }
                    // Outside a call now (or the request failed).
                    _ => {
                        this.signature.popover = None;
                        this.signature.head = None;
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The popover, above the cursor line (below it near the top).
    pub(super) fn render_signature_help(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let popover = self.signature.popover.as_ref()?;
        let layout = self.layout.as_ref()?;
        let (x, top) = self.screen_position(self.primary().head)?;
        let signature = popover.help.signatures.get(popover.active)?;
        let encoding = self
            .server()
            .map_or(Encoding::Utf16, |server| server.encoding);
        let parameter = active_parameter(&popover.help, signature);
        let highlight = parameter
            .and_then(|index| parameter_range(signature, index, encoding))
            .map(|range| {
                (
                    range,
                    HighlightStyle {
                        color: Some(theme::accent_text()),
                        font_weight: Some(FontWeight::BOLD),
                        underline: Some(UnderlineStyle {
                            thickness: px(1.),
                            color: Some(theme::accent_text()),
                            wavy: false,
                        }),
                        ..Default::default()
                    },
                )
            });
        let parameter_docs = parameter
            .and_then(|index| signature.parameters.as_ref()?.get(index))
            .and_then(|parameter| parameter.documentation.as_ref())
            .map(documentation_blocks)
            .unwrap_or_default();
        let signature_docs = signature
            .documentation
            .as_ref()
            .map(documentation_blocks)
            .unwrap_or_default();
        let separator = (!parameter_docs.is_empty() || !signature_docs.is_empty())
            .then(|| div().h(px(1.)).bg(theme::border()));
        let below = top < px(120.);
        let bounds = layout.bounds;
        let arrow = |label: &'static str, step: isize, cx: &mut Context<Self>| {
            div()
                .id(label)
                .px_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(|style| style.bg(theme::hover_row()))
                .child(label)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |this, _, _, cx| this.cycle_signature(step, cx)))
        };
        let counter = popover.has_several().then(|| {
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap_1()
                .text_color(theme::muted())
                .font_family(theme::UI_FONT)
                .child(
                    arrow("▲", -1, cx)
                        .pin("previous")
                        .label("Previous Signature"),
                )
                .child(selectable(
                    "counter",
                    format!("{}/{}", popover.active + 1, popover.help.signatures.len()),
                ))
                .child(arrow("▼", 1, cx).pin("next").label("Next Signature"))
        });
        Some(
            div()
                .id("signature-help")
                .absolute()
                .left((x - bounds.left() - px(8.)).max(px(0.)))
                .when(below, |popover| popover.top(top + layout.line_height))
                .when(!below, |popover| popover.bottom(bounds.size.height - top))
                .max_w(px(640.))
                .max_h(px(260.))
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
                .on_mouse_move(|_, _, cx| cx.stop_propagation())
                // Pressing in it (to select text) must not move the cursor.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .child(
                    div().flex().gap_2().children(counter).child(
                        div()
                            .font_family(theme::MONO_FONT)
                            .text_color(theme::text())
                            .child(
                                selectable("label", signature.label.clone())
                                    .with_highlights(highlight),
                            ),
                    ),
                )
                .children(separator)
                .children(navigation::markdown_blocks(
                    "parameter-doc",
                    &parameter_docs,
                ))
                .children(navigation::markdown_blocks(
                    "signature-doc",
                    &signature_docs,
                ))
                .pin("signature-help")
                .label("Signature Help")
                .into_any_element(),
        )
    }
}

fn documentation_blocks(documentation: &Documentation) -> Vec<Block> {
    match documentation {
        Documentation::String(text) => vec![Block::Paragraph(text.clone())],
        Documentation::MarkupContent(markup) => language::markdown::parse(&markup.value),
    }
}

/// The index of the highlighted parameter of `signature`: its own active
/// parameter, else the response's, else the first; `None` when that is
/// past the signature's parameters (e.g. after the last argument).
pub(crate) fn active_parameter(
    help: &SignatureHelp,
    signature: &SignatureInformation,
) -> Option<usize> {
    let count = signature.parameters.as_ref().map_or(0, Vec::len);
    let active = signature
        .active_parameter
        .or(help.active_parameter)
        .unwrap_or(0) as usize;
    (active < count).then_some(active)
}

/// Byte range of parameter `index` in `signature.label`: from its label
/// offsets (in the server's position encoding), or by finding its label
/// string after the previous parameters'.
pub(crate) fn parameter_range(
    signature: &SignatureInformation,
    index: usize,
    encoding: Encoding,
) -> Option<Range<usize>> {
    let label = &signature.label;
    let parameters = signature.parameters.as_ref()?;
    let mut search_from = 0;
    for (i, parameter) in parameters.iter().enumerate().take(index + 1) {
        let range = match &parameter.label {
            ParameterLabel::LabelOffsets([start, end]) => {
                let start = byte_offset(label, *start as usize, encoding)?;
                let end = byte_offset(label, *end as usize, encoding)?;
                (start <= end).then_some(start..end)
            }
            ParameterLabel::Simple(text) if !text.is_empty() => label
                .get(search_from..)
                .and_then(|rest| rest.find(text.as_str()))
                .map(|found| search_from + found..search_from + found + text.len()),
            ParameterLabel::Simple(_) => None,
        };
        if i == index {
            return range;
        }
        if let Some(range) = range {
            search_from = range.end;
        }
    }
    None
}

/// The byte offset of `units` code units into `text`.
fn byte_offset(text: &str, units: usize, encoding: Encoding) -> Option<usize> {
    match encoding {
        Encoding::Utf8 => text.is_char_boundary(units).then_some(units),
        Encoding::Utf16 => {
            let mut counted = 0;
            for (offset, c) in text.char_indices() {
                if counted >= units {
                    return (counted == units).then_some(offset);
                }
                counted += c.len_utf16();
            }
            (counted == units).then_some(text.len())
        }
    }
}

#[cfg(test)]
mod tests {
    use lsp::types::ParameterInformation;

    use super::*;

    fn signature(label: &str, parameters: Vec<ParameterLabel>) -> SignatureInformation {
        SignatureInformation {
            label: label.into(),
            documentation: None,
            parameters: Some(
                parameters
                    .into_iter()
                    .map(|label| ParameterInformation {
                        label,
                        documentation: None,
                    })
                    .collect(),
            ),
            active_parameter: None,
        }
    }

    fn help(signature: SignatureInformation, active: Option<u32>) -> SignatureHelp {
        SignatureHelp {
            signatures: vec![signature],
            active_signature: Some(0),
            active_parameter: active,
        }
    }

    fn highlighted(signature: &SignatureInformation, index: usize, encoding: Encoding) -> &str {
        let range = parameter_range(signature, index, encoding).expect("a range");
        &signature.label[range]
    }

    #[test]
    fn label_offsets_select_the_parameter_in_the_servers_encoding() {
        let offsets = |start, end| ParameterLabel::LabelOffsets([start, end]);
        let sig = signature(
            "fn shift(point: Point, by: i32) -> i32",
            vec![offsets(9, 21), offsets(23, 30)],
        );
        assert_eq!(highlighted(&sig, 0, Encoding::Utf16), "point: Point");
        assert_eq!(highlighted(&sig, 1, Encoding::Utf8), "by: i32");
        // `é` is two UTF-8 bytes but one UTF-16 unit.
        let sig = signature("fn f(é: u8, b: u8)", vec![offsets(5, 10), offsets(12, 17)]);
        assert_eq!(highlighted(&sig, 1, Encoding::Utf16), "b: u8");
        let sig = signature("fn f(é: u8, b: u8)", vec![offsets(5, 11), offsets(13, 18)]);
        assert_eq!(highlighted(&sig, 0, Encoding::Utf8), "é: u8");
        // Offsets inside a char or past the end highlight nothing.
        let sig = signature("fn f(é)", vec![offsets(6, 7)]);
        assert_eq!(parameter_range(&sig, 0, Encoding::Utf8), None);
        let sig = signature("f(a)", vec![offsets(2, 9)]);
        assert_eq!(parameter_range(&sig, 0, Encoding::Utf16), None);
    }

    #[test]
    fn string_labels_are_found_after_the_previous_parameters() {
        let simple = |text: &str| ParameterLabel::Simple(text.into());
        // Python-style labels where the same text repeats.
        let sig = signature(
            "(a: int, b: int, ab: int) -> None",
            vec![simple("a: int"), simple("b: int"), simple("ab: int")],
        );
        // Each label is searched for after the previous one's.
        assert_eq!(parameter_range(&sig, 1, Encoding::Utf16), Some(9..15));
        assert_eq!(highlighted(&sig, 2, Encoding::Utf16), "ab: int");
        let sig = signature("(x, x)", vec![simple("x"), simple("x")]);
        assert_eq!(parameter_range(&sig, 1, Encoding::Utf8), Some(4..5));
        let sig = signature("(x)", vec![simple("y")]);
        assert_eq!(parameter_range(&sig, 0, Encoding::Utf8), None);
    }

    #[test]
    fn the_active_parameter_comes_from_the_signature_then_the_response() {
        let simple = |text: &str| ParameterLabel::Simple(text.into());
        let mut sig = signature("(a, b)", vec![simple("a"), simple("b")]);
        assert_eq!(active_parameter(&help(sig.clone(), None), &sig), Some(0));
        assert_eq!(active_parameter(&help(sig.clone(), Some(1)), &sig), Some(1));
        // Past the last parameter: nothing is highlighted.
        assert_eq!(active_parameter(&help(sig.clone(), Some(2)), &sig), None);
        sig.active_parameter = Some(0);
        assert_eq!(active_parameter(&help(sig.clone(), Some(1)), &sig), Some(0));
        let empty = signature("()", Vec::new());
        assert_eq!(active_parameter(&help(empty.clone(), None), &empty), None);
    }
}
