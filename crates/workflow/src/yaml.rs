//! YAML for workflow files: the text as a tree of [`Node`]s that keep
//! their byte ranges and the order of mapping keys (a workflow's steps run
//! and show in file order). Plain scalars are typed by YAML 1.2's core
//! schema (`null`, `true`/`false`, numbers; `yes`/`on` stay strings), and
//! anchors, aliases and `<<` merge keys work as in Docker Compose files
//! (`x-…` keys hold the shared parts). A file that does not parse still
//! gives the tree read up to the problem, so an editor can map what is
//! there to lines.

use std::collections::HashMap;

use saphyr_parser::{Event, Parser, ScalarStyle, Span};
use serde_json::{Number, Value};

/// A value and where it is in the text.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub data: Data,
    /// Byte range: a scalar's text (quotes included, trailing blanks of a
    /// block scalar not), a block collection from its first item to the
    /// end of its last, a flow collection with its brackets.
    pub start: usize,
    pub end: usize,
    /// A scalar's content as written (after unquoting): the text a string
    /// field takes from an unquoted `8000` or `true`.
    pub scalar: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Data {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    List(Vec<Node>),
    Map(Vec<Entry>),
}

/// One key of a mapping, in file order.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub key: String,
    /// Where the key starts.
    pub key_start: usize,
    pub value: Node,
}

impl Entry {
    /// From the key to the end of its value.
    pub fn end(&self) -> usize {
        self.value.end.max(self.key_start)
    }
}

/// A problem with the YAML itself and where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub message: String,
    pub offset: usize,
}

/// What [`load`] read.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    /// The document; when `errors` hold a syntax error, what was read
    /// before it (open collections end at the end of the text).
    pub root: Option<Node>,
    /// Syntax errors (the first stops the reading) and duplicate keys.
    pub errors: Vec<Error>,
}

impl Node {
    fn new(data: Data, start: usize, end: usize) -> Node {
        Node {
            data,
            start,
            end,
            scalar: None,
        }
    }

    pub fn as_map(&self) -> Option<&[Entry]> {
        match &self.data {
            Data::Map(entries) => Some(entries),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Node]> {
        match &self.data {
            Data::List(items) => Some(items),
            _ => None,
        }
    }

    /// A string as written; `None` for anything else.
    pub fn as_str(&self) -> Option<&str> {
        match &self.data {
            Data::String(text) => Some(text),
            _ => None,
        }
    }

    /// The text of a scalar that is not null: a string, or a number or
    /// boolean as written (`8000`, `1.10`, `true`).
    pub fn text(&self) -> Option<&str> {
        match self.data {
            Data::Null | Data::List(_) | Data::Map(_) => None,
            _ => self.scalar.as_deref(),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self.data, Data::Null)
    }

    /// The entry for `key` of a mapping.
    pub fn entry(&self, key: &str) -> Option<&Entry> {
        self.as_map()?.iter().find(|entry| entry.key == key)
    }

    /// The value at `key` of a mapping.
    pub fn get(&self, key: &str) -> Option<&Node> {
        self.entry(key).map(|entry| &entry.value)
    }

    /// What it is, for messages: `a string`, `a number`, `a list`, ….
    pub fn kind(&self) -> &'static str {
        match self.data {
            Data::Null => "empty",
            Data::Bool(_) => "true or false",
            Data::Number(_) => "a number",
            Data::String(_) => "a string",
            Data::List(_) => "a list",
            Data::Map(_) => "a mapping",
        }
    }

    /// As JSON (mapping keys sorted, positions dropped).
    pub fn to_json(&self) -> Value {
        match &self.data {
            Data::Null => Value::Null,
            Data::Bool(flag) => Value::Bool(*flag),
            Data::Number(number) => Value::Number(number.clone()),
            Data::String(text) => Value::String(text.clone()),
            Data::List(items) => Value::Array(items.iter().map(Node::to_json).collect()),
            Data::Map(entries) => Value::Object(
                entries
                    .iter()
                    .map(|entry| (entry.key.clone(), entry.value.to_json()))
                    .collect(),
            ),
        }
    }
}

/// Reads `text` (one YAML document; empty text has no root).
pub fn load(text: &str) -> Document {
    let mut loader = Loader::new(text);
    for item in Parser::new_from_str(text) {
        match item {
            Ok((event, span)) => {
                if !loader.event(event, span) {
                    break;
                }
            }
            Err(err) => {
                let offset = loader.byte(err.marker().index());
                loader.errors.push(Error {
                    message: format!("invalid YAML: {}", err.info()),
                    offset,
                });
                break;
            }
        }
    }
    loader.finish()
}

enum Frame {
    List {
        start: usize,
        items: Vec<Node>,
        anchor: usize,
    },
    Map {
        start: usize,
        entries: Vec<Entry>,
        /// A key read, waiting for its value.
        key: Option<(String, usize)>,
        /// `<<` values, merged in when the mapping ends.
        merges: Vec<Node>,
        anchor: usize,
    },
}

struct Loader<'a> {
    text: &'a str,
    /// Byte offset of each character index (the parser counts
    /// characters), and the text's length at the end.
    bytes: Vec<usize>,
    stack: Vec<Frame>,
    anchors: HashMap<usize, Node>,
    root: Option<Node>,
    documents: usize,
    errors: Vec<Error>,
}

impl<'a> Loader<'a> {
    fn new(text: &'a str) -> Self {
        let mut bytes: Vec<usize> = text.char_indices().map(|(at, _)| at).collect();
        bytes.push(text.len());
        Self {
            text,
            bytes,
            stack: Vec::new(),
            anchors: HashMap::new(),
            root: None,
            documents: 0,
            errors: Vec::new(),
        }
    }

    fn byte(&self, index: usize) -> usize {
        self.bytes
            .get(index)
            .copied()
            .unwrap_or(self.text.len())
    }

    fn range(&self, span: Span) -> (usize, usize) {
        (self.byte(span.start.index()), self.byte(span.end.index()))
    }

    /// Takes one event; `false` stops the reading.
    fn event(&mut self, event: Event<'_>, span: Span) -> bool {
        let (start, end) = self.range(span);
        match event {
            Event::DocumentStart(_) => {
                self.documents += 1;
                if self.documents > 1 {
                    self.errors.push(Error {
                        message: "a workflow file holds one YAML document (remove the second `---`)"
                            .into(),
                        offset: start,
                    });
                    return false;
                }
            }
            Event::Scalar(value, style, anchor, tag) => {
                let explicit_string = tag.as_ref().is_some_and(|tag| tag.suffix == "str");
                let data = if style == ScalarStyle::Plain && !explicit_string {
                    plain(&value)
                } else {
                    Data::String(value.to_string())
                };
                // Block scalars end after their last line's indentation.
                let end = start + self.text[start..end.max(start)].trim_end().len();
                let node = Node {
                    data,
                    start,
                    end,
                    scalar: Some(value.into_owned()),
                };
                if anchor > 0 {
                    self.anchors.insert(anchor, node.clone());
                }
                self.value(node);
            }
            Event::Alias(anchor) => match self.anchors.get(&anchor) {
                Some(node) => {
                    let mut node = node.clone();
                    node.start = start;
                    node.end = end;
                    self.value(node);
                }
                None => {
                    self.errors.push(Error {
                        message: "invalid YAML: unknown alias".into(),
                        offset: start,
                    });
                    return false;
                }
            },
            Event::SequenceStart(anchor, _) => self.stack.push(Frame::List {
                start,
                items: Vec::new(),
                anchor,
            }),
            Event::MappingStart(anchor, _) => self.stack.push(Frame::Map {
                start,
                entries: Vec::new(),
                key: None,
                merges: Vec::new(),
                anchor,
            }),
            Event::SequenceEnd | Event::MappingEnd => {
                if let Some(frame) = self.stack.pop() {
                    // Flow collections end with their bracket; block ones
                    // with their last item.
                    let closing = (end > start).then_some(end);
                    let node = self.close(frame, closing);
                    self.value(node);
                }
            }
            Event::StreamStart
            | Event::StreamEnd
            | Event::DocumentEnd
            | Event::Nothing => {}
        }
        true
    }

    /// The node `frame` makes; `closing` is where a flow collection's
    /// bracket ends.
    fn close(&mut self, frame: Frame, closing: Option<usize>) -> Node {
        match frame {
            Frame::List {
                start,
                items,
                anchor,
            } => {
                let end = closing
                    .or_else(|| items.last().map(|item| item.end))
                    .unwrap_or(start);
                let node = Node::new(Data::List(items), start, end);
                if anchor > 0 {
                    self.anchors.insert(anchor, node.clone());
                }
                node
            }
            Frame::Map {
                start,
                mut entries,
                key,
                merges,
                anchor,
            } => {
                if let Some((key, key_start)) = key {
                    // A key without a value (the text ended).
                    entries.push(Entry {
                        key,
                        key_start,
                        value: Node::new(Data::Null, key_start, key_start),
                    });
                }
                let end = closing
                    .or_else(|| entries.last().map(Entry::end))
                    .unwrap_or(start);
                // Explicit keys win over merged ones; earlier merged
                // mappings over later ones.
                for merged in merges {
                    let sources: Vec<&Node> = match &merged.data {
                        Data::List(items) => items.iter().collect(),
                        _ => vec![&merged],
                    };
                    for source in sources {
                        let Some(from) = source.as_map() else {
                            self.errors.push(Error {
                                message: "`<<` merges a mapping or a list of mappings".into(),
                                offset: source.start,
                            });
                            continue;
                        };
                        for entry in from {
                            if !entries.iter().any(|known| known.key == entry.key) {
                                entries.push(entry.clone());
                            }
                        }
                    }
                }
                let node = Node::new(Data::Map(entries), start, end);
                if anchor > 0 {
                    self.anchors.insert(anchor, node.clone());
                }
                node
            }
        }
    }

    /// A finished node: the root, an item, a key or a key's value.
    fn value(&mut self, node: Node) {
        match self.stack.last_mut() {
            None => {
                if self.root.is_none() {
                    self.root = Some(node);
                }
            }
            Some(Frame::List { items, .. }) => items.push(node),
            Some(Frame::Map {
                entries,
                key,
                merges,
                ..
            }) => match key.take() {
                None => match &node.scalar {
                    Some(text) => *key = Some((text.clone(), node.start)),
                    None => {
                        self.errors.push(Error {
                            message: "a key must be a plain value, not a list or mapping".into(),
                            offset: node.start,
                        });
                        *key = Some((String::new(), node.start));
                    }
                },
                Some((name, key_start)) => {
                    if name == "<<" {
                        merges.push(node);
                    } else if entries.iter().any(|entry| entry.key == name) {
                        self.errors.push(Error {
                            message: format!("duplicate key `{name}`"),
                            offset: key_start,
                        });
                    } else {
                        entries.push(Entry {
                            key: name,
                            key_start,
                            value: node,
                        });
                    }
                }
            },
        }
    }

    fn finish(mut self) -> Document {
        // Collections left open by a syntax error end at the text's end.
        while let Some(frame) = self.stack.pop() {
            let node = self.close(frame, Some(self.text.len()));
            self.value(node);
        }
        Document {
            root: self.root,
            errors: self.errors,
        }
    }
}

/// A plain scalar by YAML 1.2's core schema.
pub(crate) fn plain(text: &str) -> Data {
    match text {
        "" | "~" | "null" | "Null" | "NULL" => return Data::Null,
        "true" | "True" | "TRUE" => return Data::Bool(true),
        "false" | "False" | "FALSE" => return Data::Bool(false),
        _ => {}
    }
    let unsigned = text.strip_prefix(['-', '+']).unwrap_or(text);
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if digits(unsigned) {
        let number = text
            .strip_prefix('+')
            .unwrap_or(text)
            .parse::<i64>()
            .map(Number::from)
            .or_else(|_| text.trim_start_matches('+').parse::<u64>().map(Number::from));
        if let Ok(number) = number {
            return Data::Number(number);
        }
    }
    let radix = |prefix: &str, radix: u32| {
        text.strip_prefix(prefix)
            .filter(|rest| !rest.is_empty())
            .and_then(|rest| u64::from_str_radix(rest, radix).ok())
    };
    if let Some(number) = radix("0o", 8).or_else(|| radix("0x", 16)) {
        return Data::Number(Number::from(number));
    }
    // [-+]? ( . digits | digits ( . digits? )? ) ( [eE] [-+]? digits )?
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(at) => (&unsigned[..at], Some(&unsigned[at + 1..])),
        None => (unsigned, None),
    };
    let mantissa_ok = match mantissa.split_once('.') {
        Some(("", fraction)) => digits(fraction),
        Some((whole, fraction)) => digits(whole) && (fraction.is_empty() || digits(fraction)),
        None => digits(mantissa),
    };
    let exponent_ok = exponent.is_none_or(|exponent| {
        digits(exponent.strip_prefix(['-', '+']).unwrap_or(exponent))
    });
    if mantissa_ok
        && exponent_ok
        && let Ok(number) = text.parse::<f64>()
        && let Some(number) = Number::from_f64(number)
    {
        return Data::Number(number);
    }
    Data::String(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(text: &str) -> Node {
        let document = load(text);
        assert_eq!(document.errors, [], "{text}");
        document.root.expect("a root")
    }

    fn keys(node: &Node) -> Vec<&str> {
        node.as_map()
            .unwrap()
            .iter()
            .map(|entry| entry.key.as_str())
            .collect()
    }

    #[test]
    fn mappings_keep_their_order_and_scalars_are_typed() {
        let text = "zeta: 1\nalpha: -2.5\nmid: true\nnone:\ntilde: ~\nword: yes\nquoted: '8'\nhex: 0x1f\nver: 1.10\nbig: 18446744073709551615\n";
        let node = root(text);
        assert_eq!(
            keys(&node),
            ["zeta", "alpha", "mid", "none", "tilde", "word", "quoted", "hex", "ver", "big"]
        );
        let value = |key: &str| node.get(key).unwrap().to_json();
        assert_eq!(value("zeta"), serde_json::json!(1));
        assert_eq!(value("alpha"), serde_json::json!(-2.5));
        assert_eq!(value("mid"), serde_json::json!(true));
        assert_eq!(value("none"), Value::Null);
        assert_eq!(value("tilde"), Value::Null);
        assert_eq!(value("word"), serde_json::json!("yes"), "YAML 1.2: not a boolean");
        assert_eq!(value("quoted"), serde_json::json!("8"));
        assert_eq!(value("hex"), serde_json::json!(31));
        assert_eq!(value("big"), serde_json::json!(u64::MAX));
        // Numbers and booleans keep their text for string fields.
        assert_eq!(node.get("ver").unwrap().text(), Some("1.10"));
        assert_eq!(node.get("mid").unwrap().text(), Some("true"));
        assert_eq!(node.get("none").unwrap().text(), None);
        for (plain_text, number) in [("1e3", true), ("+.5", true), ("1.", true), ("1.2.3", false), ("e3", false), (".inf", false)] {
            assert_eq!(matches!(plain(plain_text), Data::Number(_)), number, "{plain_text}");
        }
    }

    #[test]
    fn nodes_know_where_they_are_in_bytes() {
        let text = "name: Démo\nsteps:\n  api:\n    run: |\n      uv run x\n      y\n    ready: {port: 8000}\n  web: [a, b]\n";
        let node = root(text);
        let name = node.get("name").unwrap();
        assert_eq!(&text[name.start..name.end], "Démo");
        let steps = node.get("steps").unwrap();
        let api = steps.entry("api").unwrap();
        assert_eq!(&text[api.key_start..api.key_start + 3], "api");
        let run = api.value.get("run").unwrap();
        assert_eq!(run.as_str(), Some("uv run x\ny\n"));
        assert_eq!(&text[run.start..run.end], "uv run x\n      y", "block scalars: the content");
        let ready = api.value.get("ready").unwrap();
        assert_eq!(&text[ready.start..ready.end], "{port: 8000}");
        // A block mapping ends with its last value.
        assert_eq!(&text[api.value.start..api.value.end], &text[text.find("run:").unwrap()..text.find('}').unwrap() + 1]);
        let web = steps.get("web").unwrap();
        assert_eq!(&text[web.start..web.end], "[a, b]");
        assert_eq!(steps.end, web.end);
    }

    #[test]
    fn anchors_aliases_and_merge_keys_work_as_in_compose() {
        let text = "x-api: &api\n  project: {local: C:/api}\n  env: {A: '1'}\nsteps:\n  build:\n    <<: *api\n    run: b\n  serve:\n    <<: [*api, {cwd: srv}]\n    env: {B: '2'}\n    run: s\n";
        let node = root(text);
        let steps = node.get("steps").unwrap();
        let build = steps.get("build").unwrap();
        assert_eq!(keys(build), ["run", "project", "env"]);
        assert_eq!(build.get("project").unwrap().to_json(), serde_json::json!({"local": "C:/api"}));
        let serve = steps.get("serve").unwrap();
        assert_eq!(keys(serve), ["env", "run", "project", "cwd"]);
        assert_eq!(serve.get("env").unwrap().to_json(), serde_json::json!({"B": "2"}), "explicit wins");
        // An alias is located where it is used.
        let text = "a: &x [1, 2]\nb: *x\n";
        let node = root(text);
        let b = node.get("b").unwrap();
        assert_eq!(&text[b.start..b.end], "*x");
        assert_eq!(b.to_json(), serde_json::json!([1, 2]));
    }

    #[test]
    fn problems_are_located_and_what_was_read_is_kept() {
        let text = "steps:\n  a:\n    run: x\n  a:\n    run: y\n";
        assert_eq!(
            load(text).errors,
            [Error {
                message: "duplicate key `a`".into(),
                offset: text.rfind("a:").unwrap()
            }]
        );
        let text = "steps:\n  a:\n    run: x\n  b:\n    run: [\n  c: 1\n";
        let document = load(text);
        assert_eq!(document.errors.len(), 1);
        assert!(document.errors[0].message.starts_with("invalid YAML"), "{:?}", document.errors);
        let steps = document.root.unwrap();
        let steps = steps.get("steps").unwrap();
        assert_eq!(keys(steps), ["a", "b"]);
        assert_eq!(steps.get("b").unwrap().end, text.len(), "open until the end");
        // Not a plain key; a second document.
        assert!(load("[a]: 1\n").errors[0].message.contains("plain value"));
        assert!(load("a: 1\n---\nb: 2\n").errors[0].message.contains("one YAML document"));
        assert_eq!(load("").root, None);
        assert_eq!(load("# only a comment\n").root, None);
    }

    #[test]
    fn json_is_yaml() {
        let node = root(r#"{"steps": {"a": {"run": "x", "depends_on": ["b"]}}, "n": 1.5}"#);
        assert_eq!(
            node.to_json(),
            serde_json::json!({"steps": {"a": {"run": "x", "depends_on": ["b"]}}, "n": 1.5})
        );
    }
}
