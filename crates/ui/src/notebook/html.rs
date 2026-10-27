//! `text/html` outputs (and ipywidgets' HTML widgets) rendered natively
//! from `notebook::html`'s blocks: selectable text with the supported
//! inline styles, links that open in the browser, lists, quotes, `data:`
//! and inline SVG images, and tables (DataFrames): measured column widths,
//! bold header and index cells, numeric columns right-aligned, alternating
//! rows, a header that sticks to the top of the view while the table
//! scrolls by, and horizontal scrolling for wide tables.
//!
//! Parsing and measuring happen once per HTML text (bounded cache).

use std::{
    cell::RefCell,
    collections::HashMap,
    panic::Location,
    sync::{Arc, LazyLock},
};

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, FontStyle, FontWeight, GlobalElementId,
    HighlightStyle, Hsla, ImageFormat, InspectorElementId, IntoElement, LayoutId, Pixels,
    SharedString, StrikethroughStyle, Style, UnderlineStyle, Window, div, font, point, prelude::*,
    px, rgba,
};
use notebook::html::{
    self, Align, Block, Color, ImageSource, Inline, SlotKind, Table, TextKind, TextStyle,
    Unsupported,
};
use parking_lot::Mutex;

use super::output::{DecodedImage, Lru, content_hash, decode_image, image_element};
use crate::{pinpoint::Pin, selectable::selectable, theme};

/// Table text size and line height.
const TABLE_FONT_SIZE: f32 = 13.;
const TABLE_LINE_HEIGHT: f32 = 20.;
/// Horizontal padding of a table cell (each side).
const CELL_PADDING: f32 = 8.;

type Parsed = Arc<Result<Vec<Block>, Unsupported>>;

static PARSED: LazyLock<Mutex<Lru<Parsed>>> = LazyLock::new(|| Mutex::new(Lru::new(128)));
static IMAGES: LazyLock<Mutex<Lru<Option<DecodedImage>>>> =
    LazyLock::new(|| Mutex::new(Lru::new(64)));

/// `html` parsed (and its tables measured) once per content.
pub fn parsed(html: &str, cx: &App) -> Parsed {
    let key = content_hash([html.as_bytes()]);
    PARSED.lock().get_or_insert_with(key, || {
        let mut result = html::parse(html);
        if let Ok(blocks) = &mut result {
            let measure = measurer(cx);
            // Both sides' padding, the border, and a margin for kerning
            // the per-character sum does not see.
            html::layout_tables(blocks, CELL_PADDING * 2. + 6., &|text, bold| {
                measure(text, bold) * 1.04
            });
        }
        Arc::new(result)
    })
}

/// Text width in the table font (UI font at [`TABLE_FONT_SIZE`]), summed
/// per character; characters the font has no advance for count as their
/// display width in half-ems.
fn measurer(cx: &App) -> impl Fn(&str, bool) -> f32 {
    let text_system = cx.text_system().clone();
    let regular = text_system.resolve_font(&font(theme::UI_FONT));
    let bold = text_system.resolve_font(&gpui::Font {
        weight: FontWeight::BOLD,
        ..font(theme::UI_FONT)
    });
    let size = px(TABLE_FONT_SIZE);
    let advances: RefCell<HashMap<(char, bool), f32>> = RefCell::new(HashMap::new());
    move |text: &str, is_bold: bool| {
        let mut advances = advances.borrow_mut();
        text.chars()
            .map(|c| {
                *advances.entry((c, is_bold)).or_insert_with(|| {
                    let id = if is_bold { bold } else { regular };
                    text_system
                        .advance(id, size, c)
                        .map(|advance| f32::from(advance.width))
                        .unwrap_or_else(|_| {
                            unicode_width::UnicodeWidthChar::width(c).unwrap_or(1) as f32
                                * TABLE_FONT_SIZE
                                * 0.6
                        })
                })
            })
            .sum()
    }
}

/// The blocks as an element.
pub fn render_html(id: impl Into<ElementId>, blocks: &[Block]) -> AnyElement {
    div()
        .id(id)
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .text_color(theme::text())
        .children(render_blocks(blocks, None))
        .into_any_element()
}

fn render_blocks(blocks: &[Block], background: Option<Hsla>) -> Vec<AnyElement> {
    // Tables are what agents point at: the first is `table`, others by block.
    let first_table = blocks
        .iter()
        .position(|block| matches!(block, Block::Table(_)));
    blocks
        .iter()
        .enumerate()
        .map(|(index, block)| {
            let table_id: Option<SharedString> = matches!(block, Block::Table(_)).then(|| {
                if Some(index) == first_table {
                    "table".into()
                } else {
                    format!("table:{}", index + 1).into()
                }
            });
            render_block(index, block, background, table_id)
        })
        .collect()
}

pub(super) fn to_hsla(color: Color) -> Hsla {
    rgba(
        (u32::from(color.r) << 24)
            | (u32::from(color.g) << 16)
            | (u32::from(color.b) << 8)
            | (color.a.clamp(0., 1.) * 255.).round() as u32,
    )
    .into()
}

/// WCAG relative luminance.
fn luminance(color: Hsla) -> f32 {
    let rgba = color.to_rgb();
    let channel = |c: f32| {
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(rgba.r) + 0.7152 * channel(rgba.g) + 0.0722 * channel(rgba.b)
}

fn contrast(a: Hsla, b: Hsla) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// The text colour on `background`: the theme's when it reads, else black
/// or white (HTML assumes a white page; the theme may be dark).
pub(super) fn text_on(background: Option<Hsla>) -> Hsla {
    let text = theme::text();
    match background {
        Some(background) if background.a > 0.3 && contrast(text, background) < 3. => {
            if luminance(background) > 0.4 {
                gpui::black()
            } else {
                gpui::white()
            }
        }
        _ => text,
    }
}

/// `color` when it reads on `background` (the theme's when `None`); else
/// none (the default text colour).
fn readable(color: Color, background: Option<Hsla>) -> Option<Hsla> {
    let color = to_hsla(color);
    let background = background.unwrap_or_else(theme::background);
    (contrast(color, background) >= 1.6).then_some(color)
}

fn highlight(style: &TextStyle, background: Option<Hsla>) -> HighlightStyle {
    let span_background = style.background.map(to_hsla);
    let behind = span_background.or(background);
    HighlightStyle {
        font_weight: style.bold.then_some(FontWeight::BOLD),
        font_style: style.italic.then_some(FontStyle::Italic),
        strikethrough: style.strike.then_some(StrikethroughStyle {
            thickness: px(1.),
            color: None,
        }),
        underline: style.underline.then(|| UnderlineStyle {
            thickness: px(1.),
            ..UnderlineStyle::default()
        }),
        color: style
            .color
            .and_then(|color| readable(color, behind))
            .or_else(|| span_background.map(|bg| text_on(Some(bg)))),
        background_color: span_background
            .or_else(|| style.code.then(|| theme::text().opacity(0.08))),
        ..HighlightStyle::default()
    }
}

/// Selectable inline text with its styles and links (absolute URLs open
/// in the browser).
fn inline_text(id: impl Into<ElementId>, inline: &Inline, background: Option<Hsla>) -> AnyElement {
    let mut highlights: Vec<_> = inline
        .styles
        .iter()
        .map(|(range, style)| (range.clone(), highlight(style, background)))
        .collect();
    for (range, _) in &inline.links {
        highlights.push((
            range.clone(),
            HighlightStyle {
                color: Some(theme::accent_text()),
                ..HighlightStyle::default()
            },
        ));
    }
    // Highlights must not overlap: later ones (links) win where they do.
    let highlights = flatten_highlights(highlights);
    let text = selectable(id, SharedString::from(inline.text.clone())).with_highlights(highlights);
    if inline.links.is_empty() {
        return text.into_any_element();
    }
    let ranges = inline
        .links
        .iter()
        .map(|(range, _)| range.clone())
        .collect();
    let hrefs: Vec<String> = inline.links.iter().map(|(_, href)| href.clone()).collect();
    text.with_links(ranges, move |index, _, cx| {
        if let Some(href) = hrefs.get(index)
            && is_web_url(href)
        {
            cx.open_url(href);
        }
    })
    .into_any_element()
}

fn is_web_url(href: &str) -> bool {
    let lower = href.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")
}

/// Splits overlapping highlight ranges into disjoint ones, combining the
/// styles that cover each piece (later ranges' fields win).
fn flatten_highlights(
    highlights: Vec<(std::ops::Range<usize>, HighlightStyle)>,
) -> Vec<(std::ops::Range<usize>, HighlightStyle)> {
    if highlights.len() < 2 {
        return highlights;
    }
    let mut cuts: Vec<usize> = highlights
        .iter()
        .flat_map(|(range, _)| [range.start, range.end])
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = Vec::new();
    for window in cuts.windows(2) {
        let (start, end) = (window[0], window[1]);
        let mut combined: Option<HighlightStyle> = None;
        for (range, style) in &highlights {
            if range.start <= start && end <= range.end {
                combined = Some(match combined {
                    Some(base) => base.highlight(*style),
                    None => *style,
                });
            }
        }
        if let Some(style) = combined {
            out.push((start..end, style));
        }
    }
    out
}

fn aligned(element: AnyElement, align: Option<Align>) -> gpui::Div {
    let row = div().w_full().flex();
    match align {
        Some(Align::Center) => row.justify_center(),
        Some(Align::Right) => row.justify_end(),
        _ => row,
    }
    .child(element)
}

fn render_block(
    index: usize,
    block: &Block,
    background: Option<Hsla>,
    table_id: Option<SharedString>,
) -> AnyElement {
    let frame = div().id(index).w_full().min_w_0();
    match block {
        Block::Text {
            kind: TextKind::Paragraph,
            inline,
            align,
        } => frame.child(aligned(inline_text("text", inline, background), *align)),
        Block::Text {
            kind: TextKind::Heading(level),
            inline,
            align,
        } => frame
            .font_weight(FontWeight::SEMIBOLD)
            .map(|heading| match level {
                1 => heading.text_2xl(),
                2 => heading.text_xl(),
                3 => heading.text_lg(),
                _ => heading.text_base(),
            })
            .child(aligned(inline_text("text", inline, background), *align)),
        Block::Text {
            kind: TextKind::Pre,
            inline,
            ..
        } => frame
            .px_2()
            .py_1()
            .rounded_md()
            .bg(theme::text().opacity(0.05))
            .overflow_x_scroll()
            .map(|mut pre| {
                pre.style().restrict_scroll_to_axis = Some(true);
                pre
            })
            .font_family(theme::MONO_FONT)
            .text_size(px(theme::EDITOR_FONT_SIZE))
            .line_height(px(theme::EDITOR_LINE_HEIGHT))
            .whitespace_nowrap()
            .child(inline_text("pre", inline, background)),
        Block::List { start, items } => {
            frame
                .flex()
                .flex_col()
                .gap_1()
                .children(items.iter().enumerate().map(|(index, item)| {
                    let marker: SharedString = match start {
                        Some(start) => format!("{}.", start + index as u64).into(),
                        None => "•".into(),
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
                                .children(render_blocks(item, background)),
                        )
                }))
        }
        Block::Quote(blocks) => frame
            .pl_3()
            .border_l_4()
            .border_color(theme::border())
            .flex()
            .flex_col()
            .gap_2()
            .children(render_blocks(blocks, background)),
        Block::Rule => frame.h(px(1.)).bg(theme::border()),
        Block::Image(image) => frame.child(render_image(image)),
        Block::Table(table) => frame.child(render_table(table, background, table_id)),
        Block::Box {
            blocks,
            background: color,
        } => {
            let color = to_hsla(*color);
            frame
                .p_2()
                .rounded_sm()
                .bg(color)
                .text_color(text_on(Some(color)))
                .flex()
                .flex_col()
                .gap_2()
                .children(render_blocks(blocks, Some(color)))
        }
    }
    .into_any_element()
}

fn mime_format(mime: &str) -> Option<ImageFormat> {
    Some(match mime {
        "image/png" => ImageFormat::Png,
        "image/jpeg" | "image/jpg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::Webp,
        "image/svg+xml" => ImageFormat::Svg,
        "image/bmp" => ImageFormat::Bmp,
        _ => return None,
    })
}

/// An image from a `data:` payload or SVG source (cached).
pub(super) fn cached_image(format: ImageFormat, data: &str, base64: bool) -> Option<DecodedImage> {
    let key = content_hash([data.as_bytes(), &[format as u8, base64 as u8]]);
    IMAGES
        .lock()
        .get_or_insert_with(key, || decode_image(format, data, base64))
}

fn render_image(image: &html::Image) -> AnyElement {
    let size = match (image.width, image.height) {
        (Some(width), Some(height)) => Some((width, height)),
        _ => None,
    };
    let decoded = match &image.source {
        ImageSource::Data { mime, data, base64 } => {
            mime_format(mime).and_then(|format| cached_image(format, data, *base64))
        }
        ImageSource::Svg(svg) => cached_image(ImageFormat::Svg, svg, false),
        ImageSource::Remote(src) => {
            let label = if image.alt.is_empty() {
                format!("[image: {src}]")
            } else {
                format!("[{}: {src}]", image.alt)
            };
            let inline = Inline {
                links: vec![(0..label.len(), src.clone())],
                text: label,
                styles: Vec::new(),
            };
            return div()
                .text_sm()
                .child(inline_text("remote-image", &inline, None))
                .into_any_element();
        }
    };
    match decoded {
        Some(decoded) => {
            let size = size.or_else(|| {
                let (w, h) = decoded.size?;
                match (image.width, image.height) {
                    (Some(width), None) => Some((width, width * h / w)),
                    (None, Some(height)) => Some((height * w / h, height)),
                    _ => None,
                }
            });
            image_element(&decoded, size)
        }
        None => div()
            .text_sm()
            .italic()
            .text_color(theme::muted())
            .child(selectable(
                "alt",
                if image.alt.is_empty() {
                    "[image]".to_owned()
                } else {
                    format!("[{}]", image.alt)
                },
            ))
            .into_any_element(),
    }
}

/// `id`: the pin of its grid (the drawn table, not the full-width block).
fn render_table(table: &Table, background: Option<Hsla>, id: Option<SharedString>) -> AnyElement {
    let widths = &table.widths;
    let width_of = |column: usize, span: usize| -> f32 {
        widths
            .get(column..(column + span).min(widths.len()))
            .map_or(CELL_PADDING * 2., |widths| widths.iter().sum())
    };
    let total: f32 = widths.iter().sum();
    let head_rows = table.head_rows();
    let stripe = theme::text().opacity(0.04);
    let row_element = |index: usize| -> AnyElement {
        let row = &table.rows[index];
        let head = index < head_rows;
        let body_index = index.saturating_sub(head_rows);
        let mut element = div()
            .id(("row", index))
            .flex()
            .flex_none()
            .w(px(total))
            .when(index > 0, |row| {
                row.border_t_1().border_color(theme::border())
            })
            .when(head, |row| row.bg(theme::panel()))
            .when(!head && body_index % 2 == 1, |row| row.bg(stripe))
            .when(!head, |row| row.hover(|style| style.bg(theme::hover_row())));
        for (slot_index, slot) in row.slots.iter().enumerate() {
            let width = width_of(slot.column, slot.span);
            let cell = div()
                .id(("cell", slot_index))
                .flex_none()
                .w(px(width))
                .px(px(CELL_PADDING))
                .py(px(2.))
                .overflow_hidden()
                .whitespace_nowrap()
                .flex()
                .when(slot.column > 0, |cell| {
                    cell.border_l_1().border_color(theme::border())
                });
            let cell = match slot.kind {
                SlotKind::Cell(cell_index) => {
                    let data = &row.cells[cell_index];
                    let align = data.align.or_else(|| {
                        (slot.span == 1 && table.numeric.get(slot.column) == Some(&true))
                            .then_some(Align::Right)
                    });
                    let cell_background = data.background.map(to_hsla);
                    let behind = cell_background.or(background);
                    let cell = match align {
                        Some(Align::Right) => cell.justify_end(),
                        Some(Align::Center) => cell.justify_center(),
                        _ => cell,
                    }
                    .when_some(cell_background, |cell, color| {
                        cell.bg(color).text_color(text_on(Some(color)))
                    })
                    .when(row.ellipsis, |cell| cell.text_color(theme::muted()));
                    cell.child(inline_text(("text", cell_index), &data.inline, behind))
                        .pin(format!("cell:{}:{}", index + 1, slot.column + 1))
                        .interactive(false)
                        .into_any_element()
                }
                SlotKind::Covered {
                    row: source,
                    cell: source_cell,
                } => {
                    // The spanning cell continues: no line between them.
                    let data = &table.rows[source].cells[source_cell];
                    cell.mt(px(-1.))
                        .pt(px(3.))
                        .when_some(data.background.map(to_hsla), |cell, color| cell.bg(color))
                        .into_any_element()
                }
                SlotKind::Empty => cell.into_any_element(),
            };
            element = element.child(cell);
        }
        element
            .pin(format!("row:{}", index + 1))
            .interactive(false)
            .into_any_element()
    };
    let head = (head_rows > 0).then(|| {
        div()
            .flex()
            .flex_col()
            .flex_none()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border())
            .children((0..head_rows).map(row_element))
            .into_any_element()
    });
    let body = div()
        .flex()
        .flex_col()
        .flex_none()
        .children((head_rows..table.rows.len()).map(row_element))
        .into_any_element();
    // As wide as its columns (plus the border), so a wide table overflows
    // the scroller instead of being squeezed into it.
    let grid = div()
        .flex_none()
        .w(px(total + 2.))
        .border_1()
        .border_color(theme::border())
        .rounded_sm()
        .bg(background.unwrap_or_else(theme::background))
        .text_size(px(TABLE_FONT_SIZE))
        .line_height(px(TABLE_LINE_HEIGHT))
        .child(StickyHead { head, body })
        .pin(id.unwrap_or_else(|| "table".into()));
    let mut scroller = div()
        .id("table-scroll")
        .w_full()
        .flex()
        .overflow_x_scroll()
        .child(grid);
    scroller.style().restrict_scroll_to_axis = Some(true);
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .when_some(table.caption.as_ref(), |frame, caption| {
            frame.child(
                div()
                    .text_sm()
                    .text_color(theme::muted())
                    .child(inline_text("caption", caption, background)),
            )
        })
        .child(scroller)
        .when(table.hidden_rows > 0, |frame| {
            frame.child(
                div()
                    .text_sm()
                    .italic()
                    .text_color(theme::muted())
                    .child(selectable(
                        "hidden-rows",
                        format!(
                            "{} more row{} not shown here (Open in browser shows the whole table).",
                            table.hidden_rows,
                            if table.hidden_rows == 1 { "" } else { "s" }
                        ),
                    )),
            )
        })
        .into_any_element()
}

/// A table's head above its body; when the table's top has scrolled out
/// of the visible area (the content mask), the head is drawn at the top
/// of what is visible instead, over the body, until the table's end.
struct StickyHead {
    head: Option<AnyElement>,
    body: AnyElement,
}

impl IntoElement for StickyHead {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for StickyHead {
    type RequestLayoutState = Option<LayoutId>;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Option<LayoutId>) {
        let head = self
            .head
            .as_mut()
            .map(|head| head.request_layout(window, cx));
        let body = self.body.request_layout(window, cx);
        let style = Style {
            display: gpui::Display::Flex,
            flex_direction: gpui::FlexDirection::Column,
            ..Style::default()
        };
        let children: Vec<LayoutId> = head.into_iter().chain([body]).collect();
        (window.request_layout(style, children, cx), head)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        head_layout: &mut Option<LayoutId>,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.body.prepaint(window, cx);
        let (Some(head), Some(layout)) = (self.head.as_mut(), *head_layout) else {
            return;
        };
        let head_height = window.layout_bounds(layout).size.height;
        let visible_top = window.content_mask().bounds.top();
        let room = (bounds.size.height - head_height).max(px(0.));
        let offset = (visible_top - bounds.top()).clamp(px(0.), room);
        window.with_element_offset(point(px(0.), offset), |window| head.prepaint(window, cx));
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Option<LayoutId>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.body.paint(window, cx);
        if let Some(head) = self.head.as_mut() {
            head.paint(window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;

    const DATAFRAME: &str = include_str!("../../../notebook/tests/fixtures/html/dataframe.html");
    const STYLER: &str = include_str!("../../../notebook/tests/fixtures/html/styler.html");

    struct Html(&'static str);

    impl Render for Html {
        fn render(&mut self, _: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
            let parsed = parsed(self.0, cx);
            let blocks = parsed.as_ref().as_ref().expect("renders");
            div()
                .id("scroll")
                .size_full()
                .overflow_y_scroll()
                .child(render_html("html", blocks))
        }
    }

    #[gpui::test]
    fn tables_are_measured_once_and_render(cx: &mut TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| Html(DATAFRAME));
        cx.run_until_parked();
        let first = cx.update(|_, cx| parsed(DATAFRAME, cx));
        let Ok(blocks) = first.as_ref() else { panic!() };
        let Block::Table(table) = &blocks[0] else {
            panic!()
        };
        assert_eq!(table.widths.len(), 4);
        assert!(table.widths.iter().all(|width| *width > CELL_PADDING * 2.));
        // Cached: the same parse comes back.
        let again = cx.update(|_, cx| parsed(DATAFRAME, cx));
        assert!(Arc::ptr_eq(&first, &again));
    }

    #[gpui::test]
    fn styler_backgrounds_get_readable_text(cx: &mut TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| Html(STYLER));
        cx.run_until_parked();
        let yellow = to_hsla(html::parse_color("yellow").unwrap());
        let dark = to_hsla(html::parse_color("#023858").unwrap());
        // Whatever the theme's text colour, cells keep a readable one.
        assert!(contrast(text_on(Some(yellow)), yellow) >= 3.);
        assert!(contrast(text_on(Some(dark)), dark) >= 3.);
        // Colours that vanish on the background are dropped.
        let white = html::parse_color("white").unwrap();
        assert_eq!(readable(white, Some(gpui::white())), None);
        assert!(readable(white, Some(gpui::black())).is_some());
    }

    #[test]
    fn overlapping_highlights_are_split() {
        let bold = HighlightStyle {
            font_weight: Some(FontWeight::BOLD),
            ..HighlightStyle::default()
        };
        let link = HighlightStyle {
            color: Some(gpui::red()),
            ..HighlightStyle::default()
        };
        let flat = flatten_highlights(vec![(0..10, bold), (5..8, link)]);
        assert_eq!(flat.len(), 3);
        assert_eq!(flat[0], (0..5, bold));
        assert_eq!(flat[1].0, 5..8);
        assert_eq!(flat[1].1.font_weight, Some(FontWeight::BOLD));
        assert_eq!(flat[1].1.color, Some(gpui::red()));
        assert_eq!(flat[2], (8..10, bold));
    }
}
