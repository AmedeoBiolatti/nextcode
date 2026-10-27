//! CommonMark text only: neither image resolution nor link activation is exposed.
use crate::{selectable::selectable, theme};
use gpui::{
    AnyElement, FontStyle, FontWeight, HighlightStyle, SharedString, StrikethroughStyle, div,
    prelude::*, px,
};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::ops::Range;

#[derive(Default, Clone, Copy)]
struct Style {
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
}
struct Block {
    text: SharedString,
    spans: Vec<(Range<usize>, Style)>,
    heading: bool,
    code: bool,
}
pub(super) struct Markdown {
    source: String,
    blocks: Vec<Block>,
}
impl Markdown {
    pub(super) fn source(&self) -> &str {
        &self.source
    }
    pub(super) fn new(source: &str) -> Self {
        let mut blocks = Vec::new();
        let mut text = String::new();
        let mut spans = Vec::new();
        let mut style = Style::default();
        let mut stack = Vec::new();
        let mut heading = false;
        let mut code = false;
        let flush = |blocks: &mut Vec<Block>,
                     text: &mut String,
                     spans: &mut Vec<(Range<usize>, Style)>,
                     heading,
                     code| {
            if !text.is_empty() {
                blocks.push(Block {
                    text: std::mem::take(text).into(),
                    spans: std::mem::take(spans),
                    heading,
                    code,
                });
            }
        };
        for event in Parser::new_ext(
            source,
            Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS,
        ) {
            match event {
                Event::Start(tag) => {
                    stack.push(style);
                    match tag {
                        Tag::Heading { .. } => {
                            flush(&mut blocks, &mut text, &mut spans, heading, code);
                            heading = true;
                            style.bold = true;
                        }
                        Tag::CodeBlock(_) => {
                            flush(&mut blocks, &mut text, &mut spans, heading, code);
                            code = true;
                        }
                        Tag::Strong => style.bold = true,
                        Tag::Emphasis => style.italic = true,
                        Tag::Strikethrough => style.strike = true,
                        Tag::Item => text.push_str("• "),
                        _ => {}
                    }
                }
                Event::End(tag) => {
                    if matches!(
                        tag,
                        TagEnd::Paragraph
                            | TagEnd::Heading(_)
                            | TagEnd::CodeBlock
                            | TagEnd::Item
                            | TagEnd::TableRow
                    ) {
                        flush(&mut blocks, &mut text, &mut spans, heading, code);
                    }
                    if matches!(tag, TagEnd::Heading(_)) {
                        heading = false;
                    }
                    if matches!(tag, TagEnd::CodeBlock) {
                        code = false;
                    }
                    if matches!(tag, TagEnd::TableCell) {
                        text.push_str("   |   ");
                    }
                    style = stack.pop().unwrap_or_default();
                }
                Event::Text(value) => {
                    let start = text.len();
                    text.push_str(&value);
                    spans.push((start..text.len(), style));
                }
                Event::Code(value) => {
                    let start = text.len();
                    text.push_str(&value);
                    spans.push((
                        start..text.len(),
                        Style {
                            code: true,
                            ..style
                        },
                    ));
                }
                Event::SoftBreak | Event::HardBreak => text.push('\n'),
                Event::TaskListMarker(done) => text.push_str(if done { "[x] " } else { "[ ] " }),
                Event::Rule => {
                    flush(&mut blocks, &mut text, &mut spans, heading, code);
                    blocks.push(Block {
                        text: "────────".into(),
                        spans: Vec::new(),
                        heading: false,
                        code: false,
                    });
                }
                // HTML, destinations, image bytes and metadata never become active UI.
                _ => {}
            }
        }
        flush(&mut blocks, &mut text, &mut spans, heading, code);
        Self {
            source: source.to_owned(),
            blocks,
        }
    }
    pub(super) fn render(&self) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .children(self.blocks.iter().enumerate().map(|(index, block)| {
                let highlights = block.spans.iter().map(|(range, style)| {
                    (
                        range.clone(),
                        HighlightStyle {
                            font_weight: style.bold.then_some(FontWeight::BOLD),
                            font_style: style.italic.then_some(FontStyle::Italic),
                            strikethrough: style.strike.then_some(StrikethroughStyle {
                                thickness: px(1.),
                                color: None,
                            }),
                            background_color: style.code.then(|| theme::text().opacity(0.08)),
                            ..HighlightStyle::default()
                        },
                    )
                });
                div()
                    .id(index)
                    .when(block.heading, |row| row.text_lg())
                    .when(block.code, |row| {
                        row.p_2().bg(theme::panel()).font_family(theme::MONO_FONT)
                    })
                    .child(
                        selectable("extension-markdown", block.text.clone())
                            .with_highlights(highlights),
                    )
            }))
            .into_any_element()
    }
}
