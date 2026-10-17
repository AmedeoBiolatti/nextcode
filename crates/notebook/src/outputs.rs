//! Folding kernel iopub messages into a cell's outputs as Jupyter does,
//! and choosing how to show an output.

use serde_json::Value;

use crate::model::{Metadata, MimeBundle, Output};

/// Per-execution state between messages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutputState {
    /// `clear_output(wait=True)` arrived: clear before the next output.
    pub clear_pending: bool,
}

/// What [`apply`] did.
#[derive(Debug, Clone, PartialEq)]
pub enum Applied {
    /// The outputs changed.
    Changed,
    /// Nothing to do (unknown message type, or `clear_output(wait)` pending).
    Unchanged,
    /// `update_display_data`: the caller updates every output with that
    /// display id in the notebook ([`update_display`]).
    UpdateDisplay {
        display_id: String,
        data: MimeBundle,
        metadata: Metadata,
    },
}

/// Folds one iopub message (`msg_type`, `content` as the kernel sent it)
/// into `outputs`: `stream` appends to the previous output of the same
/// stream (with `\r`/`\b` folding), `display_data` (keeping
/// `transient.display_id`), `execute_result`, `error`, `clear_output`
/// (`wait`: deferred to the next output).
pub fn apply(
    outputs: &mut Vec<Output>,
    state: &mut OutputState,
    msg_type: &str,
    content: &Value,
) -> Applied {
    let str_field = |key: &str| content.get(key).and_then(Value::as_str).unwrap_or("");
    let metadata = || {
        content
            .get("metadata")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    };
    let data = || bundle_from_json(content.get("data").unwrap_or(&Value::Null));
    let display_id = || {
        content
            .pointer("/transient/display_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let output = match msg_type {
        "stream" => {
            let name = str_field("name");
            let text = str_field("text");
            flush_clear(outputs, state);
            if let Some(Output::Stream {
                name: last_name,
                text: last_text,
            }) = outputs.last_mut()
                && last_name == name
            {
                push_stream(last_text, text);
            } else {
                let mut folded = String::new();
                push_stream(&mut folded, text);
                outputs.push(Output::Stream {
                    name: name.to_owned(),
                    text: folded,
                });
            }
            return Applied::Changed;
        }
        "display_data" => Output::DisplayData {
            data: data(),
            metadata: metadata(),
            display_id: display_id(),
        },
        "execute_result" => Output::ExecuteResult {
            execution_count: content.get("execution_count").and_then(Value::as_u64),
            data: data(),
            metadata: metadata(),
        },
        "error" => Output::Error {
            ename: str_field("ename").to_owned(),
            evalue: str_field("evalue").to_owned(),
            traceback: content
                .get("traceback")
                .and_then(Value::as_array)
                .map(|lines| {
                    lines
                        .iter()
                        .filter_map(|line| line.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        },
        "clear_output" => {
            if content
                .get("wait")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                state.clear_pending = true;
                return Applied::Unchanged;
            }
            state.clear_pending = false;
            if outputs.is_empty() {
                return Applied::Unchanged;
            }
            outputs.clear();
            return Applied::Changed;
        }
        "update_display_data" => {
            return match display_id() {
                Some(display_id) => Applied::UpdateDisplay {
                    display_id,
                    data: data(),
                    metadata: metadata(),
                },
                None => Applied::Unchanged,
            };
        }
        _ => return Applied::Unchanged,
    };
    flush_clear(outputs, state);
    outputs.push(output);
    Applied::Changed
}

fn flush_clear(outputs: &mut Vec<Output>, state: &mut OutputState) {
    if state.clear_pending {
        state.clear_pending = false;
        outputs.clear();
    }
}

/// Replaces the data/metadata of outputs with `display_id`; true if any.
pub fn update_display(
    outputs: &mut [Output],
    display_id: &str,
    data: &MimeBundle,
    metadata: &Metadata,
) -> bool {
    let mut any = false;
    for output in outputs {
        if let Output::DisplayData {
            data: old_data,
            metadata: old_metadata,
            display_id: Some(id),
        } = output
            && id == display_id
        {
            *old_data = data.clone();
            *old_metadata = metadata.clone();
            any = true;
        }
    }
    any
}

/// Appends `chunk` to a stream's text as a terminal shows it: `\r` returns
/// to the start of the current line (the rest is overwritten by what
/// follows; `\r\n` is a newline), `\b` erases the previous character.
///
/// A `\r` at the very end of a chunk stays at the end of `text` until the
/// next chunk tells whether it starts a `\r\n`.
pub fn push_stream(text: &mut String, chunk: &str) {
    // Byte offset in `text` where the next character overwrites (`None`:
    // append).
    let mut cursor: Option<usize> = None;
    if text.ends_with('\r') {
        text.pop();
        cursor = Some(line_start(text));
    }
    let mut chars = chunk.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => match chars.peek() {
                Some('\n') => {}
                None => {
                    text.push('\r');
                    return;
                }
                Some(_) => cursor = Some(line_start(text)),
            },
            '\n' => {
                text.push('\n');
                cursor = None;
            }
            '\x08' => match cursor {
                None => {
                    if text.chars().next_back().is_some_and(|last| last != '\n') {
                        text.pop();
                    }
                }
                Some(at) => {
                    if at > line_start(text) {
                        let prev = text[..at].chars().next_back().map_or(0, char::len_utf8);
                        cursor = Some(at - prev);
                    }
                }
            },
            c => match cursor {
                None => text.push(c),
                Some(at) => {
                    let old_len = text[at..].chars().next().map_or(0, char::len_utf8);
                    let mut buf = [0; 4];
                    text.replace_range(at..at + old_len, c.encode_utf8(&mut buf));
                    let next = at + c.len_utf8();
                    cursor = (next < text.len()).then_some(next);
                }
            },
        }
    }
}

fn line_start(text: &str) -> usize {
    text.rfind('\n').map_or(0, |i| i + 1)
}

/// Whether nbformat keeps a mime's value as JSON (not lines of text).
pub fn is_json_mime(mime: &str) -> bool {
    mime == "application/json" || (mime.starts_with("application/") && mime.ends_with("+json"))
}

/// A mime bundle from a message's or file's `data` (lists of lines
/// joined for non-JSON mimes).
pub fn bundle_from_json(data: &Value) -> MimeBundle {
    let Some(map) = data.as_object() else {
        return MimeBundle::new();
    };
    map.iter()
        .map(|(mime, value)| {
            let value = match value {
                Value::Array(lines)
                    if !is_json_mime(mime) && lines.iter().all(Value::is_string) =>
                {
                    Value::String(lines.iter().filter_map(Value::as_str).collect())
                }
                other => other.clone(),
            };
            (mime.clone(), value)
        })
        .collect()
}

/// How to show a mime bundle: the richest representation the editor
/// renders.
#[derive(Debug, Clone, PartialEq)]
pub enum Rich<'a> {
    /// Base64.
    Png(&'a str),
    /// Base64.
    Jpeg(&'a str),
    Svg(&'a str),
    Markdown(&'a str),
    /// `text/html`: rendered natively when it fits the safe subset
    /// (`crate::html`), else the `text/plain` fallback, if any, with an
    /// "Open in browser" action.
    Html {
        html: &'a str,
        plain: Option<&'a str>,
    },
    Latex(&'a str),
    Json(&'a Value),
    /// An ipywidgets view of model `model_id` (see `crate::widgets`), with
    /// the text fallback.
    Widget {
        model_id: Option<&'a str>,
        plain: Option<&'a str>,
    },
    /// marimo UI elements with the Markdown/text around them
    /// ([`MARIMO_UI_MIME`] parts), with the text fallback.
    MarimoUi {
        parts: &'a [Value],
        plain: Option<&'a str>,
    },
    Plain(&'a str),
    /// Only mimes the editor cannot show.
    Unsupported(Vec<&'a str>),
}

pub const WIDGET_MIME: &str = "application/vnd.jupyter.widget-view+json";
/// marimo UI elements of a marimo cell's output (the bridge's helper):
/// `{parts: [{markdown} | {text} | {element: {id, kind, label, value,
/// args}}]}`.
pub const MARIMO_UI_MIME: &str = "application/vnd.nextcode.marimo-ui+json";

/// The parts of a bundle's marimo UI view ([`MARIMO_UI_MIME`]), if any.
pub fn marimo_ui_parts(data: &MimeBundle) -> Option<&[Value]> {
    data.get(MARIMO_UI_MIME)?
        .get("parts")?
        .as_array()
        .map(Vec::as_slice)
}

/// Sets the value of element `object_id` in `output`'s marimo UI view (the
/// element shows it after a change); whether it was there with another
/// value.
pub fn set_marimo_ui_value(output: &mut Output, object_id: &str, value: &Value) -> bool {
    let (Output::DisplayData { data, .. } | Output::ExecuteResult { data, .. }) = output else {
        return false;
    };
    let Some(parts) = data
        .get_mut(MARIMO_UI_MIME)
        .and_then(|ui| ui.get_mut("parts"))
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    let mut changed = false;
    for part in parts {
        if let Some(element) = part.get_mut("element")
            && element.get("id").and_then(Value::as_str) == Some(object_id)
            && element.get("value") != Some(value)
        {
            element["value"] = value.clone();
            changed = true;
        }
    }
    changed
}

/// Order: marimo UI view, widget view, `image/png`, `image/jpeg`,
/// `image/svg+xml`, `text/markdown`, `text/html`, `text/latex`,
/// `application/json`, `text/plain`.
pub fn richest(data: &MimeBundle) -> Rich<'_> {
    let text = |mime: &str| data.get(mime).and_then(Value::as_str);
    let plain = text("text/plain");
    if let Some(parts) = marimo_ui_parts(data) {
        return Rich::MarimoUi { parts, plain };
    }
    if let Some(view) = data.get(WIDGET_MIME) {
        return Rich::Widget {
            model_id: view.get("model_id").and_then(Value::as_str),
            plain,
        };
    }
    if let Some(png) = text("image/png") {
        return Rich::Png(png);
    }
    if let Some(jpeg) = text("image/jpeg") {
        return Rich::Jpeg(jpeg);
    }
    if let Some(svg) = text("image/svg+xml") {
        return Rich::Svg(svg);
    }
    if let Some(markdown) = text("text/markdown") {
        return Rich::Markdown(markdown);
    }
    if let Some(html) = text("text/html") {
        return Rich::Html { html, plain };
    }
    if let Some(latex) = text("text/latex") {
        return Rich::Latex(latex);
    }
    if let Some(json) = data.get("application/json") {
        return Rich::Json(json);
    }
    if let Some(plain) = plain {
        return Rich::Plain(plain);
    }
    Rich::Unsupported(data.keys().map(String::as_str).collect())
}

/// An output as plain text (headless results, copy): stream text, the
/// bundle's `text/plain` (else its mime types), `ename: evalue` and the
/// traceback without ANSI escapes.
pub fn plain_text(output: &Output) -> String {
    match output {
        Output::Stream { text, .. } => text.clone(),
        Output::DisplayData { data, .. } | Output::ExecuteResult { data, .. } => {
            match data.get("text/plain").and_then(Value::as_str) {
                Some(plain) => plain.to_owned(),
                None => {
                    let mimes: Vec<&str> = data.keys().map(String::as_str).collect();
                    format!("[{}]", mimes.join(", "))
                }
            }
        }
        Output::Error {
            ename,
            evalue,
            traceback,
        } => {
            let mut text = format!("{ename}: {evalue}");
            if !traceback.is_empty() {
                text.push('\n');
                text.push_str(&crate::ansi::strip(&traceback.join("\n")));
            }
            text
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn stream(outputs: &mut Vec<Output>, state: &mut OutputState, name: &str, text: &str) {
        let applied = apply(
            outputs,
            state,
            "stream",
            &json!({"name": name, "text": text}),
        );
        assert_eq!(applied, Applied::Changed);
    }

    #[test]
    fn streams_merge_by_name() {
        let mut outputs = Vec::new();
        let mut state = OutputState::default();
        stream(&mut outputs, &mut state, "stdout", "a\n");
        stream(&mut outputs, &mut state, "stdout", "b\n");
        stream(&mut outputs, &mut state, "stderr", "warn\n");
        stream(&mut outputs, &mut state, "stdout", "c\n");
        assert_eq!(
            outputs,
            vec![
                Output::Stream {
                    name: "stdout".into(),
                    text: "a\nb\n".into()
                },
                Output::Stream {
                    name: "stderr".into(),
                    text: "warn\n".into()
                },
                Output::Stream {
                    name: "stdout".into(),
                    text: "c\n".into()
                },
            ]
        );
    }

    #[test]
    fn carriage_return_progress() {
        let mut text = String::new();
        for chunk in ["start\n", "  0%", "\r 50%", "\r100%\r", "\ndone\n"] {
            push_stream(&mut text, chunk);
        }
        assert_eq!(text, "start\n100%\ndone\n");

        // Overwriting keeps the tail of a longer line (as a terminal).
        let mut text = String::from("abcdef");
        push_stream(&mut text, "\rXY");
        assert_eq!(text, "XYcdef");

        // A `\r` at the end of a chunk waits for the next one.
        let mut text = String::new();
        push_stream(&mut text, "line\r");
        assert_eq!(text, "line\r");
        push_stream(&mut text, "\nnext");
        assert_eq!(text, "line\nnext");

        // tqdm-like.
        let mut text = String::new();
        for i in 0..=3 {
            push_stream(&mut text, &format!("\r{i}/3 [{}]", "#".repeat(i)));
        }
        push_stream(&mut text, "\n");
        assert_eq!(text, "3/3 [###]\n");

        // Multibyte overwrite.
        let mut text = String::from("ééé");
        push_stream(&mut text, "\rx");
        assert_eq!(text, "xéé");
    }

    #[test]
    fn backspace() {
        let mut text = String::new();
        push_stream(&mut text, "abc\x08\x08d\n\x08x");
        assert_eq!(text, "ad\nx");
        let mut text = String::from("abc");
        push_stream(&mut text, "\r\x08z\x08\x08y");
        assert_eq!(text, "ybc");
    }

    #[test]
    fn clear_output_wait_is_deferred() {
        let mut outputs = Vec::new();
        let mut state = OutputState::default();
        stream(&mut outputs, &mut state, "stdout", "1\n");
        assert_eq!(
            apply(
                &mut outputs,
                &mut state,
                "clear_output",
                &json!({"wait": true})
            ),
            Applied::Unchanged
        );
        assert_eq!(outputs.len(), 1);
        stream(&mut outputs, &mut state, "stdout", "2\n");
        assert_eq!(
            outputs,
            vec![Output::Stream {
                name: "stdout".into(),
                text: "2\n".into()
            }]
        );
        assert_eq!(
            apply(
                &mut outputs,
                &mut state,
                "clear_output",
                &json!({"wait": false})
            ),
            Applied::Changed
        );
        assert!(outputs.is_empty());
    }

    #[test]
    fn display_ids_and_updates() {
        let mut outputs = Vec::new();
        let mut state = OutputState::default();
        let content = json!({
            "data": {"text/plain": ["a", "b"], "application/json": ["x"]},
            "metadata": {},
            "transient": {"display_id": "d1"},
        });
        assert_eq!(
            apply(&mut outputs, &mut state, "display_data", &content),
            Applied::Changed
        );
        let Output::DisplayData {
            data, display_id, ..
        } = &outputs[0]
        else {
            panic!()
        };
        assert_eq!(display_id.as_deref(), Some("d1"));
        assert_eq!(data["text/plain"], json!("ab"));
        assert_eq!(data["application/json"], json!(["x"]));

        let update = json!({
            "data": {"text/plain": "new"},
            "metadata": {"m": 1},
            "transient": {"display_id": "d1"},
        });
        let Applied::UpdateDisplay {
            display_id,
            data,
            metadata,
        } = apply(&mut outputs, &mut state, "update_display_data", &update)
        else {
            panic!()
        };
        assert!(update_display(&mut outputs, &display_id, &data, &metadata));
        assert!(!update_display(&mut outputs, "other", &data, &metadata));
        assert_eq!(plain_text(&outputs[0]), "new");

        apply(
            &mut outputs,
            &mut state,
            "execute_result",
            &json!({"execution_count": 3, "data": {"text/plain": "3"}, "metadata": {}}),
        );
        assert_eq!(
            outputs[1],
            Output::ExecuteResult {
                execution_count: Some(3),
                data: MimeBundle::from([("text/plain".into(), json!("3"))]),
                metadata: Metadata::new(),
            }
        );
        assert_eq!(
            apply(&mut outputs, &mut state, "status", &json!({})),
            Applied::Unchanged
        );
    }

    #[test]
    fn errors_as_plain_text() {
        let mut outputs = Vec::new();
        let mut state = OutputState::default();
        apply(
            &mut outputs,
            &mut state,
            "error",
            &json!({
                "ename": "ValueError",
                "evalue": "bad",
                "traceback": ["\x1b[0;31mValueError\x1b[0m: bad"],
            }),
        );
        assert_eq!(plain_text(&outputs[0]), "ValueError: bad\nValueError: bad");
    }

    #[test]
    fn richest_order() {
        let bundle = |pairs: &[(&str, Value)]| -> MimeBundle {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect()
        };
        let b = bundle(&[("text/plain", json!("df")), ("text/html", json!("<table>"))]);
        assert_eq!(
            richest(&b),
            Rich::Html {
                html: "<table>",
                plain: Some("df")
            }
        );
        let b = bundle(&[
            ("text/plain", json!("<Figure>")),
            ("image/png", json!("iVBOR")),
            ("image/svg+xml", json!("<svg/>")),
        ]);
        assert_eq!(richest(&b), Rich::Png("iVBOR"));
        let b = bundle(&[
            ("text/plain", json!("IntSlider(value=0)")),
            (WIDGET_MIME, json!({"model_id": "abc", "version_major": 2})),
        ]);
        assert_eq!(
            richest(&b),
            Rich::Widget {
                model_id: Some("abc"),
                plain: Some("IntSlider(value=0)")
            }
        );
        let b = bundle(&[
            ("text/markdown", json!("# x")),
            ("text/latex", json!("$x$")),
        ]);
        assert_eq!(richest(&b), Rich::Markdown("# x"));
        let b = bundle(&[
            ("application/json", json!({"a": 1})),
            ("text/plain", json!("{}")),
        ]);
        assert_eq!(richest(&b), Rich::Json(&json!({"a": 1})));
        let b = bundle(&[("text/latex", json!("$x$")), ("text/plain", json!("x"))]);
        assert_eq!(richest(&b), Rich::Latex("$x$"));
        let b = bundle(&[("application/pdf", json!("JVBER"))]);
        assert_eq!(richest(&b), Rich::Unsupported(vec!["application/pdf"]));
        assert_eq!(
            plain_text(&Output::DisplayData {
                data: b,
                metadata: Metadata::new(),
                display_id: None
            }),
            "[application/pdf]"
        );
    }
}
