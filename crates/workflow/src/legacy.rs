//! The old workflow format (JSON with comments: `steps` and `inputs` as
//! arrays of objects with an `id`, `needs`/`after`, camelCase keys,
//! seconds as numbers) converted to the YAML format, comments and blank
//! lines kept: each step becomes a key of `steps`, `needs` and `after`
//! become `depends_on` (`completed` for `after`), `onFail`/`openUrls`
//! become `on_fail`/`open_urls`, `timeout_s`/`stop_grace_s`/`delay_ms`
//! become durations, and unknown top-level keys (ignored by the old
//! format) become `x-` keys.

use std::time::Duration;

use crate::{
    file::format_duration,
    yaml::{self, Data},
};

/// Whether `path` names a file of the old format (`.json`, `.jsonc`).
pub fn is_legacy(path: &str) -> bool {
    path.ends_with(".json") || path.ends_with(".jsonc")
}

/// The YAML text for an old-format file; an error when it is not JSON.
/// The result is not validated: the caller checks it with
/// [`crate::check`] (a file that was invalid stays invalid).
pub fn convert(text: &str) -> Result<String, String> {
    let mut reader = Reader {
        text,
        bytes: text.as_bytes(),
        at: 0,
    };
    let (header, _) = reader.trivia();
    let root = reader.value()?;
    let (footer, _) = reader.trivia();
    if reader.at < reader.bytes.len() {
        return Err(reader.error("unexpected text after the workflow"));
    }
    let J::Object(members, end) = root else {
        return Err("a workflow is a JSON object".into());
    };
    let mut out = String::new();
    comments(&mut out, &header, 0);
    let entries = workflow(members);
    let mut entries = entries;
    for (index, entry) in entries.iter_mut().enumerate() {
        // A blank line between the sections, as in the template.
        if index > 0 && matches!(entry.key.as_str(), "inputs" | "steps" | "env") {
            entry.blank_before = true;
        }
    }
    emit_entries(&mut out, &entries, 0);
    comments(&mut out, &end, 0);
    comments(&mut out, &footer, 0);
    Ok(out)
}

/// A JSON value with the comments around its members.
#[derive(Debug, Clone)]
enum J {
    /// Members, and the comments after the last one.
    Object(Vec<Member>, Vec<String>),
    Array(Vec<Member>, Vec<String>),
    String(String),
    /// A number, `true`, `false` or `null` as written.
    Literal(String),
}

#[derive(Debug, Clone)]
struct Member {
    /// `None` for an array element.
    key: Option<String>,
    value: J,
    /// Comment lines before it.
    before: Vec<String>,
    blank_before: bool,
    /// A comment on the same line after it.
    trailing: Option<String>,
}

struct Reader<'a> {
    text: &'a str,
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn error(&self, message: &str) -> String {
        let (line, column) = crate::source::line_column(self.text, self.at);
        format!("invalid JSON at line {line}, column {column}: {message}")
    }

    /// Skips blanks and comments; returns the comment lines and whether
    /// an empty line was crossed.
    fn trivia(&mut self) -> (Vec<String>, bool) {
        let mut lines = Vec::new();
        let mut blank = false;
        let mut newlines = 0;
        while self.at < self.bytes.len() {
            match self.bytes[self.at] {
                b'\n' => {
                    newlines += 1;
                    if newlines >= 2 {
                        blank = true;
                    }
                    self.at += 1;
                }
                byte if byte.is_ascii_whitespace() => self.at += 1,
                b'/' if self.bytes.get(self.at + 1) == Some(&b'/') => {
                    let end = self.text[self.at..]
                        .find('\n')
                        .map_or(self.text.len(), |at| self.at + at);
                    lines.push(line_comment(&self.text[self.at + 2..end]));
                    self.at = end;
                    newlines = 0;
                }
                b'/' if self.bytes.get(self.at + 1) == Some(&b'*') => {
                    let end = self.text[self.at + 2..]
                        .find("*/")
                        .map_or(self.text.len(), |at| self.at + 2 + at);
                    lines.extend(block_comment(&self.text[self.at + 2..end]));
                    self.at = (end + 2).min(self.text.len());
                    newlines = 0;
                }
                _ => break,
            }
        }
        (lines, blank)
    }

    /// A comment after a value on the same line (after its comma).
    fn trailing(&mut self) -> Option<String> {
        let mut at = self.at;
        while at < self.bytes.len() && matches!(self.bytes[at], b' ' | b'\t' | b'\r') {
            at += 1;
        }
        let rest = &self.text[at..];
        if let Some(body) = rest.strip_prefix("//") {
            let end = body.find('\n').unwrap_or(body.len());
            self.at = at + 2 + end;
            return Some(line_comment(&body[..end]));
        }
        let body = rest.strip_prefix("/*")?;
        let end = body.find("*/")?;
        if body[..end].contains('\n') {
            return None;
        }
        self.at = at + 2 + end + 2;
        Some(block_comment(&body[..end]).join(" "))
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.bytes.get(self.at) == Some(&byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Result<J, String> {
        match self.bytes.get(self.at) {
            Some(b'{') => self.container(b'}'),
            Some(b'[') => self.container(b']'),
            Some(b'"') => self.string().map(J::String),
            Some(_) => {
                let start = self.at;
                while self.at < self.bytes.len()
                    && !matches!(self.bytes[self.at], b',' | b'}' | b']' | b'/')
                    && !self.bytes[self.at].is_ascii_whitespace()
                {
                    self.at += 1;
                }
                let literal = &self.text[start..self.at];
                let known = matches!(literal, "true" | "false" | "null")
                    || serde_json::from_str::<serde_json::Number>(literal).is_ok();
                if known {
                    Ok(J::Literal(literal.to_owned()))
                } else {
                    self.at = start;
                    Err(self.error(&format!("unexpected `{literal}`")))
                }
            }
            None => Err(self.error("the text ended")),
        }
    }

    fn string(&mut self) -> Result<String, String> {
        let start = self.at;
        self.at += 1;
        while self.at < self.bytes.len() && self.bytes[self.at] != b'"' {
            if self.bytes[self.at] == b'\\' {
                self.at += 1;
            }
            self.at += 1;
        }
        if self.at >= self.bytes.len() {
            self.at = start;
            return Err(self.error("unclosed string"));
        }
        self.at += 1;
        serde_json::from_str(&self.text[start..self.at]).map_err(|err| {
            self.at = start;
            self.error(&err.to_string())
        })
    }

    /// An object or array after its opening bracket.
    fn container(&mut self, close: u8) -> Result<J, String> {
        let object = close == b'}';
        self.at += 1;
        let mut members = Vec::new();
        loop {
            let (before, blank_before) = self.trivia();
            if self.eat(close) {
                return Ok(if object {
                    J::Object(members, before)
                } else {
                    J::Array(members, before)
                });
            }
            let key = if object {
                if self.bytes.get(self.at) != Some(&b'"') {
                    return Err(self.error("expected a key"));
                }
                let key = self.string()?;
                self.trivia();
                if !self.eat(b':') {
                    return Err(self.error("expected `:`"));
                }
                self.trivia();
                Some(key)
            } else {
                None
            };
            let value = self.value()?;
            while matches!(self.bytes.get(self.at), Some(b' ' | b'\t' | b'\r')) {
                self.at += 1;
            }
            let mut comma = self.eat(b',');
            let mut trailing = self.trailing();
            let mut more = Vec::new();
            if !comma {
                // The comma may come after comments on the next lines.
                let start = self.at;
                let (comments, _) = self.trivia();
                if self.eat(b',') {
                    comma = true;
                    more = comments;
                    if trailing.is_none() {
                        trailing = self.trailing();
                    }
                } else {
                    self.at = start;
                }
            }
            // Comments between the value and its comma stay with it.
            let mut member = Member {
                key,
                value,
                before,
                blank_before,
                trailing,
            };
            if !more.is_empty() {
                match &mut member.trailing {
                    Some(trailing) => trailing.push_str(&format!(" {}", more.join(" "))),
                    None => member.trailing = Some(more.join(" ")),
                }
            }
            members.push(member);
            if !comma {
                let (after, _) = self.trivia();
                if !self.eat(close) {
                    return Err(self.error(if object {
                        "expected `,` or `}`"
                    } else {
                        "expected `,` or `]`"
                    }));
                }
                return Ok(if object {
                    J::Object(members, after)
                } else {
                    J::Array(members, after)
                });
            }
        }
    }
}

fn line_comment(body: &str) -> String {
    body.strip_prefix(' ').unwrap_or(body).trim_end().to_owned()
}

fn block_comment(body: &str) -> Vec<String> {
    body.lines()
        .map(|line| {
            let line = line.trim();
            line.strip_prefix("* ")
                .or_else(|| line.strip_prefix('*'))
                .unwrap_or(line)
                .to_owned()
        })
        .skip_while(String::is_empty)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .skip_while(String::is_empty)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// A YAML value to write.
#[derive(Debug, Clone)]
enum Y {
    Map(Vec<Entry>, Vec<String>),
    List(Vec<Item>, Vec<String>),
    /// A string, quoted as needed when written.
    String(String),
    /// A number, boolean or null as written.
    Literal(String),
}

#[derive(Debug, Clone)]
struct Entry {
    key: String,
    value: Y,
    before: Vec<String>,
    blank_before: bool,
    trailing: Option<String>,
}

#[derive(Debug, Clone)]
struct Item {
    value: Y,
    before: Vec<String>,
    trailing: Option<String>,
}

impl Entry {
    fn from(member: Member, key: &str, value: Y) -> Entry {
        Entry {
            key: key.to_owned(),
            value,
            before: member.before,
            blank_before: member.blank_before,
            trailing: member.trailing,
        }
    }
}

/// Any JSON as YAML, keys unchanged.
fn plain_value(value: J) -> Y {
    match value {
        J::Object(members, end) => Y::Map(
            members
                .into_iter()
                .map(|member| {
                    let key = member.key.clone().unwrap_or_default();
                    let value = plain_value(member.value.clone());
                    Entry::from(member, &key, value)
                })
                .collect(),
            end,
        ),
        J::Array(members, end) => Y::List(
            members
                .into_iter()
                .map(|member| Item {
                    value: plain_value(member.value),
                    before: member.before,
                    trailing: member.trailing,
                })
                .collect(),
            end,
        ),
        J::String(text) => Y::String(text),
        J::Literal(text) => Y::Literal(text),
    }
}

/// The top level: `name`, `description` and `inputs` as they are (inputs
/// keyed by id), `steps` converted, other keys as `x-` keys.
fn workflow(members: Vec<Member>) -> Vec<Entry> {
    let mut entries = Vec::new();
    for member in members {
        let key = member.key.clone().unwrap_or_default();
        let value = member.value.clone();
        match (key.as_str(), value) {
            ("$schema", _) => {}
            ("name" | "description", value) => {
                entries.push(Entry::from(member, &key, plain_value(value)));
            }
            ("inputs", J::Array(items, end)) => {
                let inputs = keyed(items, |_, value| plain_value(value));
                entries.push(Entry::from(member, "inputs", Y::Map(inputs, end)));
            }
            ("steps", J::Array(items, end)) => {
                entries.push(Entry::from(member, "steps", Y::Map(steps(items), end)));
            }
            (_, value) => {
                let key = if key.starts_with("x-") {
                    key
                } else {
                    format!("x-{key}")
                };
                entries.push(Entry::from(member, &key, plain_value(value)));
            }
        }
    }
    entries
}

/// Array elements that are objects with an `id`, as entries keyed by it
/// (`convert` makes the rest of each object); elements without one keep
/// a made-up key so nothing is lost.
fn keyed(items: Vec<Member>, convert: impl Fn(&str, J) -> Y) -> Vec<Entry> {
    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            let (id, rest, id_comments) = match item.value {
                J::Object(members, end) => {
                    let mut id = None;
                    let mut id_comments = Vec::new();
                    let mut rest = Vec::with_capacity(members.len());
                    for member in members {
                        match (&member.key, &member.value) {
                            (Some(key), J::String(value)) if key == "id" && id.is_none() => {
                                id = Some(value.clone());
                                id_comments.extend(member.before);
                                id_comments.extend(member.trailing);
                            }
                            _ => rest.push(member),
                        }
                    }
                    (id, J::Object(rest, end), id_comments)
                }
                other => (None, other, Vec::new()),
            };
            let id = id.unwrap_or_else(|| format!("unnamed-{}", index + 1));
            let mut before = item.before;
            before.extend(id_comments);
            Entry {
                value: convert(&id, rest),
                key: id,
                before,
                blank_before: item.blank_before,
                trailing: item.trailing,
            }
        })
        .collect()
}

/// A `steps` array as a `steps` mapping.
fn steps(items: Vec<Member>) -> Vec<Entry> {
    let mut entries = keyed(items, |_, value| match value {
        J::Object(members, end) => Y::Map(step(members), end),
        other => plain_value(other),
    });
    // A blank line around steps written over several lines.
    let mut previous_block = false;
    for (index, entry) in entries.iter_mut().enumerate() {
        let block = !flows(&entry.value, 2 + entry.key.len());
        if index > 0 && (block || previous_block) {
            entry.blank_before = true;
        }
        previous_block = block;
    }
    entries
}

/// One step's members, renamed and reshaped.
fn step(members: Vec<Member>) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    // `needs` and `after` become one `depends_on` where the first was.
    let mut dependencies: Vec<(String, bool)> = Vec::new();
    let mut depends_at: Option<usize> = None;
    let mut depends_comments: (Vec<String>, bool, Vec<String>) = (Vec::new(), false, Vec::new());
    for member in members {
        let key = member.key.clone().unwrap_or_default();
        let value = member.value.clone();
        match (key.as_str(), value) {
            ("needs" | "after", value) => {
                let completed = key == "after";
                let ids: Vec<String> = match value {
                    J::String(id) => vec![id],
                    J::Array(items, end) => {
                        depends_comments.2.extend(end);
                        items
                            .into_iter()
                            .filter_map(|item| {
                                depends_comments.2.extend(item.before);
                                depends_comments.2.extend(item.trailing);
                                match item.value {
                                    J::String(id) => Some(id),
                                    _ => None,
                                }
                            })
                            .collect()
                    }
                    _ => Vec::new(),
                };
                dependencies.extend(ids.into_iter().map(|id| (id, completed)));
                depends_comments.0.extend(member.before);
                depends_comments.1 |= member.blank_before;
                depends_comments.2.extend(member.trailing);
                depends_at.get_or_insert(entries.len());
            }
            ("onFail", value) => entries.push(Entry::from(member, "on_fail", plain_value(value))),
            ("openUrls", value) => {
                entries.push(Entry::from(member, "open_urls", plain_value(value)));
            }
            ("timeout_s", J::Literal(seconds)) => {
                let value = seconds_duration(&seconds, 1.0);
                entries.push(Entry::from(member, "timeout", value));
            }
            ("stop_grace_s", J::Literal(seconds)) => {
                let value = seconds_duration(&seconds, 1.0);
                entries.push(Entry::from(member, "stop_grace_period", value));
            }
            ("ready", J::Object(conditions, end)) => {
                let conditions = conditions
                    .into_iter()
                    .map(|condition| {
                        let key = condition.key.clone().unwrap_or_default();
                        match (key.as_str(), condition.value.clone()) {
                            ("delay_ms", J::Literal(millis)) => {
                                let value = seconds_duration(&millis, 0.001);
                                Entry::from(condition, "delay", value)
                            }
                            (_, value) => Entry::from(condition, &key, plain_value(value)),
                        }
                    })
                    .collect();
                entries.push(Entry::from(member, "ready", Y::Map(conditions, end)));
            }
            ("steps", J::Array(items, end)) => {
                entries.push(Entry::from(member, "steps", Y::Map(steps(items), end)));
            }
            (_, value) => entries.push(Entry::from(member, &key, plain_value(value))),
        }
    }
    if let Some(at) = depends_at {
        let any_completed = dependencies.iter().any(|(_, completed)| *completed);
        let value = if any_completed {
            Y::Map(
                dependencies
                    .into_iter()
                    .map(|(id, completed)| Entry {
                        key: id,
                        value: Y::String(if completed { "completed" } else { "ready" }.into()),
                        before: Vec::new(),
                        blank_before: false,
                        trailing: None,
                    })
                    .collect(),
                Vec::new(),
            )
        } else {
            Y::List(
                dependencies
                    .into_iter()
                    .map(|(id, _)| Item {
                        value: Y::String(id),
                        before: Vec::new(),
                        trailing: None,
                    })
                    .collect(),
                Vec::new(),
            )
        };
        let (before, blank_before, trailing) = depends_comments;
        entries.insert(
            at,
            Entry {
                key: "depends_on".into(),
                value,
                before,
                blank_before,
                trailing: (!trailing.is_empty()).then(|| trailing.join(" ")),
            },
        );
    }
    entries
}

/// A number of seconds (`unit` 1) or milliseconds (`unit` 0.001) as a
/// duration; kept as written when it is not a number.
fn seconds_duration(number: &str, unit: f64) -> Y {
    match number.parse::<f64>() {
        Ok(value) if value >= 0.0 && value.is_finite() => {
            Y::String(format_duration(Duration::from_secs_f64(value * unit)))
        }
        _ => Y::Literal(number.to_owned()),
    }
}

/// Widest line a flow collection may make.
const WIDTH: usize = 96;

fn comments(out: &mut String, lines: &[String], indent: usize) {
    for line in lines {
        out.push_str(&" ".repeat(indent));
        if line.is_empty() {
            out.push_str("#\n");
        } else {
            out.push_str("# ");
            out.push_str(line);
            out.push('\n');
        }
    }
}

fn trailing(out: &mut String, comment: &Option<String>) {
    if let Some(comment) = comment {
        out.push_str("  # ");
        out.push_str(comment);
    }
    out.push('\n');
}

/// Whether `value` is written on one line (a scalar, or a small flow
/// collection without comments) after `used` columns.
fn flows(value: &Y, used: usize) -> bool {
    match value {
        Y::String(text) => !text.contains('\n') || used + text.len() < WIDTH,
        Y::Literal(_) => true,
        Y::Map(..) | Y::List(..) => flow(value).is_some_and(|text| used + text.len() <= WIDTH),
    }
}

/// `value` as a one-line flow collection or scalar; `None` when it holds
/// comments or something that only fits in block style.
fn flow(value: &Y) -> Option<String> {
    match value {
        Y::String(text) => (!text.contains('\n')).then(|| scalar(text, true)),
        Y::Literal(text) => Some(text.clone()),
        Y::Map(entries, end) => {
            if !end.is_empty() {
                return None;
            }
            let mut parts = Vec::with_capacity(entries.len());
            for entry in entries {
                // Groups' steps one per line.
                let steps = entry.key == "steps" && matches!(&entry.value, Y::Map(steps, _) if !steps.is_empty());
                if !entry.before.is_empty() || entry.trailing.is_some() || steps {
                    return None;
                }
                parts.push(format!("{}: {}", key(&entry.key, true), flow(&entry.value)?));
            }
            Some(format!("{{{}}}", parts.join(", ")))
        }
        Y::List(items, end) => {
            if !end.is_empty() {
                return None;
            }
            let mut parts = Vec::with_capacity(items.len());
            for item in items {
                if !item.before.is_empty() || item.trailing.is_some() {
                    return None;
                }
                parts.push(flow(&item.value)?);
            }
            Some(format!("[{}]", parts.join(", ")))
        }
    }
}

fn emit_entries(out: &mut String, entries: &[Entry], indent: usize) {
    for (index, entry) in entries.iter().enumerate() {
        if entry.blank_before && index > 0 {
            out.push('\n');
        }
        comments(out, &entry.before, indent);
        out.push_str(&" ".repeat(indent));
        let name = key(&entry.key, false);
        out.push_str(&name);
        out.push(':');
        // Steps and inputs one per line, however short.
        let used = match (&entry.value, entry.key.as_str()) {
            (Y::Map(entries, _), "steps" | "inputs") if !entries.is_empty() => usize::MAX / 2,
            _ => indent + name.len() + 2,
        };
        emit_value(out, &entry.value, &entry.trailing, indent, used);
    }
}

/// A value after `key:` (or `-`) at `indent`, the line having `used`
/// columns.
fn emit_value(out: &mut String, value: &Y, comment: &Option<String>, indent: usize, used: usize) {
    match value {
        Y::Map(entries, end) if entries.is_empty() && end.is_empty() => {
            out.push_str(" {}");
            trailing(out, comment);
        }
        Y::List(items, end) if items.is_empty() && end.is_empty() => {
            out.push_str(" []");
            trailing(out, comment);
        }
        Y::String(text) => {
            out.push(' ');
            out.push_str(&scalar(text, false));
            trailing(out, comment);
        }
        Y::Literal(text) => {
            out.push(' ');
            out.push_str(text);
            trailing(out, comment);
        }
        Y::Map(entries, end) => match flow(value).filter(|text| used + text.len() <= WIDTH) {
            Some(text) => {
                out.push(' ');
                out.push_str(&text);
                trailing(out, comment);
            }
            None => {
                trailing(out, comment);
                emit_entries(out, entries, indent + 2);
                comments(out, end, indent + 2);
            }
        },
        Y::List(items, end) => match flow(value).filter(|text| used + text.len() <= WIDTH) {
            Some(text) => {
                out.push(' ');
                out.push_str(&text);
                trailing(out, comment);
            }
            None => {
                trailing(out, comment);
                for item in items {
                    emit_item(out, item, indent + 2);
                }
                comments(out, end, indent + 2);
            }
        },
    }
}

/// A list item at `indent`: `- value`, or `- key: …` with the mapping's
/// other keys under the first.
fn emit_item(out: &mut String, item: &Item, indent: usize) {
    comments(out, &item.before, indent);
    let pad = " ".repeat(indent);
    match &item.value {
        Y::Map(entries, end)
            if !entries.is_empty()
                && flow(&item.value).is_none_or(|text| indent + 2 + text.len() > WIDTH) =>
        {
            // The first key goes on the dash's line; its comments before
            // the dash.
            let mut entries = entries.clone();
            let first_comments = std::mem::take(&mut entries[0].before);
            comments(out, &first_comments, indent);
            let mut block = String::new();
            emit_entries(&mut block, &entries, indent + 2);
            comments(&mut block, end, indent + 2);
            let block = block
                .strip_prefix(&" ".repeat(indent + 2))
                .unwrap_or(&block)
                .to_owned();
            out.push_str(&pad);
            out.push_str("- ");
            out.push_str(&block);
            if item.trailing.is_some() {
                // Rare: a comment after a whole object in an array.
                comments(out, &item.trailing.iter().cloned().collect::<Vec<_>>(), indent + 2);
            }
        }
        value => {
            out.push_str(&pad);
            out.push('-');
            emit_value(out, value, &item.trailing, indent, indent + 2);
        }
    }
}

/// A mapping key, quoted when it is not a plain word.
fn key(text: &str, flow: bool) -> String {
    scalar(text, flow)
}

/// A string as a YAML scalar: plain when it reads back as the same
/// string, else single-quoted, else (newlines, control characters)
/// double-quoted.
fn scalar(text: &str, flow: bool) -> String {
    if plain_ok(text, flow) {
        text.to_owned()
    } else if !text.chars().any(char::is_control) {
        format!("'{}'", text.replace('\'', "''"))
    } else {
        serde_json::to_string(text).expect("a string serializes")
    }
}

fn plain_ok(text: &str, flow: bool) -> bool {
    const FIRST: &[char] = &[
        '-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@',
        '`',
    ];
    // YAML 1.1 booleans: strings here, but other tools read them as
    // booleans.
    const AMBIGUOUS: &[&str] = &["yes", "no", "on", "off"];
    !text.is_empty()
        && text.trim() == text
        && !text.starts_with(FIRST)
        && !text.contains(": ")
        && !text.contains(" #")
        && !text.ends_with(':')
        && !text.chars().any(char::is_control)
        && matches!(yaml::plain(text), Data::String(_))
        && !AMBIGUOUS.contains(&text.to_ascii_lowercase().as_str())
        && !(flow && text.contains([',', '[', ']', '{', '}']))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_are_quoted_only_when_needed() {
        for (text, written) in [
            ("uv run x", "uv run x"),
            ("C:/src/api", "C:/src/api"),
            ("~/web", "~/web"),
            ("${inputs.env}", "${inputs.env}"),
            ("ready in \\d+ ms", "ready in \\d+ ms"),
            ("8000", "'8000'"),
            ("true", "'true'"),
            ("yes", "'yes'"),
            ("", "''"),
            ("a: b", "'a: b'"),
            ("it's # here", "'it''s # here'"),
            ("- x", "'- x'"),
            ("{{.ID}}", "'{{.ID}}'"),
            ("two\nlines", "\"two\\nlines\""),
        ] {
            assert_eq!(scalar(text, false), written, "{text}");
            let read = yaml::load(&format!("k: {written}\n")).root.unwrap();
            assert_eq!(read.get("k").unwrap().as_str(), Some(text), "{written}");
        }
        assert_eq!(scalar("a,b", false), "a,b");
        assert_eq!(scalar("a,b", true), "'a,b'");
    }

    #[test]
    fn an_old_file_converts_with_its_comments() {
        let old = r#"// Header
// two lines
{
  "name": "Demo",
  "$schema": "https://example.com/schema.json",
  "inputs": [
    // The target.
    { "id": "env", "type": "choice", "options": ["dev", "prod"], "default": "dev" }
  ],
  "steps": [
    {
      // Unique id.
      "id": "vpn",
      "check": "ping -n 1 host", // quick
      "onFail": "Connect the VPN, then Retry."
    },
    {
      "id": "api",
      "needs": ["vpn"],
      "project": { "devcontainer": "C:/src/api" },
      "run": "uv run uvicorn app:app --port 8000",
      "ready": { "port": 8000 },
      "timeout_s": 600,
      "stop_grace_s": 30,
      "openUrls": true,
      "env": { "A": "1", "N": 2 }
    },
    { "id": "wait", "needs": "api", "ready": { "delay_ms": 1500 } },
    {
      "id": "report",
      "needs": "wait",
      "after": ["api"], /* even when it failed */
      "run": "python report.py '${steps.api.exit_code}'",
      "artifacts": [{ "label": "report", "path": "out/report.html" }, "out/log.txt"]
    },
    {
      "id": "svc",
      "steps": [
        { "id": "db", "run": "docker compose up db", "ready": { "port": 5432 } },
        { "id": "seed", "needs": ["db"], "run": "./seed.sh" },
      ],
    },
  ],
  "owner": "me",
}
"#;
        let converted = convert(old).unwrap();
        let expected = r#"# Header
# two lines
name: Demo

inputs:
  # The target.
  env: {type: choice, options: [dev, prod], default: dev}

steps:
  # Unique id.
  vpn:
    check: ping -n 1 host  # quick
    on_fail: Connect the VPN, then Retry.

  api:
    depends_on: [vpn]
    project: {devcontainer: C:/src/api}
    run: uv run uvicorn app:app --port 8000
    ready: {port: 8000}
    timeout: 10m
    stop_grace_period: 30s
    open_urls: true
    env: {A: '1', N: 2}

  wait: {depends_on: [api], ready: {delay: 1.5s}}

  report:
    depends_on: {wait: ready, api: completed}  # even when it failed
    run: python report.py '${steps.api.exit_code}'
    artifacts: [{label: report, path: out/report.html}, out/log.txt]

  svc:
    steps:
      db: {run: docker compose up db, ready: {port: 5432}}
      seed: {depends_on: [db], run: ./seed.sh}
x-owner: me
"#;
        assert_eq!(converted, expected, "{converted}");
        let workflow = crate::parse(&converted, "f").unwrap();
        assert_eq!(workflow.steps.len(), 5);
        assert_eq!(workflow.steps[1].timeout, Some(Duration::from_secs(600)));
        assert_eq!(workflow.steps[1].env["N"], "2");
        assert_eq!(workflow.steps[4].steps.len(), 2);
    }

    #[test]
    fn long_values_and_commented_ones_are_written_in_block_style() {
        let old = r#"{"steps": [{"id": "a",
  "run": "a very long command line that goes on and on and on and on and on and on and on and on",
  "artifacts": [
    // first
    "x.txt",
    {"label": "board", "url": "http://localhost:6006/a/long/path/that/does/not/fit/on/one/line/at/all"}
  ],
  "env": {
    // why
    "A": "1"
  }
}]}"#;
        let converted = convert(old).unwrap();
        let expected = r#"steps:
  a:
    run: a very long command line that goes on and on and on and on and on and on and on and on
    artifacts:
      # first
      - x.txt
      - label: board
        url: http://localhost:6006/a/long/path/that/does/not/fit/on/one/line/at/all
    env:
      # why
      A: '1'
"#;
        assert_eq!(converted, expected, "{converted}");
        assert!(crate::parse(&converted, "f").is_ok());
    }

    #[test]
    fn invalid_json_says_where() {
        let err = convert("{\n  \"steps\": [\n    { \"id\": \"a\" \"run\": \"x\" }\n  ]\n}").unwrap_err();
        assert!(err.starts_with("invalid JSON at line 3"), "{err}");
        assert!(convert("[1]").unwrap_err().contains("JSON object"));
        assert!(is_legacy("C:/w/dev.jsonc") && is_legacy("dev.json") && !is_legacy("dev.yaml"));
    }
}
