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
  context [--max-lines N] [--around N]
        what the user is looking at: focused area, active file, cursor,
        selection, lines around the cursor (--around, default 20; 0: none),
        diagnostics, tabs, terminal tail, Problems, Tests, debugger, notebook
        cell, workflow run; texts capped at --max-lines (default 40).
        Also `next_code context` / `next_code_agent context`.

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
        "context" => context(rest)?,
        "wait-pin" | "wait_pin" => wait_pin(rest)?,
        other => {
            return Err(format!(
                "unknown command {other:?}: use list, find, highlight, clear, pick, pins, wait-pin, windows or context"
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

fn context(args: &[&str]) -> Result<Value, String> {
    let mut request = json!({ "cmd": "context" });
    let mut index = 0;
    while index < args.len() {
        match args[index] {
            "--max-lines" => {
                request["max_lines"] = number(value_of(args, index)?, "--max-lines")?.into();
                index += 1;
            }
            "--around" => {
                request["around"] = number(value_of(args, index)?, "--around")?.into();
                index += 1;
            }
            other => return Err(format!("context: unexpected {other:?}")),
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
        Some("context") => context_markdown(reply),
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

/// A `context` reply as compact Markdown for an agent's prompt (the
/// CLI's default output and what Copy Context for Agent copies).
pub fn context_markdown(reply: &Value) -> String {
    let str_of =
        |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let num = |value: &Value, key: &str| value.get(key).and_then(Value::as_u64);
    let flag = |value: &Value, key: &str| value.get(key).and_then(Value::as_bool) == Some(true);
    let items = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let mut out = String::from("# What the user is looking at (NextCode)\n");
    let mut head = Vec::new();
    if let Some(title) = str_of(reply, "title").filter(|t| !t.is_empty()) {
        head.push(format!("window \"{title}\""));
    }
    head.push(format!(
        "focus: {}",
        str_of(reply, "focus").unwrap_or_else(|| "nothing".into())
    ));
    out.push_str(&format!("{}\n", head.join(" · ")));
    if let Some(project) = reply.get("project") {
        let roots: Vec<String> = items(project, "roots")
            .iter()
            .filter_map(|root| str_of(root, "path"))
            .collect();
        let remote = if flag(project, "remote") {
            " (remote)"
        } else {
            ""
        };
        out.push_str(&format!("project: {}{remote}\n", roots.join(", ")));
    }

    if let Some(editor) = reply.get("editor") {
        let file = str_of(editor, "file").unwrap_or_else(|| "untitled".into());
        let language = str_of(editor, "language");
        let fence_language = language.as_deref().unwrap_or("").to_lowercase();
        let mut facts = Vec::new();
        if let Some(language) = &language {
            facts.push(language.clone());
        }
        if flag(editor, "dirty") {
            facts.push("unsaved changes".into());
        }
        if let Some(count) = num(editor, "line_count") {
            facts.push(format!("{count} lines"));
        }
        out.push_str(&format!("\n## Editor: {file} ({})\n", facts.join(", ")));
        let cursors: Vec<String> = items(editor, "cursors").iter().map(point).collect();
        let mut line = format!("cursor {}", cursors.join(", "));
        if let Some([first, last]) = editor
            .get("visible_lines")
            .and_then(Value::as_array)
            .and_then(|lines| <&[Value; 2]>::try_from(lines.as_slice()).ok())
        {
            line.push_str(&format!(" · lines {first}–{last} on screen"));
        }
        out.push_str(&format!("{line}\n"));
        for selection in items(editor, "selections") {
            let range = format!(
                "{}–{}",
                selection.get("start").map(point).unwrap_or_default(),
                selection.get("end").map(point).unwrap_or_default()
            );
            let cut = if flag(&selection, "truncated") {
                ", truncated"
            } else {
                ""
            };
            out.push_str(&format!("\n### Selection {range}{cut}\n"));
            fenced(
                &mut out,
                &fence_language,
                &str_of(&selection, "text").unwrap_or_default(),
            );
        }
        if let Some(around) = editor.get("around") {
            let start = num(around, "start_line").unwrap_or(1);
            let text = str_of(around, "text").unwrap_or_default();
            let numbered: Vec<String> = text
                .lines()
                .enumerate()
                .map(|(index, line)| {
                    format!("{:>4}| {line}", start as usize + index)
                        .trim_end()
                        .to_owned()
                })
                .collect();
            let end = start as usize + numbered.len().max(1) - 1;
            out.push_str(&format!("\n### Around the cursor (lines {start}–{end})\n"));
            fenced(&mut out, &fence_language, &numbered.join("\n"));
        }
        let diagnostics = items(editor, "diagnostics");
        if !diagnostics.is_empty() {
            let total = num(editor, "diagnostics_total").unwrap_or(diagnostics.len() as u64);
            out.push_str(&format!("\n### Diagnostics in this file ({total})\n"));
            for diagnostic in &diagnostics {
                let origin = match (str_of(diagnostic, "source"), str_of(diagnostic, "code")) {
                    (Some(source), Some(code)) => format!(" [{source} {code}]"),
                    (Some(source), None) => format!(" [{source}]"),
                    (None, Some(code)) => format!(" [{code}]"),
                    (None, None) => String::new(),
                };
                let here = if flag(diagnostic, "at_cursor") {
                    " ← at cursor"
                } else {
                    ""
                };
                out.push_str(&format!(
                    "- {}:{} {}{origin}: {}{here}\n",
                    num(diagnostic, "line").unwrap_or(0),
                    num(diagnostic, "column").unwrap_or(0),
                    str_of(diagnostic, "severity").unwrap_or_default(),
                    one_line(&str_of(diagnostic, "message").unwrap_or_default()),
                ));
            }
        }
        push_notes(&mut out, "file", &items(editor, "notes"));
    }

    if let Some(notebook) = reply.get("notebook") {
        let file = str_of(notebook, "file").unwrap_or_default();
        let dirty = if flag(notebook, "dirty") {
            ", unsaved changes"
        } else {
            ""
        };
        out.push_str(&format!(
            "\n## Notebook: {file} ({} cells{dirty})\n",
            num(notebook, "cells").unwrap_or(0)
        ));
        if let Some(cell) = notebook.get("cell") {
            let kind = str_of(cell, "kind").unwrap_or_default();
            out.push_str(&format!(
                "\n### Cell {} ({kind}, {})\n",
                num(cell, "index").unwrap_or(0),
                str_of(cell, "run").unwrap_or_default()
            ));
            let language = if kind == "code" { "python" } else { "" };
            fenced(
                &mut out,
                language,
                &str_of(cell, "source").unwrap_or_default(),
            );
            if let Some(error) = str_of(cell, "error").or_else(|| str_of(cell, "marimo_error")) {
                out.push_str(&format!("error: {}\n", one_line(&error)));
            }
            for output in items(cell, "outputs") {
                out.push_str(&format!(
                    "output ({}):\n",
                    str_of(&output, "type").unwrap_or_default()
                ));
                fenced(&mut out, "", &str_of(&output, "text").unwrap_or_default());
            }
        }
        push_notes(&mut out, "notebook", &items(notebook, "notes"));
    }

    if let Some(workflow) = reply.get("workflow") {
        let dirty = if flag(workflow, "dirty") {
            ", unsaved changes"
        } else {
            ""
        };
        out.push_str(&format!(
            "\n## Workflow: {} ({}{dirty}) — {}\n",
            str_of(workflow, "name").unwrap_or_default(),
            str_of(workflow, "file").unwrap_or_default(),
            str_of(workflow, "outcome").unwrap_or_default()
        ));
        for problem in items(workflow, "problems") {
            let place = match (num(&problem, "line"), num(&problem, "column")) {
                (Some(line), Some(column)) => format!(" (line {line}, column {column})"),
                _ => String::new(),
            };
            out.push_str(&format!(
                "- problem{place}: {}\n",
                one_line(&str_of(&problem, "message").unwrap_or_default())
            ));
        }
        for step in items(workflow, "steps") {
            let status = str_of(&step, "status").unwrap_or_default();
            let mut line = format!("- {}: ", str_of(&step, "id").unwrap_or_default());
            if let Some(error) = str_of(&step, "error").or_else(|| str_of(&step, "reason")) {
                let error = one_line(&error);
                // "failed (exit code 3)" already says the status.
                if error.starts_with(&status) {
                    line.push_str(&error);
                } else {
                    line.push_str(&format!("{status} — {error}"));
                }
            } else if let Some(detail) = str_of(&step, "detail") {
                line.push_str(&format!("{status} ({})", one_line(&detail)));
            } else {
                line.push_str(&status);
            }
            out.push_str(&format!("{line}\n"));
            if let Some(lines) = str_of(&step, "last_lines") {
                fenced(&mut out, "", &lines);
            }
        }
    }

    let tabs = items(reply, "tabs");
    if !tabs.is_empty() {
        out.push_str("\n## Open tabs\n");
        for tab in &tabs {
            let mut marks = Vec::new();
            let label = match str_of(tab, "kind").filter(|kind| kind != "editor") {
                Some(kind) => {
                    marks.push(kind);
                    str_of(tab, "title").unwrap_or_default()
                }
                None => str_of(tab, "file").unwrap_or_default(),
            };
            if flag(tab, "active") {
                marks.push("active".into());
            }
            if flag(tab, "dirty") {
                marks.push("unsaved".into());
            }
            if marks.is_empty() {
                out.push_str(&format!("- {label}\n"));
            } else {
                out.push_str(&format!("- {label} ({})\n", marks.join(", ")));
            }
        }
    }
    let panels: Vec<String> = items(reply, "open_panels")
        .iter()
        .filter_map(|panel| panel.as_str().map(str::to_owned))
        .collect();
    if !panels.is_empty() {
        out.push_str(&format!("open panels: {}\n", panels.join(", ")));
    }

    if let Some(terminal) = reply.get("terminal") {
        let state = match (flag(terminal, "running"), num(terminal, "exit_code")) {
            (true, _) => String::new(),
            (false, Some(code)) => format!(" (exited {code})"),
            (false, None) => match terminal.get("exit_code") {
                Some(code) => format!(" (exited {code})"),
                None => " (ended)".into(),
            },
        };
        let cut = if flag(terminal, "truncated") {
            ", last lines"
        } else {
            ""
        };
        out.push_str(&format!(
            "\n## Terminal: {}{state}{cut}\n",
            str_of(terminal, "run_name")
                .or_else(|| str_of(terminal, "title"))
                .unwrap_or_default()
        ));
        let mut facts = Vec::new();
        if let Some(cwd) = str_of(terminal, "cwd") {
            facts.push(format!("cwd: {cwd}"));
        }
        if let Some(code) = terminal
            .get("last_command_exit_code")
            .and_then(Value::as_i64)
        {
            facts.push(format!("last command exited {code}"));
        }
        if !facts.is_empty() {
            out.push_str(&format!("{}\n", facts.join(" · ")));
        }
        fenced(&mut out, "", &str_of(terminal, "lines").unwrap_or_default());
    }

    if let Some(problems) = reply.get("problems") {
        let (errors, warnings) = (
            num(problems, "errors").unwrap_or(0),
            num(problems, "warnings").unwrap_or(0),
        );
        let list = items(problems, "items");
        if errors + warnings > 0 || !list.is_empty() {
            out.push_str(&format!(
                "\n## Problems: {errors} errors, {warnings} warnings\n"
            ));
            for problem in &list {
                out.push_str(&format!(
                    "- {}:{}:{} {}: {}\n",
                    str_of(problem, "file").unwrap_or_default(),
                    num(problem, "line").unwrap_or(0),
                    num(problem, "column").unwrap_or(0),
                    str_of(problem, "severity").unwrap_or_default(),
                    one_line(&str_of(problem, "message").unwrap_or_default()),
                ));
            }
            let total = num(problems, "total").unwrap_or(0) as usize;
            if total > list.len() {
                out.push_str(&format!("- … {} more\n", total - list.len()));
            }
        }
    }

    if let Some(tests) = reply.get("tests") {
        let running = if flag(tests, "running") {
            " (running)"
        } else {
            ""
        };
        out.push_str(&format!(
            "\n## Tests{running}: {} passed, {} failed, {} skipped\n",
            num(tests, "passed").unwrap_or(0),
            num(tests, "failed").unwrap_or(0),
            num(tests, "skipped").unwrap_or(0)
        ));
        if let Some(status) = str_of(tests, "status") {
            out.push_str(&format!("{status}\n"));
        }
        for failure in items(tests, "failures") {
            let location = match (str_of(&failure, "file"), num(&failure, "line")) {
                (Some(file), Some(line)) => format!(" ({file}:{line})"),
                (Some(file), None) => format!(" ({file})"),
                _ => String::new(),
            };
            let message = str_of(&failure, "message")
                .map(|m| format!(": {}", one_line(&m)))
                .unwrap_or_default();
            out.push_str(&format!(
                "- {} {}{location}{message}\n",
                str_of(&failure, "status").unwrap_or_default(),
                str_of(&failure, "id").unwrap_or_default()
            ));
            if let Some(output) = str_of(&failure, "output_tail") {
                fenced(&mut out, "", &output);
            }
        }
    }

    if let Some(debugger) = reply.get("debugger") {
        let mut title = format!(
            "\n## Debugger: {} — {}",
            str_of(debugger, "name").unwrap_or_default(),
            str_of(debugger, "state").unwrap_or_default()
        );
        if let Some(reason) = str_of(debugger, "stop_reason") {
            title.push_str(&format!(" ({reason})"));
        }
        if let Some(location) = debugger.get("location") {
            title.push_str(&format!(
                " at {}:{}",
                str_of(location, "file").unwrap_or_default(),
                num(location, "line").unwrap_or(0)
            ));
        }
        out.push_str(&format!("{title}\n"));
        if let Some(code) = debugger.get("exit_code").and_then(Value::as_i64) {
            out.push_str(&format!("exit code: {code}\n"));
        }
        if let Some(exception) = str_of(debugger, "exception") {
            out.push_str(&format!("exception: {}\n", one_line(&exception)));
        }
        if let Some(error) = str_of(debugger, "error") {
            out.push_str(&format!("error: {}\n", one_line(&error)));
        }
        if let Some(tail) = str_of(debugger, "output_tail") {
            out.push_str("output (last lines):\n");
            fenced(&mut out, "", &tail);
        }
        for frame in items(debugger, "frames") {
            let mark = if flag(&frame, "selected") { "▶ " } else { "" };
            out.push_str(&format!(
                "- {mark}{} {}:{}\n",
                str_of(&frame, "name").unwrap_or_default(),
                str_of(&frame, "file").unwrap_or_else(|| "?".into()),
                num(&frame, "line").unwrap_or(0)
            ));
        }
        let locals = items(debugger, "locals");
        if !locals.is_empty() {
            out.push_str(&format!(
                "{}:\n",
                str_of(debugger, "scope").unwrap_or_else(|| "Locals".into())
            ));
            for local in &locals {
                let kind = str_of(local, "type").filter(|t| !t.is_empty());
                let kind = kind.map(|t| format!(": {t}")).unwrap_or_default();
                out.push_str(&format!(
                    "  - {}{kind} = {}\n",
                    str_of(local, "name").unwrap_or_default(),
                    str_of(local, "value").unwrap_or_default()
                ));
            }
        }
    }
    out.trim_end().to_owned()
}

/// A `notes` list as `### Notes in this <place>` lines (`cell` when set).
fn push_notes(out: &mut String, place: &str, notes: &[Value]) {
    if notes.is_empty() {
        return;
    }
    fn text<'a>(note: &'a Value, key: &str) -> &'a str {
        note.get(key).and_then(Value::as_str).unwrap_or_default()
    }
    let num = |note: &Value, key: &str| note.get(key).and_then(Value::as_u64);
    out.push_str(&format!("\n### Notes in this {place} ({})\n", notes.len()));
    for note in notes {
        let to = note.get("to").and_then(Value::as_str);
        let to = to.map(|to| format!(" → {to}")).unwrap_or_default();
        let replies = num(note, "replies").filter(|n| *n > 0);
        let replies = replies
            .map(|n| format!(" (+{n} replies)"))
            .unwrap_or_default();
        let cell = num(note, "cell")
            .map(|c| format!("cell {c} "))
            .unwrap_or_default();
        out.push_str(&format!(
            "- {} {cell}line {} [{}{to}] {}: {}{replies}\n",
            text(note, "id"),
            num(note, "line").unwrap_or(0),
            text(note, "status"),
            text(note, "author"),
            one_line(text(note, "text")),
        ));
    }
}

/// `{"line", "column"}` as `line:column`.
fn point(value: &Value) -> String {
    let get = |key| value.get(key).and_then(Value::as_u64).unwrap_or(0);
    format!("{}:{}", get("line"), get("column"))
}

fn one_line(text: &str) -> String {
    let mut line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 300 {
        line = line.chars().take(299).collect::<String>() + "…";
    }
    line
}

/// Appends `text` in a code fence longer than any backtick run inside it.
fn fenced(out: &mut String, language: &str, text: &str) {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(longest.max(2) + 1);
    out.push_str(&format!("{fence}{language}\n{text}\n{fence}\n"));
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
    fn context_options() {
        assert_eq!(
            parsed("context --max-lines 80 --around 0 --format json"),
            json!({"cmd": "context", "max_lines": 80, "around": 0, "output": "json"})
        );
        assert!(
            parse(&args("context --frob"))
                .unwrap_err()
                .contains("unexpected")
        );
    }

    #[test]
    fn context_reads_as_markdown() {
        let reply = json!({
            "ok": true, "title": "app", "focus": "panel:terminal",
            "project": {"roots": [{"name": "app", "path": "/home/me/app"}], "remote": true},
            "editor": {
                "file": "/home/me/app/main.py", "language": "Python", "dirty": true, "line_count": 9,
                "cursors": [{"line": 3, "column": 5}], "visible_lines": [1, 9],
                "selections": [{"start": {"line": 2, "column": 1}, "end": {"line": 3, "column": 5},
                                "text": "a = 1\nb = ```", "truncated": false}],
                "around": {"start_line": 2, "text": "a = 1\nb = 2"},
                "diagnostics": [{"line": 3, "column": 1, "severity": "error", "source": "pyright",
                                 "message": "bad\nthing", "at_cursor": true}],
                "diagnostics_total": 1
            },
            "tabs": [{"file": "/home/me/app/main.py", "kind": "editor", "dirty": true, "active": true},
                     {"title": "Compare main", "kind": "compare"}],
            "terminal": {"title": "bash", "lines": "$ false", "running": false, "exit_code": 1},
            "problems": {"errors": 1, "warnings": 0, "total": 1,
                         "items": [{"file": "/home/me/app/main.py", "line": 3, "column": 1,
                                    "severity": "error", "message": "bad"}]},
            "tests": {"passed": 2, "failed": 1, "skipped": 0, "running": false,
                      "failures": [{"id": "t.py::test_x", "file": "t.py", "line": 4,
                                    "status": "failed", "message": "assert 1 == 2"}]}
        });
        let text = context_markdown(&reply);
        for expected in [
            "window \"app\" · focus: panel:terminal",
            "project: /home/me/app (remote)",
            "## Editor: /home/me/app/main.py (Python, unsaved changes, 9 lines)",
            "cursor 3:5 · lines 1–9 on screen",
            "### Selection 2:1–3:5\n````python\na = 1\nb = ```\n````",
            "### Around the cursor (lines 2–3)\n```python\n   2| a = 1\n   3| b = 2\n```",
            "- 3:1 error [pyright]: bad thing ← at cursor",
            "- /home/me/app/main.py (active, unsaved)\n- Compare main (compare)",
            "## Terminal: bash (exited 1)\n```\n$ false\n```",
            "## Problems: 1 errors, 0 warnings\n- /home/me/app/main.py:3:1 error: bad",
            "## Tests: 2 passed, 1 failed, 0 skipped\n- failed t.py::test_x (t.py:4): assert 1 == 2",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
        let (rendered, code) = render(&json!({"cmd": "context"}), &reply);
        assert_eq!((rendered, code), (text, 0));
    }

    #[test]
    fn context_markdown_shows_cell_notes_and_the_shell_directory() {
        let reply = json!({
            "focus": "notebook",
            "notebook": {"file": "a.ipynb", "cells": 3, "notes": [
                {"id": "n1", "cell": 2, "line": 1, "status": "open", "to": "agent",
                 "author": "me", "text": "why\nslow?", "replies": 1}]},
            "terminal": {"title": "bash", "lines": "$ false", "running": true,
                         "cwd": "/work/sub", "last_command_exit_code": 1}
        });
        let text = context_markdown(&reply);
        for expected in [
            "### Notes in this notebook (1)\n- n1 cell 2 line 1 [open → agent] me: why slow? (+1 replies)",
            "## Terminal: bash\ncwd: /work/sub · last command exited 1\n```\n$ false\n```",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
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
