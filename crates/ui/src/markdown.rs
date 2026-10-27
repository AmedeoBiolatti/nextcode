//! Markdown rendered as elements (the Markdown preview, notebooks'
//! Markdown cells and `text/markdown` outputs): CommonMark with tables,
//! strikethrough, task lists and math (shown as code). Text is
//! selectable; links open on click; images come from a notebook cell's
//! attachments, `data:` URIs or files next to the document.
//!
//! Parsing (and highlighting code blocks) happens once per text: rendered
//! texts and resolved images are kept in bounded caches.

use std::{
    ops::Range,
    path::{Component, Path, PathBuf},
    sync::{Arc, LazyLock},
};

use gpui::{
    AnyElement, App, ElementId, FontStyle, FontWeight, HighlightStyle, ImageFormat, SharedString,
    StrikethroughStyle, UnderlineStyle, div, prelude::*, px,
};
use parking_lot::Mutex;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use syntax::{HighlightSpan, Language, Syntax};

use crate::{
    images::{DecodedImage, Lru, content_hash, decode_image, decoded, image_element, mime_format},
    selectable::selectable,
    theme,
};

/// Where a cell's images come from.
#[derive(Clone, Copy, Default)]
pub struct MarkdownImages<'a> {
    /// The cell's `attachments` (`attachment:name` images).
    pub attachments: Option<&'a serde_json::Value>,
    /// The notebook's folder: relative image and link paths.
    pub base_dir: Option<&'a Path>,
}

/// CommonMark (with tables, strikethrough, task lists and math) as an
/// element.
pub fn markdown(id: impl Into<ElementId>, text: &str, images: MarkdownImages) -> AnyElement {
    MarkdownDocument::parse(text).render(id, images)
}

/// A parsed document that can be prepared off-thread and rendered repeatedly.
#[derive(Clone)]
pub struct MarkdownDocument(Arc<Vec<Block>>);

impl MarkdownDocument {
    pub fn parse(text: &str) -> Self {
        Self(parsed(text))
    }

    pub fn render(&self, id: impl Into<ElementId>, images: MarkdownImages) -> AnyElement {
        div()
            .id(id)
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .font_family(theme::UI_FONT)
            .text_color(theme::text())
            .children(render_blocks(&self.0, &images))
            .into_any_element()
    }
}

/// Parsed texts, by content hash.
static PARSED: LazyLock<Mutex<Lru<Arc<Vec<Block>>>>> = LazyLock::new(|| Mutex::new(Lru::new(256)));
/// Images by source (and content, for attachments and data URIs).
static IMAGES: LazyLock<Mutex<Lru<Option<DecodedImage>>>> =
    LazyLock::new(|| Mutex::new(Lru::new(64)));

fn parsed(text: &str) -> Arc<Vec<Block>> {
    let key = content_hash([text.as_bytes()]);
    PARSED
        .lock()
        .get_or_insert_with(key, || Arc::new(parse(text)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextKind {
    Paragraph,
    /// 1-6.
    Heading(u8),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct InlineStyle {
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
    link: bool,
}

/// A run of text with styled ranges and links (byte ranges).
#[derive(Debug, Clone, Default, PartialEq)]
struct Inline {
    text: SharedString,
    styles: Vec<(Range<usize>, InlineStyle)>,
    links: Vec<(Range<usize>, String)>,
}

#[derive(Debug, Clone, PartialEq)]
enum Block {
    Text {
        kind: TextKind,
        inline: Inline,
    },
    Image {
        src: String,
        alt: String,
    },
    Code {
        language: Option<Language>,
        text: SharedString,
        highlights: Vec<HighlightSpan>,
    },
    Quote(Vec<Block>),
    List {
        /// The first number of an ordered list.
        start: Option<u64>,
        items: Vec<Item>,
    },
    Rule,
    Table {
        head: Vec<Inline>,
        rows: Vec<Vec<Inline>>,
    },
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Item {
    /// Task list items: whether checked.
    checked: Option<bool>,
    blocks: Vec<Block>,
}

/// Containers being filled while parsing.
enum Container {
    Blocks(Vec<Block>),
    Quote(Vec<Block>),
    List {
        start: Option<u64>,
        items: Vec<Item>,
    },
    Item(Item),
    Table {
        head: Vec<Inline>,
        rows: Vec<Vec<Inline>>,
        row: Vec<Inline>,
    },
}

/// The inline text being built (a paragraph, heading or table cell).
#[derive(Default)]
struct InlineBuilder {
    text: String,
    styles: Vec<(Range<usize>, InlineStyle)>,
    links: Vec<(Range<usize>, String)>,
}

impl InlineBuilder {
    fn push(&mut self, text: &str, style: InlineStyle) {
        if text.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(text);
        let end = self.text.len();
        match self.styles.last_mut() {
            Some((range, last)) if *last == style && range.end == start => range.end = end,
            _ => self.styles.push((start..end, style)),
        }
    }

    fn finish(self) -> Inline {
        Inline {
            text: self.text.into(),
            styles: self.styles,
            links: self.links,
        }
    }
}

#[derive(Default)]
struct Builder {
    stack: Vec<Container>,
    inline: Option<(TextKind, InlineBuilder)>,
    bold: u32,
    italic: u32,
    strike: u32,
    /// Open links: where they start in the current inline text, and URL.
    links: Vec<(usize, String)>,
    /// An image whose alt text is being read: URL and alt text.
    image: Option<(String, String)>,
    /// A code block being read: info string and text.
    code: Option<(String, String)>,
    /// An HTML block being read.
    html: Option<String>,
}

impl Builder {
    fn style(&self, code: bool) -> InlineStyle {
        InlineStyle {
            bold: self.bold > 0,
            italic: self.italic > 0,
            strike: self.strike > 0,
            code,
            link: !self.links.is_empty(),
        }
    }

    fn inline(&mut self) -> &mut InlineBuilder {
        &mut self
            .inline
            .get_or_insert_with(|| (TextKind::Paragraph, InlineBuilder::default()))
            .1
    }

    fn push_text(&mut self, text: &str, code: bool) {
        let style = self.style(code);
        self.inline().push(text, style);
    }

    fn in_table(&self) -> bool {
        matches!(self.stack.last(), Some(Container::Table { .. }))
    }

    /// Ends the current paragraph or heading; open links continue in the
    /// next one.
    fn flush(&mut self) {
        let Some((kind, mut inline)) = self.inline.take() else {
            return;
        };
        let end = inline.text.len();
        for (start, url) in &mut self.links {
            if *start < end {
                inline.links.push((*start..end, url.clone()));
            }
            *start = 0;
        }
        if kind != TextKind::Paragraph || !inline.text.trim().is_empty() {
            self.push_block(Block::Text {
                kind,
                inline: inline.finish(),
            });
        }
    }

    fn push_block(&mut self, block: Block) {
        match self.stack.last_mut() {
            Some(Container::Blocks(blocks) | Container::Quote(blocks)) => blocks.push(block),
            Some(Container::Item(item)) => item.blocks.push(block),
            // Blocks directly in a list or table don't occur.
            _ => {}
        }
    }

    /// An image at this point of the text: its own block, or its alt text
    /// in a table cell.
    fn image(&mut self, src: String, alt: String) {
        if self.in_table() {
            self.push_text(&alt, false);
        } else {
            let kind = self.inline.as_ref().map(|(kind, _)| *kind);
            self.flush();
            self.push_block(Block::Image { src, alt });
            if let Some(TextKind::Heading(level)) = kind {
                self.inline = Some((TextKind::Heading(level), InlineBuilder::default()));
            }
        }
    }

    fn event(&mut self, event: Event) {
        if let Some((_, alt)) = &mut self.image {
            match event {
                Event::Text(text) | Event::Code(text) => alt.push_str(&text),
                Event::End(TagEnd::Image) => {
                    if let Some((src, alt)) = self.image.take() {
                        self.image(src, alt);
                    }
                }
                _ => {}
            }
            return;
        }
        if let Some((_, code)) = &mut self.code {
            match event {
                Event::Text(text) => code.push_str(&text),
                Event::End(TagEnd::CodeBlock) => {
                    if let Some((info, mut text)) = self.code.take() {
                        if text.ends_with('\n') {
                            text.pop();
                        }
                        self.push_block(code_block(&info, text));
                    }
                }
                _ => {}
            }
            return;
        }
        if let Some(html) = &mut self.html {
            match event {
                Event::Html(text) | Event::Text(text) => html.push_str(&text),
                Event::End(TagEnd::HtmlBlock) => {
                    if let Some(html) = self.html.take() {
                        self.html_block(&html);
                    }
                }
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.push_text(&text, false),
            Event::Code(text) | Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.push_text(&text, true)
            }
            Event::Html(html) => self.html_block(&html),
            Event::InlineHtml(html) => self.inline_html(&html),
            Event::FootnoteReference(name) => self.push_text(&format!("[{name}]"), false),
            Event::SoftBreak => self.push_text(" ", false),
            Event::HardBreak => self.push_text("\n", false),
            Event::Rule => {
                self.flush();
                self.push_block(Block::Rule);
            }
            Event::TaskListMarker(checked) => {
                if let Some(Container::Item(item)) = self
                    .stack
                    .iter_mut()
                    .rev()
                    .find(|c| matches!(c, Container::Item(_)))
                {
                    item.checked = Some(checked);
                }
            }
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {
                self.flush();
                self.inline = Some((TextKind::Paragraph, InlineBuilder::default()));
            }
            Tag::Heading { level, .. } => {
                self.flush();
                let level = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    HeadingLevel::H4 => 4,
                    HeadingLevel::H5 => 5,
                    HeadingLevel::H6 => 6,
                };
                self.inline = Some((TextKind::Heading(level), InlineBuilder::default()));
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.stack.push(Container::Quote(Vec::new()));
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                let info = match kind {
                    CodeBlockKind::Fenced(info) => info.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((info, String::new()));
            }
            Tag::HtmlBlock => {
                self.flush();
                self.html = Some(String::new());
            }
            Tag::List(start) => {
                self.flush();
                self.stack.push(Container::List {
                    start,
                    items: Vec::new(),
                });
            }
            Tag::Item => {
                self.flush();
                self.stack.push(Container::Item(Item::default()));
            }
            Tag::Table(_) => {
                self.flush();
                self.stack.push(Container::Table {
                    head: Vec::new(),
                    rows: Vec::new(),
                    row: Vec::new(),
                });
            }
            Tag::TableCell => self.inline = Some((TextKind::Paragraph, InlineBuilder::default())),
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => {
                let start = self.inline().text.len();
                self.links.push((start, dest_url.to_string()));
            }
            Tag::Image { dest_url, .. } => self.image = Some((dest_url.to_string(), String::new())),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) => self.flush(),
            TagEnd::BlockQuote(_) => {
                self.flush();
                if let Some(Container::Quote(blocks)) = self.stack.pop() {
                    self.push_block(Block::Quote(blocks));
                }
            }
            TagEnd::List(_) => {
                self.flush();
                if let Some(Container::List { start, items }) = self.stack.pop() {
                    self.push_block(Block::List { start, items });
                }
            }
            TagEnd::Item => {
                self.flush();
                if let Some(Container::Item(item)) = self.stack.pop()
                    && let Some(Container::List { items, .. }) = self.stack.last_mut()
                {
                    items.push(item);
                }
            }
            TagEnd::TableCell => {
                let inline = self.take_cell();
                if let Some(Container::Table { row, .. }) = self.stack.last_mut() {
                    row.push(inline);
                }
            }
            TagEnd::TableHead => {
                if let Some(Container::Table { head, row, .. }) = self.stack.last_mut() {
                    *head = std::mem::take(row);
                }
            }
            TagEnd::TableRow => {
                if let Some(Container::Table { rows, row, .. }) = self.stack.last_mut() {
                    rows.push(std::mem::take(row));
                }
            }
            TagEnd::Table => {
                if let Some(Container::Table { head, rows, .. }) = self.stack.pop() {
                    self.push_block(Block::Table { head, rows });
                }
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                if let Some((start, url)) = self.links.pop() {
                    let inline = self.inline();
                    let end = inline.text.len();
                    if start < end {
                        inline.links.push((start..end, url));
                    }
                }
            }
            _ => {}
        }
    }

    /// The table cell's text, open links closed at its end.
    fn take_cell(&mut self) -> Inline {
        let mut inline = self
            .inline
            .take()
            .map(|(_, inline)| inline)
            .unwrap_or_default();
        let end = inline.text.len();
        for (start, url) in &mut self.links {
            if *start < end {
                inline.links.push((*start..end, url.clone()));
            }
            *start = 0;
        }
        inline.finish()
    }

    /// Raw HTML between blocks: `<img>` tags as images, the rest as text
    /// without tags.
    fn html_block(&mut self, html: &str) {
        self.flush();
        let mut text = String::new();
        let mut rest = html;
        while let Some(open) = rest.find('<') {
            text.push_str(&rest[..open]);
            let Some(close) = rest[open..].find('>') else {
                rest = &rest[open..];
                break;
            };
            let tag = &rest[open..open + close + 1];
            if let Some((src, alt)) = html_image(tag) {
                self.push_html_text(&text);
                text.clear();
                self.push_block(Block::Image { src, alt });
            } else if is_line_break(tag) {
                text.push('\n');
            }
            rest = &rest[open + close + 1..];
        }
        text.push_str(rest);
        self.push_html_text(&text);
    }

    fn push_html_text(&mut self, text: &str) {
        let text = collapse_whitespace(&decode_entities(text));
        if !text.is_empty() {
            let mut inline = InlineBuilder::default();
            inline.push(&text, InlineStyle::default());
            self.push_block(Block::Text {
                kind: TextKind::Paragraph,
                inline: inline.finish(),
            });
        }
    }

    fn inline_html(&mut self, tag: &str) {
        if let Some((src, alt)) = html_image(tag) {
            self.image(src, alt);
        } else if is_line_break(tag) {
            self.push_text("\n", false);
        }
    }

    fn finish(mut self) -> Vec<Block> {
        self.flush();
        // Unclosed containers (never from pulldown-cmark) are dropped.
        match self.stack.into_iter().next() {
            Some(Container::Blocks(blocks)) => blocks,
            _ => Vec::new(),
        }
    }
}

fn parse(text: &str) -> Vec<Block> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH
        | Options::ENABLE_FOOTNOTES;
    let mut builder = Builder {
        stack: vec![Container::Blocks(Vec::new())],
        ..Builder::default()
    };
    for event in Parser::new_ext(text, options) {
        builder.event(event);
    }
    builder.finish()
}

/// A fenced block's language from its info string (`python`, `py`,
/// `{.python}`, `ipython3`...).
fn code_language(info: &str) -> Option<Language> {
    let tag = info
        .trim()
        .trim_start_matches('{')
        .trim_start_matches('.')
        .split(|c: char| c.is_whitespace() || c == ',' || c == '}')
        .next()?
        .to_ascii_lowercase();
    if tag.is_empty() {
        return None;
    }
    match tag.as_str() {
        "python3" | "ipython" | "ipython3" | "py3" | "pycon" => return Some(Language::Python),
        "shell" | "console" | "shell-session" => return Some(Language::Bash),
        _ => {}
    }
    Language::ALL
        .into_iter()
        .find(|language| language.id() == tag || language.name().eq_ignore_ascii_case(&tag))
        .or_else(|| Language::from_path(Path::new(&format!("code.{tag}"))))
}

fn code_block(info: &str, text: String) -> Block {
    let language = code_language(info);
    let highlights = language
        .and_then(|language| {
            let buffer = text::Buffer::new(&text);
            Syntax::new(language, &buffer).map(|syntax| syntax.highlights(&buffer, 0..buffer.len()))
        })
        .unwrap_or_default();
    Block::Code {
        language,
        text: text.into(),
        highlights,
    }
}

/// `src` and `alt` of an `<img>` tag.
fn html_image(tag: &str) -> Option<(String, String)> {
    let lower = tag.to_ascii_lowercase();
    if !lower.starts_with("<img") {
        return None;
    }
    let attribute = |name: &str| {
        lower.match_indices(name).find_map(|(at, _)| {
            let before = lower[..at].chars().next_back()?;
            if !before.is_whitespace() {
                return None;
            }
            let after = at + name.len();
            let rest = tag[after..].trim_start().strip_prefix('=')?.trim_start();
            let value = match rest.chars().next()? {
                quote @ ('"' | '\'') => &rest[1..][..rest[1..].find(quote)?],
                _ => rest
                    .split(|c: char| c.is_whitespace() || c == '>' || c == '/')
                    .next()?,
            };
            Some(decode_entities(value))
        })
    };
    Some((attribute("src")?, attribute("alt").unwrap_or_default()))
}

fn is_line_break(tag: &str) -> bool {
    let name: String = tag
        .trim_start_matches('<')
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    matches!(
        name.to_ascii_lowercase().as_str(),
        "br" | "p" | "div" | "li" | "tr"
    )
}

fn decode_entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", "\u{a0}")
        .replace("&amp;", "&")
}

/// Runs of spaces and tabs as one space; blank lines dropped.
fn collapse_whitespace(text: &str) -> String {
    text.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// What clicking a link opens.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LinkTarget {
    Url(String),
    /// A file next to the notebook (opened by the system if it exists).
    File(PathBuf),
}

impl LinkTarget {
    fn open(&self, cx: &mut App) {
        match self {
            Self::Url(url) => cx.open_url(url),
            Self::File(path) if path.exists() => cx.open_with_system(path),
            Self::File(_) => {}
        }
    }
}

/// Absolute URLs open in the browser; relative paths (without `#…` or
/// `?…`) are files under `base_dir`; anchors open nothing.
fn link_target(href: &str, base_dir: Option<&Path>) -> Option<LinkTarget> {
    let href = href.trim();
    let scheme = href
        .split_once(':')
        .map(|(scheme, _)| scheme)
        .filter(|scheme| {
            scheme.len() > 1
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        });
    if scheme.is_some() {
        return Some(LinkTarget::Url(href.to_owned()));
    }
    let path = href.split(['#', '?']).next().unwrap_or_default();
    if path.is_empty() {
        return None;
    }
    Some(LinkTarget::File(base_dir?.join(relative_path(path)?)))
}

/// `path` as a relative path that stays inside its base (no root, no
/// `..`), with `%20` spaces decoded.
fn relative_path(path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(path.replace("%20", " "));
    path.components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
        .then_some(path)
}

fn extension_format(path: &Path) -> Option<ImageFormat> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "gif" => ImageFormat::Gif,
        "svg" => ImageFormat::Svg,
        "webp" => ImageFormat::Webp,
        "bmp" => ImageFormat::Bmp,
        _ => return None,
    })
}

/// The image `src` names: an attachment, a `data:` URI or a file under
/// `base_dir` (cached).
fn resolve_image(src: &str, images: &MarkdownImages) -> Option<DecodedImage> {
    let src = src.trim();
    if let Some(name) = src.strip_prefix("attachment:") {
        let bundle = images.attachments?.get(name)?.as_object()?;
        let (format, data) = bundle.iter().find_map(|(mime, data)| {
            let data = match data {
                serde_json::Value::String(data) => data.clone(),
                // nbformat may split the base64 into lines.
                serde_json::Value::Array(lines) => {
                    lines.iter().filter_map(|line| line.as_str()).collect()
                }
                _ => return None,
            };
            Some((mime_format(mime)?, data))
        })?;
        let key = content_hash([b"attachment".as_slice(), data.as_bytes()]);
        return IMAGES
            .lock()
            .get_or_insert_with(key, || decode_image(format, &data, true));
    }
    if let Some(uri) = src.strip_prefix("data:") {
        let (header, data) = uri.split_once(',')?;
        let mime = header.split(';').next()?;
        let format = mime_format(mime)?;
        let base64 = header.split(';').any(|part| part == "base64");
        let key = content_hash([b"data".as_slice(), src.as_bytes()]);
        return IMAGES
            .lock()
            .get_or_insert_with(key, || decode_image(format, data, base64));
    }
    if src.contains("://") {
        return None;
    }
    let path = images.base_dir?.join(relative_path(src)?);
    let format = extension_format(&path)?;
    let key = content_hash([b"file".as_slice(), path.as_os_str().as_encoded_bytes()]);
    IMAGES.lock().get_or_insert_with(key, || {
        let bytes = std::fs::read(&path).ok()?;
        Some(decoded(format, bytes))
    })
}

fn highlight(style: InlineStyle) -> HighlightStyle {
    HighlightStyle {
        font_weight: style.bold.then_some(FontWeight::BOLD),
        font_style: style.italic.then_some(FontStyle::Italic),
        strikethrough: style.strike.then_some(StrikethroughStyle {
            thickness: px(1.),
            color: None,
        }),
        background_color: style.code.then(|| theme::text().opacity(0.08)),
        color: style.link.then(theme::accent_text),
        underline: style.link.then(|| UnderlineStyle {
            thickness: px(1.),
            ..UnderlineStyle::default()
        }),
        ..HighlightStyle::default()
    }
}

/// Selectable text with its styles and clickable links.
fn inline_element(id: &'static str, inline: &Inline, images: &MarkdownImages) -> AnyElement {
    let highlights = inline
        .styles
        .iter()
        .filter(|(_, style)| *style != InlineStyle::default())
        .map(|(range, style)| (range.clone(), highlight(*style)));
    let text = selectable(id, inline.text.clone()).with_highlights(highlights);
    if inline.links.is_empty() {
        return text.into_any_element();
    }
    let ranges = inline
        .links
        .iter()
        .map(|(range, _)| range.clone())
        .collect();
    let targets: Vec<Option<LinkTarget>> = inline
        .links
        .iter()
        .map(|(_, href)| link_target(href, images.base_dir))
        .collect();
    text.with_links(ranges, move |index, _, cx| {
        if let Some(Some(target)) = targets.get(index) {
            target.open(cx);
        }
    })
    .into_any_element()
}

fn render_blocks(blocks: &[Block], images: &MarkdownImages) -> Vec<AnyElement> {
    blocks
        .iter()
        .enumerate()
        .map(|(index, block)| render_block(index, block, images))
        .collect()
}

fn render_block(index: usize, block: &Block, images: &MarkdownImages) -> AnyElement {
    let frame = div().id(index).w_full();
    match block {
        Block::Text {
            kind: TextKind::Paragraph,
            inline,
        } => frame.child(inline_element("text", inline, images)),
        Block::Text {
            kind: TextKind::Heading(level),
            inline,
        } => frame
            .font_weight(FontWeight::SEMIBOLD)
            .map(|heading| match level {
                1 => heading
                    .text_2xl()
                    .pb_1()
                    .border_b_1()
                    .border_color(theme::border()),
                2 => heading
                    .text_xl()
                    .pb_1()
                    .border_b_1()
                    .border_color(theme::border()),
                3 => heading.text_lg(),
                _ => heading.text_base(),
            })
            .child(inline_element("text", inline, images)),
        Block::Image { src, alt } => match resolve_image(src, images) {
            Some(image) => frame.child(image_element(&image, None)),
            None => frame.text_color(theme::muted()).italic().child(selectable(
                "alt",
                if alt.is_empty() {
                    "[image]".to_owned()
                } else {
                    format!("[{alt}]")
                },
            )),
        },
        Block::Code {
            language,
            text,
            highlights,
        } => {
            let theme = theme::current();
            let highlights = highlights.iter().map(|span| {
                let color = theme.syntax_color(*language, span.highlight);
                (
                    span.range.clone(),
                    HighlightStyle {
                        color: Some(color),
                        ..HighlightStyle::default()
                    },
                )
            });
            frame
                .px_2()
                .py_1()
                .bg(theme::panel())
                .border_1()
                .border_color(theme::border())
                .rounded_md()
                .overflow_x_scroll()
                .font_family(theme::MONO_FONT)
                .text_size(px(theme::EDITOR_FONT_SIZE))
                .line_height(px(theme::EDITOR_LINE_HEIGHT))
                .whitespace_nowrap()
                .child(selectable("code", text.clone()).with_highlights(highlights))
        }
        Block::Quote(blocks) => frame
            .pl_3()
            .border_l_4()
            .border_color(theme::border())
            .text_color(theme::muted())
            .flex()
            .flex_col()
            .gap_2()
            .children(render_blocks(blocks, images)),
        Block::List { start, items } => {
            frame
                .flex()
                .flex_col()
                .gap_1()
                .children(items.iter().enumerate().map(|(index, item)| {
                    let marker: SharedString = match (item.checked, start) {
                        (Some(true), _) => "☑".into(),
                        (Some(false), _) => "☐".into(),
                        (None, Some(start)) => format!("{}.", start + index as u64).into(),
                        (None, None) => "•".into(),
                    };
                    div()
                        .id(index)
                        .flex()
                        .gap_2()
                        .child(div().flex_none().min_w(px(16.)).child(marker))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .children(render_blocks(&item.blocks, images)),
                        )
                }))
        }
        Block::Rule => frame.h(px(1.)).bg(theme::border()),
        Block::Table { head, rows } => {
            let columns = head.len().max(rows.iter().map(Vec::len).max().unwrap_or(0));
            let empty = Inline::default();
            let row = |index: usize, cells: &[Inline], header: bool| {
                div()
                    .id(index)
                    .flex()
                    .when(header, |row| {
                        row.font_weight(FontWeight::SEMIBOLD).bg(theme::panel())
                    })
                    .when(index > 0, |row| {
                        row.border_t_1().border_color(theme::border())
                    })
                    .children((0..columns).map(|column| {
                        div()
                            .id(column)
                            .flex_1()
                            .min_w_0()
                            .px_2()
                            .py_1()
                            .when(column > 0, |cell| {
                                cell.border_l_1().border_color(theme::border())
                            })
                            .child(inline_element(
                                "text",
                                cells.get(column).unwrap_or(&empty),
                                images,
                            ))
                    }))
            };
            frame
                .flex()
                .flex_col()
                .border_1()
                .border_color(theme::border())
                .rounded_sm()
                .when(!head.is_empty(), |table| table.child(row(0, head, true)))
                .children(
                    rows.iter().enumerate().map(|(index, cells)| {
                        row(index + usize::from(!head.is_empty()), cells, false)
                    }),
                )
        }
    }
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, Window};
    use serde_json::json;

    use super::*;

    /// A 2×3 PNG.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAADCAIAAAA2iEnWAAAAEElEQVR4nGP4z8AARAwoFABE0AX7pM/egAAAAABJRU5ErkJggg==";

    fn text(block: &Block) -> (&TextKind, &str) {
        match block {
            Block::Text { kind, inline } => (kind, &inline.text[..]),
            other => panic!("not text: {other:?}"),
        }
    }

    fn inline(block: &Block) -> &Inline {
        match block {
            Block::Text { inline, .. } => inline,
            other => panic!("not text: {other:?}"),
        }
    }

    #[test]
    fn headings_paragraphs_and_inline_styles() {
        let blocks =
            parse("# Title\n\nSome *em* **strong** ~~gone~~ `code` text\nnext line\n\n---\n");
        assert_eq!(text(&blocks[0]), (&TextKind::Heading(1), "Title"));
        let paragraph = inline(&blocks[1]);
        assert_eq!(
            &paragraph.text[..],
            "Some em strong gone code text next line"
        );
        let styled = |word: &str| {
            let start = paragraph.text.find(word).unwrap();
            paragraph
                .styles
                .iter()
                .find(|(range, _)| range.start == start)
                .map(|(range, style)| (&paragraph.text[range.clone()], *style))
                .unwrap()
        };
        assert!(styled("em").1.italic);
        assert!(styled("strong").1.bold);
        assert!(styled("gone").1.strike);
        assert!(styled("code").1.code);
        assert_eq!(blocks[2], Block::Rule);
        assert_eq!(blocks.len(), 3);
    }

    #[test]
    fn code_blocks_are_highlighted_when_their_language_is_known() {
        let blocks =
            parse("```python\ndef f():\n    return 1\n```\n\n    indented\n\n```\nplain\n```\n");
        let Block::Code {
            language,
            text,
            highlights,
        } = &blocks[0]
        else {
            panic!("{blocks:?}");
        };
        assert_eq!(*language, Some(Language::Python));
        assert_eq!(&text[..], "def f():\n    return 1");
        assert!(!highlights.is_empty());
        assert!(
            matches!(&blocks[1], Block::Code { language: None, text, .. } if &text[..] == "indented")
        );
        assert!(
            matches!(&blocks[2], Block::Code { language: None, text, .. } if &text[..] == "plain")
        );
        assert_eq!(code_language("{.py}"), Some(Language::Python));
        assert_eq!(code_language("ipython3"), Some(Language::Python));
        assert_eq!(code_language("Rust ignore"), Some(Language::Rust));
        assert_eq!(code_language("brainfuck"), None);
    }

    #[test]
    fn quotes_lists_and_task_lists_nest() {
        let blocks =
            parse("> quoted\n> more\n\n1. one\n2. two\n   - nested\n\n- [x] done\n- [ ] todo\n");
        let Block::Quote(quoted) = &blocks[0] else {
            panic!("{blocks:?}");
        };
        assert_eq!(text(&quoted[0]).1, "quoted more");
        let Block::List { start, items } = &blocks[1] else {
            panic!("{blocks:?}");
        };
        assert_eq!(*start, Some(1));
        assert_eq!(text(&items[0].blocks[0]).1, "one");
        let Block::List {
            start: None,
            items: nested,
        } = &items[1].blocks[1]
        else {
            panic!("{items:?}");
        };
        assert_eq!(text(&nested[0].blocks[0]).1, "nested");
        let Block::List { items: tasks, .. } = &blocks[2] else {
            panic!("{blocks:?}");
        };
        assert_eq!(tasks[0].checked, Some(true));
        assert_eq!(tasks[1].checked, Some(false));
        assert_eq!(text(&tasks[1].blocks[0]).1, "todo");
    }

    #[test]
    fn tables_keep_their_cells() {
        let blocks = parse("| a | **b** |\n|---|:-:|\n| 1 | 2 |\n| 3 |\n");
        let Block::Table { head, rows } = &blocks[0] else {
            panic!("{blocks:?}");
        };
        assert_eq!(
            head.iter().map(|c| &c.text[..]).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(head[1].styles[0].1.bold);
        assert_eq!(rows.len(), 2);
        assert_eq!(&rows[0][1].text[..], "2");
    }

    #[test]
    fn links_mark_their_text_and_resolve_their_target() {
        let blocks = parse("See [the docs](https://example.org/x) and [notes](notes.md#top).");
        let paragraph = inline(&blocks[0]);
        assert_eq!(&paragraph.text[..], "See the docs and notes.");
        let links: Vec<_> = paragraph
            .links
            .iter()
            .map(|(range, href)| (&paragraph.text[range.clone()], href.as_str()))
            .collect();
        assert_eq!(
            links,
            [
                ("the docs", "https://example.org/x"),
                ("notes", "notes.md#top")
            ]
        );
        assert!(
            paragraph
                .styles
                .iter()
                .any(|(range, style)| style.link && range.start == 4)
        );

        let base = Path::new("/tmp/nb");
        assert_eq!(
            link_target("https://example.org/x", Some(base)),
            Some(LinkTarget::Url("https://example.org/x".into()))
        );
        assert_eq!(
            link_target("mailto:a@b.c", None),
            Some(LinkTarget::Url("mailto:a@b.c".into()))
        );
        assert_eq!(
            link_target("notes.md#top", Some(base)),
            Some(LinkTarget::File(base.join("notes.md")))
        );
        assert_eq!(link_target("#top", Some(base)), None);
        assert_eq!(link_target("../secret.txt", Some(base)), None);
        assert_eq!(link_target("notes.md", None), None);
    }

    #[test]
    fn images_come_from_attachments_data_uris_and_files() {
        let blocks = parse(
            "Before ![a plot](attachment:plot.png) after\n\n<img src=\"data:image/png;base64,x\" alt=\"inline\">\n",
        );
        assert_eq!(text(&blocks[0]).1, "Before ");
        assert_eq!(
            blocks[1],
            Block::Image {
                src: "attachment:plot.png".into(),
                alt: "a plot".into()
            }
        );
        assert_eq!(text(&blocks[2]).1, " after");
        assert!(matches!(&blocks[3], Block::Image { alt, .. } if alt == "inline"));

        let attachments = json!({"plot.png": {"image/png": PNG}});
        let images = MarkdownImages {
            attachments: Some(&attachments),
            base_dir: None,
        };
        let image = resolve_image("attachment:plot.png", &images).expect("decoded");
        assert_eq!(image.size, Some((2., 3.)));
        let again = resolve_image("attachment:plot.png", &images).unwrap();
        assert!(Arc::ptr_eq(&image.image, &again.image), "cached");
        assert!(resolve_image("attachment:missing.png", &images).is_none());
        let uri = format!("data:image/png;base64,{PNG}");
        assert_eq!(resolve_image(&uri, &images).unwrap().size, Some((2., 3.)));
        assert!(resolve_image("https://example.org/a.png", &images).is_none());

        let dir = std::env::temp_dir().join("next_code-markdown-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("p.png"),
            crate::images::decode_base64(PNG).unwrap(),
        )
        .unwrap();
        let files = MarkdownImages {
            attachments: None,
            base_dir: Some(&dir),
        };
        assert_eq!(resolve_image("p.png", &files).unwrap().size, Some((2., 3.)));
        assert!(
            resolve_image("../p.png", &files).is_none(),
            "stays under the folder"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn html_and_math_show_as_text() {
        let blocks = parse("<div>\nHello <b>world</b>\n</div>\n\nArea $\\pi r^2$ and a<br>break\n");
        assert_eq!(text(&blocks[0]).1, "Hello world");
        let paragraph = inline(&blocks[1]);
        assert_eq!(&paragraph.text[..], "Area \\pi r^2 and a\nbreak");
        assert!(paragraph.styles.iter().any(|(_, style)| style.code));
    }

    struct Markdown(&'static str);

    impl Render for Markdown {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(markdown("md", self.0, MarkdownImages::default()))
        }
    }

    #[gpui::test]
    fn every_construct_renders(cx: &mut TestAppContext) {
        let text = "# H1\n## H2\n### H3\n#### H4\n\nText *em* **b** ~~s~~ `c` [link](https://x.org) $m$\n\n```python\nx = 1\n```\n\n> quote\n\n1. a\n2. b\n   - c\n- [x] t\n\n---\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n![alt](missing.png)\n\n<p>html</p>\n";
        cx.add_window(|_, _| Markdown(text));
        cx.run_until_parked();
    }
}
