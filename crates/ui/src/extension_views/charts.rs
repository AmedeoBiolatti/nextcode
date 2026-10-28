use super::ExtensionView;
use crate::{controls, pinpoint::Pin, theme};
use extension_api::{ChartSpec, UiEvent, UiEventKind};
use gpui::{
    AnyElement, Bounds, Context, Hsla, MouseButton, PathBuilder, Pixels, Point, SharedString,
    Window, canvas, div, fill, point, prelude::*, px, size,
};
use std::{cell::Cell, rc::Rc, sync::Arc};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Range {
    pub min: f64,
    pub max: f64,
}
impl Range {
    fn new(min: f64, max: f64) -> Self {
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
    fn fit(self, full: Self) -> Self {
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
}
#[derive(Clone, Copy, Debug)]
struct Datum {
    x: f64,
    y: f64,
    series: u32,
    index: u32,
    bar: Option<(f64, f64)>,
}
struct Geometry {
    points: Vec<Datum>,
    names: Vec<String>,
    colors: Vec<u8>,
    labels: Vec<String>,
    x: Range,
    y: Range,
    line: bool,
    x_label: String,
    y_label: String,
}
impl Geometry {
    fn new(chart: &ChartSpec) -> Self {
        let mut points = Vec::new();
        let mut names = Vec::new();
        let mut colors = Vec::new();
        let mut labels = Vec::new();
        let mut line = false;
        let (x_label, y_label) = match chart {
            ChartSpec::Line {
                series,
                x_label,
                y_label,
            }
            | ChartSpec::Scatter {
                series,
                x_label,
                y_label,
            } => {
                line = matches!(chart, ChartSpec::Line { .. });
                for (series_index, series) in series.iter().enumerate() {
                    names.push(series.name.clone());
                    colors.push(series.color.unwrap_or(series_index as u8));
                    for (index, point) in series.points.iter().enumerate() {
                        points.push(Datum {
                            x: point.x,
                            y: point.y,
                            series: series_index as u32,
                            index: index as u32,
                            bar: None,
                        });
                    }
                }
                (x_label, y_label)
            }
            ChartSpec::Bar {
                labels: categories,
                series,
                x_label,
                y_label,
            } => {
                labels = categories.clone();
                let width = 0.8 / series.len().max(1) as f64;
                for (series_index, series) in series.iter().enumerate() {
                    names.push(series.name.clone());
                    colors.push(series.color.unwrap_or(series_index as u8));
                    for (index, value) in series.values.iter().enumerate() {
                        let left = index as f64 - 0.4 + series_index as f64 * width;
                        points.push(Datum {
                            x: left + width / 2.,
                            y: *value,
                            series: series_index as u32,
                            index: index as u32,
                            bar: Some((left, left + width)),
                        });
                    }
                }
                (x_label, y_label)
            }
            ChartSpec::Histogram {
                values,
                bins,
                x_label,
                y_label,
            } => {
                names.push("Frequency".into());
                colors.push(0);
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
                for (index, count) in counts.into_iter().enumerate() {
                    let left = range.at(index as f64 / bins as f64);
                    let right = range.at((index + 1) as f64 / bins as f64);
                    points.push(Datum {
                        x: left + (right - left) / 2.,
                        y: f64::from(count),
                        series: 0,
                        index: index as u32,
                        bar: Some((left, right)),
                    });
                }
                (x_label, y_label)
            }
        };
        let xmin = points
            .iter()
            .map(|point| point.bar.map_or(point.x, |bar| bar.0))
            .fold(f64::INFINITY, f64::min);
        let xmax = points
            .iter()
            .map(|point| point.bar.map_or(point.x, |bar| bar.1))
            .fold(f64::NEG_INFINITY, f64::max);
        let mut ymin = points
            .iter()
            .map(|point| point.y)
            .fold(f64::INFINITY, f64::min);
        let mut ymax = points
            .iter()
            .map(|point| point.y)
            .fold(f64::NEG_INFINITY, f64::max);
        if points.iter().any(|point| point.bar.is_some()) {
            ymin = ymin.min(0.);
            ymax = ymax.max(0.);
        }
        Self {
            points,
            names,
            colors,
            labels,
            x: Range::new(xmin, xmax),
            y: Range::new(ymin, ymax),
            line,
            x_label: x_label.clone(),
            y_label: y_label.clone(),
        }
    }
    fn hit(&self, x: Range, position: (f64, f64), width: f64, height: f64) -> Option<(u32, u32)> {
        if !(0.0..=1.0).contains(&position.0) || !(0.0..=1.0).contains(&position.1) {
            return None;
        }
        let mut nearest = None;
        let mut best = 100.;
        for point in &self.points {
            if let Some((left, right)) = point.bar {
                let a = 1. - self.y.fraction(point.y);
                let zero = 1. - self.y.fraction(0.);
                if position.0 >= x.fraction(left)
                    && position.0 <= x.fraction(right)
                    && position.1 >= a.min(zero)
                    && position.1 <= a.max(zero)
                {
                    return Some((point.series, point.index));
                }
            } else {
                let fraction = x.fraction(point.x);
                if !(0.0..=1.0).contains(&fraction) {
                    continue;
                }
                let dx = (fraction - position.0) * width;
                let dy = (1. - self.y.fraction(point.y) - position.1) * height;
                let distance = dx * dx + dy * dy;
                if distance <= best {
                    best = distance;
                    nearest = Some((point.series, point.index));
                }
            }
        }
        nearest
    }
}

pub(super) struct ChartState {
    geometry: Arc<Geometry>,
    zoom: Range,
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    hover: Option<(u32, u32)>,
    selected: Option<(u32, u32)>,
}
impl ChartState {
    pub(super) fn new(spec: &ChartSpec) -> Self {
        let geometry = Arc::new(Geometry::new(spec));
        Self {
            zoom: geometry.x,
            geometry,
            bounds: Rc::new(Cell::new(None)),
            hover: None,
            selected: None,
        }
    }
    pub(super) fn replace(&mut self, spec: &ChartSpec) {
        let geometry = Arc::new(Geometry::new(spec));
        let full_before = self.zoom == self.geometry.x;
        self.zoom = if full_before {
            geometry.x
        } else {
            self.zoom.fit(geometry.x)
        };
        self.geometry = geometry;
        self.hover = None;
        if self.selected.is_some_and(|selection| {
            !self
                .geometry
                .points
                .iter()
                .any(|point| (point.series, point.index) == selection)
        }) {
            self.selected = None;
        }
    }
    fn hit(&self, position: Point<Pixels>) -> Option<(u32, u32)> {
        let bounds = self.bounds.get()?;
        let width = f64::from(f32::from(bounds.size.width));
        let height = f64::from(f32::from(bounds.size.height));
        if width <= 0. || height <= 0. {
            return None;
        }
        self.geometry.hit(
            self.zoom,
            (
                f64::from(f32::from(position.x - bounds.origin.x)) / width,
                f64::from(f32::from(position.y - bounds.origin.y)) / height,
            ),
            width,
            height,
        )
    }
}
pub(super) fn color(index: u8) -> Hsla {
    // A fixed twelve-color palette, adjusted to the current theme's saturation/lightness.
    let mut color = theme::accent();
    color.h = (color.h + index as f32 * 0.1618) % 1.;
    color
}
fn stroke(window: &mut Window, from: Point<Pixels>, to: Point<Pixels>, color: Hsla, width: f32) {
    let mut path = PathBuilder::stroke(px(width));
    path.move_to(from);
    path.line_to(to);
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}
fn draw(
    geometry: &Geometry,
    zoom: Range,
    bounds: Bounds<Pixels>,
    selected: Option<(u32, u32)>,
    hover: Option<(u32, u32)>,
    window: &mut Window,
) {
    if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
        return;
    }
    let at = |x: f64, y: f64| {
        point(
            bounds.origin.x + bounds.size.width * x as f32,
            bounds.origin.y + bounds.size.height * y as f32,
        )
    };
    for tick in 0..=4 {
        let fraction = f64::from(tick) / 4.;
        stroke(
            window,
            at(fraction, 0.),
            at(fraction, 1.),
            theme::border(),
            1.,
        );
        stroke(
            window,
            at(0., fraction),
            at(1., fraction),
            theme::border(),
            1.,
        );
    }
    stroke(window, at(0., 0.), at(0., 1.), theme::text(), 1.);
    stroke(window, at(0., 1.), at(1., 1.), theme::text(), 1.);
    let mut previous: Option<&Datum> = None;
    for datum in &geometry.points {
        let x = zoom.fraction(datum.x);
        let y = 1. - geometry.y.fraction(datum.y);
        let selected = selected == Some((datum.series, datum.index))
            || hover == Some((datum.series, datum.index));
        let color = color(geometry.colors[datum.series as usize]);
        if let Some((left, right)) = datum.bar {
            let left = zoom.fraction(left).max(0.);
            let right = zoom.fraction(right).min(1.);
            if right > left {
                let zero = 1. - geometry.y.fraction(0.);
                let from = at(left, y.min(zero));
                let to = at(right, y.max(zero));
                window.paint_quad(fill(
                    Bounds::new(
                        from,
                        size(
                            (to.x - from.x - px(1.)).max(px(1.)),
                            (to.y - from.y).max(px(1.)),
                        ),
                    ),
                    if selected { theme::text() } else { color },
                ));
            }
        } else {
            if geometry.line
                && let Some(before) = previous.filter(|before| before.series == datum.series)
            {
                // Clip in data space before converting to f32 pixels, including segments with both endpoints outside.
                if let Some((a, b)) = clip_line(
                    (zoom.fraction(before.x), 1. - geometry.y.fraction(before.y)),
                    (x, y),
                ) {
                    stroke(window, at(a.0, a.1), at(b.0, b.1), color, 1.5);
                }
            }
            if (0.0..=1.0).contains(&x) {
                let center = at(x, y);
                let radius = if selected { 4. } else { 2.5 };
                window.paint_quad(fill(
                    Bounds::new(
                        point(center.x - px(radius), center.y - px(radius)),
                        size(px(radius * 2.), px(radius * 2.)),
                    ),
                    if selected { theme::text() } else { color },
                ));
            }
        }
        previous = Some(datum);
    }
}
// Liang–Barsky clipping to the native plot's finite unit square.
fn clip_line(a: (f64, f64), b: (f64, f64)) -> Option<((f64, f64), (f64, f64))> {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let mut low: f64 = 0.;
    let mut high: f64 = 1.;
    for (p, q) in [(-dx, a.0), (dx, 1. - a.0), (-dy, a.1), (dy, 1. - a.1)] {
        if p == 0. {
            if q < 0. {
                return None;
            }
        } else {
            let t = q / p;
            if p < 0. {
                low = low.max(t);
            } else {
                high = high.min(t);
            }
            if low > high {
                return None;
            }
        }
    }
    Some((
        (a.0 + low * dx, a.1 + low * dy),
        (a.0 + high * dx, a.1 + high * dy),
    ))
}
impl ExtensionView {
    fn zoom_chart(&mut self, id: &str, factor: Option<f64>, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(chart) = self.charts.get_mut(id) else {
            return;
        };
        chart.zoom = factor.map_or(chart.geometry.x, |factor| {
            chart.zoom.zoom(chart.geometry.x, factor)
        });
        let zoom = chart.zoom;
        chart.hover = None;
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
    pub(super) fn render_chart(
        &self,
        id: &str,
        title: &str,
        height: u16,
        legend: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let Some(chart) = self.charts.get(id) else {
            return div().into_any_element();
        };
        let geometry = chart.geometry.clone();
        let zoom = chart.zoom;
        let bounds = chart.bounds.clone();
        let selected = chart.selected;
        let hover = chart.hover;
        let weak = cx.entity().downgrade();
        let hover_weak = weak.clone();
        let click_weak = weak.clone();
        let hover_id = id.to_owned();
        let click_id = id.to_owned();
        let leave_weak = weak.clone();
        let leave_id = id.to_owned();
        let plot = div()
            .id(SharedString::from(format!("{id}-plot")))
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .on_mouse_move(move |event, _, cx| {
                hover_weak
                    .update(cx, |view, cx| {
                        if let Some(chart) = view.charts.get_mut(&hover_id) {
                            let next = chart.hit(event.position);
                            if chart.hover != next {
                                chart.hover = next;
                                cx.notify();
                            }
                        }
                    })
                    .ok();
            })
            .on_hover(move |hovered, _, cx| {
                if !hovered {
                    leave_weak
                        .update(cx, |view, cx| {
                            if let Some(chart) = view.charts.get_mut(&leave_id)
                                && chart.hover.take().is_some()
                            {
                                cx.notify();
                            }
                        })
                        .ok();
                }
            })
            .on_mouse_down(MouseButton::Left, move |event, _, cx| {
                click_weak
                    .update(cx, |view, cx| {
                        if view.busy {
                            return;
                        }
                        let hit = view
                            .charts
                            .get(&click_id)
                            .and_then(|chart| chart.hit(event.position));
                        if let Some((series, index)) = hit {
                            if let Some(chart) = view.charts.get_mut(&click_id) {
                                chart.selected = hit;
                            }
                            view.emit(
                                UiEvent {
                                    widget: click_id.clone(),
                                    kind: UiEventKind::ChartSelect { series, index },
                                },
                                cx,
                            );
                            cx.notify();
                        }
                    })
                    .ok();
            })
            .child(
                canvas(
                    move |plot, _, _| {
                        bounds.set(Some(plot));
                    },
                    move |plot, _, window, _| draw(&geometry, zoom, plot, selected, hover, window),
                )
                .size_full(),
            )
            .pin("plot")
            .label("Chart plot");
        let controls = [("+", Some(0.5)), ("−", Some(2.)), ("Reset", None)]
            .into_iter()
            .map(|(label, factor)| {
                let weak = weak.clone();
                let id = id.to_owned();
                controls::button(
                    SharedString::from(format!("{id}-zoom-{label}")),
                    label,
                    false,
                    None,
                    self.busy,
                    Rc::new(move |_, cx| {
                        weak.update(cx, |view, cx| view.zoom_chart(&id, factor, cx))
                            .ok();
                    }),
                )
                .pin(match factor {
                    Some(value) if value < 1. => "zoom-in",
                    Some(_) => "zoom-out",
                    None => "reset",
                })
                .label(label)
            });
        let info = chart
            .hover
            .or(chart.selected)
            .and_then(|selected| {
                chart
                    .geometry
                    .points
                    .iter()
                    .find(|point| (point.series, point.index) == selected)
            })
            .map(|point| {
                let category = chart
                    .geometry
                    .labels
                    .get(point.index as usize)
                    .map(String::as_str);
                format!(
                    "{} · {} · x={} y={}",
                    chart
                        .geometry
                        .names
                        .get(point.series as usize)
                        .map_or("", String::as_str),
                    category.map_or_else(|| format!("#{}", point.index), str::to_owned),
                    number(point.x),
                    number(point.y)
                )
            });
        let control = div()
            .flex()
            .flex_col()
            .gap_1()
            .w_full()
            .min_w(px(180.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(SharedString::from(title.to_owned()))
                    .children(controls),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(SharedString::from(chart.geometry.y_label.clone())),
            )
            .child(
                div()
                    .flex()
                    .h(px(f32::from(height) - 76.))
                    .min_h(px(44.))
                    .child(
                        div()
                            .w(px(64.))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .justify_between()
                            .children((0..=4).map(|tick| {
                                div()
                                    .text_xs()
                                    .text_color(theme::muted())
                                    .child(number(chart.geometry.y.at(1. - f64::from(tick) / 4.)))
                            })),
                    )
                    .child(plot),
            )
            .child(
                div()
                    .ml(px(64.))
                    .flex()
                    .justify_between()
                    .children((0..=4).map(|tick| {
                        let x = zoom.at(f64::from(tick) / 4.);
                        let label = if chart.geometry.labels.is_empty() {
                            number(x)
                        } else {
                            chart
                                .geometry
                                .labels
                                .get(x.round().max(0.) as usize)
                                .cloned()
                                .unwrap_or_default()
                        };
                        div().text_xs().text_color(theme::muted()).child(label)
                    })),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(SharedString::from(chart.geometry.x_label.clone())),
            )
            .when(legend, |view| {
                view.child(
                    div().flex().flex_wrap().gap_2().children(
                        chart
                            .geometry
                            .names
                            .iter()
                            .enumerate()
                            .map(|(index, name)| {
                                div()
                                    .flex()
                                    .gap_1()
                                    .items_center()
                                    .child(
                                        div().size(px(8.)).bg(color(chart.geometry.colors[index])),
                                    )
                                    .child(SharedString::from(name.clone()))
                            }),
                    ),
                )
            })
            .when_some(info, |view, info| view.child(div().text_xs().child(info)));
        self.focusable_control(id, false, control.into_any_element())
    }
}
fn number(value: f64) -> String {
    if value.abs() >= 1e6 || (value != 0. && value.abs() < 0.001) {
        format!("{value:.2e}")
    } else {
        format!("{value:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use extension_api::{BarSeries, ChartPoint, ChartSeries};
    #[test]
    fn explicit_colors_survive_missing_series_and_keep_original_event_indices() {
        let series = |name: &str, color| ChartSeries {
            name: name.into(),
            color,
            points: vec![ChartPoint { x: 1., y: 2. }],
        };
        let spec = |series| ChartSpec::Line {
            series,
            x_label: String::new(),
            y_label: String::new(),
        };
        let full = Geometry::new(&spec(vec![
            series("A", Some(2)),
            series("B", Some(11)),
            series("C", None),
        ]));
        assert_eq!(full.colors, [2, 11, 2]);
        let missing = Geometry::new(&spec(vec![series("B", Some(11))]));
        assert_eq!(missing.colors, [11]);
        assert_eq!(missing.points[0].series, 0);
        assert_eq!(missing.hit(missing.x, (0.5, 0.5), 400., 200.), Some((0, 0)));
        let bars = Geometry::new(&ChartSpec::Bar {
            labels: vec!["x".into()],
            series: vec![BarSeries {
                name: "B".into(),
                color: Some(11),
                values: vec![2.],
            }],
            x_label: String::new(),
            y_label: String::new(),
        });
        assert_eq!(bars.colors, [11]);
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
    }
    #[test]
    fn zoomed_hit_keeps_original_point_indices() {
        let spec = ChartSpec::Scatter {
            series: vec![ChartSeries {
                color: None,
                name: "A".into(),
                points: vec![
                    ChartPoint { x: 0., y: 0. },
                    ChartPoint { x: 5., y: 5. },
                    ChartPoint { x: 10., y: 10. },
                ],
            }],
            x_label: String::new(),
            y_label: String::new(),
        };
        let geometry = Geometry::new(&spec);
        assert_eq!(
            geometry.hit(Range::new(4., 6.), (0.5, 0.5), 400., 200.),
            Some((0, 1))
        );
        assert_eq!(geometry.hit(Range::new(4., 6.), (0., 0.), 400., 200.), None);
    }
    #[test]
    fn grouped_negative_bars_and_histogram_bins_hit_original_indices() {
        let geometry = Geometry::new(&ChartSpec::Bar {
            labels: vec!["A".into(), "B".into()],
            series: vec![
                BarSeries {
                    color: None,
                    name: "a".into(),
                    values: vec![-2., 4.],
                },
                BarSeries {
                    color: None,
                    name: "b".into(),
                    values: vec![3., -1.],
                },
            ],
            x_label: String::new(),
            y_label: String::new(),
        });
        let point = geometry
            .points
            .iter()
            .find(|point| point.series == 1 && point.index == 1)
            .unwrap();
        assert_eq!(
            geometry.hit(
                geometry.x,
                (geometry.x.fraction(point.x), 1. - geometry.y.fraction(-0.5)),
                400.,
                200.
            ),
            Some((1, 1))
        );
        let hist = Geometry::new(&ChartSpec::Histogram {
            values: vec![0., 0., 1., 2.],
            bins: 2,
            x_label: String::new(),
            y_label: String::new(),
        });
        assert_eq!(hist.points[0].y, 2.);
        assert_eq!(hist.points[1].y, 2.);
        assert_eq!(hist.hit(hist.x, (0.75, 0.5), 400., 200.), Some((0, 1)));
    }
    #[test]
    fn line_clipping_handles_both_endpoints_outside() {
        assert_eq!(
            clip_line((-1., 0.5), (2., 0.5)),
            Some(((0., 0.5), (1., 0.5)))
        );
        assert_eq!(clip_line((-1., -1.), (-0.5, -0.5)), None);
    }
    #[test]
    fn preserved_zoom_stays_selectable_when_data_moves_to_large_magnitudes() {
        let spec = |a, b| ChartSpec::Scatter {
            series: vec![ChartSeries {
                color: None,
                name: "Signal".into(),
                points: vec![ChartPoint { x: a, y: 0. }, ChartPoint { x: b, y: 1. }],
            }],
            x_label: String::new(),
            y_label: String::new(),
        };
        let mut chart = ChartState::new(&spec(0., 1e-8));
        chart.zoom = chart.zoom.zoom(chart.geometry.x, 0.5);
        chart.replace(&spec(9e11, 1e12));
        assert_eq!(
            chart.zoom, chart.geometry.x,
            "an unrepresentable old zoom must reset to the new full range"
        );
        assert_eq!(
            chart.geometry.hit(chart.zoom, (0., 1.), 400., 200.),
            Some((0, 0))
        );
    }
}
