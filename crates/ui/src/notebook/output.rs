//! A code cell's outputs as elements: streams and tracebacks with their
//! ANSI colours, images, Markdown, HTML (the safe subset natively, see
//! `html.rs`; anything else as text with "Open in browser"), JSON,
//! ipywidgets (`widgets.rs`) and text fallbacks.
//!
//! Rendering runs every frame for every visible output, so the expensive
//! parts (base64 decoding, ANSI parsing) are done once per content and
//! kept in an [`OutputCache`].

use std::{
    cell::Cell,
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use base64::{
    Engine, alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};
use gpui::{
    AnyElement, App, ElementId, FontStyle, FontWeight, HighlightStyle, Image, ImageFormat,
    SharedString, UnderlineStyle, Window, div, img, prelude::*, px,
};
use notebook::{
    Metadata, Output, ansi,
    outputs::{Rich, richest},
};
use terminal::Palette;

use super::{
    html::{parsed, render_html},
    marimo_ui,
    markdown::{MarkdownImages, markdown},
    widgets::{WidgetEvent, WidgetInputs, WidgetRender, widget_output},
};
use crate::{
    git_dialogs::button, pinpoint::Pin, selectable::selectable, terminal_panel::to_hsla, theme,
};

#[derive(Clone, Copy)]
pub struct OutputOptions<'a> {
    /// Text outputs show this many lines until expanded (0: all).
    pub max_lines: usize,
    pub expanded: bool,
    /// The notebook's widget models (`None`: widget views show their
    /// text).
    pub widgets: Option<WidgetRender<'a>>,
    /// The text fields of marimo UI elements, by element id (`None`: those
    /// elements show their value as text).
    pub marimo_inputs: Option<&'a WidgetInputs>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputAction {
    /// "Show more" on a truncated text output.
    ShowMore,
    /// "Open in browser" on an HTML (or unrenderable SVG) output.
    OpenHtml(String),
    /// A widget control was used.
    Widget(WidgetEvent),
    /// A marimo UI element was used: `value` is its new value in marimo's
    /// front-end form, `send` false while a slider is dragged (only
    /// shown); `value: None` sends the value shown (a drag's end).
    MarimoUi {
        object_id: String,
        value: Option<serde_json::Value>,
        send: bool,
    },
}

/// Images decoded at once (the visible ones must fit).
const IMAGE_CAPACITY: usize = 128;
/// Texts parsed for ANSI colours at once.
const TEXT_CAPACITY: usize = 512;

/// Decoded images and parsed ANSI text by content hash (bounded, least
/// recently used dropped first): what rendering would otherwise redo every
/// frame. One per notebook view.
pub struct OutputCache {
    images: Lru<Option<DecodedImage>>,
    texts: Lru<Rc<CachedText>>,
}

impl Default for OutputCache {
    fn default() -> Self {
        Self {
            images: Lru::new(IMAGE_CAPACITY),
            texts: Lru::new(TEXT_CAPACITY),
        }
    }
}

impl OutputCache {
    /// `data` (base64 when `base64`, else the bytes, e.g. SVG text) as an
    /// image; `None` when it cannot be decoded.
    fn image(&mut self, format: ImageFormat, data: &str, base64: bool) -> Option<DecodedImage> {
        let key = content_hash([data.as_bytes(), &[format as u8, base64 as u8]]);
        self.images
            .get_or_insert_with(key, || decode_image(format, data, base64))
    }

    /// `parts` joined, without ANSI escapes, with their colours, cut to
    /// the lines `options` show (only those are hashed and parsed), and
    /// the lines left out.
    fn text(&mut self, parts: &[&str], options: &OutputOptions) -> (Rc<CachedText>, usize) {
        self.windowed(parts, options, None)
    }

    /// [`Self::text`] for the stream output `id`, which may keep growing:
    /// its shown lines stay parsed once it printed past the limit, and
    /// the lines left out are counted again only when it grew.
    fn stream(
        &mut self,
        id: &ElementId,
        text: &str,
        options: &OutputOptions,
    ) -> (Rc<CachedText>, usize) {
        self.windowed(&[text], options, Some(id))
    }

    fn windowed(
        &mut self,
        parts: &[&str],
        options: &OutputOptions,
        id: Option<&ElementId>,
    ) -> (Rc<CachedText>, usize) {
        let limit = (!options.expanded && options.max_lines > 0).then_some(options.max_lines);
        let (shown, rest) = window(parts, limit);
        // An output's own entry when its hidden lines are counted on it.
        let identity = id.map_or(0, |id| {
            let mut hasher = DefaultHasher::new();
            id.hash(&mut hasher);
            hasher.finish()
        });
        let tail = [limit.unwrap_or(0).to_le_bytes(), identity.to_le_bytes()];
        let key = content_hash(
            shown
                .iter()
                .map(|part| part.as_bytes())
                .chain(tail.iter().map(|bytes| bytes.as_slice())),
        );
        let text = self.texts.get_or_insert_with(key, || {
            Rc::new(CachedText::new(ansi::parse(&shown.concat())))
        });
        let hidden = match rest {
            None => 0,
            Some((cut, offset)) => {
                let rest =
                    std::iter::once(&parts[cut][offset..]).chain(parts[cut + 1..].iter().copied());
                match id {
                    Some(_) => {
                        let total = parts.iter().map(|part| part.len()).sum();
                        text.hidden_for(total, rest)
                    }
                    None => hidden_lines(rest),
                }
            }
        };
        (text, hidden)
    }
}

/// The parts of `parts` holding the first `limit` lines (the last cut
/// before the line break ending the last line), and where the rest
/// starts (part, byte offset of that line break) when there is one.
fn window<'a>(parts: &[&'a str], limit: Option<usize>) -> (Vec<&'a str>, Option<(usize, usize)>) {
    let Some(limit) = limit else {
        return (parts.to_vec(), None);
    };
    let mut shown = Vec::with_capacity(parts.len());
    let mut left = limit;
    for (index, part) in parts.iter().enumerate() {
        let mut seen = 0;
        for (at, _) in part.match_indices('\n') {
            seen += 1;
            if seen == left {
                shown.push(&part[..at]);
                return (shown, Some((index, at)));
            }
        }
        left -= seen;
        shown.push(part);
    }
    (shown, None)
}

/// The lines in `rest`, what follows the shown lines (from the line break
/// ending the last of them): one per line break, but the one ending the
/// text.
fn hidden_lines<'a>(rest: impl Iterator<Item = &'a str>) -> usize {
    let (mut count, mut last) = (0, None);
    for part in rest {
        count += part.matches('\n').count();
        if let Some(byte) = part.bytes().last() {
            last = Some(byte);
        }
    }
    count - usize::from(last == Some(b'\n'))
}

/// A bounded map: past `capacity` entries, inserting drops the least
/// recently used one.
pub(super) struct Lru<V> {
    map: HashMap<u64, (V, u64)>,
    tick: u64,
    capacity: usize,
}

impl<V: Clone> Lru<V> {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::new(),
            tick: 0,
            capacity,
        }
    }

    pub(super) fn get_or_insert_with(&mut self, key: u64, make: impl FnOnce() -> V) -> V {
        self.tick += 1;
        if let Some((value, used)) = self.map.get_mut(&key) {
            *used = self.tick;
            return value.clone();
        }
        if self.map.len() >= self.capacity
            && let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| *key)
        {
            self.map.remove(&oldest);
        }
        let value = make();
        self.map.insert(key, (value.clone(), self.tick));
        value
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.map.len()
    }
}

/// A fast non-cryptographic hash of `parts` (FxHash's mix, eight bytes at
/// a time): cache keys for outputs hashed every frame.
pub(super) fn content_hash<'a>(parts: impl IntoIterator<Item = &'a [u8]>) -> u64 {
    const K: u64 = 0x517c_c1b7_2722_0a95;
    let mix = |hash: u64, word: u64| (hash.rotate_left(5) ^ word).wrapping_mul(K);
    let mut hash = 0;
    for part in parts {
        let (chunks, rest) = part.as_chunks::<8>();
        for chunk in chunks {
            hash = mix(hash, u64::from_le_bytes(*chunk));
        }
        let mut last = [0; 8];
        last[..rest.len()].copy_from_slice(rest);
        hash = mix(mix(hash, u64::from_le_bytes(last)), part.len() as u64);
    }
    hash
}

/// An image ready to draw, with its size in pixels when its header says.
#[derive(Clone)]
pub(super) struct DecodedImage {
    pub image: Arc<Image>,
    pub size: Option<(f32, f32)>,
}

/// Base64 as nbformat writes it: with line breaks, padding optional.
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

pub(super) fn decode_base64(data: &str) -> Option<Vec<u8>> {
    let compact: Vec<u8> = data
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    BASE64.decode(compact).ok()
}

pub(super) fn decode_image(format: ImageFormat, data: &str, base64: bool) -> Option<DecodedImage> {
    let bytes = if base64 {
        decode_base64(data)?
    } else {
        data.as_bytes().to_vec()
    };
    Some(decoded(format, bytes))
}

pub(super) fn decoded(format: ImageFormat, bytes: Vec<u8>) -> DecodedImage {
    let size = image_size(format, &bytes);
    DecodedImage {
        image: Arc::new(Image::from_bytes(format, bytes)),
        size,
    }
}

/// Width and height in pixels from the image's header (PNG, JPEG, GIF,
/// SVG `width`/`height` or `viewBox`).
pub(super) fn image_size(format: ImageFormat, bytes: &[u8]) -> Option<(f32, f32)> {
    let be32 = |at: usize| Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    match format {
        ImageFormat::Png if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => {
            Some((be32(16)? as f32, be32(20)? as f32))
        }
        ImageFormat::Gif if bytes.starts_with(b"GIF") && bytes.len() >= 10 => Some((
            u16::from_le_bytes([bytes[6], bytes[7]]) as f32,
            u16::from_le_bytes([bytes[8], bytes[9]]) as f32,
        )),
        ImageFormat::Jpeg => jpeg_size(bytes),
        ImageFormat::Svg => svg_size(std::str::from_utf8(bytes).ok()?),
        _ => None,
    }
    .filter(|&(width, height)| width > 0. && height > 0.)
}

/// From the first start-of-frame segment.
fn jpeg_size(bytes: &[u8]) -> Option<(f32, f32)> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let be16 = |at: usize| Some(u16::from_be_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]));
    let mut at = 2;
    while at + 1 < bytes.len() {
        if bytes[at] != 0xff {
            at += 1;
            continue;
        }
        let marker = bytes[at + 1];
        match marker {
            0xff => at += 1,
            // Markers without a length.
            0x01 | 0xd0..=0xd8 => at += 2,
            0xc0..=0xcf if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => {
                return Some((be16(at + 7)? as f32, be16(at + 5)? as f32));
            }
            _ => at += 2 + be16(at + 2)? as usize,
        }
    }
    None
}

/// The root element's `width`/`height` (px or pt), else its `viewBox`.
fn svg_size(svg: &str) -> Option<(f32, f32)> {
    let start = svg.find("<svg")?;
    let tag = &svg[start..start + svg[start..].find('>')?];
    let attribute = |name: &str| {
        tag.match_indices(name).find_map(|(at, _)| {
            if !tag[..at].chars().next_back()?.is_whitespace() {
                return None;
            }
            let rest = tag[at + name.len()..].trim_start().strip_prefix('=')?;
            let rest = rest.trim_start();
            let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
            let value = &rest[1..];
            Some(&value[..value.find(quote)?])
        })
    };
    let length = |value: &str| {
        let value = value.trim();
        let (number, scale) = if let Some(number) = value.strip_suffix("pt") {
            (number, 4. / 3.)
        } else {
            (value.strip_suffix("px").unwrap_or(value), 1.)
        };
        number.trim().parse::<f32>().ok().map(|n| n * scale)
    };
    if let (Some(width), Some(height)) = (
        attribute("width").and_then(length),
        attribute("height").and_then(length),
    ) {
        return Some((width, height));
    }
    let view_box: Vec<f32> = attribute("viewBox")?
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse().ok())
        .collect();
    match view_box[..] {
        [_, _, width, height] => Some((width, height)),
        _ => None,
    }
}

/// An image as wide as it is (`size` in pixels) or as its container
/// allows, keeping its aspect ratio.
pub(super) fn image_element(image: &DecodedImage, size: Option<(f32, f32)>) -> AnyElement {
    match size.or(image.size) {
        Some((width, height)) if width > 0. && height > 0. => {
            let mut frame = div().w_full().max_w(px(width));
            frame.style().aspect_ratio = Some(width / height);
            frame
                .child(img(image.image.clone()).size_full())
                .into_any_element()
        }
        _ => img(image.image.clone()).max_w_full().into_any_element(),
    }
}

/// The shown part of a text output: escapes removed (one trailing line
/// break too), colours kept.
struct CachedText {
    text: SharedString,
    spans: Vec<(Range<usize>, ansi::Style)>,
    /// For a stream's entry: the lines past the shown ones, and the length
    /// of the whole text they were counted for.
    hidden: Cell<Option<(usize, usize)>>,
}

impl CachedText {
    fn new(styled: ansi::Styled) -> Self {
        let ansi::Styled { mut text, spans } = styled;
        if text.ends_with('\n') {
            text.pop();
            if text.ends_with('\r') {
                text.pop();
            }
        }
        Self {
            text: text.into(),
            spans,
            hidden: Cell::new(None),
        }
    }

    /// The lines in `rest` (see [`hidden_lines`]) of a text `total` bytes
    /// long, counted once per length.
    fn hidden_for<'a>(&self, total: usize, rest: impl Iterator<Item = &'a str>) -> usize {
        if let Some((counted_for, hidden)) = self.hidden.get()
            && counted_for == total
        {
            return hidden;
        }
        let hidden = hidden_lines(rest);
        self.hidden.set(Some((total, hidden)));
        hidden
    }
}

/// How an ANSI style shows, with `palette` for indexed colours.
fn ansi_highlight(style: &ansi::Style, palette: &Palette) -> HighlightStyle {
    let color = |color: ansi::Color| match color {
        ansi::Color::Indexed(index) => to_hsla(palette.indexed(index as usize)),
        ansi::Color::Rgb(r, g, b) => to_hsla(terminal::Rgb { r, g, b }),
    };
    HighlightStyle {
        color: style.fg.map(color),
        background_color: style.bg.map(color),
        font_weight: style.bold.then_some(FontWeight::BOLD),
        font_style: style.italic.then_some(FontStyle::Italic),
        underline: style.underline.then(|| UnderlineStyle {
            thickness: px(1.),
            ..UnderlineStyle::default()
        }),
        fade_out: style.dim.then_some(0.4),
        ..HighlightStyle::default()
    }
}

/// What an output shows.
#[derive(Debug, PartialEq)]
enum View<'a> {
    Stream {
        text: &'a str,
        stderr: bool,
    },
    Error {
        ename: &'a str,
        evalue: &'a str,
        traceback: &'a [String],
    },
    /// A display or result's richest representation, with its metadata.
    Data(Rich<'a>, &'a Metadata),
}

fn view(output: &Output) -> View<'_> {
    match output {
        Output::Stream { name, text } => View::Stream {
            text,
            stderr: name == "stderr",
        },
        Output::Error {
            ename,
            evalue,
            traceback,
        } => View::Error {
            ename,
            evalue,
            traceback,
        },
        Output::DisplayData { data, metadata, .. }
        | Output::ExecuteResult { data, metadata, .. } => View::Data(richest(data), metadata),
    }
}

/// An image's display size from the output's metadata (`width`, `height`
/// per mime; IPython sets them for retina images).
fn metadata_size(
    metadata: &Metadata,
    mime: &str,
    natural: Option<(f32, f32)>,
) -> Option<(f32, f32)> {
    let entry = metadata.get(mime)?;
    let get = |key: &str| entry.get(key).and_then(|v| v.as_f64()).map(|v| v as f32);
    match (get("width"), get("height"), natural) {
        (Some(width), Some(height), _) => Some((width, height)),
        (Some(width), None, Some((w, h))) => Some((width, width * h / w)),
        (None, Some(height), Some((w, h))) => Some((height * w / h, height)),
        _ => None,
    }
}

pub(super) type OnAction = Rc<dyn Fn(OutputAction, &mut Window, &mut App)>;

/// One output as an element.
pub fn render_output(
    id: impl Into<ElementId>,
    output: &notebook::Output,
    cache: &mut OutputCache,
    options: OutputOptions,
    on_action: impl Fn(OutputAction, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let on_action: OnAction = Rc::new(on_action);
    render_output_inner(id, output, cache, &options, &on_action, cx)
}

/// [`render_output`] for outputs inside outputs (Output widgets).
pub(super) fn render_output_inner(
    id: impl Into<ElementId>,
    output: &notebook::Output,
    cache: &mut OutputCache,
    options: &OutputOptions,
    on_action: &OnAction,
    cx: &App,
) -> AnyElement {
    let id = id.into();
    let body = match view(output) {
        View::Stream { text, stderr } => {
            let (text, hidden) = cache.stream(&id, text, options);
            text_block(&text, hidden, on_action).when(stderr, |block| {
                block.bg(theme::error().opacity(0.08)).px_1().rounded_sm()
            })
        }
        View::Error {
            ename,
            evalue,
            traceback: [],
        } => mono()
            .text_color(theme::error())
            .child(selectable("error", format!("{ename}: {evalue}"))),
        View::Error { traceback, .. } => {
            let mut parts = Vec::with_capacity(traceback.len() * 2);
            for (index, line) in traceback.iter().enumerate() {
                if index > 0 {
                    parts.push("\n");
                }
                parts.push(line.as_str());
            }
            let (text, hidden) = cache.text(&parts, options);
            text_block(&text, hidden, on_action)
        }
        View::Data(rich, metadata) => {
            let element = match (plotly_figure(output), &rich) {
                (Some(figure), Rich::Unsupported(_) | Rich::Plain(_)) => {
                    let plain = match rich {
                        Rich::Plain(text) => Some(text),
                        _ => None,
                    };
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .children(plain.map(|plain| plain_block(plain, cache, options, on_action)))
                        .child(note(
                            "A Plotly figure: it draws with JavaScript, so it opens in the browser.",
                        ))
                        .child(div().flex().child(open_button(plotly_page(figure), on_action.clone())))
                        .into_any_element()
                }
                _ => data_element(rich, metadata, cache, options, on_action, cx),
            };
            return div().id(id).w_full().child(element).into_any_element();
        }
    };
    div().id(id).w_full().child(body).into_any_element()
}

const PLOTLY_MIME: &str = "application/vnd.plotly.v1+json";

/// A display's Plotly figure (`{data, layout, config}`), if it has one.
fn plotly_figure(output: &Output) -> Option<&serde_json::Value> {
    match output {
        Output::DisplayData { data, .. } | Output::ExecuteResult { data, .. } => {
            data.get(PLOTLY_MIME)
        }
        _ => None,
    }
}

/// A page drawing `figure` with plotly.js (from its CDN, as Plotly's own
/// `notebook_connected` renderer does).
fn plotly_page(figure: &serde_json::Value) -> String {
    let json = serde_json::to_string(figure)
        .unwrap_or_else(|_| "{}".into())
        .replace("</", "<\\/");
    format!(
        "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\">\
         <script src=\"https://cdn.plot.ly/plotly-2.35.2.min.js\"></script></head>\
         <body><div id=\"plot\" style=\"width:100%;height:90vh\"></div><script>\
         const figure = {json};\
         Plotly.newPlot('plot', figure.data || [], figure.layout || {{}}, figure.config || {{}});\
         </script></body></html>\n"
    )
}

fn data_element(
    rich: Rich,
    metadata: &Metadata,
    cache: &mut OutputCache,
    options: &OutputOptions,
    on_action: &OnAction,
    cx: &App,
) -> AnyElement {
    let image = |cache: &mut OutputCache, format, mime: &str, data: &str, base64| {
        let decoded = cache.image(format, data, base64);
        decoded.map(|decoded| {
            let size = metadata_size(metadata, mime, decoded.size);
            image_element(&decoded, size)
        })
    };
    match rich {
        Rich::Png(data) => image(cache, ImageFormat::Png, "image/png", data, true)
            .unwrap_or_else(|| note("The image could not be decoded.").into_any_element()),
        Rich::Jpeg(data) => image(cache, ImageFormat::Jpeg, "image/jpeg", data, true)
            .unwrap_or_else(|| note("The image could not be decoded.").into_any_element()),
        Rich::Svg(svg) => {
            let open = svg.to_owned();
            let fallback_action = on_action.clone();
            let placeholder = move || {
                let (open, on_action) = (open.clone(), fallback_action.clone());
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(note("The SVG image cannot be shown here."))
                    .child(open_button(open, on_action))
                    .into_any_element()
            };
            match cache.image(ImageFormat::Svg, svg, false) {
                Some(decoded) => {
                    let size = metadata_size(metadata, "image/svg+xml", decoded.size);
                    match size.or(decoded.size) {
                        Some((width, height)) => {
                            let mut frame = div().w_full().max_w(px(width));
                            frame.style().aspect_ratio = Some(width / height);
                            frame
                                .child(
                                    img(decoded.image.clone())
                                        .size_full()
                                        .with_fallback(placeholder),
                                )
                                .into_any_element()
                        }
                        None => img(decoded.image.clone())
                            .max_w_full()
                            .with_fallback(placeholder)
                            .into_any_element(),
                    }
                }
                None => placeholder(),
            }
        }
        Rich::Markdown(text) => markdown("markdown", text, MarkdownImages::default(), cx),
        Rich::Html { html, plain } => {
            let open = div()
                .flex()
                .child(open_button(html.to_owned(), on_action.clone()));
            match parsed(html, cx).as_ref() {
                Ok(blocks) => div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(render_html("html", blocks))
                    .child(open)
                    .into_any_element(),
                Err(unsupported) => div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(plain.map(|plain| plain_block(plain, cache, options, on_action)))
                    .child(note(format!(
                        "Shown as text: this HTML needs a browser ({}).",
                        unsupported.reason
                    )))
                    .child(open)
                    .into_any_element(),
            }
        }
        Rich::Latex(text) => plain_block(text, cache, options, on_action).into_any_element(),
        Rich::Json(value) => {
            let pretty = serde_json::to_string_pretty(value).unwrap_or_default();
            plain_block(&pretty, cache, options, on_action).into_any_element()
        }
        Rich::Widget { model_id, plain } => {
            widget_output(model_id, plain, cache, options, on_action, cx)
        }
        Rich::MarimoUi { parts, .. } => marimo_ui::render(parts, options, on_action, cx),
        Rich::Plain(text) => plain_block(text, cache, options, on_action).into_any_element(),
        Rich::Unsupported(mimes) => {
            note(format!("No view for this output ({}).", mimes.join(", "))).into_any_element()
        }
    }
}

/// Output text in the editor font.
fn mono() -> gpui::Div {
    div()
        .font_family(theme::MONO_FONT)
        .text_size(px(theme::EDITOR_FONT_SIZE))
        .line_height(px(theme::EDITOR_LINE_HEIGHT))
        .text_color(theme::text())
}

/// A muted remark in the UI font.
pub(super) fn note(text: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_sm()
        .italic()
        .text_color(theme::muted())
        .child(selectable("note", text))
}

fn open_button(html: String, on_action: OnAction) -> impl IntoElement {
    button("open-html", "Open in browser".into(), false)
        .text_sm()
        .on_click(move |_, window, cx| on_action(OutputAction::OpenHtml(html.clone()), window, cx))
        .pin("open-in-browser")
        .label("Open in browser")
}

/// `text` as a plain text output.
pub(super) fn plain_block(
    text: &str,
    cache: &mut OutputCache,
    options: &OutputOptions,
    on_action: &OnAction,
) -> gpui::Div {
    let (text, hidden) = cache.text(&[text], options);
    text_block(&text, hidden, on_action)
}

/// Coloured, selectable text (cut to the shown lines already), with a
/// "Show more" button when `hidden` lines are left out.
fn text_block(text: &CachedText, hidden: usize, on_action: &OnAction) -> gpui::Div {
    let shown = text.text.clone();
    let theme = theme::current();
    let highlights: Vec<_> = text
        .spans
        .iter()
        .map(|(range, style)| (range.clone(), ansi_highlight(style, &theme.terminal)))
        .collect();
    let on_action = on_action.clone();
    mono()
        .flex()
        .flex_col()
        .items_start()
        .child(selectable("text", shown).with_highlights(highlights))
        .when(hidden > 0, |block| {
            let label = match hidden {
                1 => "Show more (1 more line)".to_owned(),
                n => format!("Show more ({n} more lines)"),
            };
            block.child(
                button("show-more", label.clone().into(), false)
                    .mt_1()
                    .font_family(theme::UI_FONT)
                    .text_sm()
                    .on_click(move |_, window, cx| on_action(OutputAction::ShowMore, window, cx))
                    .pin("show-more")
                    .label(label),
            )
        })
}

/// Where [`open_html`] writes: one file per content.
fn html_path(html: &str) -> PathBuf {
    let hash = content_hash([html.as_bytes()]);
    std::env::temp_dir()
        .join("next_code-notebook")
        .join(format!("output-{hash:016x}.html"))
}

/// Writes `html` to a temporary file (named by its content hash) and opens
/// it in the browser. A fragment (an output's HTML) gets a UTF-8 page
/// around it.
pub fn open_html(html: &str, cx: &mut App) -> std::io::Result<()> {
    let path = html_path(html);
    write_html(&path, html)?;
    cx.open_with_system(&path);
    Ok(())
}

fn write_html(path: &Path, html: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let lower: String = html
        .trim_start()
        .chars()
        .take(9)
        .collect::<String>()
        .to_ascii_lowercase();
    let page = if lower.starts_with("<!doctype") || lower.starts_with("<html") {
        html.to_owned()
    } else {
        format!(
            "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"></head><body>\n{html}\n</body></html>\n"
        )
    };
    std::fs::write(path, page)
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use notebook::MimeBundle;
    use serde_json::{Value, json};

    use super::*;

    /// A 2×3 PNG.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAADCAIAAAA2iEnWAAAAEElEQVR4nGP4z8AARAwoFABE0AX7pM/egAAAAABJRU5ErkJggg==";

    fn display(data: Value, metadata: Value) -> Output {
        Output::DisplayData {
            data: notebook::outputs::bundle_from_json(&data),
            metadata: metadata.as_object().cloned().unwrap_or_default(),
            display_id: None,
        }
    }

    fn options(max_lines: usize, expanded: bool) -> OutputOptions<'static> {
        OutputOptions {
            max_lines,
            expanded,
            widgets: None,
            marimo_inputs: None,
        }
    }

    #[test]
    fn long_texts_show_their_first_lines_and_count_the_rest() {
        assert_eq!(
            window(&["a\nb\nc\nd"], Some(2)),
            (vec!["a\nb"], Some((0, 3)))
        );
        assert_eq!(window(&["a\nb"], Some(2)), (vec!["a\nb"], None));
        assert_eq!(
            window(&["a\n", "b\nc"], Some(2)),
            (vec!["a\n", "b"], Some((1, 1)))
        );
        assert_eq!(window(&["a\nb\nc"], None), (vec!["a\nb\nc"], None));
        assert_eq!(hidden_lines(["\nc\nd"].into_iter()), 2);
        assert_eq!(hidden_lines(["\nc", "\n"].into_iter()), 1);
        assert_eq!(hidden_lines(["\n"].into_iter()), 0);

        let mut cache = OutputCache::default();
        let id = ElementId::Name("stream".into());
        // Five lines (the final line break ends the last one).
        let show = |cache: &mut OutputCache, max_lines, expanded| {
            let (text, hidden) =
                cache.stream(&id, "1\n2\n3\n4\n5\n", &options(max_lines, expanded));
            (text.text.to_string(), hidden)
        };
        assert_eq!(show(&mut cache, 2, false), ("1\n2".into(), 3));
        assert_eq!(show(&mut cache, 5, false), ("1\n2\n3\n4\n5".into(), 0));
        assert_eq!(
            show(&mut cache, 2, true),
            ("1\n2\n3\n4\n5".into(), 0),
            "expanded"
        );
        assert_eq!(
            show(&mut cache, 0, false),
            ("1\n2\n3\n4\n5".into(), 0),
            "no limit"
        );
        assert_eq!(show(&mut cache, 4, false), ("1\n2\n3\n4".into(), 1));

        // A stream printing past the limit keeps its parsed lines; the
        // count of the rest follows its length.
        let (first, hidden) = cache.stream(&id, "1\n2\n3\n", &options(2, false));
        assert_eq!((&first.text[..], hidden), ("1\n2", 1));
        let (again, hidden) = cache.stream(&id, "1\n2\n3\n4\n5\n6", &options(2, false));
        assert!(Rc::ptr_eq(&first, &again), "not parsed again");
        assert_eq!(hidden, 4);
        // Another output with the same first lines counts its own rest.
        let (_, other) = cache.text(&["1\n2\n", "x"], &options(2, false));
        assert_eq!(other, 1);
        let (_, other) = cache.text(&["1\n2\n", "x\ny\n"], &options(2, false));
        assert_eq!(other, 2);
    }

    #[test]
    fn outputs_show_their_richest_representation() {
        let stream = Output::Stream {
            name: "stderr".into(),
            text: "oops\n".into(),
        };
        assert_eq!(
            view(&stream),
            View::Stream {
                text: "oops\n",
                stderr: true
            }
        );
        let image = display(
            json!({"image/png": PNG, "text/plain": "<Figure>"}),
            json!({}),
        );
        assert!(matches!(view(&image), View::Data(Rich::Png(data), _) if data == PNG));
        let html = display(
            json!({"text/html": "<b>x</b>", "text/plain": "x"}),
            json!({}),
        );
        assert!(matches!(
            view(&html),
            View::Data(
                Rich::Html {
                    plain: Some("x"),
                    ..
                },
                _
            )
        ));
        let error = Output::Error {
            ename: "ValueError".into(),
            evalue: "bad".into(),
            traceback: vec!["\x1b[31mValueError\x1b[0m: bad".into()],
        };
        assert!(matches!(
            view(&error),
            View::Error {
                ename: "ValueError",
                ..
            }
        ));
    }

    #[test]
    fn ansi_colours_map_to_the_terminal_palette() {
        let mut cache = OutputCache::default();
        let (text, _) = cache.text(
            &["\x1b[1;31mError\x1b[0m: \x1b[38;5;196mx\x1b[38;2;1;2;3my\n"],
            &options(0, false),
        );
        assert_eq!(&text.text[..], "Error: xy");
        let theme = theme::current();
        let palette = &theme.terminal;
        let styles: Vec<_> = text
            .spans
            .iter()
            .map(|(range, style)| (range.clone(), ansi_highlight(style, palette)))
            .collect();
        assert_eq!(styles.len(), 3);
        assert_eq!(styles[0].0, 0..5);
        assert_eq!(styles[0].1.color, Some(to_hsla(palette.ansi[1])));
        assert_eq!(styles[0].1.font_weight, Some(FontWeight::BOLD));
        assert_eq!(styles[1].0, 7..8);
        assert_eq!(
            styles[1].1.color,
            Some(to_hsla(terminal::Rgb { r: 255, g: 0, b: 0 })),
            "xterm cube"
        );
        assert_eq!(
            styles[2].1.color,
            Some(to_hsla(terminal::Rgb { r: 1, g: 2, b: 3 }))
        );
        let dim = ansi::Style {
            dim: true,
            underline: true,
            italic: true,
            bg: Some(ansi::Color::Indexed(2)),
            ..ansi::Style::default()
        };
        let dim = ansi_highlight(&dim, palette);
        assert_eq!(dim.background_color, Some(to_hsla(palette.ansi[2])));
        assert!(dim.fade_out.is_some() && dim.underline.is_some());
        assert_eq!(dim.font_style, Some(FontStyle::Italic));
    }

    #[test]
    fn images_are_decoded_once_and_sized_from_their_header() {
        let mut cache = OutputCache::default();
        let first = cache.image(ImageFormat::Png, PNG, true).expect("decodes");
        assert_eq!(first.size, Some((2., 3.)));
        // nbformat's line-broken base64 is the same image.
        let wrapped = format!("{}\n{}", &PNG[..20], &PNG[20..]);
        assert!(cache.image(ImageFormat::Png, &wrapped, true).is_some());
        let again = cache.image(ImageFormat::Png, PNG, true).unwrap();
        assert!(Arc::ptr_eq(&first.image, &again.image), "reused");
        assert_eq!(cache.images.len(), 2);
        assert!(cache.image(ImageFormat::Png, "not base64!", true).is_none());

        let svg = r#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" stroke-width="2" width="30pt" height="15pt" viewBox="0 0 40 20"></svg>"#;
        assert_eq!(
            image_size(ImageFormat::Svg, svg.as_bytes()),
            Some((40., 20.))
        );
        let view_box = r#"<svg viewBox="0 0 400 300"><g/></svg>"#;
        assert_eq!(
            image_size(ImageFormat::Svg, view_box.as_bytes()),
            Some((400., 300.))
        );
        // SOI, APP0 (length 4), SOF0 with height 7 and width 9.
        let jpeg = [
            0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x00, 0x00, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00,
            0x07, 0x00, 0x09, 0x03,
        ];
        assert_eq!(image_size(ImageFormat::Jpeg, &jpeg), Some((9., 7.)));

        let metadata = json!({"image/png": {"width": 100}});
        let metadata = metadata.as_object().unwrap();
        assert_eq!(
            metadata_size(metadata, "image/png", Some((2., 3.))),
            Some((100., 150.))
        );
    }

    #[test]
    fn bounded_caches_drop_the_least_recently_used() {
        let mut lru = Lru::new(2);
        lru.get_or_insert_with(1, || "a");
        lru.get_or_insert_with(2, || "b");
        lru.get_or_insert_with(1, || unreachable!());
        lru.get_or_insert_with(3, || "c");
        assert_eq!(lru.len(), 2);
        assert_eq!(lru.get_or_insert_with(1, || "again"), "a");
        assert_eq!(lru.get_or_insert_with(2, || "new"), "new", "2 was dropped");
    }

    #[test]
    fn html_opens_as_a_page_named_by_its_content() {
        let html = "<table><tr><td>é</td></tr></table>";
        let path = html_path(html);
        assert_eq!(path, html_path(html));
        assert_ne!(path, html_path("<p>other</p>"));
        write_html(&path, html).unwrap();
        let page = std::fs::read_to_string(&path).unwrap();
        assert!(page.starts_with("<!DOCTYPE html>") && page.contains("charset=\"utf-8\""));
        assert!(page.contains(html));
        std::fs::remove_file(&path).ok();
    }

    struct Outputs {
        outputs: Vec<Output>,
        cache: OutputCache,
    }

    impl Render for Outputs {
        fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
            let cache = &mut self.cache;
            div()
                .size_full()
                .flex()
                .flex_col()
                .children(self.outputs.iter().enumerate().map(|(index, output)| {
                    render_output(index, output, cache, options(2, false), |_, _, _| {}, cx)
                }))
        }
    }

    #[gpui::test]
    fn every_kind_of_output_renders(cx: &mut TestAppContext) {
        let bundle = |mime: &str, data: Value| display(json!({ mime: data }), json!({}));
        let outputs = vec![
            Output::Stream {
                name: "stdout".into(),
                text: "1\n2\n3\n4\n".into(),
            },
            Output::Stream {
                name: "stderr".into(),
                text: "\x1b[33mwarning\x1b[0m\n".into(),
            },
            Output::Error {
                ename: "E".into(),
                evalue: "v".into(),
                traceback: vec!["\x1b[31mE\x1b[0m".into(), "line".into()],
            },
            Output::Error {
                ename: "E".into(),
                evalue: "v".into(),
                traceback: Vec::new(),
            },
            bundle("image/png", json!(PNG)),
            bundle("image/png", json!("broken")),
            bundle(
                "image/svg+xml",
                json!("<svg width=\"10\" height=\"10\"></svg>"),
            ),
            bundle("text/markdown", json!("# Title\n\n*x*")),
            display(
                json!({"text/html": "<b>x</b>", "text/plain": "x"}),
                json!({}),
            ),
            bundle("text/html", json!("<b>x</b>")),
            // Needs a browser: its text instead.
            display(
                json!({"text/html": "<div id='p'></div><script>draw('p')</script>", "text/plain": "<Figure>"}),
                json!({}),
            ),
            bundle(
                PLOTLY_MIME,
                json!({"data": [{"x": [1], "y": [2]}], "layout": {}}),
            ),
            bundle("text/latex", json!("$x^2$")),
            bundle("application/json", json!({"a": [1, 2]})),
            display(
                json!({"application/vnd.jupyter.widget-view+json": {"model_id": "m"}, "text/plain": "IntSlider()"}),
                json!({}),
            ),
            bundle("application/x-unknown", json!("?")),
            Output::ExecuteResult {
                execution_count: Some(1),
                data: MimeBundle::from([("text/plain".to_owned(), json!("42"))]),
                metadata: Default::default(),
            },
        ];
        let window = cx.add_window(|_, _| Outputs {
            outputs,
            cache: OutputCache::default(),
        });
        cx.run_until_parked();
        window
            .update(cx, |view, _, cx| {
                // Streams, traceback, the script HTML's text, LaTeX, JSON,
                // the widget's text and the result (rendered HTML is not
                // text).
                assert_eq!(view.cache.texts.len(), 8);
                cx.notify();
            })
            .unwrap();
        cx.run_until_parked();
    }
}
