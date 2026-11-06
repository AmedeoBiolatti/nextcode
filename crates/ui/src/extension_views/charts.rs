//! Native charts: one canvas a chart, its axes laid out with shaped text, one path a series.
use super::ExtensionView;
use crate::{pinpoint::Pin, theme};
use axis::{Axis, Ticks};
use extension_api::{AxisFormat, ChartSpec, Scale, SeriesStyle, UiEvent, UiEventKind, Widget};
use gpui::{
    AnyElement, App, Bounds, ContentMask, Context, Hsla, MouseButton, PathBuilder, Pixels, Point,
    ShapedLine, SharedString, TextRun, Window, canvas, div, fill, point, prelude::*, px, size,
};
use std::{cell::Cell, rc::Rc, sync::Arc};

pub(super) mod axis;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Range {
    pub min: f64,
    pub max: f64,
}
impl Range {
    pub(super) fn new(min: f64, max: f64) -> Self {
        if !min.is_finite() || !max.is_finite() {
            return Self { min: 0., max: 1. };
        }
        let scale = min.abs().max(max.abs()).max(1.);
        if max - min >= scale * 1e-9 {
            Self { min, max }
        } else {
            let center = min + (max - min) / 2.;
            let padding = (scale * 0.05).max(0.5);
            Self {
                min: (center - padding).max(-1e12),
                max: (center + padding).min(1e12),
            }
        }
    }
    fn fraction(self, value: f64) -> f64 {
        (value - self.min) / (self.max - self.min)
    }
    fn at(self, fraction: f64) -> f64 {
        self.min + fraction * (self.max - self.min)
    }
    fn contains(self, value: f64) -> bool {
        value >= self.min && value <= self.max
    }
    pub(super) fn fit(self, full: Self) -> Self {
        let span = (self.max - self.min).min(full.max - full.min);
        let min = self.min.clamp(full.min, (full.max - span).max(full.min));
        let fitted = Self {
            min,
            max: (min + span).min(full.max),
        };
        // A tiny old viewport may be below the precision of a new, much
        // larger data domain. Never feed a zero span to rendering/hit testing.
        if fitted.max > fitted.min {
            fitted
        } else {
            full
        }
    }
    pub(super) fn zoom(self, full: Self, factor: f64) -> Self {
        let width = ((self.max - self.min) * factor)
            .clamp((full.max - full.min) / 1024., full.max - full.min);
        let center = self.min + (self.max - self.min) / 2.;
        Self {
            min: center - width / 2.,
            max: center + width / 2.,
        }
        .fit(full)
    }
    /// The sub-range between two fractions of this one, at least 1/1024 of `full`.
    fn between(self, full: Self, a: f64, b: f64) -> Self {
        let (a, b) = (a.min(b).clamp(0., 1.), a.max(b).clamp(0., 1.));
        let min_span = (full.max - full.min) / 1024.;
        let (mut min, mut max) = (self.at(a), self.at(b));
        if max - min < min_span {
            let center = (min + max) / 2.;
            min = center - min_span / 2.;
            max = center + min_span / 2.;
        }
        Self { min, max }.fit(full)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Line,
    Scatter,
    Bar,
    Histogram,
}
/// A datum in axis space (log axes hold logarithms); bars and bins carry their extent.
#[derive(Clone, Copy, Debug)]
struct Datum {
    x: f64,
    y: f64,
    bar: Option<(f64, f64)>,
}
struct Series {
    name: String,
    color: u8,
    style: SeriesStyle,
    data: Vec<Datum>,
    labels: Vec<Option<String>>,
}
pub(super) struct Geometry {
    kind: Kind,
    series: Vec<Series>,
    categories: Vec<String>,
    x: Range,
    y: Range,
    x_axis: Axis,
    y_axis: Axis,
    x_label: String,
    y_label: String,
}
impl Geometry {
    fn new(chart: &ChartSpec) -> Self {
        let mut series = Vec::new();
        let mut categories = Vec::new();
        let (kind, x_axis, y_axis, x_label, y_label) = match chart {
            ChartSpec::Line {
                series: data,
                x_label,
                y_label,
                x_scale,
                y_scale,
                x_format,
                y_format,
            }
            | ChartSpec::Scatter {
                series: data,
                x_label,
                y_label,
                x_scale,
                y_scale,
                x_format,
                y_format,
            } => {
                let line = matches!(chart, ChartSpec::Line { .. });
                let x_axis = Axis {
                    scale: *x_scale,
                    format: *x_format,
                };
                let y_axis = Axis {
                    scale: *y_scale,
                    format: *y_format,
                };
                for (index, s) in data.iter().enumerate() {
                    series.push(Series {
                        name: s.name.clone(),
                        color: s.color.unwrap_or(index as u8 % 12),
                        style: s.style.unwrap_or(if line {
                            SeriesStyle::Line
                        } else {
                            SeriesStyle::Points
                        }),
                        data: s
                            .points
                            .iter()
                            .map(|p| Datum {
                                x: x_axis.to_axis(p.x),
                                y: y_axis.to_axis(p.y),
                                bar: None,
                            })
                            .collect(),
                        labels: s.points.iter().map(|p| p.label.clone()).collect(),
                    });
                }
                (
                    if line { Kind::Line } else { Kind::Scatter },
                    x_axis,
                    y_axis,
                    x_label,
                    y_label,
                )
            }
            ChartSpec::Bar {
                labels,
                series: data,
                x_label,
                y_label,
                y_format,
            } => {
                categories = labels.clone();
                let width = 0.8 / data.len().max(1) as f64;
                for (index, s) in data.iter().enumerate() {
                    series.push(Series {
                        name: s.name.clone(),
                        color: s.color.unwrap_or(index as u8 % 12),
                        style: SeriesStyle::Points,
                        data: s
                            .values
                            .iter()
                            .enumerate()
                            .map(|(category, value)| {
                                let left = category as f64 - 0.4 + index as f64 * width;
                                Datum {
                                    x: left + width / 2.,
                                    y: *value,
                                    bar: Some((left, left + width)),
                                }
                            })
                            .collect(),
                        labels: vec![None; s.values.len()],
                    });
                }
                (
                    Kind::Bar,
                    Axis::PLAIN,
                    Axis {
                        scale: Scale::Linear,
                        format: *y_format,
                    },
                    x_label,
                    y_label,
                )
            }
            ChartSpec::Histogram {
                values,
                bins,
                x_label,
                y_label,
            } => {
                let min = values.iter().copied().fold(f64::INFINITY, f64::min);
                let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                let range = Range::new(min, max);
                let bins = usize::from(*bins).max(1);
                let mut counts = vec![0u32; bins];
                for value in values {
                    let index = ((range.fraction(*value) * bins as f64).floor().max(0.) as usize)
                        .min(bins - 1);
                    counts[index] += 1;
                }
                series.push(Series {
                    name: "Count".into(),
                    color: 0,
                    style: SeriesStyle::Points,
                    data: counts
                        .into_iter()
                        .enumerate()
                        .map(|(index, count)| {
                            let left = range.at(index as f64 / bins as f64);
                            let right = range.at((index + 1) as f64 / bins as f64);
                            Datum {
                                x: left + (right - left) / 2.,
                                y: f64::from(count),
                                bar: Some((left, right)),
                            }
                        })
                        .collect(),
                    labels: vec![None; bins],
                });
                (Kind::Histogram, Axis::PLAIN, Axis::PLAIN, x_label, y_label)
            }
        };
        let all = || series.iter().flat_map(|s| s.data.iter());
        let (mut xmin, mut xmax) = all().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), d| {
            let (a, b) = d.bar.unwrap_or((d.x, d.x));
            (lo.min(a), hi.max(b))
        });
        if kind == Kind::Bar {
            (xmin, xmax) = (-0.5, categories.len() as f64 - 0.5);
        }
        let (mut ymin, mut ymax) = all().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), d| {
            (lo.min(d.y), hi.max(d.y))
        });
        if matches!(kind, Kind::Bar | Kind::Histogram) {
            ymin = ymin.min(0.);
            ymax = ymax.max(0.);
        }
        // A scatter's dots aren't cut in half at the plot's edges.
        let x = if kind == Kind::Scatter {
            let range = Range::new(xmin, xmax);
            let pad = (range.max - range.min) * 0.03;
            Range::new(range.min - pad, range.max + pad)
        } else {
            Range::new(xmin, xmax)
        };
        Self {
            kind,
            series,
            categories,
            x,
            y: Range::new(ymin, ymax),
            x_axis,
            y_axis,
            x_label: x_label.clone(),
            y_label: y_label.clone(),
        }
    }

    fn datum(&self, (series, index): (u32, u32)) -> Option<&Datum> {
        self.series.get(series as usize)?.data.get(index as usize)
    }

    /// The datum a click selects: the bar under the pointer, else the nearest point within 24 px.
    fn hit(&self, frame: &Frame, zoom: Range, at: (f64, f64)) -> Option<(u32, u32)> {
        let (width, height) = frame.size();
        if !(0.0..=1.0).contains(&at.0) || !(0.0..=1.0).contains(&at.1) {
            return None;
        }
        let mut nearest = None;
        let mut best = 24. * 24.;
        for (s, series) in self.series.iter().enumerate() {
            for (i, datum) in series.data.iter().enumerate() {
                if let Some((left, right)) = datum.bar {
                    let (a, b) = (zoom.fraction(left), zoom.fraction(right));
                    if at.0 >= a && at.0 <= b {
                        return Some((s as u32, i as u32));
                    }
                    continue;
                }
                let fraction = zoom.fraction(datum.x);
                if !(0.0..=1.0).contains(&fraction) {
                    continue;
                }
                let dx = (fraction - at.0) * width;
                let dy = (1. - frame.y.fraction(datum.y) - at.1) * height;
                let distance = dx * dx + dy * dy;
                if distance <= best {
                    best = distance;
                    nearest = Some((s as u32, i as u32));
                }
            }
        }
        nearest
    }

    /// What hover shows at a pointer: every line at the nearest x, the nearest dot, or a category's bars.
    fn hover(&self, frame: &Frame, zoom: Range, at: (f64, f64)) -> Option<(f64, Vec<(u32, u32)>)> {
        if !(0.0..=1.0).contains(&at.0) || !(-0.05..=1.05).contains(&at.1) {
            return None;
        }
        match self.kind {
            Kind::Line => {
                let x = zoom.at(at.0);
                let nearest = self
                    .series
                    .iter()
                    .flat_map(|s| s.data.iter())
                    .filter(|d| zoom.contains(d.x))
                    .min_by(|a, b| (a.x - x).abs().total_cmp(&(b.x - x).abs()))?
                    .x;
                Some((nearest, self.at_x(nearest)))
            }
            Kind::Scatter => {
                let hit = self.hit(frame, zoom, at)?;
                Some((self.datum(hit)?.x, vec![hit]))
            }
            Kind::Bar | Kind::Histogram => {
                let x = zoom.at(at.0);
                let items: Vec<_> = self
                    .series
                    .iter()
                    .enumerate()
                    .flat_map(|(s, series)| {
                        series.data.iter().enumerate().filter_map(move |(i, d)| {
                            let (left, right) = d.bar?;
                            // A category's whole slot, not only its bars, is the target.
                            let slot = if self.kind == Kind::Bar {
                                (i as f64 - 0.5, i as f64 + 0.5)
                            } else {
                                (left, right)
                            };
                            (x >= slot.0 && x < slot.1).then_some((s as u32, i as u32))
                        })
                    })
                    .collect();
                let first = self.datum(*items.first()?)?;
                let center = if self.kind == Kind::Bar {
                    first.bar.map_or(first.x, |_| items[0].1 as f64)
                } else {
                    first.x
                };
                Some((center, items))
            }
        }
    }

    /// Each line's point nearest to `x`, for the lines whose data spans it.
    fn at_x(&self, x: f64) -> Vec<(u32, u32)> {
        self.series
            .iter()
            .enumerate()
            .filter_map(|(s, series)| {
                let (lo, hi) = series
                    .data
                    .iter()
                    .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), d| {
                        (lo.min(d.x), hi.max(d.x))
                    });
                if x < lo || x > hi {
                    return None;
                }
                let (index, _) = series
                    .data
                    .iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| (a.x - x).abs().total_cmp(&(b.x - x).abs()))?;
                Some((s as u32, index as u32))
            })
            .collect()
    }
}

/// What the last frame drew: the plot's window bounds and the y domain its ticks rounded to.
#[derive(Clone, Copy, Debug)]
struct Frame {
    plot: Bounds<Pixels>,
    y: Range,
}
impl Frame {
    fn size(&self) -> (f64, f64) {
        (
            f64::from(f32::from(self.plot.size.width)),
            f64::from(f32::from(self.plot.size.height)),
        )
    }
    fn fraction(&self, position: Point<Pixels>) -> Option<(f64, f64)> {
        let (width, height) = self.size();
        (width > 0. && height > 0.).then(|| {
            (
                f64::from(f32::from(position.x - self.plot.origin.x)) / width,
                f64::from(f32::from(position.y - self.plot.origin.y)) / height,
            )
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Hover {
    x: f64,
    items: Vec<(u32, u32)>,
    /// Where the pointer is, in window coordinates; `None` when another chart of the group is hovered.
    pointer: Option<Point<Pixels>>,
}

pub(super) struct ChartState {
    geometry: Arc<Geometry>,
    zoom: Range,
    frame: Rc<Cell<Option<Frame>>>,
    hover: Option<Hover>,
    selected: Option<(u32, u32)>,
    /// A drag across the plot: start and current fractions of the x axis.
    drag: Option<(f64, f64)>,
    pub(super) sync: Option<String>,
}
impl ChartState {
    pub(super) fn new(spec: &ChartSpec, sync: Option<String>) -> Self {
        let geometry = Arc::new(Geometry::new(spec));
        Self {
            zoom: geometry.x,
            geometry,
            frame: Rc::new(Cell::new(None)),
            hover: None,
            selected: None,
            drag: None,
            sync,
        }
    }
    pub(super) fn replace(&mut self, spec: &ChartSpec, sync: Option<String>) {
        let geometry = Arc::new(Geometry::new(spec));
        let full_before = self.zoom == self.geometry.x;
        self.zoom = if full_before {
            geometry.x
        } else {
            self.zoom.fit(geometry.x)
        };
        self.geometry = geometry;
        self.hover = None;
        self.drag = None;
        self.sync = sync;
        if self
            .selected
            .is_some_and(|selection| self.geometry.datum(selection).is_none())
        {
            self.selected = None;
        }
    }
    fn hover_at(&self, position: Point<Pixels>) -> Option<Hover> {
        let frame = self.frame.get()?;
        let at = frame.fraction(position)?;
        let (x, items) = self.geometry.hover(&frame, self.zoom, at)?;
        Some(Hover {
            x,
            items,
            pointer: Some(position),
        })
    }
    fn hit(&self, position: Point<Pixels>) -> Option<(u32, u32)> {
        let frame = self.frame.get()?;
        self.geometry
            .hit(&frame, self.zoom, frame.fraction(position)?)
    }
}

// --- colors

/// Twelve series colors. The first eight are a categorical palette checked for color-vision deficiencies against
/// light and dark surfaces (adjacent slots stay apart); 9–12 extend it for charts with many series.
const LIGHT: [u32; 12] = [
    0x2a78d6, 0xeb6834, 0x1baf7a, 0xeda100, 0xe87ba4, 0x008300, 0x6250d6, 0xe34948, 0x2b9fbf,
    0x9c6b4e, 0x7f8f00, 0x8a8984,
];
const DARK: [u32; 12] = [
    0x3987e5, 0xd95926, 0x199e70, 0xc98500, 0xd55181, 0x008300, 0x9085e9, 0xe66767, 0x3ab0d0,
    0xb0805f, 0x9aa82a, 0x9a9993,
];
pub(super) fn color(index: u8) -> Hsla {
    let dark = theme::luminance(theme::background()) < 0.4;
    let palette = if dark { &DARK } else { &LIGHT };
    gpui::rgb(palette[usize::from(index) % palette.len()]).into()
}

// --- drawing

const FONT_SIZE: f32 = 11.;
const LINE_HEIGHT: f32 = 14.;

fn shape(window: &mut Window, text: &str, color: Hsla) -> ShapedLine {
    let text: SharedString = text.replace('\n', " ").into();
    let run = TextRun {
        len: text.len(),
        font: gpui::font(theme::UI_FONT),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(text, px(FONT_SIZE), &[run], None)
}

struct Layout {
    plot: Bounds<Pixels>,
    y: Range,
    y_ticks: Vec<(f64, ShapedLine)>,
    x_ticks: Vec<(f64, ShapedLine)>,
    x_title: Option<ShapedLine>,
    y_title: Option<ShapedLine>,
}

/// The y domain rounded out to its outer ticks, and the ticks.
fn y_ticks(geometry: &Geometry, height: f32) -> (Range, Ticks) {
    let target = ((height / 36.).floor() as usize).clamp(2, 6);
    let data = geometry.y;
    let rounded = geometry.y_axis.scale == Scale::Linear
        && matches!(
            geometry.y_axis.format,
            AxisFormat::Number | AxisFormat::Percent
        );
    if rounded {
        let step = axis::nice_step(
            (data.max - data.min)
                * if geometry.y_axis.format == AxisFormat::Percent {
                    100.
                } else {
                    1.
                },
            target,
        ) / if geometry.y_axis.format == AxisFormat::Percent {
            100.
        } else {
            1.
        };
        // Out to the next multiples, an exact multiple (a bar chart's zero) staying where it is.
        let domain = Range::new(
            (data.min / step + 1e-9).floor() * step,
            (data.max / step - 1e-9).ceil() * step,
        );
        (
            domain,
            Ticks::with_step(domain.min, domain.max, step, geometry.y_axis),
        )
    } else {
        let pad = (data.max - data.min) * 0.05;
        let domain = Range::new(data.min - pad, data.max + pad);
        (
            domain,
            Ticks::new(domain.min, domain.max, geometry.y_axis, target),
        )
    }
}

fn layout(geometry: &Geometry, zoom: Range, bounds: Bounds<Pixels>, window: &mut Window) -> Layout {
    let muted = theme::muted();
    let y_title = (!geometry.y_label.is_empty()).then(|| shape(window, &geometry.y_label, muted));
    let x_title = (!geometry.x_label.is_empty()).then(|| shape(window, &geometry.x_label, muted));
    let top = if y_title.is_some() {
        LINE_HEIGHT + 6.
    } else {
        6.
    };
    let bottom = LINE_HEIGHT + 4. + if x_title.is_some() { LINE_HEIGHT } else { 0. };
    let plot_height = (f32::from(bounds.size.height) - top - bottom).max(24.);
    let (y, ticks) = y_ticks(geometry, plot_height);
    let y_ticks: Vec<_> = ticks
        .values
        .iter()
        .zip(ticks.labels())
        .filter(|(value, _)| y.contains(**value) || (**value - y.max).abs() < 1e-9)
        .map(|(value, label)| (*value, shape(window, &label, muted)))
        .collect();
    let gutter = y_ticks
        .iter()
        .map(|(_, line)| f32::from(line.width))
        .fold(0., f32::max)
        + 8.;
    let plot = Bounds::new(
        point(bounds.origin.x + px(gutter), bounds.origin.y + px(top)),
        size(
            (bounds.size.width - px(gutter + 8.)).max(px(24.)),
            px(plot_height),
        ),
    );
    let width = f32::from(plot.size.width);
    let x_ticks = if geometry.kind == Kind::Bar {
        // A label a category, centred under it; every n-th when they don't fit.
        let shaped: Vec<_> = geometry
            .categories
            .iter()
            .enumerate()
            .filter(|(i, _)| zoom.contains(*i as f64))
            .map(|(i, label)| (i as f64, shape(window, label, muted)))
            .collect();
        let widest = shaped
            .iter()
            .map(|(_, line)| f32::from(line.width))
            .fold(0., f32::max)
            + 8.;
        let slot = width / shaped.len().max(1) as f32;
        let every = (widest / slot.max(1.)).ceil().max(1.) as usize;
        shaped.into_iter().step_by(every).collect()
    } else {
        let mut chosen = Vec::new();
        for target in (2..=7).rev() {
            let ticks = Ticks::new(zoom.min, zoom.max, geometry.x_axis, target);
            let shaped: Vec<_> = ticks
                .values
                .iter()
                .zip(ticks.labels())
                .filter(|(value, _)| zoom.contains(**value))
                .map(|(value, label)| (*value, shape(window, &label, muted)))
                .collect();
            let needed: f32 = shaped
                .iter()
                .map(|(_, line)| f32::from(line.width) + 16.)
                .sum();
            chosen = shaped;
            if needed <= width {
                break;
            }
        }
        chosen
    };
    Layout {
        plot,
        y,
        y_ticks,
        x_ticks,
        x_title,
        y_title,
    }
}

struct Paint {
    geometry: Arc<Geometry>,
    zoom: Range,
    hover: Option<Hover>,
    selected: Option<(u32, u32)>,
    drag: Option<(f64, f64)>,
    bounds: Bounds<Pixels>,
}

fn rect(window: &mut Window, bounds: Bounds<Pixels>, color: Hsla, radius: f32) {
    window.paint_quad(fill(bounds, color).corner_radii(px(radius)));
}
fn dot(window: &mut Window, center: Point<Pixels>, radius: f32, color: Hsla) {
    rect(
        window,
        Bounds::new(
            point(center.x - px(radius), center.y - px(radius)),
            size(px(radius * 2.), px(radius * 2.)),
        ),
        color,
        radius,
    );
}

fn paint(layout: Layout, state: Paint, window: &mut Window, cx: &mut App) {
    let Layout {
        plot,
        y,
        y_ticks,
        x_ticks,
        x_title,
        y_title,
    } = layout;
    let geometry = &state.geometry;
    let zoom = state.zoom;
    let bounds = state.bounds;
    let line_height = px(LINE_HEIGHT);
    let x_px = |x: f64| plot.origin.x + plot.size.width * zoom.fraction(x) as f32;
    let y_px = |v: f64| plot.origin.y + plot.size.height * (1. - y.fraction(v)) as f32;
    let at = |d: &Datum| point(x_px(d.x), y_px(d.y));
    let grid = theme::border();
    // Horizontal gridlines on the y ticks, their labels right-aligned against the plot.
    for (value, label) in &y_ticks {
        let row = y_px(*value).round();
        rect(
            window,
            Bounds::new(point(plot.origin.x, row), size(plot.size.width, px(1.))),
            grid.opacity(0.7),
            0.,
        );
        let origin = point(plot.origin.x - px(6.) - label.width, row - line_height / 2.);
        label.paint(origin, line_height, window, cx).ok();
    }
    // The baseline: zero for bars, else the plot's bottom.
    let base = if matches!(geometry.kind, Kind::Bar | Kind::Histogram) && y.contains(0.) {
        y_px(0.).round()
    } else {
        plot.origin.y + plot.size.height
    };
    rect(
        window,
        Bounds::new(point(plot.origin.x, base), size(plot.size.width, px(1.))),
        theme::muted().opacity(0.6),
        0.,
    );
    let label_top = plot.origin.y + plot.size.height + px(4.);
    for (value, label) in &x_ticks {
        let center = x_px(*value);
        let left = (center - label.width / 2.)
            .max(bounds.origin.x)
            .min(bounds.origin.x + bounds.size.width - label.width);
        label
            .paint(point(left, label_top), line_height, window, cx)
            .ok();
    }
    if let Some(title) = &x_title {
        let left = plot.origin.x + plot.size.width - title.width;
        title
            .paint(
                point(left, label_top + line_height),
                line_height,
                window,
                cx,
            )
            .ok();
    }
    if let Some(title) = &y_title {
        title
            .paint(
                point(bounds.origin.x, bounds.origin.y),
                line_height,
                window,
                cx,
            )
            .ok();
    }

    window.with_content_mask(
        Some(ContentMask {
            bounds: Bounds::new(
                point(plot.origin.x - px(5.), plot.origin.y - px(5.)),
                size(plot.size.width + px(10.), plot.size.height + px(10.)),
            ),
        }),
        |window| {
            // The hovered category's slot, under its bars.
            if let (Some(hover), Kind::Bar | Kind::Histogram) = (&state.hover, geometry.kind)
                && let Some(d) = hover.items.first().and_then(|i| geometry.datum(*i))
            {
                let (left, right) = if geometry.kind == Kind::Bar {
                    let i = hover.items[0].1 as f64;
                    (i - 0.5, i + 0.5)
                } else {
                    d.bar.unwrap_or((d.x, d.x))
                };
                let (a, b) = (x_px(left), x_px(right));
                rect(
                    window,
                    Bounds::new(point(a, plot.origin.y), size(b - a, plot.size.height)),
                    theme::hover_row(),
                    2.,
                );
            }
            for series in &geometry.series {
                draw_series(series, geometry.kind, plot, zoom, &x_px, &y_px, window);
            }
            // The crosshair and the points it reads.
            if let Some(hover) = &state.hover {
                if geometry.kind == Kind::Line {
                    let x = x_px(hover.x).round();
                    rect(
                        window,
                        Bounds::new(point(x, plot.origin.y), size(px(1.), plot.size.height)),
                        theme::muted().opacity(0.7),
                        0.,
                    );
                }
                if !matches!(geometry.kind, Kind::Bar | Kind::Histogram) {
                    for item in &hover.items {
                        if let Some(d) = geometry.datum(*item) {
                            let color = color(geometry.series[item.0 as usize].color);
                            dot(window, at(d), 5., theme::background());
                            dot(window, at(d), 3.5, color);
                        }
                    }
                }
            }
            if let Some(d) = state.selected.and_then(|s| geometry.datum(s))
                && d.bar.is_none()
            {
                dot(window, at(d), 6., theme::text());
                dot(window, at(d), 4.5, theme::background());
                dot(window, at(d), 3., theme::text());
            }
            if let Some((a, b)) = state.drag {
                let (a, b) = (a.min(b).clamp(0., 1.), a.max(b).clamp(0., 1.));
                let left = plot.origin.x + plot.size.width * a as f32;
                let right = plot.origin.x + plot.size.width * b as f32;
                rect(
                    window,
                    Bounds::new(
                        point(left, plot.origin.y),
                        size(right - left, plot.size.height),
                    ),
                    theme::accent().opacity(0.14),
                    0.,
                );
            }
        },
    );
}

fn draw_series(
    series: &Series,
    kind: Kind,
    plot: Bounds<Pixels>,
    zoom: Range,
    x_px: &impl Fn(f64) -> Pixels,
    y_px: &impl Fn(f64) -> Pixels,
    window: &mut Window,
) {
    let color = color(series.color);
    let at = |d: &Datum| point(x_px(d.x), y_px(d.y));
    if matches!(kind, Kind::Bar | Kind::Histogram) {
        let zero = y_px(0.);
        for d in &series.data {
            let Some((left, right)) = d.bar else { continue };
            let (mut a, mut b) = (x_px(left), x_px(right));
            // A hairline of surface between neighbouring bars.
            if b - a > px(4.) {
                a += px(1.);
                b -= px(1.);
            }
            let top = y_px(d.y);
            let (from, to) = (top.min(zero), top.max(zero));
            if b > a {
                window.paint_quad(
                    fill(
                        Bounds::new(point(a, from), size(b - a, (to - from).max(px(1.)))),
                        color,
                    )
                    .corner_radii(if d.y >= 0. {
                        gpui::Corners {
                            top_left: px(2.),
                            top_right: px(2.),
                            ..Default::default()
                        }
                    } else {
                        gpui::Corners {
                            bottom_left: px(2.),
                            bottom_right: px(2.),
                            ..Default::default()
                        }
                    }),
                );
            }
        }
        return;
    }
    // Only what the zoomed x range shows, plus one point either side so lines leave the plot at its edges.
    let visible: Vec<&Datum> = {
        let data = &series.data;
        let inside = |d: &Datum| zoom.contains(d.x);
        data.iter()
            .enumerate()
            .filter(|(i, d)| {
                inside(d)
                    || (*i > 0 && inside(&data[i - 1]))
                    || data.get(i + 1).is_some_and(inside)
                    || data.get(i + 1).is_some_and(|next| {
                        (d.x < zoom.min && next.x > zoom.max)
                            || (d.x > zoom.max && next.x < zoom.min)
                    })
                    || (*i > 0 && {
                        let prev = &data[i - 1];
                        (prev.x < zoom.min && d.x > zoom.max)
                            || (prev.x > zoom.max && d.x < zoom.min)
                    })
            })
            .map(|(_, d)| d)
            .collect()
    };
    let lined = !matches!(series.style, SeriesStyle::Points);
    if lined && visible.len() >= 2 {
        let points: Vec<Point<Pixels>> = if series.style == SeriesStyle::Step {
            let mut steps = Vec::with_capacity(visible.len() * 2);
            for pair in visible.windows(2) {
                let (a, b) = (at(pair[0]), at(pair[1]));
                steps.push(a);
                steps.push(point(b.x, a.y));
            }
            steps.push(at(visible[visible.len() - 1]));
            steps
        } else {
            visible.iter().map(|d| at(d)).collect()
        };
        if series.style == SeriesStyle::Area {
            let mut area = PathBuilder::fill();
            let bottom = plot.origin.y + plot.size.height;
            area.move_to(point(points[0].x, bottom));
            for p in &points {
                area.line_to(*p);
            }
            area.line_to(point(points[points.len() - 1].x, bottom));
            area.close();
            if let Ok(path) = area.build() {
                window.paint_path(path, color.opacity(0.16));
            }
        }
        let mut line = PathBuilder::stroke(px(1.75));
        if series.style == SeriesStyle::Dashed {
            line = line.dash_array(&[px(5.), px(4.)]);
        }
        line.move_to(points[0]);
        for p in &points[1..] {
            line.line_to(*p);
        }
        if let Ok(path) = line.build() {
            window.paint_path(path, color);
        }
    }
    // Dots: every point of a dot series; on a line only a point it can't show (a series of one point).
    let radius = if kind == Kind::Scatter { 3.5 } else { 3. };
    if !lined || visible.len() < 2 {
        for d in visible.iter().filter(|d| zoom.contains(d.x)) {
            if kind == Kind::Scatter {
                dot(window, at(d), radius + 1., theme::background().opacity(0.8));
            }
            dot(window, at(d), radius, color);
        }
    }
}

/// A series' key as it is drawn: a short stroke for lines, a dot for dots, a square for bars.
fn legend_key(series: &Series, kind: Kind) -> gpui::Div {
    let key = div().flex_none().bg(color(series.color));
    match (kind, series.style) {
        (Kind::Bar | Kind::Histogram, _) => key.size(px(8.)).rounded_sm(),
        (_, SeriesStyle::Points) => key.size(px(7.)).rounded_full(),
        (_, SeriesStyle::Dashed) => div()
            .flex_none()
            .flex()
            .gap(px(2.))
            .child(div().w(px(5.)).h(px(2.)).bg(color(series.color)))
            .child(div().w(px(5.)).h(px(2.)).bg(color(series.color))),
        _ => key.w(px(12.)).h(px(2.)).rounded_full(),
    }
}

impl ExtensionView {
    fn sync_group(&self, id: &str) -> Vec<String> {
        let Some(group) = self.charts.get(id).and_then(|chart| chart.sync.clone()) else {
            return Vec::new();
        };
        self.charts
            .iter()
            .filter(|(other, chart)| other.as_str() != id && chart.sync.as_ref() == Some(&group))
            .map(|(other, _)| other.clone())
            .collect()
    }
    fn set_hover(&mut self, id: &str, hover: Option<Hover>, cx: &mut Context<Self>) {
        let x = hover.as_ref().map(|hover| hover.x);
        let mut changed = false;
        if let Some(chart) = self.charts.get_mut(id)
            && chart.hover != hover
        {
            chart.hover = hover;
            changed = true;
        }
        for other in self.sync_group(id) {
            let Some(chart) = self.charts.get_mut(&other) else {
                continue;
            };
            let next = x
                .filter(|_| chart.geometry.kind == Kind::Line)
                .map(|x| Hover {
                    x,
                    items: chart.geometry.at_x(x),
                    pointer: None,
                })
                .filter(|hover| !hover.items.is_empty());
            if chart.hover != next {
                chart.hover = next;
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
    }
    /// Zoom a chart (and the charts it is synced with) to `range`, or reset with `None`.
    fn set_zoom(&mut self, id: &str, range: Option<Range>, cx: &mut Context<Self>) {
        let Some(chart) = self.charts.get_mut(id) else {
            return;
        };
        chart.zoom = range.map_or(chart.geometry.x, |range| range.fit(chart.geometry.x));
        chart.hover = None;
        let zoom = chart.zoom;
        let reset = range.is_none();
        for other in self.sync_group(id) {
            if let Some(chart) = self.charts.get_mut(&other) {
                chart.zoom = if reset {
                    chart.geometry.x
                } else {
                    zoom.fit(chart.geometry.x)
                };
                chart.hover = None;
            }
        }
        self.emit(
            UiEvent {
                widget: id.to_owned(),
                kind: UiEventKind::ChartZoom {
                    start: zoom.min,
                    end: zoom.max,
                },
            },
            cx,
        );
        cx.notify();
    }
    fn zoom_chart(&mut self, id: &str, factor: Option<f64>, cx: &mut Context<Self>) {
        let Some(chart) = self.charts.get(id) else {
            return;
        };
        let range = factor.map(|factor| chart.zoom.zoom(chart.geometry.x, factor));
        self.set_zoom(id, range, cx);
    }
    pub(super) fn chart_key(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(id) = self
            .controls
            .iter()
            .find(|(id, handle)| self.charts.contains_key(*id) && handle.is_focused(window))
            .map(|(id, _)| id.clone())
        else {
            return false;
        };
        let factor = match key {
            "+" | "=" | "up" => Some(0.5),
            "-" | "down" => Some(2.),
            "0" | "home" => None,
            _ => return false,
        };
        self.zoom_chart(&id, factor, cx);
        true
    }

    fn chart_tooltip(&self, chart: &ChartState) -> Option<AnyElement> {
        let hover = chart.hover.as_ref()?;
        let pointer = hover.pointer?;
        let frame = chart.frame.get()?;
        let geometry = &chart.geometry;
        let mut rows: Vec<_> = hover
            .items
            .iter()
            .filter_map(|item| {
                let series = geometry.series.get(item.0 as usize)?;
                let datum = series.data.get(item.1 as usize)?;
                Some((
                    series,
                    datum,
                    series.labels.get(item.1 as usize).cloned().flatten(),
                ))
            })
            .collect();
        if rows.is_empty() {
            return None;
        }
        rows.sort_by(|a, b| b.1.y.total_cmp(&a.1.y));
        let y_value = |d: &Datum| axis::exact(geometry.y_axis, geometry.y_axis.to_value(d.y), 4);
        let heading = match geometry.kind {
            Kind::Bar => geometry
                .categories
                .get(hover.items[0].1 as usize)
                .cloned()
                .unwrap_or_default(),
            Kind::Histogram => rows[0]
                .1
                .bar
                .map(|(a, b)| {
                    format!(
                        "{} – {}",
                        axis::exact(Axis::PLAIN, a, 4),
                        axis::exact(Axis::PLAIN, b, 4)
                    )
                })
                .unwrap_or_default(),
            _ => {
                let x = axis::exact(geometry.x_axis, geometry.x_axis.to_value(hover.x), 6);
                if geometry.x_label.is_empty() {
                    x
                } else {
                    format!("{} {x}", geometry.x_label)
                }
            }
        };
        let body = rows.into_iter().take(12).map(|(series, datum, label)| {
            let key = legend_key(series, geometry.kind);
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(div().w(px(10.)).flex().justify_center().child(key))
                .child(
                    div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(SharedString::from(y_value(datum))),
                )
                .child(
                    div()
                        .text_color(theme::muted())
                        .max_w(px(260.))
                        .truncate()
                        .child(SharedString::from(match label {
                            Some(label) if geometry.series.len() == 1 => label,
                            Some(label) => format!("{} · {label}", series.name),
                            None => series.name.clone(),
                        })),
                )
        });
        // Beside the pointer, on the side with more room, inside the plot's frame.
        let relative_x = f32::from(pointer.x - frame.plot.origin.x);
        let right_half = relative_x > f32::from(frame.plot.size.width) / 2.;
        // Deferred: painted over the charts drawn after this one (a grid's next cell).
        Some(
            gpui::deferred(
                div()
                    .absolute()
                    .top(px(0.))
                    .map(|tip| {
                        if right_half {
                            tip.right(frame.plot.size.width - px(relative_x) + px(14.))
                        } else {
                            tip.left(px(relative_x + 14.))
                        }
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .px_2()
                            .py(px(6.))
                            .bg(theme::panel())
                            .border_1()
                            .border_color(theme::border())
                            .rounded_md()
                            .shadow_md()
                            .text_xs()
                            .whitespace_nowrap()
                            .child(
                                div()
                                    .text_color(theme::muted())
                                    .child(SharedString::from(heading)),
                            )
                            .children(body),
                    ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    pub(super) fn render_chart(&self, widget: &Widget, cx: &Context<Self>) -> AnyElement {
        let Widget::Chart {
            id,
            title,
            height,
            legend,
            width,
            controls,
            ..
        } = widget
        else {
            return div().into_any_element();
        };
        let Some(chart) = self.charts.get(id) else {
            return div().into_any_element();
        };
        let weak = cx.entity().downgrade();
        let paint_state = Paint {
            geometry: chart.geometry.clone(),
            zoom: chart.zoom,
            hover: chart.hover.clone(),
            selected: chart.selected,
            drag: chart.drag,
            bounds: Bounds::default(),
        };
        let frame = chart.frame.clone();
        let focus = self.controls.get(id).cloned();
        let (move_weak, move_id) = (weak.clone(), id.clone());
        let (down_weak, down_id) = (weak.clone(), id.clone());
        let (up_weak, up_id) = (weak.clone(), id.clone());
        let (leave_weak, leave_id) = (weak.clone(), id.clone());
        let selector = format!("{id}-plot");
        let plot = div()
            .id(SharedString::from(format!("{id}-plot")))
            .debug_selector(move || selector)
            .relative()
            .flex_1()
            .min_h(px(48.))
            .w_full()
            .cursor_crosshair()
            .on_mouse_move(move |event, _, cx| {
                move_weak
                    .update(cx, |view, cx| {
                        let Some(chart) = view.charts.get_mut(&move_id) else {
                            return;
                        };
                        if let Some((start, _)) = chart.drag {
                            if event.pressed_button == Some(MouseButton::Left)
                                && let Some(frame) = chart.frame.get()
                                && let Some((x, _)) = frame.fraction(event.position)
                            {
                                chart.drag = Some((start, x));
                                cx.notify();
                                return;
                            }
                            chart.drag = None;
                        }
                        let hover = chart.hover_at(event.position);
                        view.set_hover(&move_id, hover, cx);
                    })
                    .ok();
            })
            .on_hover(move |hovered, _, cx| {
                if !hovered {
                    leave_weak
                        .update(cx, |view, cx| view.set_hover(&leave_id, None, cx))
                        .ok();
                }
            })
            .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                if let Some(focus) = &focus {
                    window.focus(focus);
                }
                down_weak
                    .update(cx, |view, cx| {
                        if event.click_count >= 2 {
                            view.set_zoom(&down_id, None, cx);
                            return;
                        }
                        if let Some(chart) = view.charts.get_mut(&down_id)
                            && let Some(frame) = chart.frame.get()
                            && let Some((x, _)) = frame.fraction(event.position)
                        {
                            chart.drag = Some((x, x));
                        }
                    })
                    .ok();
            })
            .on_mouse_up(MouseButton::Left, move |event, _, cx| {
                up_weak
                    .update(cx, |view, cx| {
                        let Some(chart) = view.charts.get_mut(&up_id) else {
                            return;
                        };
                        let Some((start, end)) = chart.drag.take() else {
                            return;
                        };
                        let width = chart.frame.get().map_or(0., |frame| frame.size().0);
                        if (end - start).abs() * width > 4. {
                            let range = chart.zoom.between(chart.geometry.x, start, end);
                            view.set_zoom(&up_id, Some(range), cx);
                            return;
                        }
                        if let Some((series, index)) = chart.hit(event.position) {
                            chart.selected = Some((series, index));
                            view.emit(
                                UiEvent {
                                    widget: up_id.clone(),
                                    kind: UiEventKind::ChartSelect { series, index },
                                },
                                cx,
                            );
                        }
                        cx.notify();
                    })
                    .ok();
            })
            .child(
                canvas(
                    {
                        let geometry = chart.geometry.clone();
                        let zoom = chart.zoom;
                        move |bounds, window, _| {
                            let layout = layout(&geometry, zoom, bounds, window);
                            frame.set(Some(Frame {
                                plot: layout.plot,
                                y: layout.y,
                            }));
                            (layout, bounds)
                        }
                    },
                    move |_, (layout, bounds), window, cx| {
                        paint(
                            layout,
                            Paint {
                                bounds,
                                ..paint_state
                            },
                            window,
                            cx,
                        )
                    },
                )
                .size_full(),
            )
            .children(self.chart_tooltip(chart))
            .pin("plot")
            .label("Chart plot");
        let zoomed = chart.zoom != chart.geometry.x;
        let group = SharedString::from(format!("chart-{id}"));
        // Small icon buttons at the title row's right end: shown while the chart is hovered or zoomed.
        let buttons = [
            ("+", "Zoom in", Some(0.5)),
            ("−", "Zoom out", Some(2.)),
            ("↺", "Reset zoom", None),
        ]
        .into_iter()
        .map(|(icon, label, factor)| {
            let weak = weak.clone();
            let id = id.clone();
            div()
                .id(SharedString::from(format!("{id}-zoom-{label}")))
                .flex_none()
                .size(px(18.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .text_xs()
                .text_color(theme::muted())
                .cursor_pointer()
                .hover(|style| style.bg(theme::hover_row()).text_color(theme::text()))
                .on_click(move |_, _, cx| {
                    weak.update(cx, |view, cx| view.zoom_chart(&id, factor, cx))
                        .ok();
                })
                .child(icon)
                .pin(match factor {
                    Some(value) if value < 1. => "zoom-in",
                    Some(_) => "zoom-out",
                    None => "reset",
                })
                .label(label)
        });
        let geometry = &chart.geometry;
        let show_legend = *legend && geometry.series.len() > 1;
        let legend_row = show_legend.then(|| {
            div()
                .flex()
                .flex_wrap()
                .gap_x_3()
                .gap_y_1()
                .text_xs()
                .text_color(theme::muted())
                .children(geometry.series.iter().map(|series| {
                    let key = legend_key(series, geometry.kind);
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(key)
                        .child(SharedString::from(series.name.clone()))
                }))
        });
        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .min_h(px(22.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .child(SharedString::from(title.clone())),
            )
            .children(
                controls
                    .iter()
                    .map(|control| self.render_widget(control, cx)),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .gap(px(2.))
                    .when(!zoomed, |buttons| {
                        buttons
                            .opacity(0.)
                            .group_hover(group.clone(), |style| style.opacity(1.))
                    })
                    .children(buttons),
            );
        let focused = self.controls.get(id).cloned();
        div()
            .id(SharedString::from(format!("{id}-focus")))
            .group(group)
            .anchor_scroll(self.anchors.get(id).cloned())
            .key_context("ExtensionControl")
            .when_some(focused, |chart, handle| chart.track_focus(&handle))
            .flex()
            .flex_col()
            .gap_1()
            .h(px(f32::from(*height)))
            // A chart's width is what it asks for, never more than its container (a grid cell) has.
            .map(|chart| match width {
                Some(width) => chart.w(px(f32::from(*width))).max_w_full().flex_none(),
                None => chart.w_full().min_w(px(180.)),
            })
            .px(px(10.))
            .py(px(8.))
            .rounded_md()
            .border_1()
            .border_color(theme::border())
            .when(self.focus_visible, |chart| {
                chart.focus(|style| style.border_color(theme::accent()))
            })
            .child(header)
            .children(legend_row)
            .child(plot)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use extension_api::{BarSeries, ChartPoint, ChartSeries};

    fn frame() -> Frame {
        Frame {
            plot: Bounds::new(point(px(0.), px(0.)), size(px(400.), px(200.))),
            y: Range::new(0., 10.),
        }
    }

    #[test]
    fn explicit_colors_survive_missing_series_and_keep_original_event_indices() {
        let series = |name: &str, color| ChartSeries {
            color,
            ..ChartSeries::new(name, [ChartPoint::new(1., 2.)])
        };
        let full = Geometry::new(&ChartSpec::line([
            series("A", Some(2)),
            series("B", Some(11)),
            series("C", None),
        ]));
        let colors: Vec<_> = full.series.iter().map(|s| s.color).collect();
        assert_eq!(colors, [2, 11, 2]);
        let missing = Geometry::new(&ChartSpec::line([series("B", Some(11))]));
        assert_eq!(missing.series[0].color, 11);
        let mut frame = frame();
        frame.y = missing.y;
        assert_eq!(missing.hit(&frame, missing.x, (0.5, 0.5)), Some((0, 0)));
        let bars = Geometry::new(&ChartSpec::bar(
            vec!["x".into()],
            [BarSeries::new("B", vec![2.]).color(11)],
        ));
        assert_eq!(bars.series[0].color, 11);
    }

    #[test]
    fn zoom_is_finite_clamped_and_resettable() {
        let full = Range::new(-10., 10.);
        let mut zoom = full;
        for _ in 0..100 {
            zoom = zoom.zoom(full, 0.5);
        }
        assert!(zoom.max > zoom.min);
        assert!(zoom.min >= full.min && zoom.max <= full.max);
        for _ in 0..100 {
            zoom = zoom.zoom(full, 2.);
        }
        assert_eq!(zoom, full);
        assert!(Range::new(1e12, 1e12).max.is_finite());
        // A drag selects the range between its ends, never less than 1/1024 of the data.
        assert_eq!(full.between(full, 0.75, 0.25), Range::new(-5., 5.));
        let tiny = full.between(full, 0.5, 0.5);
        assert!(tiny.max - tiny.min >= 20. / 1024. - 1e-12);
    }

    #[test]
    fn zoomed_hit_keeps_original_point_indices() {
        let geometry = Geometry::new(&ChartSpec::scatter([ChartSeries::new(
            "A",
            [
                ChartPoint::new(0., 0.),
                ChartPoint::new(5., 5.),
                ChartPoint::new(10., 10.),
            ],
        )]));
        let frame = frame();
        assert_eq!(
            geometry.hit(&frame, Range::new(4., 6.), (0.5, 0.5)),
            Some((0, 1))
        );
        assert_eq!(geometry.hit(&frame, Range::new(4., 6.), (0., 0.)), None);
    }

    #[test]
    fn hover_reads_every_line_at_the_nearest_x() {
        let geometry = Geometry::new(&ChartSpec::line([
            ChartSeries::new("a", [ChartPoint::new(0., 1.), ChartPoint::new(10., 2.)]),
            ChartSeries::new(
                "b",
                [
                    ChartPoint::new(0., 5.),
                    ChartPoint::new(5., 6.),
                    ChartPoint::new(10., 7.),
                ],
            ),
            ChartSeries::new("late", [ChartPoint::new(8., 3.)]),
        ]));
        let frame = frame();
        // Near x = 5: only b has a point there; a's nearest is still shown, late doesn't span it.
        let (x, items) = geometry.hover(&frame, geometry.x, (0.52, 0.5)).unwrap();
        assert_eq!(x, 5.);
        assert_eq!(items, [(0, 0), (1, 1)]);
        let (x, items) = geometry.hover(&frame, geometry.x, (0.99, 0.5)).unwrap();
        assert_eq!(x, 10.);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn grouped_negative_bars_and_histogram_bins_hit_original_indices() {
        let geometry = Geometry::new(&ChartSpec::bar(
            vec!["A".into(), "B".into()],
            [
                BarSeries::new("a", vec![-2., 4.]),
                BarSeries::new("b", vec![3., -1.]),
            ],
        ));
        let datum = geometry.series[1].data[1];
        let frame = frame();
        assert_eq!(
            geometry.hit(&frame, geometry.x, (geometry.x.fraction(datum.x), 0.5)),
            Some((1, 1))
        );
        // Hover takes a category's whole slot.
        let (_, items) = geometry.hover(&frame, geometry.x, (0.55, 0.5)).unwrap();
        assert_eq!(items, [(0, 1), (1, 1)]);
        let hist = Geometry::new(&ChartSpec::histogram(vec![0., 0., 1., 2.], 2));
        assert_eq!(hist.series[0].data[0].y, 2.);
        assert_eq!(hist.series[0].data[1].y, 2.);
        assert_eq!(hist.hit(&frame, hist.x, (0.75, 0.5)), Some((0, 1)));
    }

    #[test]
    fn log_axes_hold_logarithms_and_y_rounds_out_to_ticks() {
        let geometry = Geometry::new(
            &ChartSpec::line([ChartSeries::new(
                "loss",
                [ChartPoint::new(1., 0.01), ChartPoint::new(2., 1.)],
            )])
            .scales(Scale::Linear, Scale::Log),
        );
        assert_eq!(geometry.y, Range::new(-2., 0.));
        let linear = Geometry::new(&ChartSpec::line([ChartSeries::new(
            "auc",
            [ChartPoint::new(0., 0.21), ChartPoint::new(1., 0.95)],
        )]));
        let (domain, ticks) = y_ticks(&linear, 150.);
        assert!(
            (domain.min - 0.2).abs() < 1e-9 && (domain.max - 1.).abs() < 1e-9,
            "{domain:?}"
        );
        assert_eq!(ticks.labels(), ["0.2", "0.4", "0.6", "0.8", "1"]);
        // A count starting at zero keeps zero as its bottom.
        let hist = Geometry::new(&ChartSpec::histogram(vec![0., 0., 1., 2., 2., 2.], 3));
        assert_eq!(y_ticks(&hist, 150.).0.min, 0.);
        // The ticks are the domain's own step: the top gridline is the domain's top, above every point.
        let score = Geometry::new(&ChartSpec::line([ChartSeries::new(
            "best",
            [ChartPoint::new(74., 0.5), ChartPoint::new(75., 0.6406)],
        )]));
        let (domain, ticks) = y_ticks(&score, 120.);
        assert!(domain.max >= 0.6406);
        assert!(
            (ticks.values.last().unwrap() - domain.max).abs() < 1e-9,
            "{ticks:?} {domain:?}"
        );
    }

    #[test]
    fn preserved_zoom_stays_selectable_when_data_moves_to_large_magnitudes() {
        let spec = |a, b| {
            ChartSpec::scatter([ChartSeries::new(
                "Signal",
                [ChartPoint::new(a, 0.), ChartPoint::new(b, 1.)],
            )])
        };
        let mut chart = ChartState::new(&spec(0., 1e-8), None);
        chart.zoom = chart.zoom.zoom(chart.geometry.x, 0.5);
        chart.replace(&spec(9e11, 1e12), None);
        assert_eq!(
            chart.zoom, chart.geometry.x,
            "an unrepresentable old zoom must reset to the new full range"
        );
    }
}
