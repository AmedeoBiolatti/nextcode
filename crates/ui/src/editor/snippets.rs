//! LSP snippets (completion items with `insertTextFormat: 2`): the snippet
//! grammar, expansion with tab stops, and the session that moves between
//! them with Tab / Shift+Tab.

use std::path::Path;

use super::*;

actions!(editor, [NextTabStop, PreviousTabStop, EndSnippet]);

/// Expanded snippet text with its tab stops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snippet {
    pub text: String,
    /// In visiting order: `$1`, `$2`, … then `$0` (the end of the text when
    /// the snippet has none). Each stop lists its ranges (mirrors).
    pub stops: Vec<Vec<Range<usize>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Text(String),
    /// `$1`, `${1:placeholder}` or `${1|choice,…|}` (the first choice).
    Stop {
        index: u32,
        children: Vec<Node>,
    },
    /// `$NAME`, `${NAME}`, `${NAME:default}` (transforms are ignored).
    Variable {
        name: String,
        default: Vec<Node>,
    },
}

struct Parser<'a> {
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
}

impl Parser<'_> {
    /// Nodes until `}` (when `nested`) or the end; returns with the `}`
    /// consumed when nested.
    fn nodes(&mut self, nested: bool) -> Option<Vec<Node>> {
        let mut nodes = Vec::new();
        let mut text = String::new();
        loop {
            let Some(&(_, c)) = self.chars.peek() else {
                // An unclosed `${…` is not a placeholder.
                return (!nested).then(|| {
                    flush(&mut text, &mut nodes);
                    nodes
                });
            };
            match c {
                '}' if nested => {
                    self.chars.next();
                    flush(&mut text, &mut nodes);
                    return Some(nodes);
                }
                '\\' => {
                    self.chars.next();
                    match self.chars.peek() {
                        Some(&(_, escaped @ ('$' | '}' | '\\'))) => {
                            self.chars.next();
                            text.push(escaped);
                        }
                        _ => text.push('\\'),
                    }
                }
                '$' => {
                    let checkpoint = self.chars.clone();
                    self.chars.next();
                    match self.dollar() {
                        Some(node) => {
                            flush(&mut text, &mut nodes);
                            nodes.push(node);
                        }
                        None => {
                            self.chars = checkpoint;
                            self.chars.next();
                            text.push('$');
                        }
                    }
                }
                c => {
                    self.chars.next();
                    text.push(c);
                }
            }
        }
    }

    fn int(&mut self) -> Option<u32> {
        let mut digits = String::new();
        while let Some(&(_, c)) = self.chars.peek().filter(|(_, c)| c.is_ascii_digit()) {
            digits.push(c);
            self.chars.next();
        }
        digits.parse().ok()
    }

    fn name(&mut self) -> Option<String> {
        let mut name = String::new();
        while let Some(&(_, c)) = self
            .chars
            .peek()
            .filter(|(_, c)| c.is_ascii_alphanumeric() || *c == '_')
        {
            if name.is_empty() && c.is_ascii_digit() {
                return None;
            }
            name.push(c);
            self.chars.next();
        }
        (!name.is_empty()).then_some(name)
    }

    /// After `$`: a tab stop, placeholder, choice or variable.
    fn dollar(&mut self) -> Option<Node> {
        let &(_, c) = self.chars.peek()?;
        if c.is_ascii_digit() {
            let index = self.int()?;
            return Some(Node::Stop {
                index,
                children: Vec::new(),
            });
        }
        if c != '{' {
            let name = self.name()?;
            return Some(Node::Variable {
                name,
                default: Vec::new(),
            });
        }
        self.chars.next();
        let &(_, c) = self.chars.peek()?;
        if c.is_ascii_digit() {
            let index = self.int()?;
            match self.chars.next()?.1 {
                '}' => Some(Node::Stop {
                    index,
                    children: Vec::new(),
                }),
                ':' => Some(Node::Stop {
                    index,
                    children: self.nodes(true)?,
                }),
                '|' => {
                    let first = self.choice()?;
                    Some(Node::Stop {
                        index,
                        children: vec![Node::Text(first)],
                    })
                }
                _ => None,
            }
        } else {
            let name = self.name()?;
            match self.chars.next()?.1 {
                '}' => Some(Node::Variable {
                    name,
                    default: Vec::new(),
                }),
                ':' => Some(Node::Variable {
                    name,
                    default: self.nodes(true)?,
                }),
                '/' => {
                    self.skip_transform()?;
                    Some(Node::Variable {
                        name,
                        default: Vec::new(),
                    })
                }
                _ => None,
            }
        }
    }

    /// `a,b|}` after `${1|`: the first option.
    fn choice(&mut self) -> Option<String> {
        let mut options = vec![String::new()];
        loop {
            match self.chars.next()?.1 {
                '\\' => {
                    let (_, c) = self.chars.next()?;
                    if !matches!(c, '$' | '}' | '\\' | ',' | '|') {
                        options.last_mut()?.push('\\');
                    }
                    options.last_mut()?.push(c);
                }
                ',' => options.push(String::new()),
                '|' => {
                    return (self.chars.next()?.1 == '}').then(|| options.swap_remove(0));
                }
                c => options.last_mut()?.push(c),
            }
        }
    }

    /// `regex/format/options}` after `${NAME/`.
    fn skip_transform(&mut self) -> Option<()> {
        let mut slashes = 1;
        loop {
            match self.chars.next()?.1 {
                '\\' => {
                    self.chars.next()?;
                }
                '/' => slashes += 1,
                '}' if slashes >= 3 => return Some(()),
                _ => {}
            }
        }
    }
}

fn flush(text: &mut String, nodes: &mut Vec<Node>) {
    if !text.is_empty() {
        nodes.push(Node::Text(std::mem::take(text)));
    }
}

fn parse_nodes(source: &str) -> Vec<Node> {
    let mut parser = Parser {
        chars: source.char_indices().peekable(),
    };
    parser.nodes(false).unwrap_or_default()
}

/// Expands `source`: `newline` replaces each `\n` (the buffer's line ending
/// plus the indentation of the line the snippet goes on), `variable`
/// resolves `$NAME`s (`None`: unknown, so its default or its name is used).
pub(crate) fn expand(
    source: &str,
    newline: &str,
    variable: &dyn Fn(&str) -> Option<String>,
) -> Snippet {
    let nodes = parse_nodes(source);
    // Bare `$1`s mirror the text of the first `${1:…}`.
    let mut defaults: Vec<(u32, Vec<Node>)> = Vec::new();
    collect_defaults(&nodes, &mut defaults);
    let mut expansion = Expansion {
        text: String::new(),
        stops: Vec::new(),
        newline,
        variable,
        defaults: &defaults,
    };
    expansion.emit(&nodes, 0);
    let Expansion {
        text, mut stops, ..
    } = expansion;
    stops.sort_by_key(|(index, _)| if *index == 0 { u32::MAX } else { *index });
    let mut grouped: Vec<(u32, Vec<Range<usize>>)> = Vec::new();
    for (index, range) in stops {
        match grouped.last_mut() {
            Some((last, ranges)) if *last == index => ranges.push(range),
            _ => grouped.push((index, vec![range])),
        }
    }
    if grouped.last().is_none_or(|(index, _)| *index != 0) {
        let end = text.len()..text.len();
        grouped.push((0, vec![end]));
    }
    for (_, ranges) in &mut grouped {
        ranges.sort_by_key(|range| range.start);
    }
    Snippet {
        text,
        stops: grouped.into_iter().map(|(_, ranges)| ranges).collect(),
    }
}

fn collect_defaults(nodes: &[Node], defaults: &mut Vec<(u32, Vec<Node>)>) {
    for node in nodes {
        match node {
            Node::Stop { index, children } => {
                if !children.is_empty() && !defaults.iter().any(|(known, _)| known == index) {
                    defaults.push((*index, children.clone()));
                }
                collect_defaults(children, defaults);
            }
            Node::Variable { default, .. } => collect_defaults(default, defaults),
            Node::Text(_) => {}
        }
    }
}

struct Expansion<'a> {
    text: String,
    /// Stop index and range, in text order.
    stops: Vec<(u32, Range<usize>)>,
    newline: &'a str,
    variable: &'a dyn Fn(&str) -> Option<String>,
    defaults: &'a [(u32, Vec<Node>)],
}

impl Expansion<'_> {
    fn push_text(&mut self, text: &str) {
        let mut lines = text.split('\n');
        if let Some(first) = lines.next() {
            self.text.push_str(first);
        }
        for line in lines {
            self.text.push_str(self.newline);
            self.text.push_str(line);
        }
    }

    /// `depth` stops runaway mirrors (`${1:$1}`).
    fn emit(&mut self, nodes: &[Node], depth: usize) {
        for node in nodes {
            match node {
                Node::Text(text) => self.push_text(text),
                Node::Stop { index, children } => {
                    let start = self.text.len();
                    let children = if children.is_empty() && depth < 8 {
                        self.defaults
                            .iter()
                            .find(|(known, _)| known == index)
                            .map_or(&[][..], |(_, nodes)| nodes.as_slice())
                    } else {
                        children.as_slice()
                    };
                    if depth < 8 {
                        self.emit(children, depth + 1);
                    }
                    self.stops.push((*index, start..self.text.len()));
                }
                Node::Variable { name, default } => match (self.variable)(name) {
                    Some(value) if !value.is_empty() => self.push_text(&value),
                    known => {
                        if !default.is_empty() {
                            self.emit(default, depth + 1);
                        } else if known.is_none() {
                            self.push_text(name);
                        }
                    }
                },
            }
        }
    }
}

/// Tab stops of an inserted snippet, kept in step with edits.
pub(super) struct SnippetSession {
    /// Visiting order; the last one is `$0`.
    stops: Vec<Vec<Range<usize>>>,
    active: usize,
    /// The whole inserted text: the session ends when a cursor leaves it.
    range: Range<usize>,
}

impl Editor {
    /// Values of the snippet variables at the primary selection.
    fn snippet_variable(&self, name: &str) -> Option<String> {
        let primary = self.primary();
        let line = self.buffer.line_of(primary.start());
        let path = self.buffer.path();
        let file_name = || {
            path.and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned())
        };
        Some(match name {
            "TM_SELECTED_TEXT" => self.buffer.slice(primary.range()),
            "TM_CURRENT_LINE" => self.buffer.line_text(line),
            "TM_CURRENT_WORD" => {
                let head = primary.head;
                let start = language::completion::word_start(&self.buffer, head);
                let rest = self.buffer.slice(head..self.buffer.line_end(line));
                let after: String = rest.chars().take_while(|&c| is_word_char(c)).collect();
                format!("{}{after}", self.buffer.slice(start..head))
            }
            "TM_LINE_INDEX" => line.to_string(),
            "TM_LINE_NUMBER" => (line + 1).to_string(),
            "TM_FILENAME" => file_name().unwrap_or_default(),
            "TM_FILENAME_BASE" => path
                .and_then(Path::file_stem)
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            "TM_DIRECTORY" => path
                .and_then(Path::parent)
                .map(|dir| dir.display().to_string())
                .unwrap_or_default(),
            "TM_FILEPATH" => path.map(|p| p.display().to_string()).unwrap_or_default(),
            _ => return None,
        })
    }

    /// Expands snippet `source` for insertion at `at` (a buffer offset on
    /// the line it goes on).
    pub(super) fn expand_snippet(&self, source: &str, at: usize) -> Snippet {
        let line = self.buffer.line_of(at.min(self.buffer.len()));
        let text = self.buffer.line_text(line);
        let indent = &text[..text.len() - text.trim_start().len()];
        let newline = format!("{}{indent}", self.buffer.line_ending());
        expand(source, &newline, &|name| self.snippet_variable(name))
    }

    /// Replaces `range` with the snippet `source` at the primary selection
    /// (other selections stay) and selects its first tab stop.
    #[cfg(test)]
    pub(super) fn insert_snippet(
        &mut self,
        range: Range<usize>,
        source: &str,
        cx: &mut Context<Self>,
    ) {
        let snippet = self.expand_snippet(source, range.start);
        let edits = vec![(range.clone(), snippet.text.clone())];
        let primary = self.selections.primary_index();
        let others: Vec<Selection> = self
            .selections
            .all()
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != primary)
            .map(|(_, s)| {
                Selection::new(
                    lines::map_through(&edits, s.anchor, Bias::Right),
                    lines::map_through(&edits, s.head, Bias::Right),
                )
            })
            .collect();
        let start = range.start;
        self.commit(
            edits,
            |_| SelectionSet::single(Selection::cursor(start)),
            cx,
        );
        self.start_snippet(snippet, start, others, cx);
    }

    /// After `snippet.text` was inserted at `base`: selects the first tab
    /// stop (all its mirrors) alongside `others`, and keeps a session for
    /// Tab / Shift+Tab when there is more than the final position.
    pub(super) fn start_snippet(
        &mut self,
        snippet: Snippet,
        base: usize,
        others: Vec<Selection>,
        cx: &mut Context<Self>,
    ) {
        let stops: Vec<Vec<Range<usize>>> = snippet
            .stops
            .into_iter()
            .map(|ranges| {
                ranges
                    .into_iter()
                    .map(|range| base + range.start..base + range.end)
                    .collect()
            })
            .collect();
        let session = SnippetSession {
            range: base..base + snippet.text.len(),
            stops,
            active: 0,
        };
        let done = session.stops.len() <= 1;
        self.select_stop(&session, others, cx);
        self.snippet = (!done).then_some(session);
    }

    /// Selects the active stop's ranges, the first one primary.
    fn select_stop(
        &mut self,
        session: &SnippetSession,
        others: Vec<Selection>,
        cx: &mut Context<Self>,
    ) {
        let ranges = &session.stops[session.active];
        let mut items: Vec<Selection> = ranges
            .iter()
            .map(|range| Selection::new(range.start, range.end))
            .collect();
        let primary = 0;
        items.extend(others);
        let selections = SelectionSet::new(items, primary).clamped(&self.buffer);
        // `set_selections` would end the session if a stop moved outside.
        self.selections = selections;
        self.reveal_selections();
        self.autoscroll = true;
        self.auto_closed.clear();
        cx.notify();
    }

    fn move_tab_stop(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(mut session) = self.snippet.take() else {
            return;
        };
        if forward {
            session.active += 1;
        } else if session.active > 0 {
            session.active -= 1;
        }
        let last = session.active + 1 >= session.stops.len();
        self.select_stop(&session, Vec::new(), cx);
        if !last {
            self.snippet = Some(session);
        }
        self.completion = None;
        self.completion_generation += 1;
    }

    pub(super) fn next_tab_stop(
        &mut self,
        _: &NextTabStop,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_tab_stop(true, cx);
    }

    pub(super) fn previous_tab_stop(
        &mut self,
        _: &PreviousTabStop,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_tab_stop(false, cx);
    }

    pub(super) fn end_snippet(&mut self, _: &EndSnippet, _: &mut Window, cx: &mut Context<Self>) {
        self.snippet = None;
        cx.notify();
    }

    /// Moves the stops through the latest changes (edits inside a stop
    /// grow it).
    pub(super) fn snippet_edited(&mut self) {
        let Some(session) = &mut self.snippet else {
            return;
        };
        let map = |range: &mut Range<usize>, change: &Change| {
            let start = change.map_offset(range.start, Bias::Left);
            let end = change.map_offset(range.end, Bias::Right).max(start);
            *range = start..end;
        };
        for change in &self.changes {
            map(&mut session.range, change);
            for range in session.stops.iter_mut().flatten() {
                map(range, change);
            }
        }
    }

    /// Ends the session when a cursor left the snippet.
    pub(super) fn snippet_selections_changed(&mut self) {
        let Some(session) = &self.snippet else {
            return;
        };
        let inside = |offset: usize| session.range.start <= offset && offset <= session.range.end;
        if !self.selections.all().iter().all(|s| inside(s.head)) {
            self.snippet = None;
        }
    }

    pub(super) fn in_snippet(&self) -> bool {
        self.snippet.is_some()
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;
    use crate::editor::tests::{editor, state, type_text, update};

    fn plain(source: &str) -> Snippet {
        expand(source, "\n", &|name| {
            (name == "TM_FILENAME").then(|| "a.rs".into())
        })
    }

    #[test]
    fn parses_stops_placeholders_choices_and_escapes() {
        let s = plain("fn ${1:name}(${2:arg}: ${3|i32,u8|}) {\n    $0\n}");
        assert_eq!(s.text, "fn name(arg: i32) {\n    \n}");
        assert_eq!(
            s.stops,
            [vec![3..7], vec![8..11], vec![13..16], vec![24..24]]
        );

        // Nested placeholders, mirrors, escapes, unknown and known variables.
        let s = plain(
            r"${1:a ${2:b}} $1 \$x \} $TM_FILENAME ${UNKNOWN:dflt} $NOPE ${TM_FILENAME/(.*)/$1/}",
        );
        assert_eq!(s.text, "a b a b $x } a.rs dflt NOPE a.rs");
        assert_eq!(s.stops[0], [0..3, 4..7]);
        // The mirror repeats the nested stop too.
        assert_eq!(s.stops[1], [2..3, 6..7]);
        assert_eq!(s.stops.last().unwrap(), &[s.text.len()..s.text.len()]);

        // Not snippet syntax: kept literally.
        let s = plain("cost: $ 5 ${oops");
        assert_eq!(s.text, "cost: $ 5 ${oops");
        assert_eq!(s.stops, [vec![16..16]]);

        // Lines after the first take the insertion line's indentation.
        let s = expand("if x {\n\t$0\n}", "\r\n  ", &|_| None);
        assert_eq!(s.text, "if x {\r\n  \t\r\n  }");
        assert_eq!(s.stops, [vec![11..11]]);
    }

    #[gpui::test]
    fn tab_moves_between_stops_and_mirrors_edit_together(cx: &mut gpui::TestAppContext) {
        let window = editor("a.rs", "let |", cx);
        update(window, cx, |e, _, cx| {
            e.insert_snippet(4..4, "${1:v} = ${2:f}($1);$0", cx)
        });
        assert_eq!(state(window, cx), "let «v» = f(«v»);");
        assert!(window.read_with(cx, |e, _| e.in_snippet()).unwrap());
        // Typing over the first stop edits its mirror too.
        type_text(window, "total", cx);
        assert_eq!(state(window, cx), "let total| = f(total|);");
        update(window, cx, |e, w, cx| e.next_tab_stop(&NextTabStop, w, cx));
        assert_eq!(state(window, cx), "let total = «f»(total);");
        update(window, cx, |e, w, cx| {
            e.previous_tab_stop(&PreviousTabStop, w, cx)
        });
        assert_eq!(state(window, cx), "let «total» = f(«total»);");
        update(window, cx, |e, w, cx| e.next_tab_stop(&NextTabStop, w, cx));
        update(window, cx, |e, w, cx| e.next_tab_stop(&NextTabStop, w, cx));
        // `$0` ends the session.
        assert_eq!(state(window, cx), "let total = f(total);|");
        assert!(!window.read_with(cx, |e, _| e.in_snippet()).unwrap());
    }

    #[gpui::test]
    fn moving_outside_the_snippet_ends_it(cx: &mut gpui::TestAppContext) {
        let window = editor("a.rs", "x\n|", cx);
        update(window, cx, |e, _, cx| {
            e.insert_snippet(2..2, "f(${1:a}, ${2:b})", cx)
        });
        assert_eq!(state(window, cx), "x\nf(«a», b)");
        update(window, cx, |e, w, cx| e.up(&Up, w, cx));
        assert!(!window.read_with(cx, |e, _| e.in_snippet()).unwrap());
    }
}
