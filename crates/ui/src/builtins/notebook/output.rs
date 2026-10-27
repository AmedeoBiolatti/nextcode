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
    hash::{DefaultHasher, Hash, Hasher},
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
};

use gpui::{
    AnyElement, App, ElementId, FontStyle, FontWeight, HighlightStyle, ImageFormat, SharedString,
    UnderlineStyle, Window, div, img, prelude::*, px,
};
use notebook::{
    Metadata, Output, ansi,
    outputs::{Rich, richest},
};
use terminal::Palette;

use super::{
    html::{parsed, render_html},
    marimo_ui,
    widgets::{WidgetEvent, WidgetInputs, WidgetRender, widget_output},
};
use crate::{
    git_dialogs::button,
    images::{DecodedImage, Lru, content_hash, decode_image, fit, image_element, sized},
    markdown::{MarkdownImages, markdown},
    pinpoint::Pin,
    selectable::selectable,
    terminal_panel::to_hsla,
    theme,
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
    fit(get("width"), get("height"), natural)
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
    output.data()?.get(PLOTLY_MIME)
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
                    let image = img(decoded.image.clone()).with_fallback(placeholder);
                    sized(image, size.or(decoded.size))
                }
                None => placeholder(),
            }
        }
        Rich::Markdown(text) => markdown("markdown", text, MarkdownImages::default()),
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
        Rich::MarimoUi { parts, .. } => marimo_ui::render(parts, options, on_action),
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

    use std::sync::Arc;

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

        let metadata = json!({"image/png": {"width": 100}});
        let metadata = metadata.as_object().unwrap();
        assert_eq!(
            metadata_size(metadata, "image/png", Some((2., 3.))),
            Some((100., 150.))
        );
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
