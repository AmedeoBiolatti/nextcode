use anyhow::{Result, bail, ensure};
use extension_api::*;
use std::collections::{BTreeMap, HashSet};

pub(crate) fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "invalid widget/panel id"
    );
    Ok(())
}
pub(crate) fn validate_state(state: &serde_json::Value) -> Result<()> {
    validate_encoded_size(state, 16 * 1024)
}
pub(crate) fn validate_encoded_size(value: &impl serde::Serialize, maximum: usize) -> Result<()> {
    struct Counter {
        remaining: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.remaining = self
                .remaining
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("encoded JSON exceeds limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter { remaining: maximum }, value)?;
    Ok(())
}
struct Bounds<'a> {
    ids: HashSet<&'a str>,
    text: usize,
    points: usize,
}
impl<'a> Bounds<'a> {
    fn text(&mut self, text: &str) -> Result<()> {
        self.text += text.len();
        ensure!(self.text <= 65536, "panel text exceeds 64 KiB");
        Ok(())
    }
    fn numbers(&mut self, values: impl Iterator<Item = f64>) -> Result<()> {
        for value in values {
            ensure!(
                value.is_finite() && value.abs() <= 1e12,
                "invalid chart number"
            );
            self.points += 1;
        }
        ensure!(self.points <= 4096, "panel exceeds 4096 chart values");
        Ok(())
    }
    fn chart(&mut self, chart: &ChartSpec) -> Result<()> {
        match chart {
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
                self.text(x_label)?;
                self.text(y_label)?;
                ensure!(
                    !series.is_empty() && series.len() <= 6,
                    "chart requires 1..6 series"
                );
                for series in series {
                    self.text(&series.name)?;
                    ensure!(
                        series.color.is_none_or(|color| color < 12),
                        "invalid chart color: expected 0..11"
                    );
                    ensure!(!series.points.is_empty(), "empty chart series");
                    // Each point is one datum; both coordinates are validated.
                    for point in &series.points {
                        ensure!(
                            point.x.is_finite() && point.x.abs() <= 1e12,
                            "invalid chart x"
                        );
                    }
                    self.numbers(series.points.iter().map(|p| p.y))?;
                }
            }
            ChartSpec::Bar {
                labels,
                series,
                x_label,
                y_label,
            } => {
                self.text(x_label)?;
                self.text(y_label)?;
                ensure!(
                    !labels.is_empty() && !series.is_empty() && series.len() <= 6,
                    "invalid bar shape"
                );
                for label in labels {
                    ensure!(!label.is_empty(), "empty bar label");
                    self.text(label)?;
                }
                for series in series {
                    self.text(&series.name)?;
                    ensure!(
                        series.color.is_none_or(|color| color < 12),
                        "invalid chart color: expected 0..11"
                    );
                    ensure!(series.values.len() == labels.len(), "bar width mismatch");
                    self.numbers(series.values.iter().copied())?;
                }
            }
            ChartSpec::Histogram {
                values,
                bins,
                x_label,
                y_label,
            } => {
                self.text(x_label)?;
                self.text(y_label)?;
                ensure!(
                    !values.is_empty() && (1..=64).contains(bins),
                    "invalid histogram"
                );
                self.numbers(values.iter().copied())?;
            }
        }
        Ok(())
    }
    fn widget(&mut self, widget: &'a Widget, depth: usize) -> Result<()> {
        ensure!(depth <= 8, "panel exceeds depth 8");
        validate_id(widget.id())?;
        ensure!(self.ids.insert(widget.id()), "duplicate widget id");
        ensure!(self.ids.len() <= 128, "panel exceeds 128 widgets");
        self.text(widget.id())?;
        match widget {
            Widget::Row { children, gap, .. } | Widget::Column { children, gap, .. } => {
                ensure!(*gap <= 48, "invalid gap");
                for child in children {
                    self.widget(child, depth + 1)?;
                }
            }
            Widget::Scroll { child, height, .. } => {
                ensure!((32..=600).contains(height), "invalid scroll height");
                self.widget(child, depth + 1)?;
            }
            Widget::Space { size, .. } => ensure!(*size <= 48, "invalid space"),
            Widget::Label { text, .. }
            | Widget::Markdown { text, .. }
            | Widget::Button { text, .. } => self.text(text)?,
            Widget::TextInput {
                label,
                value,
                placeholder,
                ..
            } => {
                ensure!(value.len() <= 4096, "text input exceeds 4 KiB");
                self.text(label)?;
                self.text(value)?;
                self.text(placeholder)?;
            }
            Widget::Checkbox { label, color, .. } => {
                self.text(label)?;
                ensure!(
                    color.is_none_or(|color| color < 12),
                    "invalid checkbox color: expected 0..11"
                );
            }
            Widget::Select {
                label,
                value,
                options,
                ..
            } => {
                self.text(label)?;
                self.text(value)?;
                ensure!(
                    !options.is_empty() && options.len() <= 32,
                    "invalid select options"
                );
                let mut values = HashSet::new();
                for option in options {
                    ensure!(
                        !option.value.is_empty() && values.insert(&option.value),
                        "invalid select value"
                    );
                    self.text(&option.value)?;
                    self.text(&option.label)?;
                }
                ensure!(values.contains(value), "selected value missing");
            }
            Widget::Progress { label, value, .. } => {
                self.text(label)?;
                ensure!(
                    value.is_finite() && (0.0..=1.0).contains(value),
                    "invalid progress"
                );
            }
            Widget::Table { columns, rows, .. } => {
                ensure!(
                    !columns.is_empty()
                        && columns.len() <= 16
                        && rows.len().saturating_mul(columns.len()) <= 1024,
                    "invalid table shape"
                );
                for column in columns {
                    self.text(column)?;
                }
                for row in rows {
                    ensure!(row.len() == columns.len(), "table row width mismatch");
                    for cell in row {
                        self.text(cell)?;
                    }
                }
            }
            Widget::Chart {
                title,
                chart,
                height,
                width,
                ..
            } => {
                self.text(title)?;
                ensure!((120..=500).contains(height), "invalid chart height");
                ensure!(
                    width.is_none_or(|width| (180..=1600).contains(&width)),
                    "invalid chart width: expected 180..1600"
                );
                self.chart(chart)?;
            }
        }
        Ok(())
    }
}
pub fn validate_panel_update(panel: &PanelUpdate) -> Result<()> {
    ensure!(
        panel
            .refresh_ms
            .is_none_or(|ms| (1000..=86_400_000).contains(&ms)),
        "refresh_ms must be between 1000 and 86400000"
    );
    validate_id(&panel.id)?;
    validate_state(&panel.state)?;
    Bounds {
        ids: HashSet::new(),
        text: 0,
        points: 0,
    }
    .widget(&panel.root, 1)
}
fn collect<'a>(widget: &'a Widget, widgets: &mut Vec<&'a Widget>) {
    widgets.push(widget);
    match widget {
        Widget::Row { children, .. } | Widget::Column { children, .. } => {
            for child in children {
                collect(child, widgets);
            }
        }
        Widget::Scroll { child, .. } => collect(child, widgets),
        _ => (),
    }
}
pub fn validate_panel_event(
    panel: &PanelUpdate,
    event: &UiEvent,
    inputs: &BTreeMap<String, InputValue>,
) -> Result<()> {
    validate_panel_update(panel)?;
    let mut widgets = Vec::new();
    collect(&panel.root, &mut widgets);
    let mut controls = 0;
    for widget in &widgets {
        match widget {
            Widget::TextInput { id, .. } => {
                controls += 1;
                ensure!(
                    matches!(inputs.get(id), Some(InputValue::Text(text)) if text.len() <= 4096),
                    "missing or invalid text input"
                );
            }
            Widget::Checkbox { id, .. } => {
                controls += 1;
                ensure!(
                    matches!(inputs.get(id), Some(InputValue::Bool(_))),
                    "missing or invalid checkbox input"
                );
            }
            Widget::Select { id, options, .. } => {
                controls += 1;
                ensure!(
                    matches!(inputs.get(id), Some(InputValue::Text(text)) if options.iter().any(|o| &o.value==text)),
                    "missing or invalid select input"
                );
            }
            _ => (),
        }
    }
    ensure!(controls == inputs.len(), "unexpected input ids");
    let widget = widgets
        .into_iter()
        .find(|w| w.id() == event.widget)
        .ok_or_else(|| anyhow::anyhow!("event widget does not exist"))?;
    match (widget, &event.kind) {
        (
            Widget::Button {
                disabled: false, ..
            },
            UiEventKind::Click,
        )
        | (
            Widget::TextInput {
                disabled: false, ..
            },
            UiEventKind::Submit,
        ) => (),
        (
            Widget::Checkbox {
                id,
                disabled: false,
                ..
            },
            UiEventKind::Change {
                value: InputValue::Bool(value),
            },
        ) => ensure!(
            inputs.get(id) == Some(&InputValue::Bool(*value)),
            "change value mismatches input"
        ),
        (
            Widget::Select {
                id,
                disabled: false,
                ..
            },
            UiEventKind::Change {
                value: InputValue::Text(value),
            },
        ) => ensure!(
            matches!(inputs.get(id), Some(InputValue::Text(input)) if input == value),
            "change value mismatches input"
        ),
        (Widget::Chart { chart, .. }, UiEventKind::ChartSelect { series, index }) => {
            let (series, index) = (*series as usize, *index as usize);
            let valid = match chart {
                ChartSpec::Line { series: data, .. } | ChartSpec::Scatter { series: data, .. } => {
                    data.get(series).is_some_and(|s| index < s.points.len())
                }
                ChartSpec::Bar { series: data, .. } => {
                    data.get(series).is_some_and(|s| index < s.values.len())
                }
                ChartSpec::Histogram { bins, .. } => series == 0 && index < *bins as usize,
            };
            ensure!(valid, "chart selection out of range");
        }
        (Widget::Chart { .. }, UiEventKind::ChartZoom { start, end }) => ensure!(
            start.is_finite()
                && end.is_finite()
                && start < end
                && start.abs() <= 1e12
                && end.abs() <= 1e12,
            "invalid chart zoom"
        ),
        _ => bail!("event does not match enabled widget type"),
    }
    Ok(())
}
