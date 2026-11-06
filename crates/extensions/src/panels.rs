use anyhow::{Result, bail, ensure};
use extension_api::{limits, *};
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
    validate_encoded_size(state, limits::STATE_BYTES)
        .map_err(|_| anyhow::anyhow!("panel state exceeds 64 KiB"))
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
fn color(color: Option<u8>, what: &str) -> Result<()> {
    ensure!(
        color.is_none_or(|color| color < limits::PALETTE),
        "invalid {what} color: expected 0..11"
    );
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
        ensure!(
            self.text <= limits::TEXT_BYTES,
            "panel text exceeds 256 KiB"
        );
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
        ensure!(
            self.points <= limits::CHART_VALUES,
            "panel exceeds 16384 chart values"
        );
        Ok(())
    }
    fn options(&mut self, value: &str, options: &[SelectOption]) -> Result<()> {
        self.text(value)?;
        ensure!(
            !options.is_empty() && options.len() <= limits::OPTIONS,
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
        ensure!(values.contains(&value.to_owned()), "selected value missing");
        Ok(())
    }
    fn chart(&mut self, chart: &ChartSpec) -> Result<()> {
        match chart {
            ChartSpec::Line {
                series,
                x_label,
                y_label,
                x_scale,
                y_scale,
                ..
            }
            | ChartSpec::Scatter {
                series,
                x_label,
                y_label,
                x_scale,
                y_scale,
                ..
            } => {
                self.text(x_label)?;
                self.text(y_label)?;
                ensure!(
                    !series.is_empty() && series.len() <= limits::SERIES,
                    "chart requires 1..12 series"
                );
                for series in series {
                    self.text(&series.name)?;
                    color(series.color, "chart")?;
                    ensure!(!series.points.is_empty(), "empty chart series");
                    // Each point is one datum; both coordinates are validated.
                    for point in &series.points {
                        ensure!(
                            point.x.is_finite() && point.x.abs() <= 1e12,
                            "invalid chart x"
                        );
                        ensure!(
                            *x_scale == Scale::Linear || point.x > 0.,
                            "a log x axis takes positive values only"
                        );
                        ensure!(
                            *y_scale == Scale::Linear || point.y > 0.,
                            "a log y axis takes positive values only"
                        );
                        if let Some(label) = &point.label {
                            self.text(label)?;
                        }
                    }
                    self.numbers(series.points.iter().map(|p| p.y))?;
                }
            }
            ChartSpec::Bar {
                labels,
                series,
                x_label,
                y_label,
                ..
            } => {
                self.text(x_label)?;
                self.text(y_label)?;
                ensure!(
                    !labels.is_empty() && !series.is_empty() && series.len() <= limits::SERIES,
                    "invalid bar shape"
                );
                for label in labels {
                    ensure!(!label.is_empty(), "empty bar label");
                    self.text(label)?;
                }
                for series in series {
                    self.text(&series.name)?;
                    color(series.color, "chart")?;
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
        ensure!(depth <= limits::DEPTH, "panel exceeds depth 12");
        validate_id(widget.id())?;
        ensure!(self.ids.insert(widget.id()), "duplicate widget id");
        ensure!(
            self.ids.len() <= limits::WIDGETS,
            "panel exceeds 256 widgets"
        );
        self.text(widget.id())?;
        match widget {
            Widget::Row { gap, .. } => ensure!(*gap <= 48, "invalid gap"),
            Widget::Column { gap, width, .. } => {
                ensure!(*gap <= 48, "invalid gap");
                ensure!(
                    width.is_none_or(|width| (120..=1600).contains(&width)),
                    "invalid column width: expected 120..1600"
                );
            }
            Widget::Grid {
                gap,
                min_width,
                columns,
                ..
            } => {
                ensure!(*gap <= 48, "invalid gap");
                ensure!(
                    columns.is_none_or(|columns| (1..=12).contains(&columns)),
                    "invalid grid columns: expected 1..12"
                );
                ensure!(
                    (120..=1600).contains(min_width),
                    "invalid grid min_width: expected 120..1600"
                );
            }
            Widget::Section { title, .. } => self.text(title)?,
            Widget::Scroll { height, .. } => {
                ensure!((32..=1200).contains(height), "invalid scroll height");
            }
            Widget::Space { size, .. } => ensure!(*size <= 48, "invalid space"),
            Widget::Label { text, .. }
            | Widget::Markdown { text, .. }
            | Widget::Button { text, .. } => self.text(text)?,
            Widget::Stat {
                label,
                value,
                detail,
                color: swatch,
                ..
            } => {
                self.text(label)?;
                self.text(value)?;
                self.text(detail)?;
                color(*swatch, "stat")?;
            }
            Widget::TextInput {
                label,
                value,
                placeholder,
                ..
            } => {
                ensure!(
                    value.len() <= limits::TEXT_INPUT_BYTES,
                    "text input exceeds 4 KiB"
                );
                self.text(label)?;
                self.text(value)?;
                self.text(placeholder)?;
            }
            Widget::Checkbox {
                label,
                color: swatch,
                ..
            } => {
                self.text(label)?;
                color(*swatch, "checkbox")?;
            }
            Widget::Select {
                label,
                value,
                options,
                ..
            } => {
                self.text(label)?;
                self.options(value, options)?;
            }
            Widget::Tabs { value, options, .. } => self.options(value, options)?,
            Widget::Progress { label, value, .. } => {
                self.text(label)?;
                ensure!(
                    value.is_finite() && (0.0..=1.0).contains(value),
                    "invalid progress"
                );
            }
            Widget::Table {
                columns,
                rows,
                widths,
                selectable,
                selected,
                max_height,
                check,
                checked,
                disabled,
                swatches,
                ..
            } => {
                ensure!(
                    !columns.is_empty()
                        && columns.len() <= limits::TABLE_COLUMNS
                        && rows.len().saturating_mul(columns.len()) <= limits::TABLE_CELLS,
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
                ensure!(
                    widths.is_empty()
                        || (widths.len() == columns.len()
                            && widths.iter().all(|w| *w == 0 || (16..=1200).contains(w))),
                    "table widths: one a column, 0 or 16..1200 px"
                );
                ensure!(
                    selected.is_none_or(|row| *selectable && (row as usize) < rows.len()),
                    "invalid selected row"
                );
                ensure!(
                    max_height.is_none_or(|h| (120..=1200).contains(&h)),
                    "invalid table max_height: expected 120..1200"
                );
                ensure!(
                    *check || (checked.is_empty() && disabled.is_empty() && swatches.is_empty()),
                    "checked, disabled and swatches require check"
                );
                for list in [checked, disabled] {
                    let mut seen = HashSet::new();
                    ensure!(
                        list.iter()
                            .all(|row| (*row as usize) < rows.len() && seen.insert(row)),
                        "invalid table row index"
                    );
                }
                ensure!(
                    swatches.is_empty() || swatches.len() == rows.len(),
                    "table swatches: one a row"
                );
                for swatch in swatches {
                    color(*swatch, "table")?;
                }
            }
            Widget::Chart {
                title,
                chart,
                height,
                width,
                controls,
                sync,
                ..
            } => {
                self.text(title)?;
                ensure!((120..=600).contains(height), "invalid chart height");
                ensure!(
                    width.is_none_or(|width| (180..=1600).contains(&width)),
                    "invalid chart width: expected 180..1600"
                );
                ensure!(
                    controls.len() <= limits::CHART_CONTROLS
                        && controls.iter().all(|control| matches!(
                            control,
                            Widget::Select { .. } | Widget::Button { .. } | Widget::Checkbox { .. }
                        )),
                    "chart controls: up to 4 selects, buttons or checkboxes"
                );
                if let Some(sync) = sync {
                    validate_id(sync)?;
                    self.text(sync)?;
                }
                self.chart(chart)?;
            }
        }
        for child in widget.children() {
            self.widget(child, depth + 1)?;
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
    for child in widget.children() {
        collect(child, widgets);
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
                    matches!(inputs.get(id), Some(InputValue::Text(text)) if text.len() <= limits::TEXT_INPUT_BYTES),
                    "missing or invalid text input"
                );
            }
            Widget::Checkbox { id, .. } | Widget::Section { id, .. } => {
                controls += 1;
                ensure!(
                    matches!(inputs.get(id), Some(InputValue::Bool(_))),
                    "missing or invalid checkbox input"
                );
            }
            Widget::Select { id, options, .. } | Widget::Tabs { id, options, .. } => {
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
            }
            | Widget::Section { id, .. },
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
            }
            | Widget::Tabs {
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
        (
            Widget::Table {
                rows,
                selectable: true,
                ..
            },
            UiEventKind::SelectRow { index },
        ) => ensure!((*index as usize) < rows.len(), "table row out of range"),
        (
            Widget::Table {
                rows,
                check: true,
                disabled,
                ..
            },
            UiEventKind::CheckRow { index, .. },
        ) => ensure!(
            (*index as usize) < rows.len() && !disabled.contains(index),
            "table row out of range or disabled"
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
