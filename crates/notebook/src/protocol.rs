//! JSON lines between next_code and the kernel bridge (`assets/bridge.py`).
//!
//! The bridge runs in the notebook's Python interpreter where the project
//! lives and drives an ipykernel through `jupyter_client`; next_code never
//! speaks ZeroMQ. It is started as
//! `python -u -c BOOTSTRAP` and its first stdin line is the script itself
//! (a JSON string), so nothing is written to disk on any backend.
//!
//! Requests (one line each): `{"id": n, "op": "...", ...}`. Every request
//! gets exactly one reply line `{"reply": n, "ok": true, "result": …}` or
//! `{"reply": n, "ok": false, "error": "…"}`; an `execute` reply comes only
//! after all of its iopub output (the kernel went idle for it).
//!
//! Events (unsolicited lines) carry `"event"`:
//! - `hello` `{python, version, missing: [module…]}`: first line; `missing`
//!   lists `ipykernel`/`jupyter_client` when they cannot be imported (then
//!   only `kernelspecs` fails and `start` replies with an error).
//! - `status` `{state, request?}`: `starting`, `idle`, `busy`,
//!   `restarting`, `dead`; `request` is the request the kernel is busy/idle
//!   for when known.
//! - `output` `{request?, parent?, msg_type, content}`: iopub `stream`,
//!   `display_data`, `execute_result`, `error`, `clear_output`,
//!   `update_display_data` with the kernel's `content` unchanged; `parent`
//!   is the iopub message's parent `msg_id` (ipywidgets' Output widget
//!   captures the outputs of its `msg_id`).
//! - `comm` `{request?, parent?, msg_type, content, buffers}`: iopub
//!   `comm_open`, `comm_msg`, `comm_close` (ipywidgets) in order with the
//!   outputs; `buffers` are the binary parts in base64.
//! - `execute_input` `{request, execution_count}`.
//! - `dead` `{message}`: the kernel process exited unexpectedly.
//! - `log` `{message}`: bridge diagnostics.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The `-c` program that reads the bridge script from the first stdin line.
pub const BOOTSTRAP: &str =
    "import sys,json;exec(compile(json.loads(sys.stdin.readline()),'next_code_bridge.py','exec'))";

/// The bridge script (first stdin line, as a JSON string).
pub const BRIDGE: &str = include_str!("../assets/bridge.py");

/// The first stdin line: [`BRIDGE`] as a JSON string, newline-terminated.
pub fn bootstrap_line() -> String {
    let mut line = serde_json::to_string(BRIDGE).expect("string serializes");
    line.push('\n');
    line
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub op: Op,
}

impl Request {
    /// The request as one protocol line (with the newline).
    pub fn to_line(&self) -> String {
        let mut line = serde_json::to_string(self).expect("request serializes");
        line.push('\n');
        line
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Starts the kernel (`kernel_name`: a kernelspec; default: an
    /// ipykernel in the bridge's own interpreter) with `cwd` as its working
    /// directory. Reply: the kernel's `kernel_info_reply` content.
    Start {
        #[serde(skip_serializing_if = "Option::is_none")]
        kernel_name: Option<String>,
        cwd: String,
    },
    /// Reply: `{status: "ok" | "error" | "aborted", execution_count}`.
    Execute {
        code: String,
        /// Runs `code` as a marimo cell (see the bridge's `__nc_marimo__`).
        #[serde(skip_serializing_if = "Option::is_none")]
        marimo: Option<MarimoRun>,
    },
    /// Sets a marimo UI element's value (its front-end form, as marimo's
    /// front end sends it) in the kernel. Reply: `{names: [...]}`, the
    /// globals bound to the element (cells using them re-run).
    MarimoSetUi { object_id: String, value: Value },
    /// Reply: `null` once the interrupt was sent.
    Interrupt,
    /// Reply: the new kernel's `kernel_info_reply` content.
    Restart,
    /// Reply: `null`; the bridge exits after replying.
    Shutdown,
    /// `cursor_pos` in Unicode code points (Jupyter protocol 5.2). Reply:
    /// the `complete_reply` content (`matches`, `cursor_start`,
    /// `cursor_end`, `metadata`).
    Complete { code: String, cursor_pos: usize },
    /// Reply: the `inspect_reply` content (`found`, `data`).
    Inspect {
        code: String,
        cursor_pos: usize,
        detail_level: u8,
    },
    /// Reply: the `is_complete_reply` content (`status`, `indent`).
    IsComplete { code: String },
    /// Reply: `{name: {display_name, language, argv}}` from
    /// `jupyter_client`'s `KernelSpecManager`.
    KernelSpecs,
    /// A shell `comm_msg` (`content: {comm_id, data}`, `buffers` base64)
    /// whose header `msg_id` is `msg_id` (the kernel's echo and the
    /// outputs it causes carry it as their parent). Reply: `null` once
    /// sent.
    CommMsg {
        comm_id: String,
        data: Value,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        buffers: Vec<String>,
        msg_id: String,
    },
}

/// How a marimo cell runs: its body in a namespace holding `refs` (taken
/// from the kernel's globals), then `defs` published to the globals (names
/// the same cell defined before and no longer does are deleted); the last
/// expression's value is the cell's output.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MarimoRun {
    /// Stable id of the cell (its previous definitions are tracked by it).
    pub cell: String,
    pub defs: Vec<String>,
    pub refs: Vec<String>,
}

/// One line from the bridge.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Reply {
        id: u64,
        result: Result<Value, String>,
    },
    Event(Event),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KernelState {
    Starting,
    Idle,
    Busy,
    Restarting,
    Dead,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Hello {
        python: String,
        version: String,
        #[serde(default)]
        missing: Vec<String>,
    },
    Status {
        state: KernelState,
        #[serde(default)]
        request: Option<u64>,
    },
    Output {
        #[serde(default)]
        request: Option<u64>,
        #[serde(default)]
        parent: Option<String>,
        msg_type: String,
        content: Value,
    },
    Comm {
        #[serde(default)]
        request: Option<u64>,
        #[serde(default)]
        parent: Option<String>,
        msg_type: String,
        content: Value,
        #[serde(default)]
        buffers: Vec<String>,
    },
    ExecuteInput {
        request: u64,
        execution_count: u64,
    },
    Dead {
        message: String,
    },
    Log {
        message: String,
    },
}

/// Parses one bridge line.
pub fn parse_line(line: &str) -> Result<Message, String> {
    let value: Value =
        serde_json::from_str(line).map_err(|err| format!("invalid bridge line: {err}"))?;
    if let Some(id) = value.get("reply") {
        let id = id
            .as_u64()
            .ok_or_else(|| format!("invalid reply id in {line}"))?;
        let result = if value["ok"].as_bool() == Some(true) {
            Ok(value.get("result").cloned().unwrap_or(Value::Null))
        } else {
            Err(value["error"]
                .as_str()
                .unwrap_or("the bridge reported an error")
                .to_owned())
        };
        return Ok(Message::Reply { id, result });
    }
    serde_json::from_value(value)
        .map(Message::Event)
        .map_err(|err| format!("unknown bridge event ({err}): {line}"))
}

/// A byte offset in `text` as Unicode code points (Jupyter cursor
/// positions).
pub fn code_points(text: &str, byte_offset: usize) -> usize {
    text[..byte_offset.min(text.len())].chars().count()
}

/// The byte offset of code point `position` in `text` (clamped).
pub fn byte_offset(text: &str, position: usize) -> usize {
    text.char_indices()
        .nth(position)
        .map_or(text.len(), |(offset, _)| offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requests_are_one_tagged_line() {
        let line = Request {
            id: 3,
            op: Op::Execute {
                code: "print(1)\n".into(),
                marimo: None,
            },
        }
        .to_line();
        assert!(line.ends_with('\n') && !line[..line.len() - 1].contains('\n'));
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            value,
            json!({"id": 3, "op": "execute", "code": "print(1)\n"})
        );
        let value: Value = serde_json::from_str(
            &Request {
                id: 4,
                op: Op::Interrupt,
            }
            .to_line(),
        )
        .unwrap();
        assert_eq!(value, json!({"id": 4, "op": "interrupt"}));
    }

    #[test]
    fn replies_and_events_parse() {
        assert_eq!(
            parse_line(r#"{"reply": 2, "ok": true, "result": {"status": "ok"}}"#).unwrap(),
            Message::Reply {
                id: 2,
                result: Ok(json!({"status": "ok"}))
            }
        );
        assert_eq!(
            parse_line(r#"{"reply": 2, "ok": false, "error": "no kernel"}"#).unwrap(),
            Message::Reply {
                id: 2,
                result: Err("no kernel".into())
            }
        );
        assert_eq!(
            parse_line(r#"{"event": "status", "state": "busy", "request": 7}"#).unwrap(),
            Message::Event(Event::Status {
                state: KernelState::Busy,
                request: Some(7)
            })
        );
        assert!(parse_line("not json").is_err());
        assert!(parse_line(r#"{"event": "nope"}"#).is_err());
    }

    #[test]
    fn cursor_positions_count_code_points() {
        let text = "é = 'ü'; x";
        let end = text.len();
        assert_eq!(code_points(text, end), 10);
        assert_eq!(byte_offset(text, 10), end);
        assert_eq!(byte_offset(text, 1), 'é'.len_utf8());
    }
}
