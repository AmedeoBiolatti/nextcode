use extension_sdk::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

export_extension!(handle);

fn text(inputs: &BTreeMap<String, InputValue>, id: &str, fallback: &str) -> String {
    match inputs.get(id) {
        Some(InputValue::Text(value)) => value.clone(),
        _ => fallback.into(),
    }
}
fn label(id: &str, text: String) -> Widget {
    Widget::Label {
        id: id.into(),
        text,
    }
}
fn handle(request: Request) -> Response {
    let (id, previous, inputs, event) = match request.invocation {
        Invocation::OpenPanel { id } => (id, Value::Null, BTreeMap::new(), None),
        Invocation::UiEvent {
            panel,
            state,
            inputs,
            event,
            ..
        } => (panel, state, inputs, Some(event)),
        _ => {
            return Response {
                error: Some("This extension contributes panels, not commands".into()),
                ..Response::default()
            };
        }
    };
    if id != "dashboard" && id != "report" {
        return Response {
            error: Some("Unknown panel".into()),
            ..Response::default()
        };
    }
    let count_text = text(
        &inputs,
        "samples",
        previous["samples"].as_str().unwrap_or("48"),
    );
    let count = match count_text.parse::<usize>() {
        Ok(count) if (8..=256).contains(&count) => count,
        _ => {
            return Response {
                error: Some("Samples must be an integer from 8 to 256".into()),
                ..Response::default()
            };
        }
    };
    let scale_text = text(&inputs, "scale", previous["scale"].as_str().unwrap_or("1"));
    let scale = match scale_text.as_str() {
        "2" => 2.0,
        "4" => 4.0,
        _ => 1.0,
    };
    let smooth = match inputs.get("smooth") {
        Some(InputValue::Bool(value)) => *value,
        _ => previous["smooth"].as_bool().unwrap_or(false),
    };
    let interactions = previous["interactions"].as_u64().unwrap_or(0) + u64::from(event.is_some());
    let mut status = previous["status"]
        .as_str()
        .unwrap_or("No chart selection or zoom yet")
        .to_owned();
    if let Some(event) = event {
        status = match event.kind {
            UiEventKind::ChartSelect { series, index } => format!(
                "{}: selected series {}, original datum/bin {}",
                event.widget, series, index
            ),
            UiEventKind::ChartZoom { start, end } => {
                format!("{}: zoom [{start:.3}, {end:.3}]", event.widget)
            }
            UiEventKind::Submit | UiEventKind::Click => {
                format!("Recomputed {count} generated samples at scale {scale}")
            }
            UiEventKind::Change { .. } => {
                format!("Recomputed with smoothing {smooth}, scale {scale}")
            }
        };
    }
    // Pure deterministic synthesis: no clock, randomness, network or hidden guest state.
    let values: Vec<f64> = (0..count)
        .map(|i| {
            let x = i as f64 / 8.0;
            scale * (x.sin() + if smooth { 0.0 } else { (x * 3.0).cos() * 0.25 })
        })
        .collect();
    let points: Vec<ChartPoint> = values
        .iter()
        .enumerate()
        .map(|(i, y)| ChartPoint { x: i as f64, y: *y })
        .collect();
    let series = vec![ChartSeries {
        color: Some(0),
        name: "Generated signal".into(),
        points,
    }];
    let mean = values.iter().sum::<f64>() / count as f64;
    let minimum = values.iter().copied().fold(f64::INFINITY, f64::min);
    let maximum = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let charts = vec![
        (
            "line",
            "Generated signal",
            ChartSpec::Line {
                series: series.clone(),
                x_label: "Sample".into(),
                y_label: "Amplitude".into(),
            },
        ),
        (
            "scatter",
            "Generated samples",
            ChartSpec::Scatter {
                series,
                x_label: "Sample".into(),
                y_label: "Amplitude".into(),
            },
        ),
        (
            "bar",
            "Computed summary",
            ChartSpec::Bar {
                labels: vec!["Minimum".into(), "Mean".into(), "Maximum".into()],
                series: vec![BarSeries {
                    color: Some(0),
                    name: "Signal".into(),
                    values: vec![minimum, mean, maximum],
                }],
                x_label: "Statistic".into(),
                y_label: "Amplitude".into(),
            },
        ),
        (
            "histogram",
            "Generated amplitude distribution",
            ChartSpec::Histogram {
                values,
                bins: 12,
                x_label: "Amplitude".into(),
                y_label: "Count".into(),
            },
        ),
    ];
    let mut children = vec![
        Widget::Markdown {
            id: "intro".into(),
            text: format!(
                "# {}\nDeterministically generated synthetic data — not a live feed. Each panel retains its own state.",
                if id == "report" {
                    "Generated Report"
                } else {
                    "Generated Dashboard"
                }
            ),
        },
        Widget::Row {
            id: "form".into(),
            gap: 8,
            children: vec![
                Widget::TextInput {
                    label_inline: true,
                    id: "samples".into(),
                    label: "Samples (8–256)".into(),
                    value: count_text.clone(),
                    placeholder: "48".into(),
                    disabled: false,
                },
                Widget::Checkbox {
                    color: Some(0),
                    id: "smooth".into(),
                    label: "Smooth signal".into(),
                    value: smooth,
                    disabled: false,
                },
                Widget::Select {
                    label_inline: true,
                    id: "scale".into(),
                    label: "Scale".into(),
                    value: scale_text.clone(),
                    options: ["1", "2", "4"]
                        .into_iter()
                        .map(|value| SelectOption {
                            value: value.into(),
                            label: format!("{value}×"),
                        })
                        .collect(),
                    disabled: false,
                },
                Widget::Button {
                    id: "generate".into(),
                    text: "Recompute".into(),
                    disabled: false,
                },
            ],
        },
        label("interactions", format!("Interactions: {interactions}")),
        label("status", status.clone()),
        Widget::Progress {
            id: "capacity".into(),
            label: format!("{count}/256 samples"),
            value: count as f64 / 256.0,
        },
        Widget::Table {
            id: "summary".into(),
            columns: vec!["Statistic".into(), "Computed value".into()],
            rows: vec![
                vec!["Minimum".into(), format!("{minimum:.4}")],
                vec!["Mean".into(), format!("{mean:.4}")],
                vec!["Maximum".into(), format!("{maximum:.4}")],
            ],
        },
        Widget::Space {
            id: "chart-spacing".into(),
            size: 8,
        },
    ];
    children.extend(charts.into_iter().map(|(id, title, chart)| Widget::Chart {
        legend: false,
        id: id.into(),
        title: title.into(),
        chart,
        height: 240,
    }));
    Response {
        panels: vec![PanelUpdate {
            refresh_ms: None,
            id,
            root: Widget::Scroll {
                id: "scroll".into(),
                height: 560,
                child: Box::new(Widget::Column {
                    id: "root".into(),
                    gap: 12,
                    children,
                }),
            },
            state: json!({"samples":count_text,"scale":scale_text,"smooth":smooth,"interactions":interactions,"status":status}),
        }],
        ..Response::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_sample_counts_report_an_error_without_replacing_the_panel() {
        for value in ["abc", "7", "257"] {
            let response = handle(Request {
                api_version: API_VERSION,
                invocation: Invocation::UiEvent {
                    panel: "dashboard".into(),
                    revision: 1,
                    state: Value::Null,
                    inputs: BTreeMap::from([("samples".into(), InputValue::Text(value.into()))]),
                    event: UiEvent {
                        widget: "generate".into(),
                        kind: UiEventKind::Click,
                    },
                },
                document: None,
                settings: json!({}),
            });
            assert!(
                response.error.is_some(),
                "invalid count {value} must not be silently substituted"
            );
            assert!(response.panels.is_empty());
        }
    }
}
