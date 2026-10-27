//! The safe subset of HTML the editor renders natively (what pandas,
//! Styler, `IPython.display.HTML` snippets and simple reprs produce):
//! text blocks, lists, quotes, rules, tables, `data:` images and inline
//! `<svg>`, with inline styles limited to `color`, `background(-color)`,
//! `font-weight` and `text-align` (plus `#id` rules of `<style>` blocks,
//! which is how Styler colours cells). Everything else is reported as
//! [`Unsupported`] with the reason, so the output falls back to its text.
//!
//! No crate: outputs are generated HTML (well formed, or close to it), so a
//! small tolerant tokenizer and tree builder (implied end tags for `p`,
//! `li`, table rows and cells; void elements; raw text in `script`/`style`)
//! is all it takes, and it never runs or fetches anything.

use std::{collections::HashMap, ops::Range};

/// Larger HTML is not parsed (it falls back to text).
pub const MAX_HTML_BYTES: usize = 4 << 20;
/// Rows of one table that are rendered; the rest are counted in
/// [`Table::hidden_rows`].
pub const MAX_TABLE_ROWS: usize = 500;

/// An sRGB colour with alpha (0-1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: f32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TextStyle {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub underline: bool,
    pub strike: bool,
    pub color: Option<Color>,
    pub background: Option<Color>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// A run of text with styled byte ranges and links (`href`s).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Inline {
    pub text: String,
    pub styles: Vec<(Range<usize>, TextStyle)>,
    pub links: Vec<(Range<usize>, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    Paragraph,
    Heading(u8),
    /// `<pre>`: monospace, whitespace kept.
    Pre,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Text {
        kind: TextKind,
        inline: Inline,
        align: Option<Align>,
    },
    List {
        /// `Some(first number)` for `<ol>`.
        start: Option<u64>,
        items: Vec<Vec<Block>>,
    },
    Quote(Vec<Block>),
    Rule,
    Image(Image),
    Table(Table),
    /// A container with a background colour (other containers are
    /// flattened into their parent).
    Box {
        blocks: Vec<Block>,
        background: Color,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Image {
    pub source: ImageSource,
    pub alt: String,
    /// The `width`/`height` attributes in pixels.
    pub width: Option<f32>,
    pub height: Option<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ImageSource {
    /// A `data:` URI of an image type: its payload (base64 or decoded
    /// text).
    Data {
        mime: String,
        data: String,
        base64: bool,
    },
    /// An inline `<svg>` element's source.
    Svg(String),
    /// Anything else (`http(s):`, relative paths): shown as a link, never
    /// fetched.
    Remote(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub caption: Option<Inline>,
    pub rows: Vec<Row>,
    pub columns: usize,
    /// Per column: every body value is a number (or a missing-value
    /// marker), so the column is right-aligned.
    pub numeric: Vec<bool>,
    /// Rows past [`MAX_TABLE_ROWS`] that are not kept.
    pub hidden_rows: usize,
    /// Column widths once measured ([`layout_tables`]); empty before.
    pub widths: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// In `<thead>`, or a leading row of header cells only.
    pub head: bool,
    /// pandas' truncation row (every cell is `...`).
    pub ellipsis: bool,
    pub cells: Vec<TableCell>,
    /// What each column of the row shows, left to right.
    pub slots: Vec<Slot>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableCell {
    pub inline: Inline,
    /// `<th>`.
    pub header: bool,
    pub column: usize,
    pub colspan: usize,
    pub rowspan: usize,
    pub align: Option<Align>,
    pub background: Option<Color>,
}

/// One position of a row: `span` columns from `column`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub column: usize,
    pub span: usize,
    pub kind: SlotKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    /// `rows[row].cells[index]` starts here.
    Cell(usize),
    /// Covered by `rows[row].cells[cell]` (a `rowspan` from above).
    Covered { row: usize, cell: usize },
    /// No cell (a short row).
    Empty,
}

/// Why HTML is not rendered natively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    pub reason: String,
}

impl Unsupported {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// `html` as blocks, or why it needs a browser.
pub fn parse(html: &str) -> Result<Vec<Block>, Unsupported> {
    if html.len() > MAX_HTML_BYTES {
        return Err(Unsupported::new("it is too large"));
    }
    let root = build_tree(html)?;
    let mut rules = HashMap::new();
    collect_rules(&root, &mut rules);
    let converter = Converter { rules: &rules };
    let mut out = Vec::new();
    let mut inline = InlineBuilder::default();
    converter.children(&root.children, Ctx::default(), &mut inline, &mut out)?;
    inline.flush(TextKind::Paragraph, &mut out);
    Ok(out)
}

// -- tokenizer and tree ---------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Text(String),
    Element(Element),
}

#[derive(Debug, Clone, PartialEq, Default)]
struct Element {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<Node>,
    /// `style` text, `svg` source.
    raw: Option<String>,
}

impl Element {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Elements whose content is raw text up to their end tag.
const RAW_TEXT: &[&str] = &["script", "style", "textarea", "title", "xmp", "noscript"];

/// Elements that close an open `<p>`.
const CLOSES_P: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "center",
    "details",
    "div",
    "dl",
    "fieldset",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "ul",
];

/// Parses `html` into a tree under a nameless root.
fn build_tree(html: &str) -> Result<Element, Unsupported> {
    let mut stack: Vec<Element> = vec![Element::default()];
    let bytes = html.as_bytes();
    let mut at = 0;
    let mut text_start = 0;
    let flush_text = |stack: &mut Vec<Element>, text: &str| {
        if text.is_empty() {
            return;
        }
        let decoded = decode_entities(text);
        let children = &mut stack.last_mut().expect("root stays").children;
        if let Some(Node::Text(previous)) = children.last_mut() {
            previous.push_str(&decoded);
        } else {
            children.push(Node::Text(decoded));
        }
    };
    while at < bytes.len() {
        if bytes[at] != b'<' {
            at += 1;
            continue;
        }
        let next = bytes.get(at + 1).copied().unwrap_or(0);
        if next == b'!' || next == b'?' {
            flush_text(&mut stack, &html[text_start..at]);
            at = if html[at..].starts_with("<!--") {
                html[at + 4..]
                    .find("-->")
                    .map_or(bytes.len(), |end| at + 4 + end + 3)
            } else {
                html[at..].find('>').map_or(bytes.len(), |end| at + end + 1)
            };
            text_start = at;
            continue;
        }
        if next == b'/' {
            let Some((name, end)) = tag_name(html, at + 2) else {
                at += 1;
                continue;
            };
            flush_text(&mut stack, &html[text_start..at]);
            at = html[end..]
                .find('>')
                .map_or(bytes.len(), |close| end + close + 1);
            text_start = at;
            close_element(&mut stack, &name);
            continue;
        }
        let Some((name, after_name)) = tag_name(html, at + 1) else {
            at += 1;
            continue;
        };
        flush_text(&mut stack, &html[text_start..at]);
        let tag_start = at;
        let (attrs, self_closing, tag_end) = attributes(html, after_name);
        at = tag_end;
        text_start = at;
        if name == "svg" {
            // Kept as source: its own (XML) syntax is the image's business.
            let end = svg_end(html, at);
            let mut element = Element {
                name,
                attrs,
                ..Element::default()
            };
            element.raw = Some(html[tag_start..end].to_owned());
            stack
                .last_mut()
                .expect("root stays")
                .children
                .push(Node::Element(element));
            at = end;
            text_start = at;
            continue;
        }
        open_implied_ends(&mut stack, &name);
        let mut element = Element {
            name,
            attrs,
            ..Element::default()
        };
        if RAW_TEXT.contains(&element.name.as_str()) && !self_closing {
            let close = format!("</{}", element.name);
            let content_end = find_ascii_case_insensitive(&html[at..], &close)
                .map_or(bytes.len(), |end| at + end);
            element.raw = Some(html[at..content_end].to_owned());
            at = html[content_end..]
                .find('>')
                .map_or(bytes.len(), |end| content_end + end + 1);
            text_start = at;
            stack
                .last_mut()
                .expect("root stays")
                .children
                .push(Node::Element(element));
            continue;
        }
        if self_closing || VOID.contains(&element.name.as_str()) {
            stack
                .last_mut()
                .expect("root stays")
                .children
                .push(Node::Element(element));
        } else {
            stack.push(element);
        }
    }
    flush_text(&mut stack, &html[text_start..]);
    while stack.len() > 1 {
        pop(&mut stack);
    }
    Ok(stack.pop().expect("root stays"))
}

fn pop(stack: &mut Vec<Element>) {
    let element = stack.pop().expect("not the root");
    stack
        .last_mut()
        .expect("root stays")
        .children
        .push(Node::Element(element));
}

/// The lowercase tag name at `at` and where it ends; `None` when no name
/// starts there (a literal `<`).
fn tag_name(html: &str, at: usize) -> Option<(String, usize)> {
    let rest = html.get(at..)?;
    if !rest.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let len = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == ':' || c == '_'))
        .unwrap_or(rest.len());
    Some((rest[..len].to_ascii_lowercase(), at + len))
}

/// A start tag's attributes from `at` (after the name): (attributes with
/// lowercase names, self-closing, the offset after `>`).
fn attributes(html: &str, mut at: usize) -> (Vec<(String, String)>, bool, usize) {
    let bytes = html.as_bytes();
    let mut attrs = Vec::new();
    loop {
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        match bytes.get(at) {
            None => return (attrs, false, bytes.len()),
            Some(b'>') => return (attrs, false, at + 1),
            Some(b'/') if bytes.get(at + 1) == Some(&b'>') => return (attrs, true, at + 2),
            Some(b'/') => {
                at += 1;
                continue;
            }
            _ => {}
        }
        let name_start = at;
        while at < bytes.len()
            && !bytes[at].is_ascii_whitespace()
            && !matches!(bytes[at], b'=' | b'>')
            && !(bytes[at] == b'/' && bytes.get(at + 1) == Some(&b'>'))
        {
            at += 1;
        }
        let name = html[name_start..at].to_ascii_lowercase();
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        let mut value = String::new();
        if bytes.get(at) == Some(&b'=') {
            at += 1;
            while at < bytes.len() && bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            match bytes.get(at) {
                Some(&quote @ (b'"' | b'\'')) => {
                    let start = at + 1;
                    let end = html[start..]
                        .find(quote as char)
                        .map_or(bytes.len(), |end| start + end);
                    value = decode_entities(&html[start..end]);
                    at = (end + 1).min(bytes.len());
                }
                _ => {
                    let start = at;
                    while at < bytes.len() && !bytes[at].is_ascii_whitespace() && bytes[at] != b'>'
                    {
                        at += 1;
                    }
                    value = decode_entities(&html[start..at]);
                }
            }
        }
        if !name.is_empty() {
            attrs.push((name, value));
        }
    }
}

/// The offset after the `</svg>` closing the `<svg>` whose content starts
/// at `at` (nested `<svg>`s counted).
fn svg_end(html: &str, mut at: usize) -> usize {
    let mut depth = 1;
    let lower = html.to_ascii_lowercase();
    while depth > 0 {
        let open = lower[at..].find("<svg").map(|i| at + i);
        let Some(close) = lower[at..].find("</svg").map(|i| at + i) else {
            return html.len();
        };
        match open {
            Some(open) if open < close => {
                depth += 1;
                at = open + 4;
            }
            _ => {
                depth -= 1;
                at = lower[close..]
                    .find('>')
                    .map_or(html.len(), |end| close + end + 1);
            }
        }
    }
    at
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let needle = needle.as_bytes();
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

/// Closes what a start tag `name` ends implicitly (`<p>`, list items, table
/// rows and cells, definition terms).
fn open_implied_ends(stack: &mut Vec<Element>, name: &str) {
    let open = |stack: &[Element], names: &[&str], stop: &[&str]| {
        stack
            .iter()
            .rposition(|element| names.contains(&element.name.as_str()))
            .filter(|&index| {
                !stack[index + 1..]
                    .iter()
                    .any(|element| stop.contains(&element.name.as_str()))
            })
    };
    let close_to = |stack: &mut Vec<Element>, index: usize| {
        while stack.len() > index {
            pop(stack);
        }
    };
    if CLOSES_P.contains(&name)
        && let Some(index) = open(stack, &["p"], &["table", "td", "th", "li", "div"])
    {
        close_to(stack, index);
    }
    match name {
        "li" => {
            if let Some(index) = open(stack, &["li"], &["ul", "ol"]) {
                close_to(stack, index);
            }
        }
        "dt" | "dd" => {
            if let Some(index) = open(stack, &["dt", "dd"], &["dl"]) {
                close_to(stack, index);
            }
        }
        "td" | "th" => {
            if let Some(index) = open(stack, &["td", "th"], &["tr", "table"]) {
                close_to(stack, index);
            }
        }
        "tr" => {
            if let Some(index) = open(stack, &["tr"], &["table"]) {
                close_to(stack, index);
            }
        }
        "thead" | "tbody" | "tfoot" => {
            if let Some(index) = open(stack, &["thead", "tbody", "tfoot", "tr"], &["table"]) {
                close_to(stack, index);
            }
        }
        _ => {}
    }
}

/// An end tag: closes up to the innermost open element of that name (none
/// open: ignored; a table boundary in between: ignored).
fn close_element(stack: &mut Vec<Element>, name: &str) {
    let Some(index) = stack
        .iter()
        .skip(1)
        .rposition(|element| element.name == name)
        .map(|index| index + 1)
    else {
        return;
    };
    let crosses_table = name != "table"
        && stack[index + 1..]
            .iter()
            .any(|element| element.name == "table");
    if crosses_table {
        return;
    }
    while stack.len() > index {
        pop(stack);
    }
}

// -- entities -------------------------------------------------------------

const ENTITIES: &[(&str, &str)] = &[
    ("amp", "&"),
    ("lt", "<"),
    ("gt", ">"),
    ("quot", "\""),
    ("apos", "'"),
    ("nbsp", "\u{a0}"),
    ("ensp", "\u{2002}"),
    ("emsp", "\u{2003}"),
    ("thinsp", "\u{2009}"),
    ("copy", "©"),
    ("reg", "®"),
    ("trade", "™"),
    ("hellip", "…"),
    ("mdash", "—"),
    ("ndash", "–"),
    ("lsquo", "‘"),
    ("rsquo", "’"),
    ("ldquo", "“"),
    ("rdquo", "”"),
    ("laquo", "«"),
    ("raquo", "»"),
    ("times", "×"),
    ("divide", "÷"),
    ("minus", "−"),
    ("plusmn", "±"),
    ("deg", "°"),
    ("middot", "·"),
    ("bull", "•"),
    ("euro", "€"),
    ("pound", "£"),
    ("yen", "¥"),
    ("cent", "¢"),
    ("sect", "§"),
    ("para", "¶"),
    ("micro", "µ"),
    ("frac12", "½"),
    ("frac14", "¼"),
    ("frac34", "¾"),
    ("sup1", "¹"),
    ("sup2", "²"),
    ("sup3", "³"),
    ("larr", "←"),
    ("rarr", "→"),
    ("uarr", "↑"),
    ("darr", "↓"),
    ("harr", "↔"),
    ("le", "≤"),
    ("ge", "≥"),
    ("ne", "≠"),
    ("asymp", "≈"),
    ("infin", "∞"),
    ("check", "✓"),
    ("alpha", "α"),
    ("beta", "β"),
    ("gamma", "γ"),
    ("delta", "δ"),
    ("epsilon", "ε"),
    ("lambda", "λ"),
    ("mu", "μ"),
    ("pi", "π"),
    ("sigma", "σ"),
    ("theta", "θ"),
    ("omega", "ω"),
    ("Delta", "Δ"),
    ("Sigma", "Σ"),
    ("Omega", "Ω"),
];

/// `&name;`, `&#N;` and `&#xN;` decoded; unknown references kept as is.
pub fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let end = rest[1..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '#'))
            .map_or(rest.len(), |end| end + 1);
        let name = &rest[1..end];
        let terminated = rest[end..].starts_with(';');
        let decoded = if let Some(number) = name.strip_prefix('#') {
            let code = match number.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => number.parse().ok(),
            };
            code.and_then(char::from_u32)
                .filter(|c| *c != '\0')
                .map(String::from)
        } else {
            ENTITIES
                .iter()
                .find(|(entity, _)| *entity == name)
                .map(|(_, value)| (*value).to_owned())
        };
        match decoded {
            Some(decoded) if !name.is_empty() => {
                out.push_str(&decoded);
                rest = &rest[end + usize::from(terminated)..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

// -- CSS subset -----------------------------------------------------------

/// The declarations the editor honours.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Decls {
    color: Option<Color>,
    background: Option<Color>,
    bold: Option<bool>,
    align: Option<Align>,
}

impl Decls {
    /// `other`'s declarations over these.
    fn merged(self, other: Decls) -> Decls {
        Decls {
            color: other.color.or(self.color),
            background: other.background.or(self.background),
            bold: other.bold.or(self.bold),
            align: other.align.or(self.align),
        }
    }
}

fn parse_declarations(css: &str) -> Decls {
    let mut decls = Decls::default();
    for declaration in css.split(';') {
        let Some((property, value)) = declaration.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_end_matches("!important").trim();
        match property.trim().to_ascii_lowercase().as_str() {
            "color" => decls.color = parse_color(value),
            "background-color" => decls.background = parse_color(value),
            "background" => {
                // Only a plain colour (no images, gradients).
                if let Some(color) = parse_color(value) {
                    decls.background = Some(color);
                }
            }
            "font-weight" => {
                decls.bold = match value.to_ascii_lowercase().as_str() {
                    "bold" | "bolder" => Some(true),
                    "normal" | "lighter" => Some(false),
                    number => number.parse::<u32>().ok().map(|weight| weight >= 600),
                }
            }
            "text-align" => decls.align = parse_align(value),
            _ => {}
        }
    }
    decls
}

fn parse_align(value: &str) -> Option<Align> {
    match value.trim().to_ascii_lowercase().as_str() {
        "left" | "start" | "justify" => Some(Align::Left),
        "center" | "middle" => Some(Align::Center),
        "right" | "end" => Some(Align::Right),
        _ => None,
    }
}

/// `#id` rules of `<style>` blocks (Styler colours cells this way); other
/// selectors are ignored.
fn collect_rules(element: &Element, rules: &mut HashMap<String, Decls>) {
    for child in &element.children {
        let Node::Element(child) = child else {
            continue;
        };
        if child.name == "style" {
            if let Some(css) = &child.raw {
                parse_id_rules(css, rules);
            }
        } else {
            collect_rules(child, rules);
        }
    }
}

fn parse_id_rules(css: &str, rules: &mut HashMap<String, Decls>) {
    let mut rest = css;
    // Comments out first.
    let mut cleaned = String::new();
    while let Some(start) = rest.find("/*") {
        cleaned.push_str(&rest[..start]);
        rest = rest[start + 2..]
            .find("*/")
            .map_or("", |end| &rest[start + 2 + end + 2..]);
    }
    cleaned.push_str(rest);
    let mut rest = cleaned.as_str();
    while let Some(open) = rest.find('{') {
        let selectors = &rest[..open];
        let Some(close) = rest[open..].find('}') else {
            break;
        };
        let body = &rest[open + 1..open + close];
        rest = &rest[open + close + 1..];
        if body.contains('{') {
            // `@media { … }` and the like.
            continue;
        }
        let decls = parse_declarations(body);
        for selector in selectors.split(',') {
            let selector = selector.trim();
            if let Some(id) = selector.strip_prefix('#')
                && !id.is_empty()
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                let entry = rules.entry(id.to_owned()).or_default();
                *entry = entry.merged(decls);
            }
        }
    }
}

const NAMED_COLORS: &[(&str, u32)] = &[
    ("aqua", 0x00ffff),
    ("beige", 0xf5f5dc),
    ("black", 0x000000),
    ("blue", 0x0000ff),
    ("brown", 0xa52a2a),
    ("coral", 0xff7f50),
    ("crimson", 0xdc143c),
    ("cyan", 0x00ffff),
    ("darkblue", 0x00008b),
    ("darkgray", 0xa9a9a9),
    ("darkgreen", 0x006400),
    ("darkgrey", 0xa9a9a9),
    ("darkorange", 0xff8c00),
    ("darkred", 0x8b0000),
    ("dimgray", 0x696969),
    ("dodgerblue", 0x1e90ff),
    ("firebrick", 0xb22222),
    ("forestgreen", 0x228b22),
    ("fuchsia", 0xff00ff),
    ("gainsboro", 0xdcdcdc),
    ("gold", 0xffd700),
    ("gray", 0x808080),
    ("green", 0x008000),
    ("grey", 0x808080),
    ("hotpink", 0xff69b4),
    ("indigo", 0x4b0082),
    ("ivory", 0xfffff0),
    ("khaki", 0xf0e68c),
    ("lavender", 0xe6e6fa),
    ("lightblue", 0xadd8e6),
    ("lightcoral", 0xf08080),
    ("lightgray", 0xd3d3d3),
    ("lightgreen", 0x90ee90),
    ("lightgrey", 0xd3d3d3),
    ("lightyellow", 0xffffe0),
    ("lime", 0x00ff00),
    ("limegreen", 0x32cd32),
    ("magenta", 0xff00ff),
    ("maroon", 0x800000),
    ("navy", 0x000080),
    ("olive", 0x808000),
    ("orange", 0xffa500),
    ("orangered", 0xff4500),
    ("orchid", 0xda70d6),
    ("pink", 0xffc0cb),
    ("plum", 0xdda0dd),
    ("purple", 0x800080),
    ("red", 0xff0000),
    ("royalblue", 0x4169e1),
    ("salmon", 0xfa8072),
    ("seagreen", 0x2e8b57),
    ("silver", 0xc0c0c0),
    ("skyblue", 0x87ceeb),
    ("slategray", 0x708090),
    ("steelblue", 0x4682b4),
    ("tan", 0xd2b48c),
    ("teal", 0x008080),
    ("tomato", 0xff6347),
    ("turquoise", 0x40e0d0),
    ("violet", 0xee82ee),
    ("wheat", 0xf5deb3),
    ("white", 0xffffff),
    ("whitesmoke", 0xf5f5f5),
    ("yellow", 0xffff00),
    ("yellowgreen", 0x9acd32),
];

/// `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`, `rgb()`/`rgba()` and common
/// colour names; `transparent` and anything else: `None`.
pub fn parse_color(value: &str) -> Option<Color> {
    let value = value.trim().to_ascii_lowercase();
    let rgb = |rgb: u32| Color {
        r: (rgb >> 16) as u8,
        g: (rgb >> 8) as u8,
        b: rgb as u8,
        a: 1.,
    };
    if let Some(hex) = value.strip_prefix('#') {
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let digit = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok().map(|d| d * 17);
        let pair = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        return match hex.len() {
            3 | 4 => Some(Color {
                r: digit(0)?,
                g: digit(1)?,
                b: digit(2)?,
                a: if hex.len() == 4 {
                    digit(3)? as f32 / 255.
                } else {
                    1.
                },
            }),
            6 | 8 => Some(Color {
                r: pair(0)?,
                g: pair(2)?,
                b: pair(4)?,
                a: if hex.len() == 8 {
                    pair(6)? as f32 / 255.
                } else {
                    1.
                },
            }),
            _ => None,
        };
    }
    if let Some(args) = value
        .strip_prefix("rgba(")
        .or_else(|| value.strip_prefix("rgb("))
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let parts: Vec<&str> = args
            .split([',', ' ', '/'])
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();
        if parts.len() < 3 {
            return None;
        }
        let channel = |part: &str| -> Option<u8> {
            match part.strip_suffix('%') {
                Some(percent) => Some((percent.parse::<f32>().ok()? * 2.55).round() as u8),
                None => Some(part.parse::<f32>().ok()?.clamp(0., 255.).round() as u8),
            }
        };
        let alpha = match parts.get(3) {
            Some(part) => match part.strip_suffix('%') {
                Some(percent) => percent.parse::<f32>().ok()? / 100.,
                None => part.parse::<f32>().ok()?,
            },
            None => 1.,
        };
        return Some(Color {
            r: channel(parts[0])?,
            g: channel(parts[1])?,
            b: channel(parts[2])?,
            a: alpha.clamp(0., 1.),
        });
    }
    NAMED_COLORS
        .iter()
        .find(|(name, _)| *name == value)
        .map(|(_, value)| rgb(*value))
}

// -- inline text ----------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
struct Ctx {
    style: TextStyle,
    align: Option<Align>,
    pre: bool,
}

#[derive(Default)]
struct InlineBuilder {
    inline: Inline,
    align: Option<Align>,
    /// The last character pushed is collapsible whitespace (or nothing was
    /// pushed on this line yet).
    at_space: bool,
    started: bool,
}

impl InlineBuilder {
    fn push(&mut self, text: &str, ctx: &Ctx) {
        if text.is_empty() {
            return;
        }
        if !self.started {
            self.started = true;
            self.at_space = true;
            self.align = ctx.align;
        }
        let start = self.inline.text.len();
        if ctx.pre {
            self.inline.text.push_str(text);
            self.at_space = text.ends_with('\n');
        } else {
            for c in text.chars() {
                if c.is_ascii_whitespace() {
                    if !self.at_space {
                        self.inline.text.push(' ');
                        self.at_space = true;
                    }
                } else {
                    self.inline.text.push(c);
                    self.at_space = false;
                }
            }
        }
        let end = self.inline.text.len();
        if end > start && ctx.style != TextStyle::default() {
            self.inline.styles.push((start..end, ctx.style));
        }
    }

    fn line_break(&mut self, ctx: &Ctx) {
        if !self.started {
            self.started = true;
            self.align = ctx.align;
        }
        self.trim_trailing_space();
        self.inline.text.push('\n');
        self.at_space = true;
    }

    fn trim_trailing_space(&mut self) {
        let trimmed = self.inline.text.trim_end_matches(' ').len();
        if trimmed < self.inline.text.len() {
            self.inline.text.truncate(trimmed);
            clamp_ranges(&mut self.inline);
        }
    }

    /// The text so far (trailing whitespace and line breaks trimmed),
    /// leaving the builder empty.
    fn take(&mut self) -> Option<(Inline, Option<Align>)> {
        if !self.started {
            return None;
        }
        loop {
            self.trim_trailing_space();
            if self.inline.text.ends_with('\n') {
                self.inline.text.pop();
            } else {
                break;
            }
        }
        let builder = std::mem::take(self);
        let mut inline = builder.inline;
        clamp_ranges(&mut inline);
        if inline.text.trim().is_empty() {
            return None;
        }
        Some((inline, builder.align))
    }

    fn flush(&mut self, kind: TextKind, out: &mut Vec<Block>) {
        if let Some((inline, align)) = self.take() {
            out.push(Block::Text {
                kind,
                inline,
                align,
            });
        }
    }
}

/// Styles and links cut to the text's end; empty ones dropped.
fn clamp_ranges(inline: &mut Inline) {
    let end = inline.text.len();
    let clamp = |range: &mut Range<usize>| {
        range.end = range.end.min(end);
        range.start = range.start.min(range.end);
    };
    for (range, _) in &mut inline.styles {
        clamp(range);
    }
    for (range, _) in &mut inline.links {
        clamp(range);
    }
    inline.styles.retain(|(range, _)| !range.is_empty());
    inline.links.retain(|(range, _)| !range.is_empty());
}

// -- conversion -----------------------------------------------------------

/// Elements that are inline text (with a style change, or none).
const INLINE: &[&str] = &[
    "a", "abbr", "b", "bdi", "bdo", "big", "cite", "code", "data", "del", "dfn", "em", "font", "i",
    "ins", "kbd", "label", "mark", "q", "s", "samp", "small", "span", "strike", "strong", "sub",
    "sup", "time", "tt", "u", "var", "nobr",
];

/// Containers rendered as their content.
const CONTAINERS: &[&str] = &[
    "html",
    "body",
    "div",
    "section",
    "article",
    "aside",
    "header",
    "footer",
    "main",
    "nav",
    "figure",
    "figcaption",
    "center",
    "address",
    "details",
    "summary",
    "dl",
    "dt",
    "dd",
    "fieldset",
    "legend",
    "hgroup",
];

/// Elements skipped with their content.
const IGNORED: &[&str] = &[
    "head", "style", "meta", "link", "title", "base", "colgroup", "col", "noscript", "template",
];

/// Why an element needs a browser.
fn rejection(name: &str) -> Option<&'static str> {
    Some(match name {
        "script" => "it runs JavaScript (<script>)",
        "iframe" | "frame" | "frameset" => "it embeds a web page (<iframe>)",
        "canvas" => "it draws with JavaScript (<canvas>)",
        "object" | "embed" | "applet" => "it embeds a plugin (<object>)",
        "video" | "audio" => "it plays media",
        "form" | "input" | "button" | "select" | "textarea" | "option" => {
            "it has interactive controls"
        }
        "math" => "it uses MathML",
        _ => return None,
    })
}

struct Converter<'a> {
    rules: &'a HashMap<String, Decls>,
}

impl Converter<'_> {
    /// An element's style declarations: its `#id` rule, then its `style`
    /// attribute; `align` (and `halign`) attributes as `text-align`.
    fn decls(&self, element: &Element) -> Decls {
        let mut decls = Decls::default();
        if let Some(align) = element
            .attr("align")
            .or_else(|| element.attr("halign"))
            .and_then(parse_align)
        {
            decls.align = Some(align);
        }
        if let Some(color) = element.attr("bgcolor").and_then(parse_color) {
            decls.background = Some(color);
        }
        if element.name == "font"
            && let Some(color) = element.attr("color").and_then(parse_color)
        {
            decls.color = Some(color);
        }
        if let Some(rule) = element.attr("id").and_then(|id| self.rules.get(id)) {
            decls = decls.merged(*rule);
        }
        if let Some(style) = element.attr("style") {
            decls = decls.merged(parse_declarations(style));
        }
        decls
    }

    fn children(
        &self,
        children: &[Node],
        ctx: Ctx,
        inline: &mut InlineBuilder,
        out: &mut Vec<Block>,
    ) -> Result<(), Unsupported> {
        for child in children {
            match child {
                Node::Text(text) => inline.push(text, &ctx),
                Node::Element(element) => self.element(element, ctx, inline, out)?,
            }
        }
        Ok(())
    }

    fn element(
        &self,
        element: &Element,
        mut ctx: Ctx,
        inline: &mut InlineBuilder,
        out: &mut Vec<Block>,
    ) -> Result<(), Unsupported> {
        let name = element.name.as_str();
        if let Some(reason) = rejection(name) {
            return Err(Unsupported::new(reason));
        }
        if IGNORED.contains(&name) {
            return Ok(());
        }
        let decls = self.decls(element);
        if let Some(color) = decls.color {
            ctx.style.color = Some(color);
        }
        if let Some(bold) = decls.bold {
            ctx.style.bold = bold;
        }
        if let Some(align) = decls.align {
            ctx.align = Some(align);
        }
        if INLINE.contains(&name) {
            if let Some(background) = decls.background {
                ctx.style.background = Some(background);
            }
            match name {
                "b" | "strong" => ctx.style.bold = decls.bold.unwrap_or(true),
                "i" | "em" | "cite" | "dfn" | "var" => ctx.style.italic = true,
                "code" | "kbd" | "samp" | "tt" => ctx.style.code = true,
                "u" | "ins" => ctx.style.underline = true,
                "s" | "strike" | "del" => ctx.style.strike = true,
                "mark" if decls.background.is_none() => {
                    ctx.style.background = parse_color("#ffff0066");
                }
                _ => {}
            }
            let href = (name == "a")
                .then(|| element.attr("href"))
                .flatten()
                .filter(|href| !href.trim().is_empty());
            if href.is_some() {
                ctx.style.underline = true;
            }
            let start = inline.inline.text.len();
            self.children(&element.children, ctx, inline, out)?;
            if let Some(href) = href {
                let end = inline.inline.text.len();
                if end > start {
                    inline
                        .inline
                        .links
                        .push((start..end, href.trim().to_owned()));
                }
            }
            return Ok(());
        }
        match name {
            "br" => inline.line_break(&ctx),
            "wbr" => {}
            "hr" => {
                inline.flush(TextKind::Paragraph, out);
                out.push(Block::Rule);
            }
            "img" => {
                inline.flush(TextKind::Paragraph, out);
                out.push(Block::Image(image(element)));
            }
            "svg" => {
                inline.flush(TextKind::Paragraph, out);
                out.push(Block::Image(Image {
                    source: ImageSource::Svg(element.raw.clone().unwrap_or_default()),
                    alt: String::new(),
                    width: None,
                    height: None,
                }));
            }
            "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "pre" | "caption" => {
                inline.flush(TextKind::Paragraph, out);
                let kind = match name {
                    "pre" => {
                        ctx.pre = true;
                        ctx.style.code = false;
                        TextKind::Pre
                    }
                    "p" | "caption" => TextKind::Paragraph,
                    heading => {
                        ctx.style.bold = true;
                        TextKind::Heading(heading.as_bytes()[1] - b'0')
                    }
                };
                let mut own = InlineBuilder::default();
                let mut blocks = Vec::new();
                let children = match (&element.children[..], kind) {
                    // A line break right after `<pre>` is not content.
                    ([Node::Text(first), rest @ ..], TextKind::Pre) if first.starts_with('\n') => {
                        own.push(&first[1..], &ctx);
                        rest
                    }
                    (children, _) => children,
                };
                self.children(children, ctx, &mut own, &mut blocks)?;
                own.flush(kind, &mut blocks);
                self.boxed(blocks, decls.background, out);
            }
            "ul" | "ol" | "menu" => {
                inline.flush(TextKind::Paragraph, out);
                let start = (name == "ol").then(|| {
                    element
                        .attr("start")
                        .and_then(|start| start.trim().parse().ok())
                        .unwrap_or(1)
                });
                let mut items = Vec::new();
                let mut loose = InlineBuilder::default();
                let mut loose_blocks = Vec::new();
                for child in &element.children {
                    match child {
                        Node::Element(item) if item.name == "li" => {
                            loose.flush(TextKind::Paragraph, &mut loose_blocks);
                            if !loose_blocks.is_empty() {
                                items.push(std::mem::take(&mut loose_blocks));
                            }
                            let item_ctx = {
                                let mut item_ctx = ctx;
                                let decls = self.decls(item);
                                if let Some(color) = decls.color {
                                    item_ctx.style.color = Some(color);
                                }
                                if let Some(bold) = decls.bold {
                                    item_ctx.style.bold = bold;
                                }
                                item_ctx
                            };
                            let mut own = InlineBuilder::default();
                            let mut blocks = Vec::new();
                            self.children(&item.children, item_ctx, &mut own, &mut blocks)?;
                            own.flush(TextKind::Paragraph, &mut blocks);
                            items.push(blocks);
                        }
                        Node::Text(text) => loose.push(text, &ctx),
                        Node::Element(other) => {
                            self.element(other, ctx, &mut loose, &mut loose_blocks)?
                        }
                    }
                }
                loose.flush(TextKind::Paragraph, &mut loose_blocks);
                if !loose_blocks.is_empty() {
                    items.push(loose_blocks);
                }
                let list = Block::List { start, items };
                self.boxed(vec![list], decls.background, out);
            }
            "li" => {
                // Outside a list: a bulleted item of its own.
                inline.flush(TextKind::Paragraph, out);
                let mut own = InlineBuilder::default();
                let mut blocks = Vec::new();
                self.children(&element.children, ctx, &mut own, &mut blocks)?;
                own.flush(TextKind::Paragraph, &mut blocks);
                out.push(Block::List {
                    start: None,
                    items: vec![blocks],
                });
            }
            "blockquote" => {
                inline.flush(TextKind::Paragraph, out);
                let mut own = InlineBuilder::default();
                let mut blocks = Vec::new();
                self.children(&element.children, ctx, &mut own, &mut blocks)?;
                own.flush(TextKind::Paragraph, &mut blocks);
                self.boxed(vec![Block::Quote(blocks)], decls.background, out);
            }
            "table" => {
                inline.flush(TextKind::Paragraph, out);
                let table = self.table(element, ctx)?;
                self.boxed(vec![Block::Table(table)], decls.background, out);
            }
            "thead" | "tbody" | "tfoot" | "tr" | "td" | "th" => {
                // Table parts outside a table: their content as text.
                inline.flush(TextKind::Paragraph, out);
                let mut own = InlineBuilder::default();
                let mut blocks = Vec::new();
                self.children(&element.children, ctx, &mut own, &mut blocks)?;
                own.flush(TextKind::Paragraph, &mut blocks);
                out.extend(blocks);
            }
            container if CONTAINERS.contains(&container) => {
                if matches!(container, "summary" | "dt") {
                    ctx.style.bold = decls.bold.unwrap_or(true);
                }
                inline.flush(TextKind::Paragraph, out);
                let mut own = InlineBuilder::default();
                let mut blocks = Vec::new();
                self.children(&element.children, ctx, &mut own, &mut blocks)?;
                own.flush(TextKind::Paragraph, &mut blocks);
                if container == "dd" {
                    self.boxed(vec![Block::Quote(blocks)], decls.background, out);
                } else {
                    self.boxed(blocks, decls.background, out);
                }
            }
            other => return Err(Unsupported::new(format!("it uses <{other}>"))),
        }
        Ok(())
    }

    /// `blocks` in a [`Block::Box`] when there is a background.
    fn boxed(&self, blocks: Vec<Block>, background: Option<Color>, out: &mut Vec<Block>) {
        match background {
            Some(background) if background.a > 0. && !blocks.is_empty() => {
                out.push(Block::Box { blocks, background })
            }
            _ => out.extend(blocks),
        }
    }

    fn table(&self, table: &Element, ctx: Ctx) -> Result<Table, Unsupported> {
        let mut caption = None;
        // (in thead, row element, inherited decls)
        let mut rows: Vec<(bool, &Element, Decls)> = Vec::new();
        let mut stray = Vec::new();
        for child in &table.children {
            let Node::Element(child) = child else {
                continue;
            };
            match child.name.as_str() {
                "caption" => caption = Some(self.cell_inline(&child.children, ctx)?),
                "thead" | "tbody" | "tfoot" => {
                    let head = child.name == "thead";
                    let section = self.decls(child);
                    for row in &child.children {
                        if let Node::Element(row) = row {
                            match row.name.as_str() {
                                "tr" => rows.push((head, row, section)),
                                name if rejection(name).is_some() => {
                                    return Err(Unsupported::new(rejection(name).unwrap()));
                                }
                                _ => {}
                            }
                        }
                    }
                }
                "tr" => rows.push((false, child, Decls::default())),
                "td" | "th" => stray.push(child),
                "colgroup" | "col" | "style" => {}
                name => {
                    if let Some(reason) = rejection(name) {
                        return Err(Unsupported::new(reason));
                    }
                }
            }
        }
        let total_rows = rows.len();
        let hidden_rows = total_rows.saturating_sub(MAX_TABLE_ROWS);
        rows.truncate(MAX_TABLE_ROWS);
        let mut out_rows = Vec::with_capacity(rows.len() + 1);
        // Cells straight under `<table>` (no `<tr>`): one row.
        if !stray.is_empty() {
            let mut row = Row {
                head: false,
                ellipsis: false,
                cells: Vec::new(),
                slots: Vec::new(),
            };
            for cell in stray {
                row.cells.push(self.cell(cell, ctx, Decls::default())?);
            }
            out_rows.push(row);
        }
        for (head, row, section) in rows {
            let row_decls = section.merged(self.decls(row));
            let mut cells = Vec::new();
            for cell in &row.children {
                if let Node::Element(cell) = cell {
                    match cell.name.as_str() {
                        "td" | "th" => cells.push(self.cell(cell, ctx, row_decls)?),
                        name => {
                            if let Some(reason) = rejection(name) {
                                return Err(Unsupported::new(reason));
                            }
                        }
                    }
                }
            }
            let ellipsis = !cells.is_empty()
                && cells
                    .iter()
                    .all(|cell| matches!(cell.inline.text.trim(), "..." | "…"));
            out_rows.push(Row {
                head,
                ellipsis,
                cells,
                slots: Vec::new(),
            });
        }
        // Leading rows of header cells only are the head too (tables
        // without `<thead>`).
        if !out_rows.iter().any(|row| row.head) {
            for row in &mut out_rows {
                if !row.cells.is_empty() && row.cells.iter().all(|cell| cell.header) {
                    row.head = true;
                } else {
                    break;
                }
            }
        }
        let columns = place_cells(&mut out_rows);
        let numeric = numeric_columns(&out_rows, columns);
        Ok(Table {
            caption,
            rows: out_rows,
            columns,
            numeric,
            hidden_rows,
            widths: Vec::new(),
        })
    }

    fn cell(&self, cell: &Element, mut ctx: Ctx, row: Decls) -> Result<TableCell, Unsupported> {
        let header = cell.name == "th";
        let decls = row.merged(self.decls(cell));
        if let Some(color) = decls.color {
            ctx.style.color = Some(color);
        }
        ctx.style.bold = decls.bold.unwrap_or(header);
        ctx.align = None;
        let span = |name: &str| {
            cell.attr(name)
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(1)
                .clamp(1, 1000)
        };
        Ok(TableCell {
            inline: self.cell_inline(&cell.children, ctx)?,
            header,
            column: 0,
            colspan: span("colspan"),
            rowspan: span("rowspan"),
            align: decls.align,
            background: decls.background.filter(|color| color.a > 0.),
        })
    }

    /// A cell's content as one inline text (blocks on their own lines).
    fn cell_inline(&self, children: &[Node], ctx: Ctx) -> Result<Inline, Unsupported> {
        let mut own = InlineBuilder::default();
        let mut blocks = Vec::new();
        self.children(children, ctx, &mut own, &mut blocks)?;
        own.flush(TextKind::Paragraph, &mut blocks);
        let mut inline = Inline::default();
        flatten(&blocks, &mut inline);
        Ok(inline)
    }
}

/// Blocks as lines of one inline text (table cells).
fn flatten(blocks: &[Block], into: &mut Inline) {
    let append = |into: &mut Inline, other: &Inline| {
        if !into.text.is_empty() {
            into.text.push('\n');
        }
        let offset = into.text.len();
        into.text.push_str(&other.text);
        into.styles.extend(
            other
                .styles
                .iter()
                .map(|(range, style)| (range.start + offset..range.end + offset, *style)),
        );
        into.links.extend(
            other
                .links
                .iter()
                .map(|(range, href)| (range.start + offset..range.end + offset, href.clone())),
        );
    };
    for block in blocks {
        match block {
            Block::Text { inline, .. } => append(into, inline),
            Block::List { items, .. } => {
                for item in items {
                    let mut text = Inline::default();
                    flatten(item, &mut text);
                    let mut bullet = Inline {
                        text: "• ".into(),
                        ..Inline::default()
                    };
                    let offset = bullet.text.len();
                    bullet.text.push_str(&text.text);
                    bullet.styles = text
                        .styles
                        .into_iter()
                        .map(|(range, style)| (range.start + offset..range.end + offset, style))
                        .collect();
                    append(into, &bullet);
                }
            }
            Block::Quote(blocks) | Block::Box { blocks, .. } => flatten(blocks, into),
            Block::Rule => {}
            Block::Image(image) => append(
                into,
                &Inline {
                    text: if image.alt.is_empty() {
                        "[image]".into()
                    } else {
                        format!("[{}]", image.alt)
                    },
                    ..Inline::default()
                },
            ),
            Block::Table(_) => append(
                into,
                &Inline {
                    text: "[table]".into(),
                    ..Inline::default()
                },
            ),
        }
    }
}

fn image(element: &Element) -> Image {
    let src = element.attr("src").unwrap_or_default().trim();
    let size = |name: &str| {
        element.attr(name).and_then(|value| {
            value
                .trim()
                .trim_end_matches("px")
                .parse::<f32>()
                .ok()
                .filter(|v| *v > 0.)
        })
    };
    Image {
        source: data_uri(src).unwrap_or_else(|| ImageSource::Remote(src.to_owned())),
        alt: element.attr("alt").unwrap_or_default().to_owned(),
        width: size("width"),
        height: size("height"),
    }
}

/// A `data:image/…` URI's mime and payload.
fn data_uri(src: &str) -> Option<ImageSource> {
    let rest = src.strip_prefix("data:")?;
    let (header, payload) = rest.split_once(',')?;
    let mut parts = header.split(';');
    let mime = parts.next()?.trim().to_ascii_lowercase();
    if !mime.starts_with("image/") {
        return None;
    }
    let base64 = parts.any(|part| part.trim().eq_ignore_ascii_case("base64"));
    Some(ImageSource::Data {
        mime,
        data: if base64 {
            payload.to_owned()
        } else {
            percent_decode(payload)
        },
        base64,
    })
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%'
            && let Some(byte) = text
                .get(at + 1..at + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// -- table layout ---------------------------------------------------------

/// Gives each cell its column (cells covered by an earlier `rowspan` are
/// skipped, as browsers do) and each row its slots; returns the number of
/// columns.
fn place_cells(rows: &mut [Row]) -> usize {
    // Per column: rows (this one included) still covered by a span.
    let mut covered: Vec<usize> = Vec::new();
    let mut columns = 0;
    let mut placements: Vec<Vec<Slot>> = Vec::with_capacity(rows.len());
    for row in rows.iter_mut() {
        let mut slots = Vec::new();
        let mut column = 0;
        let free =
            |covered: &[usize], column: usize| covered.get(column).is_none_or(|left| *left == 0);
        for (index, cell) in row.cells.iter_mut().enumerate() {
            while !free(&covered, column) {
                column += 1;
            }
            cell.column = column;
            let end = column + cell.colspan;
            if covered.len() < end {
                covered.resize(end, 0);
            }
            covered[column..end].fill(cell.rowspan);
            slots.push(Slot {
                column,
                span: cell.colspan,
                kind: SlotKind::Cell(index),
            });
            column = end;
        }
        columns = columns.max(column).max(covered.len());
        placements.push(slots);
        for left in &mut covered {
            *left = left.saturating_sub(1);
        }
    }
    // Fill each row's free columns: covered by a span from above, or empty.
    let mut spans: Vec<(usize, usize, usize, usize)> = Vec::new(); // (first row, last row, row, cell)
    for (row_index, row) in rows.iter().enumerate() {
        for (index, cell) in row.cells.iter().enumerate() {
            if cell.rowspan > 1 {
                spans.push((row_index, row_index + cell.rowspan - 1, row_index, index));
            }
        }
    }
    for (row_index, slots) in placements.iter_mut().enumerate() {
        let mut taken = vec![false; columns];
        for slot in slots.iter() {
            let end = (slot.column + slot.span).min(columns);
            taken[slot.column.min(end)..end].fill(true);
        }
        for &(first, last, source_row, source_cell) in &spans {
            if row_index <= first || row_index > last {
                continue;
            }
            let cell = &rows[source_row].cells[source_cell];
            let end = (cell.column + cell.colspan).min(columns);
            if taken[cell.column..end].iter().all(|taken| !taken) {
                taken[cell.column..end].fill(true);
                slots.push(Slot {
                    column: cell.column,
                    span: end - cell.column,
                    kind: SlotKind::Covered {
                        row: source_row,
                        cell: source_cell,
                    },
                });
            }
        }
        for (column, taken) in taken.iter().enumerate() {
            if !taken {
                slots.push(Slot {
                    column,
                    span: 1,
                    kind: SlotKind::Empty,
                });
            }
        }
        slots.sort_by_key(|slot| slot.column);
    }
    for (row, slots) in rows.iter_mut().zip(placements) {
        row.slots = slots;
    }
    columns
}

/// Whether `text` reads as a number (`1,234.5`, `-2e-3`, `12%`, `$3`) or a
/// missing value (`NaN`, `None`, `<NA>`, `NaT`, empty).
fn numeric_value(text: &str) -> Option<bool> {
    let text = text.trim();
    match text {
        "" | "NaN" | "nan" | "None" | "<NA>" | "NaT" | "null" | "-" | "..." | "…" | "inf"
        | "-inf" => return None,
        _ => {}
    }
    let core = text
        .trim_start_matches(['$', '€', '£', '¥', '+', '-', '−'])
        .trim_end_matches(['%', 'j']);
    let cleaned: String = core.chars().filter(|c| *c != ',' && *c != '_').collect();
    Some(!cleaned.is_empty() && cleaned.parse::<f64>().is_ok())
}

fn numeric_columns(rows: &[Row], columns: usize) -> Vec<bool> {
    let mut numbers = vec![0usize; columns];
    let mut others = vec![0usize; columns];
    for row in rows.iter().filter(|row| !row.head && !row.ellipsis) {
        for cell in row.cells.iter().filter(|cell| cell.colspan == 1) {
            if cell.header {
                others[cell.column] += 1;
                continue;
            }
            match numeric_value(&cell.inline.text) {
                Some(true) => numbers[cell.column] += 1,
                Some(false) => others[cell.column] += 1,
                None => {}
            }
        }
    }
    (0..columns)
        .map(|column| numbers[column] > 0 && others[column] == 0)
        .collect()
}

impl Table {
    /// Column widths for `measure(text, bold)` (each cell's widest line
    /// plus `padding`): the widest single-column cell of each column, then
    /// spanning cells widen their last column when they need more.
    pub fn column_widths(&self, padding: f32, measure: impl Fn(&str, bool) -> f32) -> Vec<f32> {
        let mut widths = vec![0f32; self.columns];
        let cell_width = |cell: &TableCell| {
            cell.inline
                .text
                .split('\n')
                .map(|line| measure(line, cell.header))
                .fold(0., f32::max)
                + padding
        };
        let mut spanning = Vec::new();
        for row in &self.rows {
            for cell in &row.cells {
                if cell.colspan == 1 {
                    if let Some(width) = widths.get_mut(cell.column) {
                        *width = width.max(cell_width(cell));
                    }
                } else {
                    spanning.push(cell);
                }
            }
        }
        for cell in spanning {
            let end = (cell.column + cell.colspan).min(self.columns);
            if cell.column >= end {
                continue;
            }
            let have: f32 = widths[cell.column..end].iter().sum();
            let need = cell_width(cell);
            if need > have {
                widths[end - 1] += need - have;
            }
        }
        for width in &mut widths {
            *width = width.max(padding);
        }
        widths
    }

    /// The rows of the table head (leading rows marked `head`).
    pub fn head_rows(&self) -> usize {
        self.rows.iter().take_while(|row| row.head).count()
    }
}

/// Measures every table in `blocks` (nested ones too) with
/// [`Table::column_widths`].
pub fn layout_tables(blocks: &mut [Block], padding: f32, measure: &dyn Fn(&str, bool) -> f32) {
    for block in blocks {
        match block {
            Block::Table(table) => table.widths = table.column_widths(padding, measure),
            Block::List { items, .. } => {
                for item in items {
                    layout_tables(item, padding, measure);
                }
            }
            Block::Quote(blocks) | Block::Box { blocks, .. } => {
                layout_tables(blocks, padding, measure)
            }
            Block::Text { .. } | Block::Rule | Block::Image(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/html/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(path).expect("fixture")
    }

    fn only_table(blocks: &[Block]) -> &Table {
        blocks
            .iter()
            .find_map(|block| match block {
                Block::Table(table) => Some(table),
                _ => None,
            })
            .expect("a table")
    }

    fn texts(row: &Row) -> Vec<&str> {
        row.cells
            .iter()
            .map(|cell| cell.inline.text.as_str())
            .collect()
    }

    #[test]
    fn a_pandas_dataframe_is_a_table_with_a_bold_index_and_numeric_columns() {
        let blocks = parse(&fixture("dataframe.html")).expect("renders");
        assert_eq!(
            blocks.len(),
            1,
            "the scoped <style> shows nothing: {blocks:?}"
        );
        let table = only_table(&blocks);
        assert_eq!(table.columns, 4);
        assert_eq!(table.head_rows(), 1);
        assert_eq!(texts(&table.rows[0]), ["", "name", "value", "count"]);
        // pandas right-aligns the header row with an inline style.
        assert!(
            table.rows[0]
                .cells
                .iter()
                .all(|cell| cell.align == Some(Align::Right))
        );
        assert_eq!(texts(&table.rows[2]), ["1", "b", "-2.00", "20"]);
        let index = &table.rows[1].cells[0];
        assert!(index.header);
        assert!(index.inline.styles[0].1.bold);
        assert_eq!(table.numeric, [false, false, true, true]);
        assert!(table.rows.iter().all(|row| !row.ellipsis));
    }

    #[test]
    fn multiindex_rows_and_columns_span() {
        let table = only_table(&parse(&fixture("multiindex.html")).unwrap()).clone();
        assert_eq!(table.columns, 5);
        assert_eq!(table.head_rows(), 3);
        // `A` spans the two columns after the two index columns.
        let a = &table.rows[0].cells[2];
        assert_eq!((a.inline.text.as_str(), a.column, a.colspan), ("A", 2, 2));
        assert_eq!(a.align, Some(Align::Left), "halign");
        assert_eq!(table.rows[0].cells[3].column, 4);
        // `x` spans two body rows; the second row starts at column 1.
        let x = &table.rows[3].cells[0];
        assert_eq!((x.inline.text.as_str(), x.rowspan), ("x", 2));
        let second = &table.rows[4];
        assert_eq!(texts(second), ["2", "3", "4", "5"]);
        assert_eq!(second.cells[0].column, 1);
        assert_eq!(
            second.slots[0],
            Slot {
                column: 0,
                span: 1,
                kind: SlotKind::Covered { row: 3, cell: 0 }
            }
        );
        assert_eq!(second.slots.len(), 5);
        assert_eq!(table.numeric, [false, false, true, true, true]);
        // Widths: spanning `A` fits in its two columns.
        let widths = table.column_widths(8., |text, _| text.chars().count() as f32 * 10.);
        assert_eq!(widths.len(), 5);
        assert!(widths[2] + widths[3] >= 18.);
    }

    #[test]
    fn a_truncated_frame_keeps_the_ellipsis_row_and_column() {
        let blocks = parse(&fixture("truncated.html")).unwrap();
        let table = only_table(&blocks);
        assert_eq!(table.columns, 8);
        let ellipsis: Vec<usize> = table
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.ellipsis)
            .map(|(index, _)| index)
            .collect();
        assert_eq!(ellipsis, [4]);
        // The `...` column does not make its neighbours non-numeric, and is
        // numeric-neutral itself.
        assert_eq!(table.numeric[1..4], [true, true, true]);
        assert_eq!(table.numeric[5..], [true, true, true]);
        match blocks.last() {
            Some(Block::Text { inline, .. }) => assert_eq!(inline.text, "30 rows × 20 columns"),
            other => panic!("the shape line follows: {other:?}"),
        }
    }

    #[test]
    fn styler_id_rules_colour_cells() {
        let table = only_table(&parse(&fixture("styler.html")).unwrap()).clone();
        let row2 = &table.rows[3];
        assert_eq!(texts(row2), ["2", "c", "3.2", "300"]);
        assert_eq!(row2.cells[2].background, parse_color("yellow"));
        assert_eq!(
            row2.cells[3].background,
            Some(Color {
                r: 2,
                g: 0x38,
                b: 0x58,
                a: 1.
            })
        );
        assert_eq!(
            row2.cells[3].inline.styles[0].1.color,
            parse_color("#f1f1f1")
        );
        // `&nbsp;` blank corner.
        assert_eq!(table.rows[0].cells[0].inline.text, "");
    }

    #[test]
    fn snippets_keep_inline_styles_links_and_structure() {
        let blocks = parse(
            "<h2 style='color: red'>Title</h2>\
             <p>Some <b>bold</b>, <i>italic</i> and <code>code</code> \
             text with <a href='https://example.com'>a link</a>.<br>Next line</p>\
             <div style='background-color:#eef; text-align:center; font-weight: 700'>Boxed</div>\
             <ul><li>one<li>two</ul><ol start=3><li>three</li></ol>\
             <blockquote>quoted</blockquote><hr><pre>\n  x = 1\n  y</pre>\
             <span style='font-size: 40px; color: rgb(0, 128, 0)'>green</span>",
        )
        .unwrap();
        let Block::Text {
            kind: TextKind::Heading(2),
            inline,
            ..
        } = &blocks[0]
        else {
            panic!("{:?}", blocks[0]);
        };
        assert_eq!(inline.text, "Title");
        assert_eq!(inline.styles[0].1.color, parse_color("red"));
        let Block::Text { inline, .. } = &blocks[1] else {
            panic!()
        };
        assert_eq!(
            inline.text,
            "Some bold, italic and code text with a link.\nNext line"
        );
        let style_of = |word: &str| {
            let start = inline.text.find(word).unwrap();
            inline
                .styles
                .iter()
                .find(|(range, _)| range.start == start)
                .map(|(_, style)| *style)
                .unwrap()
        };
        assert!(style_of("bold").bold);
        assert!(style_of("italic").italic);
        assert!(style_of("code").code);
        let link = &inline.links[0];
        assert_eq!(&inline.text[link.0.clone()], "a link");
        assert_eq!(link.1, "https://example.com");
        let Block::Box {
            blocks: boxed,
            background,
        } = &blocks[2]
        else {
            panic!("{:?}", blocks[2]);
        };
        assert_eq!(*background, parse_color("#eef").unwrap());
        let Block::Text { inline, align, .. } = &boxed[0] else {
            panic!()
        };
        assert_eq!(*align, Some(Align::Center));
        assert!(inline.styles[0].1.bold);
        assert!(matches!(&blocks[3], Block::List { start: None, items } if items.len() == 2));
        assert!(matches!(&blocks[4], Block::List { start: Some(3), items } if items.len() == 1));
        assert!(matches!(&blocks[5], Block::Quote(_)));
        assert_eq!(blocks[6], Block::Rule);
        let Block::Text {
            kind: TextKind::Pre,
            inline,
            ..
        } = &blocks[7]
        else {
            panic!()
        };
        assert_eq!(inline.text, "  x = 1\n  y");
        let Block::Text { inline, .. } = &blocks[8] else {
            panic!()
        };
        assert_eq!(inline.styles[0].1.color, parse_color("#008000"));
    }

    #[test]
    fn images_are_data_uris_inline_svg_or_links() {
        let blocks = parse(
            "<img src=\"data:image/png;base64,iVBORw0KGgo=\" width=20>\
             <img src='data:image/svg+xml,%3Csvg%20xmlns%3D%22x%22%2F%3E' alt='s'>\
             <img src='https://example.com/a.png' alt='remote'>\
             <svg width='10' height='10'><svg><rect/></svg></svg><p>after</p>",
        )
        .unwrap();
        let sources: Vec<&ImageSource> = blocks
            .iter()
            .filter_map(|block| match block {
                Block::Image(image) => Some(&image.source),
                _ => None,
            })
            .collect();
        assert_eq!(
            sources[0],
            &ImageSource::Data {
                mime: "image/png".into(),
                data: "iVBORw0KGgo=".into(),
                base64: true
            }
        );
        assert_eq!(
            sources[1],
            &ImageSource::Data {
                mime: "image/svg+xml".into(),
                data: "<svg xmlns=\"x\"/>".into(),
                base64: false
            }
        );
        assert_eq!(
            sources[2],
            &ImageSource::Remote("https://example.com/a.png".into())
        );
        assert_eq!(
            sources[3],
            &ImageSource::Svg("<svg width='10' height='10'><svg><rect/></svg></svg>".into())
        );
        assert!(
            matches!(blocks.last(), Some(Block::Text { inline, .. }) if inline.text == "after")
        );
    }

    #[test]
    fn scripts_frames_canvases_and_unknown_elements_need_a_browser() {
        let plotly = "<div>                            <div id=\"abc\" class=\"plotly-graph-div\" \
                      style=\"height:525px; width:100%;\"></div>            <script \
                      type=\"text/javascript\">window.PLOTLYENV=window.PLOTLYENV || {};\
                      if (document.getElementById(\"abc\")) { Plotly.newPlot(\"abc\", \
                      [{\"x\":[1,2],\"type\":\"scatter\"}]) };</script>        </div>";
        assert_eq!(
            parse(plotly).unwrap_err().reason,
            "it runs JavaScript (<script>)"
        );
        assert!(parse("<iframe src='x'></iframe>").is_err());
        assert!(parse("<canvas></canvas>").is_err());
        assert!(parse("<p>ok <input type=checkbox></p>").is_err());
        assert_eq!(
            parse("<vega-chart></vega-chart>").unwrap_err().reason,
            "it uses <vega-chart>"
        );
        // A script inside a table is caught too.
        assert!(parse("<table><tr><td><script>x</script></td></tr></table>").is_err());
        // Comments and a doctype are fine.
        assert!(parse("<!DOCTYPE html><!-- <script> --><p>hi</p>").is_ok());
    }

    #[test]
    fn tolerant_markup_and_entities() {
        let blocks =
            parse("<p>a &amp; b &lt;c&gt; &#x263A; &#9731; &bogus; 1 < 2<p>second").unwrap();
        let text: Vec<&str> = blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text { inline, .. } => Some(inline.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, ["a & b <c> ☺ ☃ &bogus; 1 < 2", "second"]);
        // Unclosed cells and rows close implicitly.
        let table = only_table(&parse("<table><tr><td>1<td>2<tr><td>3</table>").unwrap()).clone();
        assert_eq!(table.rows.len(), 2);
        assert_eq!(texts(&table.rows[0]), ["1", "2"]);
        assert_eq!(table.rows[1].slots[1].kind, SlotKind::Empty);
    }

    #[test]
    fn colors_parse() {
        assert_eq!(
            parse_color("#0f08"),
            Some(Color {
                r: 0,
                g: 255,
                b: 0,
                a: 136. / 255.
            })
        );
        assert_eq!(
            parse_color("rgba(255, 0, 0, 0.5)"),
            Some(Color {
                r: 255,
                g: 0,
                b: 0,
                a: 0.5
            })
        );
        assert_eq!(parse_color("transparent"), None);
        assert_eq!(parse_color("url(x)"), None);
    }

    #[test]
    fn big_tables_keep_their_first_rows() {
        let mut html = String::from("<table>");
        for row in 0..MAX_TABLE_ROWS + 7 {
            html.push_str(&format!("<tr><td>{row}</td></tr>"));
        }
        html.push_str("</table>");
        let table = only_table(&parse(&html).unwrap()).clone();
        assert_eq!(table.rows.len(), MAX_TABLE_ROWS);
        assert_eq!(table.hidden_rows, 7);
    }
}
