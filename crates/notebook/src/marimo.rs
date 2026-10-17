//! marimo notebooks (`.py`): reading/writing the file layout marimo
//! writes, and marimo's static analysis (definitions and references of a
//! cell) with the dependency graph between cells.
//!
//! The writer is a port of marimo 0.25's `marimo._ast.codegen` (with
//! `toplevel.TopLevelExtraction`): cells whose code is a single function
//! or class depending only on builtins, setup definitions and other such
//! functions are written as `@app.function`/`@app.class_definition`,
//! other cells as `@app.cell def name(refs): … return defs` where only
//! definitions some cell references are returned.

mod analysis;

use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet},
};

use tree_sitter::Node;

use self::analysis::{Detail, VarKind, analyze_detail, fields, is_local, named, parse_python};
use crate::model::{Cell, CellKind, Format, MarimoCell, MarimoCellKind, MarimoInfo, Notebook};

/// `__generated_with` of new notebooks: the marimo version whose layout
/// the writer reproduces.
pub const GENERATED_WITH: &str = "0.25.1";

const DEFAULT_APP: &str = "app = marimo.App()";
const DEFAULT_FOOTER: &str = "if __name__ == \"__main__\":\n    app.run()\n";
const MAX_LINE_LENGTH: usize = 80;
const INDENT: &str = "    ";

/// Whether `text` is a marimo notebook: `import marimo` and
/// `app = marimo.App(` at the top level, and at least the app line.
pub fn is_marimo(text: &str) -> bool {
    let mut import = false;
    let mut app = false;
    for line in text.lines() {
        if line.trim_end() == "import marimo" {
            import = true;
        } else if line.starts_with("app = marimo.App(") {
            app = true;
        }
        if import && app {
            return true;
        }
    }
    false
}

// --- reading ---------------------------------------------------------------

/// The file as rows, with the rows inside multi-line strings marked.
struct Source<'a> {
    text: &'a str,
    rows: Vec<&'a str>,
    row_starts: Vec<usize>,
    /// Rows after the first row of a multi-line string literal.
    protected: Vec<bool>,
}

impl<'a> Source<'a> {
    fn new(text: &'a str, root: Node<'_>) -> Self {
        let mut rows = Vec::new();
        let mut row_starts = Vec::new();
        let mut start = 0;
        for line in text.split('\n') {
            row_starts.push(start);
            start += line.len() + 1;
            rows.push(line.strip_suffix('\r').unwrap_or(line));
        }
        let mut protected = vec![false; rows.len()];
        fn mark(node: Node<'_>, protected: &mut [bool]) {
            if node.kind() == "string" {
                let (start, end) = (node.start_position().row, node.end_position().row);
                for row in start + 1..=end.min(protected.len().saturating_sub(1)) {
                    protected[row] = true;
                }
            }
            let mut cursor = node.walk();
            let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
            for child in children {
                mark(child, protected);
            }
        }
        mark(root, &mut protected);
        Self {
            text,
            rows,
            row_starts,
            protected,
        }
    }

    fn node_text(&self, node: Node<'_>) -> &'a str {
        &self.text[node.byte_range()]
    }

    /// Rows `from..=to` dedented as marimo's `fixed_dedent`, without
    /// leading and trailing blank lines.
    fn dedent_rows(&self, from: usize, to: isize) -> String {
        if to < from as isize {
            return String::new();
        }
        let to = (to as usize).min(self.rows.len() - 1);
        let rows: Vec<&str> = self.rows[from..=to].to_vec();
        let protected = &self.protected[from..=to];
        dedent_lines(&rows, protected)
    }

    fn indent_of(&self, row: usize) -> &'a str {
        let line = self.rows[row];
        &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
    }

    /// The last row of a block ending at `end_row`, extended over the
    /// following rows that are blank or indented by `indent` (trailing
    /// comments), stopping before `limit`.
    fn extend_block(&self, end_row: usize, indent: &str, limit: usize) -> usize {
        let mut end = end_row;
        if indent.is_empty() {
            return end;
        }
        for row in end_row + 1..limit.min(self.rows.len()) {
            let line = self.rows[row];
            if line.trim().is_empty() || line.starts_with(indent) {
                end = row;
            } else {
                break;
            }
        }
        end
    }
}

/// marimo's `fixed_dedent` (then blank lines trimmed at both ends).
fn dedent_lines(rows: &[&str], protected: &[bool]) -> String {
    let leading = |line: &str| line.len() - line.trim_start_matches([' ', '\t']).len();
    let indent = rows
        .iter()
        .zip(protected)
        .find(|(line, protected)| !**protected && !line.trim().is_empty())
        .map(|(line, _)| &line[..leading(line)]);
    let Some(indent) = indent else {
        return String::new();
    };
    let refilled: Vec<Cow<'_, str>> = rows
        .iter()
        .zip(protected)
        .map(|(line, protected)| {
            if *protected || line.starts_with(indent) {
                Cow::Borrowed(*line)
            } else {
                Cow::Owned(format!("{indent}{line}"))
            }
        })
        .collect();
    let shift = refilled
        .iter()
        .zip(protected)
        .filter(|(line, protected)| !**protected && !line.trim().is_empty())
        .map(|(line, _)| leading(line))
        .min()
        .unwrap_or(0);
    // Multi-line string blocks indented less than the code keep their
    // whitespace.
    let mut keep = vec![false; refilled.len()];
    let mut i = 0;
    while i < refilled.len() {
        if !protected[i] {
            i += 1;
            continue;
        }
        let mut j = i;
        let mut block_min = usize::MAX;
        while j < refilled.len() && protected[j] {
            if !refilled[j].trim().is_empty() {
                block_min = block_min.min(leading(&refilled[j]));
            }
            j += 1;
        }
        if block_min != usize::MAX && block_min < shift {
            keep[i..j].fill(true);
        }
        i = j;
    }
    let lines: Vec<&str> = refilled
        .iter()
        .zip(&keep)
        .map(|(line, keep)| {
            if *keep {
                line.as_ref()
            } else {
                let strip = line
                    .bytes()
                    .take(shift)
                    .take_while(|b| *b == b' ' || *b == b'\t')
                    .count();
                &line[strip..]
            }
        })
        .collect();
    trim_blank_lines(&lines.join("\n")).to_owned()
}

fn trim_blank_lines(text: &str) -> &str {
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        if line.trim().is_empty() && line.ends_with('\n') {
            start += line.len();
        } else {
            break;
        }
    }
    let text = &text[start..];
    let mut end = text.len();
    while end > 0 {
        let line_start = text[..end].rfind('\n').map_or(0, |i| i + 1);
        if text[line_start..end].trim().is_empty() {
            end = line_start.saturating_sub(1);
            if line_start == 0 {
                end = 0;
            }
        } else {
            break;
        }
    }
    &text[..end]
}

pub fn parse(text: &str) -> Result<Notebook, String> {
    let tree = parse_python(text);
    let root = tree.root_node();
    let src = Source::new(text, root);
    let mut cursor = root.walk();
    let nodes: Vec<Node<'_>> = root.named_children(&mut cursor).collect();

    let import = nodes
        .iter()
        .position(|node| {
            node.kind() == "import_statement"
                && node.start_position().column == 0
                && src.node_text(*node).split_whitespace().collect::<Vec<_>>()
                    == ["import", "marimo"]
        })
        .ok_or("Not a marimo notebook: no top-level `import marimo`")?;
    let header = text[..src.row_starts[nodes[import].start_position().row]].to_owned();

    let mut info = MarimoInfo {
        header,
        ..MarimoInfo::default()
    };
    let mut cells = Vec::new();
    let mut footer_start: Option<usize> = None;
    let mut app_found = false;
    for (index, &node) in nodes.iter().enumerate().skip(import + 1) {
        if node.kind() == "comment" {
            continue;
        }
        let next_row = nodes[index + 1..]
            .iter()
            .find(|n| n.kind() != "comment")
            .map_or(src.rows.len(), |n| n.start_position().row);
        if let Some(cell) = read_cell(&src, node, next_row) {
            cells.push(cell);
            footer_start = None;
            continue;
        }
        if let Some((name, value)) = assignment(&src, node) {
            if name == "__generated_with" && !app_found {
                info.generated_with = string_value(&src, value).map(|(value, _)| value);
                continue;
            }
            if name == "app"
                && value.kind() == "call"
                && value
                    .child_by_field_name("function")
                    .is_some_and(|f| src.node_text(f) == "marimo.App")
                && !app_found
            {
                info.app = src.node_text(node).to_owned();
                app_found = true;
                continue;
            }
        }
        if app_found && footer_start.is_none() {
            footer_start = Some(src.row_starts[node.start_position().row]);
        }
    }
    if !app_found {
        return Err("Not a marimo notebook: no `app = marimo.App(...)`".into());
    }
    info.footer = footer_start.map_or_else(String::new, |start| text[start..].to_owned());
    Ok(Notebook {
        cells,
        format: Format::Marimo(info),
    })
}

/// `name = value` at the top level.
fn assignment<'t>(src: &Source<'_>, node: Node<'t>) -> Option<(String, Node<'t>)> {
    if node.kind() != "expression_statement" {
        return None;
    }
    let assignment = named(node).into_iter().next()?;
    if assignment.kind() != "assignment" {
        return None;
    }
    let left = assignment.child_by_field_name("left")?;
    let right = assignment.child_by_field_name("right")?;
    (left.kind() == "identifier").then(|| (src.node_text(left).to_owned(), right))
}

/// `app.<attr>` or `app.<attr>(kwargs)`: the attribute and the kwargs.
fn app_call<'t>(src: &Source<'_>, node: Node<'t>) -> Option<(&'static str, Option<Node<'t>>)> {
    let (target, arguments) = if node.kind() == "call" {
        (
            node.child_by_field_name("function")?,
            node.child_by_field_name("arguments"),
        )
    } else {
        (node, None)
    };
    let attr = match src.node_text(target) {
        "app.cell" => "cell",
        "app.function" => "function",
        "app.class_definition" => "class_definition",
        "app.setup" => "setup",
        _ => return None,
    };
    Some((attr, arguments))
}

/// Keyword arguments as `key=value` texts.
fn kwargs(src: &Source<'_>, arguments: Option<Node<'_>>) -> Vec<(String, String)> {
    let Some(arguments) = arguments else {
        return Vec::new();
    };
    named(arguments)
        .into_iter()
        .filter(|arg| arg.kind() == "keyword_argument")
        .filter_map(|arg| {
            let name = arg.child_by_field_name("name")?;
            let value = arg.child_by_field_name("value")?;
            Some((
                src.node_text(name).to_owned(),
                src.node_text(value).to_owned(),
            ))
        })
        .collect()
}

fn join_kwargs(kwargs: &[(String, String)]) -> String {
    kwargs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn colon_row(node: Node<'_>) -> Option<usize> {
    let mut cursor = node.walk();
    let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
    children
        .iter()
        .rev()
        .find(|child| child.kind() == ":")
        .map(|colon| colon.start_position().row)
}

fn marimo_cell(code: String, name: String, kind: MarimoCellKind, config: String) -> Cell {
    let mut cell = Cell::new(CellKind::Code, code);
    if kind == MarimoCellKind::Cell
        && let Some(markdown) = extract_markdown(&cell.source)
    {
        let code = std::mem::replace(&mut cell.source, markdown.text.clone());
        cell.kind = CellKind::Markdown;
        cell.marimo = Some(MarimoCell {
            name,
            kind,
            config,
            original: Some((markdown.text, code)),
        });
        return cell;
    }
    cell.marimo = Some(MarimoCell {
        name,
        kind,
        config,
        original: None,
    });
    cell
}

fn read_cell(src: &Source<'_>, node: Node<'_>, next_row: usize) -> Option<Cell> {
    match node.kind() {
        "decorated_definition" => {
            let decorators: Vec<Node<'_>> = named(node)
                .into_iter()
                .filter(|child| child.kind() == "decorator")
                .collect();
            let (decorator, attr, arguments) = decorators.iter().find_map(|decorator| {
                let expression = named(*decorator).into_iter().next()?;
                let (attr, arguments) = app_call(src, expression)?;
                Some((*decorator, attr, arguments))
            })?;
            let definition = node.child_by_field_name("definition")?;
            let name = src
                .node_text(definition.child_by_field_name("name")?)
                .to_owned();
            let config = join_kwargs(&kwargs(src, arguments));
            match attr {
                "cell" if definition.kind() == "function_definition" => {
                    let code = cell_body(src, definition, next_row);
                    Some(marimo_cell(code, name, MarimoCellKind::Cell, config))
                }
                "function" | "class_definition" => {
                    let body = definition.child_by_field_name("body")?;
                    let first = named(body).into_iter().next();
                    let indent = first.map_or("", |first| {
                        if first.start_position().row == definition.start_position().row {
                            ""
                        } else {
                            src.indent_of(first.start_position().row)
                        }
                    });
                    let end = src.extend_block(definition.end_position().row, indent, next_row);
                    let code = src.dedent_rows(decorator.end_position().row + 1, end as isize);
                    let kind = if attr == "function" {
                        MarimoCellKind::Function
                    } else {
                        MarimoCellKind::Class
                    };
                    Some(marimo_cell(code, name, kind, config))
                }
                _ => None,
            }
        }
        "with_statement" => {
            let clause = named(node)
                .into_iter()
                .find(|child| child.kind() == "with_clause")?;
            let item = named(clause).into_iter().next()?;
            let value = item.child_by_field_name("value")?;
            let ("setup", arguments) = app_call(src, value)? else {
                return None;
            };
            let body = node.child_by_field_name("body")?;
            let start = colon_row(node)? + 1;
            let first = named(body).into_iter().next()?;
            let indent = src.indent_of(first.start_position().row);
            let end = src.extend_block(body.end_position().row, indent, next_row);
            let code = src.dedent_rows(start, end as isize);
            let code = code
                .strip_suffix("\npass")
                .map_or(code.clone(), str::to_owned);
            let config = join_kwargs(&kwargs(src, arguments));
            Some(marimo_cell(
                code,
                "setup".into(),
                MarimoCellKind::Setup,
                config,
            ))
        }
        "expression_statement" => {
            let call = named(node).into_iter().next()?;
            if call.kind() != "call"
                || call
                    .child_by_field_name("function")
                    .is_none_or(|f| src.node_text(f) != "app._unparsable_cell")
            {
                return None;
            }
            let arguments = call.child_by_field_name("arguments")?;
            let code = named(arguments)
                .into_iter()
                .find(|arg| arg.kind() != "keyword_argument")
                .and_then(|arg| string_value(src, arg))
                .map(|(value, _)| unparsable_code(&value))
                .unwrap_or_default();
            let mut name = "_".to_owned();
            let mut config = Vec::new();
            for (key, value) in kwargs(src, Some(arguments)) {
                if key == "name" {
                    let tree = parse_python(&value);
                    let root = tree.root_node();
                    let literal = first_string(root);
                    let value_src = Source::new(&value, root);
                    if let Some((literal, _)) = literal.and_then(|n| string_value(&value_src, n)) {
                        name = literal;
                    }
                } else {
                    config.push((key, value));
                }
            }
            Some(marimo_cell(
                code,
                name,
                MarimoCellKind::Unparsable,
                join_kwargs(&config),
            ))
        }
        _ => None,
    }
}
/// The code of `app._unparsable_cell(r"""\n<code>\n""")`: the literal
/// without the newlines the writer adds, dedented. Unlike marimo (which
/// strips it), blank lines inside are kept so the file reads and writes
/// back unchanged.
fn unparsable_code(literal: &str) -> String {
    let body = literal.strip_prefix('\n').unwrap_or(literal);
    let body = body.trim_end_matches([' ', '\t']);
    let body = body.strip_suffix('\n').unwrap_or(body);
    let code = textwrap_dedent(body);
    if code.trim().is_empty() {
        String::new()
    } else {
        code
    }
}

fn first_string(node: Node<'_>) -> Option<Node<'_>> {
    if node.kind() == "string" {
        return Some(node);
    }
    named(node).into_iter().find_map(first_string)
}

/// The code of an `@app.cell` function: its body without the final
/// `return`, dedented.
fn cell_body(src: &Source<'_>, function: Node<'_>, next_row: usize) -> String {
    let Some(body) = function.child_by_field_name("body") else {
        return String::new();
    };
    let statements = named(body);
    let Some(first) = statements.first() else {
        return String::new();
    };
    let Some(colon) = colon_row(function) else {
        return String::new();
    };
    if first.start_position().row == colon {
        // `def _(): return`
        return String::new();
    }
    let last = statements[statements.len() - 1];
    let end = if last.kind() == "return_statement" {
        last.start_position().row as isize - 1
    } else {
        let indent = src.indent_of(first.start_position().row);
        src.extend_block(body.end_position().row, indent, next_row) as isize
    };
    src.dedent_rows(colon + 1, end)
}

/// A string literal's value and prefix (`r`, `f`, …); `None` for byte
/// strings and strings with interpolations.
fn string_value(src: &Source<'_>, node: Node<'_>) -> Option<(String, String)> {
    if node.kind() != "string" {
        return None;
    }
    let children = fields(node);
    let start = children.iter().find(|(_, c)| c.kind() == "string_start")?.1;
    let end = children.iter().find(|(_, c)| c.kind() == "string_end")?.1;
    if children.iter().any(|(_, c)| c.kind() == "interpolation") {
        return None;
    }
    let start_text = src.node_text(start);
    let prefix: String = start_text
        .chars()
        .take_while(|c| *c != '"' && *c != '\'')
        .collect();
    if prefix.contains(['b', 'B']) {
        return None;
    }
    let content = &src.text[start.end_byte()..end.start_byte()];
    let value = if prefix.contains(['r', 'R']) {
        content.to_owned()
    } else {
        unescape(content)
    };
    Some((value, prefix))
}

/// Python string escapes.
fn unescape(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut chars = content.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(next) = chars.next() else {
            out.push('\\');
            break;
        };
        let hex = |n: usize, chars: &mut std::iter::Peekable<std::str::Chars<'_>>| {
            let digits: String = (0..n).filter_map(|_| chars.next()).collect();
            u32::from_str_radix(&digits, 16)
                .ok()
                .and_then(char::from_u32)
        };
        match next {
            '\n' => {}
            '\\' => out.push('\\'),
            '\'' => out.push('\''),
            '"' => out.push('"'),
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            'f' => out.push('\x0c'),
            'v' => out.push('\x0b'),
            'x' => out.extend(hex(2, &mut chars)),
            'u' => out.extend(hex(4, &mut chars)),
            'U' => out.extend(hex(8, &mut chars)),
            '0'..='7' => {
                let mut value = next.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    match chars.peek().and_then(|c| c.to_digit(8)) {
                        Some(d) => {
                            value = value * 8 + d;
                            chars.next();
                        }
                        None => break,
                    }
                }
                out.extend(char::from_u32(value));
            }
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

/// Python's `textwrap.dedent`.
fn textwrap_dedent(text: &str) -> String {
    let is_blank = |line: &str| line.chars().all(|c| c == ' ' || c == '\t');
    let mut margin: Option<&str> = None;
    for line in text.split('\n') {
        if is_blank(line) {
            continue;
        }
        let indent = &line[..line.len() - line.trim_start_matches([' ', '\t']).len()];
        margin = Some(match margin {
            None => indent,
            Some(margin) => {
                let common = margin
                    .bytes()
                    .zip(indent.bytes())
                    .take_while(|(a, b)| a == b)
                    .count();
                &margin[..common]
            }
        });
    }
    let margin = margin.unwrap_or("");
    text.split('\n')
        .map(|line| {
            if is_blank(line) {
                ""
            } else {
                line.strip_prefix(margin).unwrap_or(line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// --- markdown cells --------------------------------------------------------

struct Markdown {
    text: String,
    /// Prefix of the string literal (`r`, ``).
    tag: String,
    /// Byte range of the `mo.md(...)` call in the code.
    call: std::ops::Range<usize>,
}

/// marimo's `extract_markdown`: the Markdown of a cell whose code is one
/// `mo.md("...")` call with a plain or raw string literal.
fn extract_markdown(code: &str) -> Option<Markdown> {
    let stripped = code.trim();
    let calls = stripped
        .split('\n')
        .filter(|line| line.starts_with("mo.md("))
        .count();
    if calls != 1 {
        return None;
    }
    let tree = parse_python(code);
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let [statement] = named(root)[..] else {
        return None;
    };
    if statement.kind() != "expression_statement" {
        return None;
    }
    let [call] = named(statement)[..] else {
        return None;
    };
    if call.kind() != "call" {
        return None;
    }
    let function = call.child_by_field_name("function")?;
    if &code[function.byte_range()] != "mo.md" {
        return None;
    }
    let arguments = call.child_by_field_name("arguments")?;
    let positional: Vec<Node<'_>> = named(arguments)
        .into_iter()
        .filter(|arg| arg.kind() != "keyword_argument")
        .collect();
    let [literal] = positional[..] else {
        return None;
    };
    let src = Source::new(code, root);
    let (value, tag) = string_value(&src, literal)?;
    if tag.contains(['f', 'F', 't', 'T']) {
        return None;
    }
    Some(Markdown {
        text: normalize_markdown(&value),
        tag,
        call: call.byte_range(),
    })
}

/// What marimo keeps of a `mo.md` string: lines right-stripped, dedented,
/// stripped.
fn normalize_markdown(value: &str) -> String {
    let lines: Vec<&str> = value.split('\n').map(str::trim_end).collect();
    let first = lines[0].trim_start_matches([' ', '\t']);
    let rest = textwrap_dedent(&lines[1..].join("\n"));
    format!("{first}\n{rest}").trim().to_owned()
}

/// marimo's `format_markdown`: `code` with its `mo.md(...)` call replaced
/// by `mo.md({tag}"""\n{markdown}\n""")`.
fn format_markdown(code: &str, call: std::ops::Range<usize>, tag: &str, markdown: &str) -> String {
    let markdown = markdown.replace("\"\"", "\"\\\"");
    format!(
        "{}mo.md({tag}\"\"\"\n{markdown}\n\"\"\"){}",
        &code[..call.start],
        &code[call.end..]
    )
}

/// The Python code of a Markdown cell written as marimo writes a cell
/// whose Markdown was edited.
fn markdown_code(markdown: &str, original: Option<&str>) -> String {
    let markdown = normalize_markdown(markdown);
    match original.and_then(|code| Some((code, extract_markdown(code)?))) {
        Some((code, md)) => format_markdown(code, md.call, &md.tag, &markdown),
        None => {
            let code = format!("mo.md(r\"\"\"\n{markdown}\n\"\"\")");
            format_markdown(&code, 0..code.len(), "r", &markdown)
        }
    }
}

// --- writing ---------------------------------------------------------------

/// Writes the notebook as marimo does: header, `import marimo`,
/// `__generated_with`, the app line, each cell as
/// `@app.cell[(config)]\ndef name(refs):\n    body\n    return (defs,)`
/// with refs/defs recomputed by [`analyze`], then the footer.
pub fn to_text(info: &MarimoInfo, cells: &[Cell]) -> String {
    let mut setup: Option<(String, CellConfig, Detail)> = None;
    let mut inputs = Vec::new();
    for cell in cells {
        let meta = cell.marimo.as_ref();
        let kind = meta.map_or(MarimoCellKind::Cell, |m| m.kind);
        let mut name = meta.map_or("_", |m| m.name.as_str()).to_owned();
        if name.is_empty() || name == "__" {
            name = "_".into();
        }
        let config = CellConfig::parse(meta.map_or("", |m| m.config.as_str()));
        let mut code = cell_code(cell).into_owned();
        if cell.kind != CellKind::Markdown
            && kind == MarimoCellKind::Cell
            && let Some(md) = extract_markdown(&code)
        {
            code = format_markdown(&code, md.call, &md.tag, &md.text);
        }
        let is_setup = setup.is_none() && (kind == MarimoCellKind::Setup || name == "setup");
        if is_setup {
            let detail = analyze_detail(&code);
            if detail.syntax_error.is_none() {
                setup = Some((code, config, detail));
                continue;
            }
        }
        // Function/class cells that turn out not to be top-level revert to
        // an anonymous cell, as marimo does.
        let previous_name = if matches!(kind, MarimoCellKind::Function | MarimoCellKind::Class) {
            "_".to_owned()
        } else {
            name
        };
        inputs.push((code, previous_name, config));
    }
    let setup_defs: HashSet<String> = setup.as_ref().map_or_else(HashSet::new, |(_, _, d)| {
        d.defs.iter().filter(|n| !is_local(n)).cloned().collect()
    });
    let extraction = Extraction::new(inputs, setup_defs);
    let blocks: Vec<String> = (0..extraction.statuses.len())
        .map(|i| extraction.serialize(i))
        .collect();

    let mut parts: Vec<String> = Vec::new();
    if !info.header.trim().is_empty() {
        parts.push(info.header.trim_end().to_owned());
        parts.push(String::new());
    }
    parts.push("import marimo".into());
    parts.push(String::new());
    if let Some(version) = &info.generated_with {
        parts.push(format!("__generated_with = \"{version}\""));
    }
    parts.push(if info.app.trim().is_empty() {
        DEFAULT_APP.to_owned()
    } else {
        info.app.clone()
    });
    parts.push(String::new());
    parts.push(setup.map_or_else(String::new, |(code, config, detail)| {
        setup_section(&code, &config, &detail)
    }));
    parts.push(blocks.join("\n\n\n"));
    parts.push("\n".into());
    let footer = if info.footer.trim().is_empty() {
        DEFAULT_FOOTER
    } else {
        info.footer.trim_start()
    };
    let mut text = parts.join("\n");
    text.push('\n');
    text.push_str(footer);
    text.trim_start().to_owned()
}

fn setup_section(code: &str, config: &CellConfig, detail: &Detail) -> String {
    if code.trim().is_empty() {
        return String::new();
    }
    let mut block = code.to_owned();
    let only_comments = code
        .lines()
        .all(|line| line.trim().is_empty() || line.trim().starts_with('#'));
    if only_comments {
        block.push_str("\npass");
    }
    let prefix = if detail.coroutine { "async " } else { "" };
    let line = if config.hide_code {
        format!("{prefix}with app.setup(hide_code=True):")
    } else {
        format!("{prefix}with app.setup:")
    };
    [line, indent_text(&block), "\n".into()].join("\n")
}

/// `textwrap.indent(text, "    ")`: blank lines stay as they are.
fn indent_text(text: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| {
            if line.trim().is_empty() {
                Cow::Borrowed(line)
            } else {
                Cow::Owned(format!("{INDENT}{line}"))
            }
        })
        .collect()
}

/// marimo's `format_tuple_elements`: `code` with `(...)` replaced by the
/// elements, one per line when the line would be too long.
fn format_tuple(code: &str, elems: &[String], indent: bool, allowed_naked: bool) -> String {
    let maybe_indent = |text: String| if indent { indent_text(&text) } else { text };
    if elems.is_empty() {
        return if allowed_naked {
            maybe_indent(code.replace("(...)", "").trim_end().to_owned())
        } else {
            maybe_indent(code.replace("(...)", "()"))
        };
    }
    let mut elems: Vec<String> = elems.to_vec();
    let mut naked = allowed_naked;
    if naked && elems.len() == 1 {
        naked = false;
        elems[0].push(',');
    }
    let joined = elems.join(", ");
    let attempt = if naked {
        code.replace("(...)", &joined).trim_end().to_owned()
    } else {
        code.replace("(...)", &format!("({joined})"))
    };
    let attempt = maybe_indent(attempt);
    if attempt.chars().count() < MAX_LINE_LENGTH {
        return attempt;
    }
    if elems.len() == 1 {
        elems[0] = elems[0].trim_matches(',').to_owned();
    }
    let multiline = format!("(\n{},\n)", indent_text(&elems.join(",\n")));
    maybe_indent(code.replace("(...)", &multiline))
}

/// marimo's `CellConfig`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CellConfig {
    column: Option<String>,
    disabled: bool,
    hide_code: bool,
    expand_output: bool,
}

impl CellConfig {
    fn parse(text: &str) -> Self {
        let mut config = Self::default();
        for arg in split_args(text) {
            let Some((key, value)) = arg.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "column" => config.column = (value != "None").then(|| value.to_owned()),
                "disabled" => config.disabled = value == "True",
                "hide_code" => config.hide_code = value == "True",
                "expand_output" => config.expand_output = value == "True",
                _ => {}
            }
        }
        config
    }

    fn without_defaults(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(column) = &self.column {
            out.push(format!("column={column}"));
        }
        if self.disabled {
            out.push("disabled=True".into());
        }
        if self.hide_code {
            out.push("hide_code=True".into());
        }
        if self.expand_output {
            out.push("expand_output=True".into());
        }
        out
    }

    fn all(&self) -> Vec<String> {
        let py = |b: bool| if b { "True" } else { "False" };
        vec![
            format!("column={}", self.column.as_deref().unwrap_or("None")),
            format!("disabled={}", py(self.disabled)),
            format!("hide_code={}", py(self.hide_code)),
            format!("expand_output={}", py(self.expand_output)),
        ]
    }

    fn decorator(&self, function: &str) -> String {
        let kwargs = self.without_defaults();
        if kwargs.is_empty() {
            format!("@app.{function}")
        } else {
            format_tuple(&format!("@app.{function}(...)"), &kwargs, false, false)
        }
    }
}

/// Splits call arguments at top-level commas.
fn split_args(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut current = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                current.push(c);
                if c == '\\' {
                    current.extend(chars.next());
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    current.push(c);
                }
                '(' | '[' | '{' => {
                    depth += 1;
                    current.push(c);
                }
                ')' | ']' | '}' => {
                    depth -= 1;
                    current.push(c);
                }
                ',' if depth == 0 => out.push(std::mem::take(&mut current)),
                _ => current.push(c),
            },
        }
    }
    out.push(current);
    out.into_iter()
        .map(|arg| arg.trim().to_owned())
        .filter(|arg| !arg.is_empty())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Ty {
    Cell,
    Toplevel,
    Unparsable,
    Unresolved,
}

struct Status {
    code: String,
    name: String,
    previous_name: String,
    config: CellConfig,
    ty: Ty,
    /// `None`: the code does not parse.
    detail: Option<Detail>,
}

impl Status {
    fn nonlocal_defs(&self) -> impl Iterator<Item = &String> {
        self.detail
            .iter()
            .flat_map(|d| d.defs.iter())
            .filter(|name| !is_local(name))
    }

    fn update(
        &mut self,
        allowed: &HashSet<String>,
        unresolved: &HashSet<String>,
        potential: &HashSet<String>,
        toplevel: &HashSet<String>,
    ) {
        let Some(detail) = &self.detail else {
            self.ty = Ty::Unparsable;
            return;
        };
        let Some(name) = &detail.single_definition else {
            self.ty = Ty::Cell;
            return;
        };
        if name.starts_with('_') {
            self.ty = Ty::Cell;
            return;
        }
        self.name = name.clone();
        if matches!(name.as_str(), "app" | "__generated_with" | "__name__")
            || detail.trailing_comment
        {
            self.ty = Ty::Cell;
            return;
        }
        let in_allowed = |r: &String| allowed.contains(r) || r == name || r == "_lambda";
        let unbounded = detail
            .variables
            .get(name)
            .map(|v| &v.unbounded_refs)
            .into_iter()
            .flatten();
        let order_dependent: Vec<&String> = unbounded.filter(|r| !in_allowed(r)).collect();
        if !order_dependent.is_empty() && order_dependent.iter().any(|r| !unresolved.contains(*r)) {
            self.ty = Ty::Cell;
            return;
        }
        let dependent: Vec<&String> = detail
            .refs
            .iter()
            .filter(|r| !in_allowed(r) && !toplevel.contains(*r))
            .collect();
        if dependent.is_empty() {
            self.ty = Ty::Toplevel;
            return;
        }
        if dependent.iter().all(|r| potential.contains(*r)) {
            self.ty = Ty::Unresolved;
            return;
        }
        self.ty = Ty::Cell;
    }
}

/// `TopLevelExtraction`: which cells are top-level functions/classes,
/// what every cell may use without a parameter, what is referenced.
struct Extraction {
    statuses: Vec<Status>,
    allowed: HashSet<String>,
    used_refs: HashSet<String>,
    /// Annotations of variables (`x: int = …`) by name.
    annotations: HashMap<String, (String, HashSet<String>)>,
}

impl Extraction {
    fn new(inputs: Vec<(String, String, CellConfig)>, setup_defs: HashSet<String>) -> Self {
        let mut allowed = setup_defs;
        let mut maps: HashMap<Ty, BTreeMap<String, usize>> = HashMap::new();
        let empty = HashSet::new();
        let mut statuses: Vec<Status> = Vec::new();
        for (index, (code, name, config)) in inputs.into_iter().enumerate() {
            let detail = analyze_detail(&code);
            let detail = detail.syntax_error.is_none().then_some(detail);
            let mut status = Status {
                code,
                name: name.clone(),
                previous_name: name,
                config,
                ty: Ty::Unparsable,
                detail,
            };
            status.update(&allowed, &empty, &empty, &empty);
            maps.entry(status.ty)
                .or_default()
                .insert(status.name.clone(), index);
            statuses.push(status);
        }
        let mut defs = HashSet::new();
        let mut used_refs = HashSet::new();
        for status in &statuses {
            if let Some(detail) = &status.detail {
                defs.extend(status.nonlocal_defs().cloned());
                used_refs.extend(detail.refs.iter().cloned());
            }
        }
        let potential: HashSet<String> = statuses
            .iter()
            .filter(|s| s.ty != Ty::Toplevel)
            .map(|s| s.name.clone())
            .collect();
        allowed.extend(
            analysis::BUILTINS
                .iter()
                .filter(|b| !defs.contains(**b))
                .map(|b| (*b).to_owned()),
        );
        let mut unresolved = HashSet::new();
        for (index, status) in statuses.iter_mut().enumerate() {
            if matches!(status.ty, Ty::Unresolved | Ty::Cell) {
                if let Some(map) = maps.get_mut(&status.ty) {
                    map.remove(&status.name);
                }
                let toplevel: HashSet<String> = maps
                    .get(&Ty::Toplevel)
                    .map(|m| m.keys().cloned().collect())
                    .unwrap_or_default();
                status.update(&allowed, &unresolved, &potential, &toplevel);
                match status.ty {
                    Ty::Toplevel => {
                        allowed.insert(status.name.clone());
                    }
                    Ty::Unresolved => {
                        unresolved.insert(status.name.clone());
                    }
                    _ => {}
                }
                maps.entry(status.ty)
                    .or_default()
                    .insert(status.name.clone(), index);
            } else {
                allowed.insert(status.name.clone());
            }
        }

        // Resolve cells whose dependencies are other candidate functions.
        let mut visited: HashMap<String, bool> = HashMap::new();
        let names: Vec<String> = maps
            .get(&Ty::Unresolved)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        for name in names {
            if maps
                .get(&Ty::Unresolved)
                .is_some_and(|m| m.contains_key(&name))
            {
                resolve(&name, &mut statuses, &mut maps, &mut visited, &mut allowed);
            }
        }

        // Cells keep their own names; as in marimo, only the status
        // registered under a name is renamed back (a duplicated function
        // keeps the function's name).
        for &index in maps.get(&Ty::Cell).into_iter().flat_map(|m| m.values()) {
            statuses[index].name = statuses[index].previous_name.clone();
        }

        let mut annotations = HashMap::new();
        for status in &statuses {
            let Some(detail) = &status.detail else {
                continue;
            };
            for (name, data) in &detail.variables {
                match &data.annotation {
                    Some(annotation) => {
                        annotations.insert(name.clone(), annotation.clone());
                    }
                    None => {
                        annotations.remove(name);
                    }
                }
            }
        }
        for status in &statuses {
            if status.ty == Ty::Toplevel {
                annotations.remove(&status.name);
            }
        }
        Self {
            statuses,
            allowed,
            used_refs,
            annotations,
        }
    }

    fn serialize(&self, index: usize) -> String {
        let status = &self.statuses[index];
        match (status.ty, &status.detail) {
            (Ty::Toplevel, Some(detail)) => {
                let is_class = detail
                    .variables
                    .get(&status.name)
                    .is_some_and(|v| v.kind == VarKind::Class);
                let decorator = status.config.decorator(if is_class {
                    "class_definition"
                } else {
                    "function"
                });
                format!("{decorator}\n{}", status.code.trim())
            }
            (Ty::Cell, Some(detail)) => self.function_def(status, detail),
            _ => unparsable_cell(&status.code, &status.name, &status.config),
        }
    }

    fn function_def(&self, status: &Status, detail: &Detail) -> String {
        let allowed = |name: &String| {
            self.allowed.contains(name) || (detail.defs.contains(name) && !is_local(name))
        };
        let refs: Vec<String> = detail
            .refs
            .iter()
            .filter(|r| !allowed(r))
            .map(|r| match self.annotations.get(r) {
                Some((repr, refs)) => {
                    if refs.iter().all(allowed) {
                        format!("{r}: {repr}")
                    } else {
                        format!("{r}: \"{}\"", escape_unescaped_quotes(repr))
                    }
                }
                None => r.clone(),
            })
            .collect();
        let defs: Vec<String> = detail
            .defs
            .iter()
            .filter(|d| !is_local(d) && self.used_refs.contains(*d))
            .cloned()
            .collect();
        let prefix = if detail.coroutine { "async " } else { "" };
        let mut lines = vec![
            status.config.decorator("cell"),
            format_tuple(
                &format!("{prefix}def {}(...):", status.name),
                &refs,
                false,
                false,
            ),
        ];
        let body = indent_text(&status.code);
        if !body.is_empty() {
            lines.push(body);
        }
        if detail.ends_with_definition {
            lines.push(String::new());
        }
        lines.push(format_tuple("return (...)", &defs, true, true));
        lines.join("\n")
    }
}

fn resolve(
    name: &str,
    statuses: &mut [Status],
    maps: &mut HashMap<Ty, BTreeMap<String, usize>>,
    visited: &mut HashMap<String, bool>,
    allowed: &mut HashSet<String>,
) -> bool {
    let contains = |maps: &HashMap<Ty, BTreeMap<String, usize>>, ty: Ty| {
        maps.get(&ty).is_some_and(|m| m.contains_key(name))
    };
    if !contains(maps, Ty::Unresolved) {
        if let Some(resolved) = visited.get(name) {
            return *resolved;
        }
        if contains(maps, Ty::Toplevel) {
            return true;
        }
        if contains(maps, Ty::Cell) || contains(maps, Ty::Unparsable) {
            return false;
        }
        // A cycle (or a name no cell defines).
        return true;
    }
    let Some(index) = maps.get_mut(&Ty::Unresolved).and_then(|m| m.remove(name)) else {
        return true;
    };
    let refs: Vec<String> = statuses[index]
        .detail
        .as_ref()
        .map(|d| d.refs.iter().cloned().collect())
        .unwrap_or_default();
    let mut resolved = true;
    for r in refs {
        if !resolve(&r, statuses, maps, visited, allowed) {
            resolved = false;
        }
    }
    if resolved {
        statuses[index].ty = Ty::Toplevel;
        allowed.insert(name.to_owned());
    } else {
        statuses[index].ty = Ty::Cell;
    }
    maps.entry(statuses[index].ty)
        .or_default()
        .insert(name.to_owned(), index);
    visited.insert(name.to_owned(), resolved);
    resolved
}

fn escape_unescaped_quotes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev = '\0';
    for c in text.chars() {
        if c == '"' && prev != '\\' {
            out.push('\\');
        }
        out.push(c);
        prev = c;
    }
    out
}

/// marimo's `generate_unparsable_cell`.
fn unparsable_cell(code: &str, name: &str, config: &CellConfig) -> String {
    let literal = if code.contains("\"\"\"") {
        let escaped = code.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"\"\"\n{escaped}\n\"\"\"")
    } else {
        format!("r\"\"\"\n{code}\n\"\"\"")
    };
    let mut flags = if *config == CellConfig::default() {
        Vec::new()
    } else {
        config.all()
    };
    flags.push(format!("name=\"{}\"", name.replace('\\', "\\\\")));
    let mut lines = vec!["app._unparsable_cell(".to_owned()];
    lines.push(indent_text(&format!("{literal},")));
    lines.push(indent_text(&flags.join(", ")));
    lines.push(")".into());
    lines.join("\n")
}

/// A new marimo notebook with one empty cell.
pub fn new_notebook() -> Notebook {
    let mut cell = Cell::new(CellKind::Code, "");
    cell.marimo = Some(MarimoCell {
        name: "_".into(),
        ..MarimoCell::default()
    });
    Notebook {
        cells: vec![cell],
        format: Format::Marimo(MarimoInfo {
            header: String::new(),
            generated_with: Some(GENERATED_WITH.into()),
            app: DEFAULT_APP.into(),
            footer: DEFAULT_FOOTER.into(),
        }),
    }
}

/// The Python code a marimo cell runs (a Markdown cell's `mo.md(...)`).
pub fn cell_code(cell: &Cell) -> Cow<'_, str> {
    if cell.kind != CellKind::Markdown {
        return Cow::Borrowed(&cell.source);
    }
    match cell.marimo.as_ref().and_then(|m| m.original.as_ref()) {
        Some((markdown, code)) if *markdown == cell.source => Cow::Borrowed(code),
        Some((_, code)) => Cow::Owned(markdown_code(&cell.source, Some(code))),
        None => Cow::Owned(markdown_code(&cell.source, None)),
    }
}

// --- analysis --------------------------------------------------------------

/// A cell's static analysis.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Analysis {
    /// Global names the cell defines (assignments, imports, `def`,
    /// `class`, `for`/`with` targets… at its top level), private `_x`
    /// names included.
    pub defs: BTreeSet<String>,
    /// Names the cell uses without defining them first (free variables,
    /// nested scopes included), builtins included.
    pub refs: BTreeSet<String>,
    /// The parse error, when the code is not valid Python.
    pub syntax_error: Option<String>,
}

impl Analysis {
    /// `defs` without private (`_`-prefixed) names: what the cell
    /// publishes and returns.
    pub fn public_defs(&self) -> impl Iterator<Item = &str> {
        self.defs
            .iter()
            .map(String::as_str)
            .filter(|name| !name.starts_with('_'))
    }
}

/// marimo's analysis of `code` (`marimo._ast.compiler.compile_cell`'s
/// `defs` plus private names, and `refs`). Code that does not parse has
/// no definitions or references.
pub fn analyze(code: &str) -> Analysis {
    let detail = analyze_detail(code);
    Analysis {
        defs: detail.defs,
        refs: detail.refs,
        syntax_error: detail.syntax_error,
    }
}

/// Python builtins (not cell parameters unless a cell defines them).
pub fn is_builtin(name: &str) -> bool {
    analysis::is_builtin(name)
}

/// Why a cell cannot run, as marimo reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellError {
    Syntax(String),
    /// `names` are also defined by `cells` (indices).
    MultipleDefinitions {
        names: Vec<String>,
        cells: Vec<usize>,
    },
    /// The cell is on a cycle through `cells` (indices).
    Cycle {
        cells: Vec<usize>,
    },
}

fn cell_numbers(cells: &[usize]) -> String {
    cells
        .iter()
        .map(|c| (c + 1).to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

impl std::fmt::Display for CellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CellError::Syntax(message) => f.write_str(message),
            CellError::MultipleDefinitions { names, cells } => {
                let quoted: Vec<String> = names.iter().map(|n| format!("'{n}'")).collect();
                if names.len() == 1 {
                    write!(f, "The variable {} was defined by another cell", quoted[0])?;
                } else {
                    write!(
                        f,
                        "The variables {} were defined by another cell",
                        quoted.join(", ")
                    )?;
                }
                let plural = if cells.len() == 1 { "" } else { "s" };
                write!(f, " (also defined by cell{plural} {})", cell_numbers(cells))
            }
            CellError::Cycle { cells } => {
                write!(f, "This cell is in a cycle (cells {})", cell_numbers(cells))
            }
        }
    }
}

/// Dependencies between cells: cell B depends on cell A when A defines a
/// public name B references.
///
/// A setup cell (`with app.setup:`) is the first cell of the notebook, so
/// the notebook-order tie-break runs it first; its definitions reach the
/// cells referencing them through ordinary edges.
#[derive(Debug, Clone, Default)]
pub struct Graph {
    errors: Vec<Option<CellError>>,
    parents: Vec<Vec<usize>>,
    children: Vec<Vec<usize>>,
    /// Strongly connected component of each cell.
    component: Vec<usize>,
    /// Cells of each component, in notebook order.
    components: Vec<Vec<usize>>,
}

impl Graph {
    pub fn new(analyses: &[Analysis]) -> Self {
        let n = analyses.len();
        let mut definers: HashMap<&str, Vec<usize>> = HashMap::new();
        for (cell, analysis) in analyses.iter().enumerate() {
            for name in analysis.public_defs() {
                definers.entry(name).or_default().push(cell);
            }
        }
        let mut parents = vec![BTreeSet::new(); n];
        let mut children = vec![BTreeSet::new(); n];
        for (cell, analysis) in analyses.iter().enumerate() {
            for name in &analysis.refs {
                for &definer in definers.get(name.as_str()).into_iter().flatten() {
                    if definer != cell {
                        parents[cell].insert(definer);
                        children[definer].insert(cell);
                    }
                }
            }
        }
        let parents: Vec<Vec<usize>> = parents
            .into_iter()
            .map(|s| s.into_iter().collect())
            .collect();
        let children: Vec<Vec<usize>> = children
            .into_iter()
            .map(|s| s.into_iter().collect())
            .collect();
        let (component, components) = strongly_connected(&children);

        let mut errors: Vec<Option<CellError>> = vec![None; n];
        for (cell, analysis) in analyses.iter().enumerate() {
            if let Some(message) = &analysis.syntax_error {
                errors[cell] = Some(CellError::Syntax(message.clone()));
                continue;
            }
            let mut names = Vec::new();
            let mut others = BTreeSet::new();
            for name in analysis.public_defs() {
                let cells = &definers[name];
                if cells.len() > 1 {
                    names.push(name.to_owned());
                    others.extend(cells.iter().copied().filter(|c| *c != cell));
                }
            }
            if !names.is_empty() {
                errors[cell] = Some(CellError::MultipleDefinitions {
                    names,
                    cells: others.into_iter().collect(),
                });
                continue;
            }
            let members = &components[component[cell]];
            if members.len() > 1 {
                errors[cell] = Some(CellError::Cycle {
                    cells: members.clone(),
                });
            }
        }
        Self {
            errors,
            parents,
            children,
            component,
            components,
        }
    }

    pub fn error(&self, cell: usize) -> Option<&CellError> {
        self.errors.get(cell).and_then(Option::as_ref)
    }

    /// Cells defining names `cell` references.
    pub fn parents(&self, cell: usize) -> &[usize] {
        self.parents.get(cell).map_or(&[], Vec::as_slice)
    }

    /// Cells referencing names `cell` defines.
    pub fn children(&self, cell: usize) -> &[usize] {
        self.children.get(cell).map_or(&[], Vec::as_slice)
    }

    /// Every transitive parent of `cell`, in run order.
    pub fn ancestors(&self, cell: usize) -> Vec<usize> {
        let mut seen = vec![false; self.parents.len()];
        let mut stack: Vec<usize> = self.parents(cell).to_vec();
        while let Some(next) = stack.pop() {
            if next == cell || std::mem::replace(&mut seen[next], true) {
                continue;
            }
            stack.extend_from_slice(self.parents(next));
        }
        self.order(&seen)
    }

    /// `cells` and every cell depending on them transitively, each once,
    /// in an order where each cell comes after the cells it depends on
    /// (ties: notebook order). Cells in error are included (the caller
    /// reports their error instead of running them); cells on a cycle come
    /// in notebook order.
    pub fn run_order(&self, cells: &[usize]) -> Vec<usize> {
        let mut seen = vec![false; self.parents.len()];
        let mut stack: Vec<usize> = cells.iter().copied().filter(|c| *c < seen.len()).collect();
        while let Some(next) = stack.pop() {
            if std::mem::replace(&mut seen[next], true) {
                continue;
            }
            stack.extend_from_slice(self.children(next));
        }
        self.order(&seen)
    }

    /// All cells in run order (Run All).
    pub fn topological(&self) -> Vec<usize> {
        self.order(&vec![true; self.parents.len()])
    }

    /// The cells in `subset` in dependency order (components as units,
    /// ready components by their first cell).
    fn order(&self, subset: &[bool]) -> Vec<usize> {
        let count = self.components.len();
        let mut in_subset = vec![false; count];
        for (cell, &included) in subset.iter().enumerate() {
            if included {
                in_subset[self.component[cell]] = true;
            }
        }
        let mut indegree = vec![0usize; count];
        let mut edges: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); count];
        for (cell, children) in self.children.iter().enumerate() {
            let from = self.component[cell];
            if !in_subset[from] {
                continue;
            }
            for &child in children {
                let to = self.component[child];
                if to != from && in_subset[to] && edges[from].insert(to) {
                    indegree[to] += 1;
                }
            }
        }
        let mut ready: BinaryHeap<Reverse<(usize, usize)>> = (0..count)
            .filter(|&c| in_subset[c] && indegree[c] == 0)
            .map(|c| Reverse((self.components[c][0], c)))
            .collect();
        let mut out = Vec::new();
        while let Some(Reverse((_, c))) = ready.pop() {
            out.extend(
                self.components[c]
                    .iter()
                    .copied()
                    .filter(|cell| subset[*cell]),
            );
            for &to in &edges[c] {
                indegree[to] -= 1;
                if indegree[to] == 0 {
                    ready.push(Reverse((self.components[to][0], to)));
                }
            }
        }
        out
    }
}

/// Tarjan's strongly connected components (iterative): the component of
/// each node and the members of each component in ascending order.
fn strongly_connected(edges: &[Vec<usize>]) -> (Vec<usize>, Vec<Vec<usize>>) {
    let n = edges.len();
    let mut index = vec![usize::MAX; n];
    let mut low = vec![0; n];
    let mut on_stack = vec![false; n];
    let mut stack = Vec::new();
    let mut component = vec![usize::MAX; n];
    let mut components: Vec<Vec<usize>> = Vec::new();
    let mut next_index = 0;
    for start in 0..n {
        if index[start] != usize::MAX {
            continue;
        }
        // (node, next edge position)
        let mut work = vec![(start, 0usize)];
        index[start] = next_index;
        low[start] = next_index;
        next_index += 1;
        stack.push(start);
        on_stack[start] = true;
        while let Some(&mut (node, ref mut position)) = work.last_mut() {
            if let Some(&next) = edges[node].get(*position) {
                *position += 1;
                if index[next] == usize::MAX {
                    index[next] = next_index;
                    low[next] = next_index;
                    next_index += 1;
                    stack.push(next);
                    on_stack[next] = true;
                    work.push((next, 0));
                } else if on_stack[next] {
                    low[node] = low[node].min(index[next]);
                }
                continue;
            }
            work.pop();
            if let Some(&(parent, _)) = work.last() {
                low[parent] = low[parent].min(low[node]);
            }
            if low[node] == index[node] {
                let id = components.len();
                let mut members = Vec::new();
                while let Some(member) = stack.pop() {
                    on_stack[member] = false;
                    component[member] = id;
                    members.push(member);
                    if member == node {
                        break;
                    }
                }
                members.sort_unstable();
                components.push(members);
            }
        }
    }
    (component, components)
}

#[cfg(test)]
mod tests;
