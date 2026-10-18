//! Where the cursor is in a workflow file that is being typed (so usually
//! not valid YAML): the keys from the top down to the mapping or list it is
//! in, and whether it is on a key, a value, a list item or inside `${…}`.
//! Read from the text: indentation for block structure (parent keys, list
//! items, block scalars), and the cursor's line for flow `{…}` and `[…]`.

use std::ops::Range;

/// One step down from the top of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    /// Into the value of this key.
    Key(String),
    /// Into an item of a list.
    Item,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum At {
    /// A key of the mapping at `path`.
    Key,
    /// The value of this key of the mapping at `path`.
    Value(String),
    /// An item of the list at `path`.
    Item,
    /// Inside `${…}`: what is typed after `${`.
    Variable(String),
    /// Inside text (a block scalar, a comment, a quoted string's middle).
    Nothing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    pub path: Vec<Seg>,
    pub at: At,
    /// What a completion replaces: the word around the cursor (for a
    /// variable, the part after its last `.`, `:` or `${`).
    pub word: Range<usize>,
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '$')
}

/// The word around `offset`: from the last non-word character before it to
/// the first after it.
pub fn word_at(text: &str, offset: usize) -> Range<usize> {
    let start = text[..offset]
        .char_indices()
        .rev()
        .find(|&(_, c)| !is_word(c))
        .map_or(0, |(at, c)| at + c.len_utf8());
    let end = text[offset..]
        .char_indices()
        .find(|&(_, c)| !is_word(c))
        .map_or(text.len(), |(at, _)| offset + at);
    start..end
}

/// The context of the cursor at byte `offset` of `text`.
pub fn context(text: &str, offset: usize) -> Context {
    let line_start = text[..offset].rfind('\n').map_or(0, |at| at + 1);
    let prefix = &text[line_start..offset];
    let mut word = word_at(text, offset);

    // Inside `${…}`.
    if let Some(open) = prefix.rfind("${")
        && !prefix[open..].contains('}')
    {
        let typed = prefix[open + 2..].to_owned();
        let cut = typed.rfind(['.', ':']).map_or(0, |at| at + 1);
        let start = line_start + open + 2 + cut;
        let end = text[offset..]
            .char_indices()
            .find(|&(_, c)| !(c.is_alphanumeric() || matches!(c, '_' | '-')))
            .map_or(text.len(), |(at, _)| offset + at);
        let mut context = line_context(text, line_start, offset);
        context.at = At::Variable(typed);
        context.word = start..end;
        return context;
    }
    if prefix.trim_start().starts_with('#') || in_comment(prefix) {
        return Context {
            path: Vec::new(),
            at: At::Nothing,
            word,
        };
    }
    let context = line_context(text, line_start, offset);
    // A word inside quotes replaces only up to the quote.
    if let Some(quote) = text[word.clone()].find(['\'', '"']) {
        word.end = word.start + quote;
    }
    Context { word, ..context }
}

/// Whether a ` #` comment starts before the end of `prefix` (outside
/// quotes).
fn in_comment(prefix: &str) -> bool {
    let mut quote = None;
    let mut previous = ' ';
    for c in prefix.chars() {
        match (quote, c) {
            (None, '\'' | '"') => quote = Some(c),
            (Some(open), c) if c == open => quote = None,
            (None, '#') if previous.is_whitespace() => return true,
            _ => {}
        }
        previous = c;
    }
    false
}

/// Leading spaces of `line`.
fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// A line's list dashes and where its content starts: `  - - a` is (2
/// dashes, column 6).
fn dashes(line: &str) -> (usize, usize) {
    let mut column = indent(line);
    let mut count = 0;
    loop {
        let rest = &line[column..];
        if rest == "-" || rest.starts_with("- ") {
            count += 1;
            column += 1;
            column += rest[1..].len() - rest[1..].trim_start_matches(' ').len();
        } else {
            return (count, column);
        }
    }
}

/// `key` of a `key: value` line content (outside quotes), and its value.
fn split_key(content: &str) -> Option<(String, &str)> {
    let mut quote = None;
    let bytes: Vec<char> = content.chars().collect();
    let mut byte = 0;
    for (index, &c) in bytes.iter().enumerate() {
        match (quote, c) {
            (None, '\'' | '"') if index == 0 => quote = Some(c),
            (Some(open), c) if c == open => quote = None,
            (None, ':') => {
                let next = bytes.get(index + 1).copied();
                if next.is_none_or(char::is_whitespace) {
                    let key = unquote(content[..byte].trim());
                    return Some((key, content[byte + 1..].trim_start()));
                }
            }
            (None, '{' | '[') if index == 0 => return None,
            _ => {}
        }
        byte += c.len_utf8();
    }
    None
}

fn unquote(key: &str) -> String {
    let inner = key
        .strip_prefix('\'')
        .and_then(|key| key.strip_suffix('\''))
        .or_else(|| key.strip_prefix('"').and_then(|key| key.strip_suffix('"')));
    inner.unwrap_or(key).to_owned()
}

/// Whether a value starts a block scalar (`|`, `>-`, `|2`).
fn block_scalar(value: &str) -> bool {
    let value = value.split(" #").next().unwrap_or("").trim();
    value.starts_with(['|', '>'])
        && value[1..]
            .chars()
            .all(|c| matches!(c, '-' | '+') || c.is_ascii_digit())
}

/// Whether a value is empty (or only a comment, or an anchor/tag).
fn empty_value(value: &str) -> bool {
    let value = value.trim();
    value.is_empty()
        || value.starts_with('#')
        || ((value.starts_with('&') || value.starts_with('!')) && !value.contains(' '))
}

/// The cursor's path and position from its line and the block structure
/// above it.
fn line_context(text: &str, line_start: usize, offset: usize) -> Context {
    let prefix = &text[line_start..offset];
    let word = word_at(text, offset);
    let nothing = |path| Context {
        path,
        at: At::Nothing,
        word: word.clone(),
    };
    let (dash_count, content_column) = if prefix.trim().is_empty() {
        (0, prefix.len())
    } else {
        dashes(prefix)
    };
    let column = content_column.min(prefix.len());
    let content = &prefix[column..];
    let Some(mut path) = parents(text, line_start, column, dash_count > 0) else {
        // Inside a multi-line value or a block scalar.
        return nothing(Vec::new());
    };
    // The rest of this line: dashes make items.
    for _ in 0..dash_count.saturating_sub(1) {
        path.push(Seg::Item);
    }
    if dash_count > 0 {
        path.push(Seg::Item);
    }
    match split_key(content) {
        None if content.trim_start().starts_with(['{', '[']) => {
            // A flow collection as a list item: `- {label: …`.
            if dash_count > 0 {
                return flow(content, path, word);
            }
            nothing(path)
        }
        None if dash_count > 0 && !content.is_empty() => {
            // `- val`: an item of the list (unless it becomes a key).
            path.pop();
            Context {
                path,
                at: At::Item,
                word,
            }
        }
        None if content.starts_with(['\'', '"']) && content.len() > 1 => nothing(path),
        None => {
            if dash_count > 0 && content.is_empty() {
                // `- |`: an item, maybe a mapping's first key.
                path.pop();
                return Context {
                    path,
                    at: At::Item,
                    word,
                };
            }
            Context {
                path,
                at: At::Key,
                word,
            }
        }
        Some((key, value)) => {
            if value.starts_with(['{', '[']) {
                path.push(Seg::Key(key));
                return flow(value, path, word);
            }
            if block_scalar(value) || value.starts_with(['\'', '"']) {
                return nothing(path);
            }
            Context {
                path,
                at: At::Value(key),
                word,
            }
        }
    }
}

/// The keys above a line whose content starts at `column`: each line above
/// with less indentation that opens a mapping or a list item. `item`: the
/// line itself is a list item (its list's key may sit at the same
/// indentation as the dash). `None` inside a multi-line value or a block
/// scalar.
fn parents(text: &str, line_start: usize, column: usize, item: bool) -> Option<Vec<Seg>> {
    let mut path = Vec::new();
    let mut need = column;
    // A list item's own line: its key may be at the dash's indentation.
    let mut allow_equal = item;
    if item {
        let line_end = text[line_start..]
            .find('\n')
            .map_or(text.len(), |at| line_start + at);
        need = indent(&text[line_start..line_end]);
    }
    let mut end = line_start;
    while end > 0 && need > 0 || (end > 0 && allow_equal && need == 0) {
        let start = text[..end - 1].rfind('\n').map_or(0, |at| at + 1);
        let line = text[start..end - 1].trim_end_matches('\r');
        end = start;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed == "---" {
            continue;
        }
        let line_indent = indent(line);
        let (count, content_column) = dashes(line);
        let content = &line[content_column..];
        let fits = line_indent < need || (allow_equal && line_indent == need);
        if !fits {
            continue;
        }
        if count > 0 {
            // A list item above: the cursor is in its mapping (a sibling
            // key) or under one of its keys.
            if content_column < need
                && let Some((key, value)) = split_key(content)
                && empty_value(value)
            {
                path.push(Seg::Key(key));
            } else if content_column > need {
                continue;
            }
            for _ in 0..count {
                path.push(Seg::Item);
            }
            need = line_indent;
            allow_equal = true;
            continue;
        }
        match split_key(content) {
            Some((key, value)) if empty_value(value) => {
                path.push(Seg::Key(key));
                need = line_indent;
                allow_equal = false;
            }
            // Inside a multi-line value (or a block scalar).
            Some(_) | None => return None,
        }
    }
    path.reverse();
    Some(path)
}

/// The context inside flow collections starting at the beginning of
/// `value` (`{depends_on: [a, b` …).
fn flow(value: &str, mut path: Vec<Seg>, word: Range<usize>) -> Context {
    #[derive(Clone, Copy, PartialEq)]
    enum Kind {
        Map,
        List,
    }
    struct Frame {
        kind: Kind,
        key: Option<String>,
        expecting_key: bool,
    }
    let mut stack: Vec<Frame> = Vec::new();
    let mut token = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' => {
                // A quoted scalar: up to its closing quote (unclosed: the
                // cursor is inside it).
                let mut closed = false;
                for inner in chars.by_ref() {
                    if inner == c {
                        closed = true;
                        break;
                    }
                    token.push(inner);
                }
                if !closed {
                    return Context {
                        path,
                        at: At::Nothing,
                        word,
                    };
                }
            }
            '{' | '[' => {
                if let Some(top) = stack.last()
                    && top.kind == Kind::Map
                {
                    path.push(Seg::Key(top.key.clone().unwrap_or_default()));
                } else if !stack.is_empty() {
                    path.push(Seg::Item);
                }
                stack.push(Frame {
                    kind: if c == '{' { Kind::Map } else { Kind::List },
                    key: None,
                    expecting_key: c == '{',
                });
                token.clear();
            }
            '}' | ']' => {
                stack.pop();
                if !stack.is_empty() {
                    path.pop();
                }
                token.clear();
            }
            ',' => {
                if let Some(top) = stack.last_mut() {
                    top.key = None;
                    top.expecting_key = top.kind == Kind::Map;
                }
                token.clear();
            }
            ':' if chars
                .peek()
                .is_none_or(|next| next.is_whitespace() || *next == ',') =>
            {
                if let Some(top) = stack.last_mut()
                    && top.kind == Kind::Map
                    && top.expecting_key
                {
                    top.key = Some(token.trim().to_owned());
                    top.expecting_key = false;
                }
                token.clear();
            }
            c => token.push(c),
        }
    }
    let Some(top) = stack.last() else {
        return Context {
            path,
            at: At::Nothing,
            word,
        };
    };
    let at = match top.kind {
        Kind::Map if top.expecting_key => At::Key,
        Kind::Map => At::Value(top.key.clone().unwrap_or_default()),
        Kind::List => At::Item,
    };
    Context { path, at, word }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The context at `|` in `text`.
    fn at(text: &str) -> (Vec<String>, At) {
        let offset = text.find('|').expect("a cursor");
        let text = text.replacen('|', "", 1);
        let context = context(&text, offset);
        let path = context
            .path
            .iter()
            .map(|seg| match seg {
                Seg::Key(key) => key.clone(),
                Seg::Item => "-".into(),
            })
            .collect();
        (path, context.at)
    }

    fn keys(path: &[&str]) -> Vec<String> {
        path.iter().map(|key| (*key).to_owned()).collect()
    }

    #[test]
    fn block_keys_and_values() {
        assert_eq!(at("na|"), (vec![], At::Key));
        assert_eq!(at("name: x\n|"), (vec![], At::Key));
        assert_eq!(
            at("steps:\n  api:\n    ru|"),
            (keys(&["steps", "api"]), At::Key)
        );
        assert_eq!(
            at("steps:\n  api:\n    run: x\n    |"),
            (keys(&["steps", "api"]), At::Key)
        );
        assert_eq!(
            at("steps:\n  api:\n    depends_on: |"),
            (keys(&["steps", "api"]), At::Value("depends_on".into()))
        );
        assert_eq!(
            at("steps:\n  api:\n    ready:\n      po|"),
            (keys(&["steps", "api", "ready"]), At::Key)
        );
        // Blank and comment lines in between, a key at the top again.
        assert_eq!(
            at("steps:\n  api:\n    run: x\n\n    # note\n    |"),
            (keys(&["steps", "api"]), At::Key)
        );
        assert_eq!(at("steps:\n  api:\n    run: x\nin|"), (vec![], At::Key));
        // Nested groups.
        assert_eq!(
            at("steps:\n  g:\n    steps:\n      db:\n        ready: {|"),
            (keys(&["steps", "g", "steps", "db", "ready"]), At::Key)
        );
    }

    #[test]
    fn flow_collections_on_the_line() {
        assert_eq!(
            at("steps:\n  api: {depends_on: [a, |"),
            (keys(&["steps", "api", "depends_on"]), At::Item)
        );
        assert_eq!(
            at("steps:\n  api: {run: x, rea|"),
            (keys(&["steps", "api"]), At::Key)
        );
        assert_eq!(
            at("steps:\n  api: {ready: {port: 80}, depends_on: {db: |"),
            (
                keys(&["steps", "api", "depends_on"]),
                At::Value("db".into())
            )
        );
        assert_eq!(
            at("steps:\n  api:\n    project: {devcontainer: {|"),
            (keys(&["steps", "api", "project", "devcontainer"]), At::Key)
        );
        assert_eq!(
            at("steps:\n  api: {run: 'a, b', ti|"),
            (keys(&["steps", "api"]), At::Key)
        );
    }

    #[test]
    fn list_items() {
        assert_eq!(
            at("steps:\n  a:\n    depends_on:\n      - |"),
            (keys(&["steps", "a", "depends_on"]), At::Item)
        );
        assert_eq!(
            at("steps:\n  a:\n    depends_on:\n    - x\n    - |"),
            (keys(&["steps", "a", "depends_on"]), At::Item)
        );
        assert_eq!(
            at("steps:\n  a:\n    artifacts:\n      - label: x\n        pa|"),
            (keys(&["steps", "a", "artifacts", "-"]), At::Key)
        );
        assert_eq!(
            at("steps:\n  a:\n    artifacts:\n      - {label: x, |"),
            (keys(&["steps", "a", "artifacts", "-"]), At::Key)
        );
        assert_eq!(
            at("inputs:\n  e:\n    options:\n      - value: x\n        la|"),
            (keys(&["inputs", "e", "options", "-"]), At::Key)
        );
    }

    #[test]
    fn variables_text_and_comments() {
        let (path, at_) = at("steps:\n  r:\n    run: echo ${steps.ev|");
        assert_eq!(path, keys(&["steps", "r"]));
        assert_eq!(at_, At::Variable("steps.ev".into()));
        let text = "steps:\n  r: {run: 'x ${inputs.|}'}";
        let offset = text.find('|').unwrap();
        let text = text.replacen('|', "", 1);
        let found = context(&text, offset);
        assert_eq!(found.at, At::Variable("inputs.".into()));
        assert_eq!(
            found.word,
            offset..offset,
            "replaces the part after the dot"
        );
        assert_eq!(at("steps:\n  a:\n    run: >-\n      echo |").1, At::Nothing);
        assert_eq!(at("steps:\n  a: # a com|").1, At::Nothing);
        assert_eq!(at("steps:\n  a:\n    run: 'echo it|'").1, At::Nothing);
    }

    #[test]
    fn words_stop_at_punctuation() {
        let text = "depends_on: [api, db]";
        assert_eq!(&text[word_at(text, 15)], "api");
        assert_eq!(&text[word_at(text, 0)], "depends_on");
    }
}
