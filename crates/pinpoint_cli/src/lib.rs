//! `next_code pinpoint …` and `next_code_agent pinpoint …`: command-line
//! arguments to a Pinpoint request (JSON) and a reply to readable output.
//! No GUI code: the editor's CLI and the remote agent share it.

use serde_json::{Map, Value, json};

pub const USAGE: &str = "\
usage: next_code pinpoint <command> [options]

Point at parts of the running NextCode window (see docs/pinpoint-for-agents.md).

  list [PREFIX] [--visible] [--interactive] [--depth N] [--limit N] [--full-text]
        elements on screen: path, label, text, bounds (PREFIX: a path)
  find <TEXT>... [--limit N]
        elements whose label/text/id matches, best first
  highlight <TARGET> [per-target options] [<TARGET> [options]]... [--clear]
            [--duration SECONDS] [--reveal] [--no-pulse] [--steps]
        TARGET: an element path (or nextcode://ui/<path>), a code reference
        file:line[:col[-line:col]], '{json target}', or --find TEXT.
        Per-target options (after the target; before every target they apply
        to all): --note TEXT, --step N, --style outline|fill|underline,
        --color accent|warning|error|green|blue|#rrggbb, --text TEXT [--nth N]
        (characters of the element's text).
        --clear removes earlier highlights first; --steps numbers targets 1..n;
        --reveal opens/scrolls code into view.
  clear [ID]... [--all-windows]
        remove highlights (all of the window's without IDs)
  pick  turn on the picker: the user's next click pins an element
  pins [--clear]
        what the user picked (oldest first)
  wait-pin [--timeout SECONDS] [--since SEQ] [--pick]
        wait for the user's next pick (--pick turns the picker on first)
  windows
        open windows and their project roots

Common options: --window ID (else the window whose project holds the current
directory, else the active one), --format json (raw JSON reply),
--json '{\"cmd\": …}' (send a raw request).
Exit codes: 0 ok, 1 error reply, 2 bad arguments.";

/// Reads `args` (after `pinpoint`) into a request. `{"cmd":"help"}` for
/// `--help`/no arguments. `"output": "json"` asks for raw output (the
/// editor ignores it).
pub fn parse(args: &[String]) -> Result<Value, String> {
    let mut args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut common = Map::new();
    // Options valid with every command, anywhere.
    let mut index = 0;
    while index < args.len() {
        match args[index] {
            "--window" => {
                let id = value_of(&args, index)?;
                common.insert("window".into(), number(id, "--window")?.into());
                args.drain(index..index + 2);
            }
            "--format" => {
                match value_of(&args, index)? {
                    "json" => common.insert("output".into(), "json".into()),
                    "text" => None,
                    other => return Err(format!("--format is json or text, not {other:?}")),
                };
                args.drain(index..index + 2);
            }
            "--json" => {
                let raw = value_of(&args, index)?;
                let request: Value = serde_json::from_str(raw)
                    .map_err(|error| format!("--json: bad JSON: {error}"))?;
                let Value::Object(mut request) = request else {
                    return Err("--json takes a JSON object".into());
                };
                if !request.contains_key("cmd") {
                    return Err("--json: the request needs \"cmd\"".into());
                }
                for (key, value) in common {
                    request.entry(key).or_insert(value);
                }
                return Ok(Value::Object(request));
            }
            "-h" | "--help" | "help" if index == 0 || args[index] != "help" => {
                return Ok(json!({ "cmd": "help" }));
            }
            _ => index += 1,
        }
    }
    let Some((&command, rest)) = args.split_first() else {
        return Ok(json!({ "cmd": "help" }));
    };
    let mut request = match command {
        "list" => list(rest)?,
        "find" => find(rest)?,
        "highlight" => highlight(rest)?,
        "clear" => clear(rest)?,
        "pins" => flags_only(rest, "pins", &[("--clear", "clear")])?,
        "pick" => flags_only(rest, "pick", &[])?,
        "windows" => flags_only(rest, "windows", &[])?,
        "wait-pin" | "wait_pin" => wait_pin(rest)?,
        other => {
            return Err(format!(
                "unknown command {other:?}: use list, find, highlight, clear, pick, pins, wait-pin or windows"
            ));
        }
    };
    if let Value::Object(object) = &mut request {
        object.extend(common);
    }
    Ok(request)
}

fn value_of<'a>(args: &[&'a str], index: usize) -> Result<&'a str, String> {
    args.get(index + 1)
        .copied()
        .ok_or_else(|| format!("{} needs a value", args[index]))
}

fn number(text: &str, option: &str) -> Result<u64, String> {
    text.parse()
        .map_err(|_| format!("{option} takes a number, not {text:?}"))
}

fn seconds(text: &str, option: &str) -> Result<f64, String> {
    text.parse::<f64>()
        .ok()
        .filter(|s| *s > 0.)
        .ok_or_else(|| format!("{option} takes seconds, not {text:?}"))
}

fn flags_only(args: &[&str], cmd: &str, flags: &[(&str, &str)]) -> Result<Value, String> {
    let mut request = json!({ "cmd": cmd });
    for arg in args {
        match flags.iter().find(|(flag, _)| flag == arg) {
            Some((_, key)) => request[*key] = true.into(),
            None => return Err(format!("{cmd}: unexpected {arg:?}")),
        }
    }
    Ok(request)
}

fn list(args: &[&str]) -> Result<Value, String> {
    let mut request = json!({ "cmd": "list" });
    let mut index = 0;
    while index < args.len() {
        match args[index] {
            "--prefix" => {
                request["prefix"] = value_of(args, index)?.into();
                index += 1;
            }
            "--visible" => request["visible"] = true.into(),
            "--interactive" => request["interactive"] = true.into(),
            "--full-text" => request["full_text"] = true.into(),
            "--depth" => {
                request["depth"] = number(value_of(args, index)?, "--depth")?.into();
                index += 1;
            }
            "--limit" => {
                request["limit"] = number(value_of(args, index)?, "--limit")?.into();
                index += 1;
            }
            prefix if !prefix.starts_with("--") && request.get("prefix").is_none() => {
                request["prefix"] = prefix.into()
            }
            other => return Err(format!("list: unexpected {other:?}")),
        }
        index += 1;
    }
    Ok(request)
}

fn find(args: &[&str]) -> Result<Value, String> {
    let mut request = json!({ "cmd": "find" });
    let mut words = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index] {
            "--limit" => {
                request["limit"] = number(value_of(args, index)?, "--limit")?.into();
                index += 1;
            }
            word => words.push(word),
        }
        index += 1;
    }
    if words.is_empty() {
        return Err("find needs the text to look for".into());
    }
    request["query"] = words.join(" ").into();
    Ok(request)
}

fn highlight(args: &[&str]) -> Result<Value, String> {
    let mut request = json!({ "cmd": "highlight" });
    let mut targets: Vec<Value> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        // Per-target options go on the last target, else on the request.
        let mut set = |key: &str, value: Value| match targets.last_mut() {
            Some(target) => target[key] = value,
            None => request[key] = value,
        };
        match arg {
            "--note" => {
                set("note", value_of(args, index)?.into());
                index += 1;
            }
            "--step" => {
                set("step", number(value_of(args, index)?, "--step")?.into());
                index += 1;
            }
            "--style" => {
                let style = value_of(args, index)?;
                if !["outline", "fill", "underline"].contains(&style) {
                    return Err(format!(
                        "--style is outline, fill or underline, not {style:?}"
                    ));
                }
                set("style", style.into());
                index += 1;
            }
            "--color" => {
                set("color", value_of(args, index)?.into());
                index += 1;
            }
            "--text" | "--nth" => {
                let value = value_of(args, index)?;
                let Some(target) = targets.last_mut() else {
                    return Err(format!("{arg} follows the element target it applies to"));
                };
                // `--text` narrows an element (path) to characters of it.
                if let Some(path) = target.get("ref").and_then(Value::as_str) {
                    let path = path
                        .strip_prefix("nextcode://ui/")
                        .unwrap_or(path)
                        .to_owned();
                    target.as_object_mut().unwrap().remove("ref");
                    target["path"] = path.into();
                }
                if target.get("path").is_none() {
                    return Err(format!("{arg} applies to element targets"));
                }
                if arg == "--text" {
                    target["text"] = value.into();
                } else {
                    target["nth"] = number(value, "--nth")?.into();
                }
                index += 1;
            }
            "--find" => {
                targets.push(json!({ "find": value_of(args, index)? }));
                index += 1;
            }
            "--clear" => request["clear"] = true.into(),
            "--reveal" => request["reveal"] = true.into(),
            "--steps" => request["steps"] = true.into(),
            "--no-pulse" => request["pulse"] = false.into(),
            "--duration" => {
                request["duration"] = seconds(value_of(args, index)?, "--duration")?.into();
                index += 1;
            }
            json if json.trim_start().starts_with('{') => {
                let target: Value = serde_json::from_str(json)
                    .map_err(|error| format!("bad JSON target {json:?}: {error}"))?;
                targets.push(target);
            }
            option if option.starts_with("--") => {
                return Err(format!("highlight: unknown option {option:?}"));
            }
            reference => targets.push(json!({ "ref": reference })),
        }
        index += 1;
    }
    if targets.is_empty() {
        return Err("highlight needs at least one target".into());
    }
    request["targets"] = targets.into();
    Ok(request)
}

fn clear(args: &[&str]) -> Result<Value, String> {
    let mut request = json!({ "cmd": "clear" });
    let mut ids = Vec::new();
    for arg in args {
        match *arg {
            "--all-windows" => request["all_windows"] = true.into(),
            id => ids.push(Value::from(number(id.trim_start_matches('#'), "clear")?)),
        }
    }
    if !ids.is_empty() {
        request["ids"] = ids.into();
    }
    Ok(request)
}

fn wait_pin(args: &[&str]) -> Result<Value, String> {
    let mut request = json!({ "cmd": "wait_pin" });
    let mut index = 0;
    while index < args.len() {
        match args[index] {
            "--timeout" => {
                request["timeout"] = seconds(value_of(args, index)?, "--timeout")?.into();
                index += 1;
            }
            "--since" => {
                request["since"] = number(value_of(args, index)?, "--since")?.into();
                index += 1;
            }
            "--pick" => request["pick"] = true.into(),
            other => return Err(format!("wait-pin: unexpected {other:?}")),
        }
        index += 1;
    }
    Ok(request)
}

/// Prints `reply` (to stdout; errors to stderr) and returns the exit code.
pub fn print_reply(request: &Value, reply: &Value) -> i32 {
    let (out, code) = render(request, reply);
    if code == 0 {
        println!("{out}");
    } else {
        eprintln!("{out}");
    }
    code
}

/// The text [`print_reply`] prints and the exit code.
pub fn render(request: &Value, reply: &Value) -> (String, i32) {
    let ok = reply.get("ok").and_then(Value::as_bool) == Some(true);
    let code = if ok { 0 } else { 1 };
    if request.get("output").and_then(Value::as_str) == Some("json") {
        return (pretty(reply), code);
    }
    if !ok {
        return (error_text(reply), code);
    }
    let text = match request.get("cmd").and_then(Value::as_str) {
        Some("list" | "find") => nodes_text(reply),
        Some("highlight") => highlight_text(reply),
        _ => pretty(reply),
    };
    (text, code)
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

fn error_text(reply: &Value) -> String {
    let mut text = format!(
        "error: {}",
        reply
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
    );
    if let Some(path) = reply.get("path").and_then(Value::as_str) {
        text.push_str(&format!("\n  path: {path}"));
    }
    details(reply, &mut text);
    for key in ["commands", "windows", "example", "find"] {
        if let Some(value) = reply.get(key) {
            text.push_str(&format!("\n  {key}: {value}"));
        }
    }
    text
}

fn details(value: &Value, text: &mut String) {
    if let Some(reason) = value.get("reason").and_then(Value::as_str) {
        text.push_str(&format!("\n  reason: {reason}"));
    }
    if let Some(error) = value.get("reveal_error").and_then(Value::as_str) {
        text.push_str(&format!("\n  reveal failed: {error}"));
    }
    if let Some(visible) = value.get("nearest_visible").and_then(Value::as_str) {
        text.push_str(&format!("\n  nearest visible: {visible}"));
    }
    if let Some(deepest) = value.get("deepest_existing").and_then(Value::as_str) {
        text.push_str(&format!("\n  deepest existing: {deepest}"));
        if let Some(children) = value.get("children").and_then(Value::as_array) {
            let children: Vec<_> = children.iter().filter_map(Value::as_str).collect();
            text.push_str(&format!("\n  its children: {}", children.join(", ")));
        }
    }
    if let Some(matches) = value.get("did_you_mean").and_then(Value::as_array) {
        for path in matches.iter().filter_map(Value::as_str) {
            text.push_str(&format!("\n  did you mean: {path}"));
        }
    }
    if let Some(hint) = value.get("hint").and_then(Value::as_str) {
        text.push_str(&format!("\n  hint: {hint}"));
    }
}

fn nodes_text(reply: &Value) -> String {
    let nodes = reply
        .get("nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut lines: Vec<String> = nodes
        .iter()
        .map(|node| {
            let mut line = node["path"].as_str().unwrap_or_default().to_owned();
            if let Some(label) = node.get("label").and_then(Value::as_str) {
                line.push_str(&format!("  “{label}”"));
            }
            if let Some(text) = node.get("text").and_then(Value::as_str) {
                line.push_str(&format!("  text={}", Value::from(text)));
            }
            if let Some(bounds) = node.get("bounds").and_then(Value::as_array) {
                let n = |i: usize| bounds.get(i).and_then(Value::as_f64).unwrap_or(0.);
                line.push_str(&format!("  @{},{} {}x{}", n(0), n(1), n(2), n(3)));
            }
            if node.get("visible").and_then(Value::as_bool) == Some(false) {
                line.push_str("  (hidden)");
            }
            line
        })
        .collect();
    let count = nodes.len();
    let total = reply
        .get("total")
        .and_then(Value::as_u64)
        .unwrap_or(count as u64);
    lines.push(if total as usize > count {
        format!("({count} of {total}; narrow with a prefix, --depth or --limit)")
    } else {
        format!("({count})")
    });
    if let Some(duplicates) = reply.get("duplicates").and_then(Value::as_array) {
        lines.push(format!(
            "warning: paths drawn more than once: {}",
            duplicates
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    lines.join("\n")
}

fn highlight_text(reply: &Value) -> String {
    let mut text = String::new();
    for result in reply
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let status = result.get("status").and_then(Value::as_str).unwrap_or("?");
        let id = result
            .get("id")
            .and_then(Value::as_u64)
            .map_or_else(|| "-".to_owned(), |id| format!("#{id}"));
        let target = result
            .get("target")
            .or_else(|| result.get("find"))
            .map(Value::to_string)
            .unwrap_or_default();
        let target = match result.get("error").and_then(Value::as_str) {
            Some(error) => format!("{target} — {error}"),
            None => target,
        };
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("{id} {status} {target}"));
        let mut more = String::new();
        details(result, &mut more);
        text.push_str(&more);
    }
    if let Some(hint) = reply.get("hint").and_then(Value::as_str) {
        text.push_str(&format!("\nhint: {hint}"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    fn parsed(text: &str) -> Value {
        parse(&args(text)).unwrap()
    }

    #[test]
    fn commands() {
        assert_eq!(parsed(""), json!({"cmd": "help"}));
        assert_eq!(parsed("--help"), json!({"cmd": "help"}));
        assert_eq!(
            parsed("list workspace/left-dock --visible --depth 2"),
            json!({"cmd": "list", "prefix": "workspace/left-dock", "visible": true, "depth": 2})
        );
        assert_eq!(
            parsed("find Commit and Push --window 7"),
            json!({"cmd": "find", "query": "Commit and Push", "window": 7})
        );
        assert_eq!(parsed("clear 3 #4"), json!({"cmd": "clear", "ids": [3, 4]}));
        assert_eq!(
            parsed("wait-pin --timeout 30 --pick"),
            json!({"cmd": "wait_pin", "timeout": 30.0, "pick": true})
        );
        assert_eq!(
            parsed("pins --clear"),
            json!({"cmd": "pins", "clear": true})
        );
        assert_eq!(
            parsed("windows --format json"),
            json!({"cmd": "windows", "output": "json"})
        );
        assert!(
            parse(&args("frobnicate"))
                .unwrap_err()
                .contains("unknown command")
        );
        assert!(
            parse(&args("list --depth x"))
                .unwrap_err()
                .contains("number")
        );
        assert!(parse(&args("find")).is_err());
    }

    #[test]
    fn guided_highlight() {
        let request = parse(&[
            "highlight".into(),
            "--clear".into(),
            "workspace/left-dock/dock-tabs/tab:commit".into(),
            "--step".into(),
            "1".into(),
            "--note".into(),
            "Open the Commit panel".into(),
            "--find".into(),
            "Commit".into(),
            "--step".into(),
            "2".into(),
            "src/main.rs:12:5-12:18".into(),
            "--color".into(),
            "warning".into(),
        ])
        .unwrap();
        assert_eq!(
            request,
            json!({
                "cmd": "highlight",
                "clear": true,
                "targets": [
                    {"ref": "workspace/left-dock/dock-tabs/tab:commit", "step": 1, "note": "Open the Commit panel"},
                    {"find": "Commit", "step": 2},
                    {"ref": "src/main.rs:12:5-12:18", "color": "warning"},
                ],
            })
        );
        // Options before every target apply to all of them.
        assert_eq!(
            parsed("highlight --style fill a/b --text Run --nth 2"),
            json!({"cmd": "highlight", "style": "fill",
                   "targets": [{"path": "a/b", "text": "Run", "nth": 2}]})
        );
        assert!(parse(&args("highlight")).is_err());
        assert!(parse(&args("highlight --text x a")).is_err());
        assert!(parse(&args("highlight a --style bold")).is_err());
    }

    #[test]
    fn raw_json() {
        let request = parse(&[
            "--window".into(),
            "2".into(),
            "--json".into(),
            r#"{"cmd":"list","prefix":"workspace"}"#.into(),
        ])
        .unwrap();
        assert_eq!(
            request,
            json!({"cmd": "list", "prefix": "workspace", "window": 2})
        );
        assert!(parse(&["--json".into(), "[]".into()]).is_err());
        assert!(parse(&["--json".into(), "{}".into()]).is_err());
    }

    #[test]
    fn output() {
        let request = json!({"cmd": "list"});
        let reply = json!({"ok": true, "total": 3, "nodes": [
            {"path": "workspace/title-bar/run", "label": "Run", "bounds": [10.0, 4.0, 24.0, 24.0], "visible": true},
            {"path": "workspace/x", "text": "a\"b", "bounds": [0, 0, 1, 1], "visible": false},
        ]});
        let (text, code) = render(&request, &reply);
        assert_eq!(code, 0);
        assert_eq!(
            text,
            "workspace/title-bar/run  “Run”  @10,4 24x24\nworkspace/x  text=\"a\\\"b\"  @0,0 1x1  (hidden)\n(2 of 3; narrow with a prefix, --depth or --limit)"
        );
        let error = json!({"ok": false, "error": "no element has this prefix path",
            "path": "workspace/lft", "did_you_mean": ["workspace/left-dock"]});
        let (text, code) = render(&request, &error);
        assert_eq!(code, 1);
        assert!(text.contains("did you mean: workspace/left-dock"));
        let (text, _) = render(&json!({"cmd": "list", "output": "json"}), &error);
        assert!(text.starts_with('{'));
    }
}
