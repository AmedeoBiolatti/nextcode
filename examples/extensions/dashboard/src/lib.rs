//! A generated-data dashboard using every native widget of extension API 4: tabs, a sidebar beside a growing
//! column, a grid of stat tiles and charts, a checkable table that doubles as the charts' legend, synced line
//! charts in several styles, a log axis picked from a chart's title row, and a section that folds away.
//! The data is synthetic and deterministic: no clock, randomness, network or hidden guest state.
use extension_sdk::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

export_extension!(handle);

const CHANNELS: [(&str, &str); 4] = [
    ("sine", "Sine"),
    ("square", "Square-ish"),
    ("ramp", "Ramp"),
    ("decay", "Decay"),
];

fn text(
    inputs: &BTreeMap<String, InputValue>,
    previous: &Value,
    id: &str,
    fallback: &str,
) -> String {
    match inputs.get(id) {
        Some(InputValue::Text(value)) => value.clone(),
        _ => previous[id].as_str().unwrap_or(fallback).into(),
    }
}
fn flag(inputs: &BTreeMap<String, InputValue>, previous: &Value, id: &str, fallback: bool) -> bool {
    match inputs.get(id) {
        Some(InputValue::Bool(value)) => *value,
        _ => previous[id].as_bool().unwrap_or(fallback),
    }
}
fn error(message: &str) -> Response {
    Response {
        error: Some(message.into()),
        ..Response::default()
    }
}

fn signal(channel: usize, i: usize, scale: f64, smooth: bool) -> f64 {
    let x = i as f64 / 8.0;
    let wobble = if smooth { 0.0 } else { (x * 3.0).cos() * 0.15 };
    scale
        * match channel {
            0 => x.sin() + wobble,
            1 => (x.sin() * 3.0).tanh() + wobble,
            2 => (x % 4.0) / 2.0 - 1.0 + wobble,
            _ => (-(x / 6.0)).exp() * 2.0 + 0.01 + wobble.abs(),
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
        _ => return error("This extension contributes panels, not commands"),
    };
    if id != "dashboard" && id != "report" {
        return error("Unknown panel");
    }
    let count_text = text(&inputs, &previous, "samples", "96");
    let count = match count_text.parse::<usize>() {
        Ok(count) if (8..=256).contains(&count) => count,
        _ => return error("Samples must be an integer from 8 to 256"),
    };
    let scale_text = text(&inputs, &previous, "scale", "1");
    let scale = match scale_text.as_str() {
        "2" => 2.0,
        "4" => 4.0,
        _ => 1.0,
    };
    let smooth = flag(&inputs, &previous, "smooth", false);
    let page = text(&inputs, &previous, "page", "signals");
    let axis = text(&inputs, &previous, "decay-axis", "log");
    // The channels shown: the table's ticks, kept in the state.
    let mut shown: Vec<bool> = (0..CHANNELS.len())
        .map(|i| previous["shown"][i].as_bool().unwrap_or(i < 3))
        .collect();
    let mut picked = previous["picked"].as_u64().unwrap_or(0) as usize;
    let mut status = previous["status"]
        .as_str()
        .unwrap_or(
            "Tick channels to chart them; drag across a chart to zoom, double-click to reset.",
        )
        .to_owned();
    if let Some(event) = &event {
        match &event.kind {
            UiEventKind::CheckRow { index, value } => {
                if let Some(slot) = shown.get_mut(*index as usize) {
                    *slot = *value;
                }
            }
            UiEventKind::SelectRow { index } => picked = (*index as usize).min(CHANNELS.len() - 1),
            UiEventKind::ChartSelect { series, index } => {
                status = format!("{}: series {series}, sample {index}", event.widget)
            }
            UiEventKind::ChartZoom { start, end } => {
                status = format!("{}: samples {start:.1} to {end:.1}", event.widget)
            }
            _ => {}
        }
    }
    let data: Vec<Vec<f64>> = (0..CHANNELS.len())
        .map(|c| (0..count).map(|i| signal(c, i, scale, smooth)).collect())
        .collect();
    let stats = |values: &[f64]| {
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let min = values.iter().copied().fold(f64::INFINITY, f64::min);
        let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        (min, mean, max)
    };
    let series = |c: usize| {
        ChartSeries::new(
            CHANNELS[c].1,
            data[c].iter().enumerate().map(|(i, y)| {
                ChartPoint::labelled(i as f64, *y, format!("{} #{i}", CHANNELS[c].0))
            }),
        )
        .color(c as u8)
    };
    let ticked: Vec<usize> = (0..CHANNELS.len()).filter(|c| shown[*c]).collect();

    let sidebar = Widget::column(
        "controls",
        [
            Widget::text_input("samples", "Samples", &count_text)
                .label_inline()
                .placeholder("8–256"),
            Widget::select(
                "scale",
                "Scale",
                &scale_text,
                ["1", "2", "4"].map(|v| SelectOption::new(v, format!("{v}×"))),
            )
            .label_inline(),
            Widget::checkbox("smooth", "Smooth", smooth),
            Widget::button("generate", "Recompute"),
            Widget::table(
                "channels",
                vec!["Channel".into(), "Mean".into()],
                CHANNELS
                    .iter()
                    .enumerate()
                    .map(|(c, (_, name))| vec![(*name).into(), format!("{:.3}", stats(&data[c]).1)])
                    .collect(),
            )
            .widths([0, 72])
            .selectable(Some(picked as u32))
            .checks(
                ticked.iter().map(|c| *c as u32),
                [],
                (0..CHANNELS.len()).map(|c| Some(c as u8)),
            ),
            Widget::progress(
                "capacity",
                format!("{count} of 256 samples"),
                count as f64 / 256.0,
            ),
        ],
    )
    .gap(10)
    .width(260);

    let (min, mean, max) = stats(&data[picked]);
    let tiles = Widget::grid(
        "stats",
        160,
        [
            Widget::stat("stat-min", "Minimum", format!("{min:.3}")).detail(CHANNELS[picked].1),
            Widget::stat("stat-mean", "Mean", format!("{mean:.3}")).color(picked as u8),
            Widget::stat("stat-max", "Maximum", format!("{max:.3}")),
            Widget::stat("stat-count", "Samples", count.to_string())
                .detail(format!("scale {scale}×")),
        ],
    );
    let styles = [
        SeriesStyle::Line,
        SeriesStyle::Step,
        SeriesStyle::Dashed,
        SeriesStyle::Area,
    ];
    let main = match page.as_str() {
        "summary" => vec![
            tiles,
            Widget::grid(
                "summary-charts",
                320,
                [
                    Widget::chart(
                        "bars",
                        "Mean by channel",
                        ChartSpec::bar(
                            CHANNELS.iter().map(|(_, name)| (*name).into()).collect(),
                            [
                                BarSeries::new(
                                    "Mean",
                                    (0..CHANNELS.len()).map(|c| stats(&data[c]).1).collect(),
                                )
                                .color(0),
                                BarSeries::new(
                                    "Maximum",
                                    (0..CHANNELS.len()).map(|c| stats(&data[c]).2).collect(),
                                )
                                .color(1),
                            ],
                        ),
                    ),
                    Widget::chart(
                        "histogram",
                        format!("{} distribution", CHANNELS[picked].1),
                        ChartSpec::histogram(data[picked].clone(), 16).labels("amplitude", "count"),
                    ),
                ],
            ),
        ],
        _ => {
            let mut charts: Vec<Widget> = ticked
                .iter()
                .map(|c| {
                    Widget::chart(
                        format!("signal-{}", CHANNELS[*c].0),
                        CHANNELS[*c].1,
                        ChartSpec::line([series(*c).style(styles[*c])]).labels("sample", ""),
                    )
                    .height(200)
                    .sync("signals")
                })
                .collect();
            if ticked.len() > 1 {
                charts.push(
                    Widget::chart(
                        "overlay",
                        "All ticked channels",
                        ChartSpec::line(ticked.iter().map(|c| series(*c))).labels("sample", ""),
                    )
                    .height(200)
                    .legend(false)
                    .sync("signals"),
                );
            }
            vec![
                tiles,
                Widget::grid("signals", 340, charts),
                Widget::section(
                    "decay-section",
                    "Decay on a picked axis",
                    flag(&inputs, &previous, "decay-section", true),
                    [Widget::chart(
                        "decay",
                        "Decay",
                        ChartSpec::scatter([series(3)]).scales(
                            Scale::Linear,
                            if axis == "log" {
                                Scale::Log
                            } else {
                                Scale::Linear
                            },
                        ),
                    )
                    .controls([Widget::select(
                        "decay-axis",
                        "",
                        &axis,
                        [
                            SelectOption::new("log", "log y"),
                            SelectOption::new("linear", "linear y"),
                        ],
                    )])],
                ),
            ]
        }
    };
    let mut content = main;
    content.push(Widget::label("status", status.clone()));
    let root = Widget::column(
        "root",
        [
            Widget::tabs(
                "page",
                &page,
                [
                    SelectOption::new("signals", "Signals"),
                    SelectOption::new("summary", "Summary"),
                ],
            ),
            Widget::row(
                "workspace",
                [sidebar, Widget::column("content", content).gap(12).grow()],
            )
            .gap(16),
        ],
    )
    .gap(12);
    Response {
        panels: vec![PanelUpdate {
            refresh_ms: None,
            id,
            root,
            state: json!({"samples": count_text, "scale": scale_text, "smooth": smooth, "page": page,
                          "decay-axis": axis, "shown": shown, "picked": picked, "status": status}),
        }],
        ..Response::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(
        inputs: BTreeMap<String, InputValue>,
        state: Value,
        widget: &str,
        kind: UiEventKind,
    ) -> Response {
        handle(Request {
            api_version: API_VERSION,
            invocation: Invocation::UiEvent {
                panel: "dashboard".into(),
                revision: 1,
                state,
                inputs,
                event: UiEvent {
                    widget: widget.into(),
                    kind,
                },
            },
            document: None,
            settings: json!({}),
        })
    }

    #[test]
    fn invalid_sample_counts_report_an_error_without_replacing_the_panel() {
        for value in ["abc", "7", "257"] {
            let response = event(
                BTreeMap::from([("samples".into(), InputValue::Text(value.into()))]),
                Value::Null,
                "generate",
                UiEventKind::Click,
            );
            assert!(
                response.error.is_some(),
                "invalid count {value} must not be silently substituted"
            );
            assert!(response.panels.is_empty());
        }
    }

    #[test]
    fn ticking_a_channel_charts_it_and_the_state_remembers() {
        let opened = handle(Request {
            api_version: API_VERSION,
            invocation: Invocation::OpenPanel {
                id: "dashboard".into(),
            },
            document: None,
            settings: json!({}),
        });
        let panel = &opened.panels[0];
        let json = serde_json::to_string(panel).unwrap();
        // DASHBOARD_JSON=<path> keeps the response, to check it against the host's validator.
        if let Ok(path) = std::env::var("DASHBOARD_JSON") {
            std::fs::write(path, serde_json::to_string(&opened).unwrap()).unwrap();
        }
        assert!(json.contains("\"signal-ramp\"") && !json.contains("\"signal-decay\""));
        let ticked = event(
            BTreeMap::new(),
            panel.state.clone(),
            "channels",
            UiEventKind::CheckRow {
                index: 3,
                value: true,
            },
        );
        let json = serde_json::to_string(&ticked.panels[0]).unwrap();
        assert!(json.contains("\"signal-decay\""));
        assert_eq!(ticked.panels[0].state["shown"][3], true);
    }
}
