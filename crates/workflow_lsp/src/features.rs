//! The server's answers on a document's text and byte offsets (the
//! protocol's positions are converted in `lib.rs`): diagnostics,
//! completion, hover, definition, references, rename and symbols.

use std::ops::Range;

use workflow::yaml::{Data, Node};

use crate::{
    analysis::{Analysis, Occurrence, Place, Target},
    context::{At, context, word_at},
    docs,
};

/// A problem: where and what.
pub fn diagnostics(text: &str) -> Vec<(Range<usize>, String)> {
    match workflow::check(text, "workflow") {
        Ok(_) => Vec::new(),
        Err(problems) => problems
            .into_iter()
            .map(|problem| {
                let start = problem.offset.unwrap_or(0).min(text.len());
                (start..token_end(text, start), problem.message)
            })
            .collect(),
    }
}

/// The end of the token starting at `start` (a word, or a quoted string),
/// at least one character on, within its line.
fn token_end(text: &str, start: usize) -> usize {
    let rest = &text[start..];
    let line_end = rest.find('\n').unwrap_or(rest.len());
    let end = match rest.chars().next() {
        Some(quote @ ('\'' | '"')) => rest[1..line_end].find(quote).map_or(line_end, |at| at + 2),
        _ => rest[..line_end]
            .find(|c: char| c.is_whitespace() || matches!(c, ',' | '}' | ']') || c == ':')
            .unwrap_or(line_end),
    };
    let end = if end == 0 {
        rest.chars()
            .next()
            .map_or(0, char::len_utf8)
            .min(line_end.max(1))
    } else {
        end
    };
    start + end.min(rest.len())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Key,
    Value,
    Step,
    Input,
    Variable,
}

/// One completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub label: String,
    pub kind: Kind,
    pub detail: Option<String>,
    /// Markdown.
    pub documentation: Option<String>,
    /// What replaces `replace` (a snippet when `snippet`).
    pub insert: String,
    pub snippet: bool,
    pub replace: Range<usize>,
}

fn item(label: &str, kind: Kind, insert: String, replace: &Range<usize>) -> Item {
    Item {
        label: label.to_owned(),
        kind,
        detail: None,
        documentation: None,
        insert,
        snippet: false,
        replace: replace.clone(),
    }
}

/// The keys a mapping at `place` takes.
fn keys_of(
    analysis: &Analysis,
    place: &Place,
    offset: usize,
    path: &[crate::context::Seg],
) -> Vec<&'static str> {
    let present = analysis.present_keys(path, offset);
    let keys: Vec<&'static str> = match place {
        Place::Top => workflow::TOP_KEYS
            .iter()
            .copied()
            .filter(|key| *key != "$schema")
            .collect(),
        Place::Step(chain) if analysis.is_group(chain) => workflow::GROUP_KEYS.to_vec(),
        Place::Step(_) => workflow::STEP_KEYS.to_vec(),
        Place::Ready(_) if present.is_empty() => vec!["exit", "output", "port", "manual", "delay"],
        Place::Project(_) => vec!["local", "wsl", "path", "devcontainer", "config"],
        Place::Folder(_) => vec!["local", "wsl", "path"],
        Place::Dependency(_) => vec!["condition"],
        Place::Artifact(_) => vec!["label", "path", "url"],
        Place::Option(_) => vec!["value", "label"],
        Place::Input(id) => {
            let kind = analysis
                .input_node(id)
                .and_then(|node| node.get("type"))
                .and_then(Node::text)
                .unwrap_or("string");
            workflow::inputs::INPUT_KEYS
                .iter()
                .copied()
                .filter(|key| match *key {
                    "options" => kind == "choice",
                    "min" | "max" | "integer" => kind == "number",
                    "required" | "placeholder" => matches!(kind, "string" | "number"),
                    _ => true,
                })
                .collect()
        }
        _ => Vec::new(),
    };
    keys.into_iter()
        .filter(|key| !present.iter().any(|known| known == key) && !docs::hidden(place, key))
        .collect()
}

/// A step's summary for hovers and completion docs.
fn step_summary(analysis: &Analysis, chain: &[String]) -> String {
    let id = chain.last().map_or("", String::as_str);
    let Some(node) = analysis.step_node(chain) else {
        return format!("**{id}**");
    };
    let text = |key: &str| node.get(key).and_then(Node::text).map(str::to_owned);
    let mut lines = vec![match text("title") {
        Some(title) => format!("**{id}**: {title}"),
        None => format!("**{id}**"),
    }];
    if let Some(steps) = node.get("steps").and_then(Node::as_map) {
        let ids: Vec<&str> = steps.iter().map(|entry| entry.key.as_str()).collect();
        lines.push(format!("A group of {}: {}", ids.len(), ids.join(", ")));
    }
    for key in ["check", "run"] {
        if let Some(command) = text(key) {
            let first = command.lines().next().unwrap_or_default();
            lines.push(format!("`{key}`: `{first}`"));
        }
    }
    if let Some(ready) = node.get("ready").and_then(Node::as_map)
        && let Some(entry) = ready.first()
    {
        let value = entry.value.text().unwrap_or("");
        lines.push(format!("ready: {} {value}", entry.key));
    }
    let span = analysis
        .chain(chain)
        .and_then(|spans| spans.last().copied());
    if let Some(span) = span
        && !span.depends_on.is_empty()
    {
        let ids: Vec<&str> = span.depends_on.iter().map(|(id, _)| id.as_str()).collect();
        lines.push(format!("depends on: {}", ids.join(", ")));
    }
    lines.join("\n\n")
}

/// An input's summary.
fn input_summary(analysis: &Analysis, id: &str) -> String {
    let node = analysis.input_node(id);
    let text = |key: &str| {
        node.and_then(|node| node.get(key))
            .and_then(Node::text)
            .map(str::to_owned)
    };
    let mut lines = vec![match text("label") {
        Some(label) => format!("**{id}** (input): {label}"),
        None => format!("**{id}** (input)"),
    }];
    lines.push(format!(
        "type: {}",
        text("type").unwrap_or_else(|| "string".into())
    ));
    if let Some(default) = text("default") {
        lines.push(format!("default: `{default}`"));
    }
    if let Some(description) = text("description") {
        lines.push(description);
    }
    lines.join("\n\n")
}

/// Completions at `offset`.
pub fn complete(text: &str, offset: usize) -> Vec<Item> {
    let analysis = Analysis::new(text);
    let context = context(text, offset);
    let place = Place::of(&context.path);
    let replace = context.word.start..offset.max(context.word.start);
    let step_items = |ids: Vec<(String, Vec<String>)>, suffix: &str| -> Vec<Item> {
        ids.into_iter()
            .map(|(id, chain)| Item {
                documentation: Some(step_summary(&analysis, &chain)),
                ..item(&id, Kind::Step, format!("{id}{suffix}"), &replace)
            })
            .collect()
    };
    // Siblings a step at `chain` may depend on, minus itself and those
    // listed already.
    let siblings = |chain: &[String]| -> Vec<(String, Vec<String>)> {
        let listed: Vec<String> = analysis
            .chain(chain)
            .and_then(|spans| spans.last().map(|span| span.depends_on.clone()))
            .unwrap_or_default()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let typed = &text[replace.clone()];
        analysis
            .siblings(chain)
            .iter()
            .filter(|span| {
                Some(&span.id) != chain.last() && (!listed.contains(&span.id) || span.id == typed)
            })
            .map(|span| {
                let mut target = chain[..chain.len().saturating_sub(1)].to_vec();
                target.push(span.id.clone());
                (span.id.clone(), target)
            })
            .collect()
    };
    let values = |values: &[(&str, &str)]| -> Vec<Item> {
        values
            .iter()
            .map(|(value, doc)| Item {
                documentation: (!doc.is_empty()).then(|| (*doc).to_owned()),
                ..item(value, Kind::Value, (*value).to_owned(), &replace)
            })
            .collect()
    };
    let durations = || {
        values(&[
            ("30s", "30 seconds"),
            ("2m", "2 minutes"),
            ("10m", "10 minutes"),
            ("1h", "an hour"),
            ("500ms", "half a second"),
        ])
    };
    let conditions = || {
        values(&[
            ("ready", docs::condition("ready")),
            ("completed", docs::condition("completed")),
        ])
    };
    let booleans = || values(&[("true", ""), ("false", "")]);
    match (&context.at, &place) {
        (At::Key, Place::Steps(_) | Place::Inputs | Place::Env | Place::Other) => Vec::new(),
        (At::Key, Place::DependsOn(chain)) => step_items(siblings(chain), ": ready"),
        (At::Key, place) => keys_of(&analysis, place, offset, &context.path)
            .into_iter()
            .map(|key| Item {
                documentation: docs::key(place, key),
                ..item(key, Kind::Key, format!("{key}: "), &replace)
            })
            .collect(),
        (At::Value(key), Place::Step(chain)) => match key.as_str() {
            "depends_on" => step_items(siblings(chain), ""),
            "open_urls" => booleans(),
            "timeout" | "stop_grace_period" => durations(),
            "ready" => snippets(
                &[
                    (
                        "{port: …}",
                        "{port: ${1:8000}}",
                        "Something listens on this port.",
                    ),
                    (
                        "{output: …}",
                        "{output: '${1:ready}'}",
                        "A regex the output matches.",
                    ),
                    (
                        "{manual: …}",
                        "{manual: '${1:Continue when …}'}",
                        "Someone presses Continue.",
                    ),
                    (
                        "{exit: true}",
                        "{exit: true}",
                        "The command exits with code 0.",
                    ),
                    (
                        "{delay: …}",
                        "{delay: ${1:5s}}",
                        "This long after it started.",
                    ),
                ],
                &replace,
            ),
            "project" => snippets(
                &[
                    (
                        "{local: …}",
                        "{local: ${1:C:/src/app}}",
                        "A folder on this machine.",
                    ),
                    (
                        "{wsl: …, path: …}",
                        "{wsl: ${1:Ubuntu}, path: ${2:~/src/app}}",
                        "A folder in WSL.",
                    ),
                    (
                        "{devcontainer: …}",
                        "{devcontainer: ${1:C:/src/app}}",
                        "A folder in its dev container.",
                    ),
                ],
                &replace,
            ),
            _ => Vec::new(),
        },
        (At::Value(_), Place::DependsOn(_)) => conditions(),
        (At::Value(key), Place::Dependency(_)) if key == "condition" => conditions(),
        (At::Value(key), Place::Ready(_)) => match key.as_str() {
            "exit" => values(&[("true", "")]),
            "delay" => durations(),
            _ => Vec::new(),
        },
        (At::Value(key), Place::Input(id)) => match key.as_str() {
            "type" => values(&[
                ("string", "A textbox (the default)."),
                ("number", "A textbox checked as a number."),
                ("boolean", "A checkbox."),
                ("choice", "A dropdown of `options`."),
            ]),
            "required" | "integer" => booleans(),
            "default" => {
                let node = analysis.input_node(id);
                let kind = node
                    .and_then(|node| node.get("type"))
                    .and_then(Node::text)
                    .unwrap_or("string");
                match kind {
                    "boolean" => booleans(),
                    "choice" => node
                        .and_then(|node| node.get("options"))
                        .and_then(Node::as_list)
                        .map(|options| {
                            options
                                .iter()
                                .filter_map(|option| {
                                    option.text().or_else(|| option.get("value")?.text())
                                })
                                .map(|value| item(value, Kind::Value, value.to_owned(), &replace))
                                .collect()
                        })
                        .unwrap_or_default(),
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        },
        (At::Item, Place::DependsOn(chain)) => step_items(siblings(chain), ""),
        (At::Variable(typed), _) => variables(&analysis, text, offset, typed, &replace),
        _ => Vec::new(),
    }
}

fn snippets(choices: &[(&str, &str, &str)], replace: &Range<usize>) -> Vec<Item> {
    choices
        .iter()
        .map(|(label, body, doc)| Item {
            documentation: Some((*doc).to_owned()),
            snippet: true,
            ..item(label, Kind::Value, (*body).to_owned(), replace)
        })
        .collect()
}

/// Completions inside `${…}` (`typed` is what follows `${`).
fn variables(
    analysis: &Analysis,
    text: &str,
    offset: usize,
    typed: &str,
    replace: &Range<usize>,
) -> Vec<Item> {
    let closed = text[offset..]
        .trim_start_matches(|c: char| c.is_alphanumeric() || matches!(c, '_' | '-'))
        .starts_with('}');
    let close = if closed { "" } else { "}" };
    let chain = analysis.step_chain_at(offset);
    let simple = |label: &str, insert: String, doc: &str, kind: Kind| Item {
        documentation: Some(doc.to_owned()),
        ..item(label, kind, insert, replace)
    };
    let parts: Vec<&str> = typed.split('.').collect();
    match parts.as_slice() {
        [_] if typed.starts_with("env:") => Vec::new(),
        [_] => vec![
            simple(
                "steps",
                "steps.".into(),
                "What a step it depends on left: `${steps.<id>.status}`, `.exit_code`, `.artifacts.<label>`.",
                Kind::Variable,
            ),
            simple(
                "inputs",
                "inputs.".into(),
                "A value the run was given: `${inputs.<id>}`.",
                Kind::Variable,
            ),
            simple(
                "env",
                "env:".into(),
                "An environment variable where the step runs: `${env:NAME}`.",
                Kind::Variable,
            ),
            simple(
                "workspaceFolder",
                format!("workspaceFolder{close}"),
                "The project's root (the home folder without a project).",
                Kind::Variable,
            ),
            simple(
                "userHome",
                format!("userHome{close}"),
                "The home folder where the step runs.",
                Kind::Variable,
            ),
        ],
        ["steps", _] => analysis
            .scope(&chain)
            .into_iter()
            .map(|(id, target)| Item {
                documentation: Some(step_summary(analysis, &target)),
                ..item(&id, Kind::Step, format!("{id}."), replace)
            })
            .collect(),
        ["steps", id, _] => {
            let group = analysis
                .scope(&chain)
                .into_iter()
                .find(|(scoped, _)| scoped == id)
                .map(|(_, target)| analysis.is_group(&target));
            match group {
                None => Vec::new(),
                Some(true) => vec![simple(
                    "status",
                    format!("status{close}"),
                    "The group's state: ready, failed or stopped.",
                    Kind::Variable,
                )],
                Some(false) => vec![
                    simple(
                        "status",
                        format!("status{close}"),
                        "`ready`, `failed` or `stopped`.",
                        Kind::Variable,
                    ),
                    simple(
                        "exit_code",
                        format!("exit_code{close}"),
                        "The command's exit code; empty without one.",
                        Kind::Variable,
                    ),
                    simple(
                        "artifacts",
                        "artifacts.".into(),
                        "An artifact's resolved path or URL, by label or index.",
                        Kind::Variable,
                    ),
                ],
            }
        }
        ["steps", id, "artifacts", _] => {
            let Some((_, target)) = analysis
                .scope(&chain)
                .into_iter()
                .find(|(scoped, _)| scoped == id)
            else {
                return Vec::new();
            };
            analysis
                .artifact_labels(&target)
                .into_iter()
                .enumerate()
                .map(|(index, label)| {
                    let name = label.unwrap_or_else(|| index.to_string());
                    simple(
                        &name,
                        format!("{name}{close}"),
                        &format!("Artifact {index} of `{id}`."),
                        Kind::Variable,
                    )
                })
                .collect()
        }
        ["inputs", _] => analysis
            .inputs
            .iter()
            .map(|span| Item {
                documentation: Some(input_summary(analysis, &span.id)),
                ..item(
                    &span.id,
                    Kind::Input,
                    format!("{}{close}", span.id),
                    replace,
                )
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The step or input named at `offset` (a step's or input's key, a
/// dependency, a `${steps.…}` or `${inputs.…}` variable) and the name's
/// range.
fn target_at(analysis: &Analysis, offset: usize) -> Option<(Target, Occurrence)> {
    let text = analysis.text;
    if let Some((variable, range)) = analysis.variable_at(offset) {
        let chain = analysis.step_chain_at(offset);
        let (target, name) = analysis.variable_target(&variable, range.start, &chain)?;
        return (name.start <= offset && offset <= name.end).then_some((target, name));
    }
    let word = word_at(text, offset);
    if word.is_empty() {
        return None;
    }
    let name = text[word.clone()].to_owned();
    let found = context(text, word.end);
    let sibling = |chain: &[String]| {
        let mut target = chain[..chain.len().saturating_sub(1)].to_vec();
        target.push(name.clone());
        Target::Step(target)
    };
    let target = match (&found.at, Place::of(&found.path)) {
        (At::Key, Place::Steps(mut chain)) => {
            chain.push(name.clone());
            Target::Step(chain)
        }
        (At::Key, Place::Inputs) => Target::Input(name.clone()),
        (At::Key, Place::DependsOn(chain)) | (At::Item, Place::DependsOn(chain)) => sibling(&chain),
        (At::Value(key), Place::Step(chain)) if key == "depends_on" => sibling(&chain),
        _ => return None,
    };
    let exists = match &target {
        Target::Step(chain) => analysis.chain(chain).is_some(),
        Target::Input(id) => analysis.inputs.iter().any(|span| span.id == *id),
    };
    exists.then_some((target, word))
}

/// Markdown for what is at `offset`, and the range it is about.
pub fn hover(text: &str, offset: usize) -> Option<(String, Range<usize>)> {
    let analysis = Analysis::new(text);
    if let Some((variable, range)) = analysis.variable_at(offset) {
        let chain = analysis.step_chain_at(offset);
        let about = match analysis.variable_target(&variable, range.start, &chain) {
            Some((Target::Step(target), _)) => {
                let field = variable.splitn(3, '.').nth(2).unwrap_or("");
                let what = match field.split('.').next().unwrap_or("") {
                    "status" => "its status: `ready`, `failed` or `stopped`",
                    "exit_code" => "its command's exit code (empty without one)",
                    "artifacts" => "an artifact's resolved path or URL",
                    _ => "what it left",
                };
                format!(
                    "`${{{variable}}}`: {what}, replaced when this step starts.\n\n{}",
                    step_summary(&analysis, &target)
                )
            }
            Some((Target::Input(id), _)) => {
                format!(
                    "`${{{variable}}}`: the run's value.\n\n{}",
                    input_summary(&analysis, &id)
                )
            }
            None if variable.starts_with("env:") => format!(
                "`${{{variable}}}`: the environment variable `{}` where the step runs, read when it starts.",
                &variable[4..]
            ),
            None => match variable.as_str() {
                "workspaceFolder" => {
                    "`${workspaceFolder}`: the project's root (the home folder without a project)."
                        .to_owned()
                }
                "userHome" => "`${userHome}`: the home folder where the step runs.".to_owned(),
                _ => return None,
            },
        };
        return Some((
            about,
            range.start.saturating_sub(2)..(range.end + 1).min(text.len()),
        ));
    }
    if let Some((target, range)) = target_at(&analysis, offset) {
        let about = match &target {
            Target::Step(chain) => step_summary(&analysis, chain),
            Target::Input(id) => input_summary(&analysis, id),
        };
        return Some((about, range));
    }
    let word = word_at(text, offset);
    if word.is_empty() {
        return None;
    }
    let name = &text[word.clone()];
    let found = context(text, word.end);
    let place = Place::of(&found.path);
    let about = match (&found.at, &place) {
        (At::Key, place) => docs::key(place, name)?,
        (At::Value(_), Place::DependsOn(_)) => docs::condition(name).to_owned(),
        (At::Value(key), Place::Dependency(_)) if key == "condition" => {
            docs::condition(name).to_owned()
        }
        _ => return None,
    };
    Some((about, word))
}

/// Where what is named at `offset` is declared.
pub fn definition(text: &str, offset: usize) -> Option<Range<usize>> {
    let analysis = Analysis::new(text);
    let (target, _) = target_at(&analysis, offset)?;
    analysis.declaration(&target)
}

/// Every mention of what is named at `offset` (its declaration first when
/// asked for).
pub fn references(text: &str, offset: usize, declaration: bool) -> Vec<Range<usize>> {
    let analysis = Analysis::new(text);
    let Some((target, _)) = target_at(&analysis, offset) else {
        return Vec::new();
    };
    let mut found: Vec<Range<usize>> = Vec::new();
    if declaration {
        found.extend(analysis.declaration(&target));
    }
    found.extend(analysis.references(&target));
    found
}

/// The name a rename at `offset` would change, and its range.
pub fn prepare_rename(text: &str, offset: usize) -> Option<(Range<usize>, String)> {
    let analysis = Analysis::new(text);
    let (_, range) = target_at(&analysis, offset)?;
    Some((range.clone(), text[range].to_owned()))
}

/// The edits renaming what is named at `offset` to `new_name`: its key,
/// the `depends_on` entries and variables naming it.
pub fn rename(
    text: &str,
    offset: usize,
    new_name: &str,
) -> Result<Vec<(Range<usize>, String)>, String> {
    let analysis = Analysis::new(text);
    let (target, _) = target_at(&analysis, offset).ok_or("nothing to rename here")?;
    let new_name = new_name.trim();
    match &target {
        Target::Step(chain) => {
            if new_name.is_empty()
                || new_name.contains('/')
                || new_name.contains(char::is_whitespace)
            {
                return Err(format!("`{new_name}` is not a step id (no `/` or spaces)"));
            }
            if analysis
                .siblings(chain)
                .iter()
                .any(|span| span.id == new_name)
            {
                return Err(format!("a step `{new_name}` exists already"));
            }
        }
        Target::Input(_) => {
            if new_name.is_empty()
                || new_name.contains(['/', '.', '{', '}', '$'])
                || new_name.contains(char::is_whitespace)
            {
                return Err(format!(
                    "`{new_name}` is not an input id (no `/`, `.`, `{{`, `}}`, `$` or spaces)"
                ));
            }
            if analysis.inputs.iter().any(|span| span.id == new_name) {
                return Err(format!("an input `{new_name}` exists already"));
            }
        }
    }
    let mut edits: Vec<(Range<usize>, String)> = analysis
        .declaration(&target)
        .into_iter()
        .chain(analysis.references(&target))
        .map(|range| (range, new_name.to_owned()))
        .collect();
    edits.sort_by_key(|(range, _)| range.start);
    edits.dedup_by_key(|(range, _)| range.start);
    Ok(edits)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Step,
    Group,
    Inputs,
    Input,
}

/// An outline entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub detail: Option<String>,
    pub kind: SymbolKind,
    pub range: Range<usize>,
    pub selection: Range<usize>,
    pub children: Vec<Symbol>,
}

/// The inputs, then the steps (groups with their steps).
pub fn symbols(text: &str) -> Vec<Symbol> {
    let analysis = Analysis::new(text);
    let mut symbols = Vec::new();
    if !analysis.inputs.is_empty()
        && let Some(entry) = analysis.root.as_ref().and_then(|root| root.entry("inputs"))
    {
        symbols.push(Symbol {
            name: "inputs".into(),
            detail: None,
            kind: SymbolKind::Inputs,
            range: entry.key_start..entry.end(),
            selection: analysis.name_at(entry.key_start, "inputs"),
            children: analysis
                .inputs
                .iter()
                .map(|span| Symbol {
                    name: span.id.clone(),
                    detail: analysis
                        .input_node(&span.id)
                        .and_then(|node| node.get("type"))
                        .and_then(Node::text)
                        .map(str::to_owned),
                    kind: SymbolKind::Input,
                    range: span.start..span.end.max(span.start + span.id.len()),
                    selection: analysis.name_at(span.start, &span.id),
                    children: Vec::new(),
                })
                .collect(),
        });
    }
    fn steps(
        analysis: &Analysis,
        spans: &[workflow::StepSpan],
        chain: &mut Vec<String>,
    ) -> Vec<Symbol> {
        spans
            .iter()
            .map(|span| {
                chain.push(span.id.clone());
                let node = analysis.step_node(chain);
                let text = |key: &str| node.and_then(|node| node.get(key)).and_then(Node::text);
                let detail = text("title")
                    .or_else(|| text("run"))
                    .or_else(|| text("check"))
                    .map(|detail| detail.lines().next().unwrap_or_default().to_owned());
                let children = steps(analysis, &span.steps, chain);
                chain.pop();
                Symbol {
                    name: span.id.clone(),
                    detail,
                    kind: if node
                        .and_then(|node| node.get("steps"))
                        .is_some_and(|steps| matches!(steps.data, Data::Map(_)))
                    {
                        SymbolKind::Group
                    } else {
                        SymbolKind::Step
                    },
                    range: span.start..span.end.max(span.start + span.id.len()),
                    selection: analysis.name_at(span.start, &span.id),
                    children,
                }
            })
            .collect()
    }
    symbols.extend(steps(&analysis, &analysis.steps, &mut Vec::new()));
    symbols
}

#[cfg(test)]
mod tests;
