//! What an agent (or a pick) points at, and how it is written in JSON and
//! as plain references (`nextcode://ui/<path>`, `src/main.rs:12:5-12:18`).

use std::ops::Range;

use serde_json::{Value, json};

/// The scheme that marks a UI path in plain-text references.
pub const UI_SCHEME: &str = "nextcode://ui/";

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// A pinned element, by exact path.
    Element {
        path: String,
    },
    /// Characters of an element's text.
    Text {
        path: String,
        select: TextSelect,
    },
    Code(CodeWant),
    Terminal(TerminalWant),
}

#[derive(Clone, Debug, PartialEq)]
pub enum TextSelect {
    /// The `nth` (1-based) occurrence of `text`.
    Match { text: String, nth: usize },
    /// Byte offsets into the element's text.
    Range(Range<usize>),
}

/// Code in a file: 1-based lines, columns in characters, `end_column`
/// inclusive. No column: whole line(s); no end line: `line`; a column but
/// no end column: to the end of the end line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeWant {
    /// As the agent gave it: absolute or project-relative, `/` or `\`.
    pub file: String,
    /// 1-based notebook cell.
    pub cell: Option<usize>,
    pub line: u32,
    pub column: Option<u32>,
    pub end_line: Option<u32>,
    pub end_column: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalRef {
    /// 1-based, in the terminal panel's tab order.
    Index(usize),
    Active,
}

/// Terminal text: either a match (`text`, `nth`) or a line/column range
/// (1-based, same rules as [`CodeWant`]; lines count from the top of the
/// visible screen).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalWant {
    pub terminal: TerminalRef,
    pub text: Option<String>,
    /// 1-based occurrence of `text`.
    pub nth: usize,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub end_line: Option<u32>,
    pub end_column: Option<u32>,
}

impl Target {
    /// Reads a JSON target: an object (see the module docs of
    /// `pinpoint`) or a string reference (see [`parse_reference`]).
    /// `Find` targets are resolved by the caller: `Ok(Err(query))`.
    pub fn from_json(value: &Value) -> Result<Result<Target, String>, String> {
        if let Some(text) = value.as_str() {
            return parse_reference(text).map(Ok);
        }
        let Some(object) = value.as_object() else {
            return Err(format!(
                "a target is an object like {{\"path\": \"menu-bar/run\"}} or a string reference, not {value}"
            ));
        };
        let string = |key: &str| object.get(key).and_then(Value::as_str);
        let number =
            |key: &str| -> Result<Option<u64>, String> {
                match object.get(key) {
                    None | Some(Value::Null) => Ok(None),
                    Some(value) => value.as_u64().filter(|n| *n > 0).map(Some).ok_or_else(|| {
                        format!("\"{key}\" must be a positive integer, got {value}")
                    }),
                }
            };
        let u32_of = |key: &str| -> Result<Option<u32>, String> {
            number(key)?
                .map(|n| u32::try_from(n).map_err(|_| format!("\"{key}\" is too large")))
                .transpose()
        };
        if let Some(query) = string("find") {
            return Ok(Err(query.to_owned()));
        }
        if let Some(file) = string("file") {
            let line = u32_of("line")?.ok_or("a code target needs \"line\" (1-based)")?;
            return Ok(Ok(Target::Code(CodeWant {
                file: file.to_owned(),
                cell: number("cell")?.map(|n| n as usize),
                line,
                column: u32_of("column")?,
                end_line: u32_of("end_line")?,
                end_column: u32_of("end_column")?,
            })));
        }
        if let Some(terminal) = object.get("terminal") {
            let terminal = match terminal {
                Value::String(s) if s == "active" => TerminalRef::Active,
                value => TerminalRef::Index(
                    value
                        .as_u64()
                        .filter(|n| *n > 0)
                        .ok_or("\"terminal\" is a 1-based number or \"active\"")?
                        as usize,
                ),
            };
            let want = TerminalWant {
                terminal,
                text: string("text").map(str::to_owned),
                nth: number("nth")?.unwrap_or(1) as usize,
                line: u32_of("line")?,
                column: u32_of("column")?,
                end_line: u32_of("end_line")?,
                end_column: u32_of("end_column")?,
            };
            if want.text.is_none() && want.line.is_none() {
                return Err("a terminal target needs \"text\" or \"line\"".into());
            }
            return Ok(Ok(Target::Terminal(want)));
        }
        let Some(path) = string("path") else {
            return Err(format!(
                "a target needs one of \"path\", \"find\", \"file\" or \"terminal\"; got {value}"
            ));
        };
        let path = path.strip_prefix(UI_SCHEME).unwrap_or(path).to_owned();
        if let Some(text) = string("text") {
            return Ok(Ok(Target::Text {
                path,
                select: TextSelect::Match {
                    text: text.to_owned(),
                    nth: number("nth")?.unwrap_or(1) as usize,
                },
            }));
        }
        if let Some(range) = object.get("range") {
            let bounds = range
                .as_array()
                .filter(|pair| pair.len() == 2)
                .and_then(|pair| Some((pair[0].as_u64()?, pair[1].as_u64()?)))
                .filter(|(start, end)| start < end)
                .ok_or("\"range\" is [start, end): byte offsets with start < end")?;
            return Ok(Ok(Target::Text {
                path,
                select: TextSelect::Range(bounds.0 as usize..bounds.1 as usize),
            }));
        }
        Ok(Ok(Target::Element { path }))
    }

    /// The JSON form, as `highlight` replies echo it.
    pub fn to_json(&self) -> Value {
        match self {
            Target::Element { path } => json!({ "path": path }),
            Target::Text {
                path,
                select: TextSelect::Match { text, nth },
            } => json!({ "path": path, "text": text, "nth": nth }),
            Target::Text {
                path,
                select: TextSelect::Range(range),
            } => json!({ "path": path, "range": [range.start, range.end] }),
            Target::Code(code) => {
                let mut value = json!({ "file": code.file, "line": code.line });
                for (key, field) in [
                    ("column", code.column),
                    ("end_line", code.end_line),
                    ("end_column", code.end_column),
                ] {
                    if let Some(n) = field {
                        value[key] = n.into();
                    }
                }
                if let Some(cell) = code.cell {
                    value["cell"] = cell.into();
                }
                value
            }
            Target::Terminal(terminal) => {
                let mut value = json!({
                    "terminal": match terminal.terminal {
                        TerminalRef::Active => json!("active"),
                        TerminalRef::Index(n) => json!(n),
                    },
                });
                if let Some(text) = &terminal.text {
                    value["text"] = text.clone().into();
                    value["nth"] = terminal.nth.into();
                }
                for (key, field) in [
                    ("line", terminal.line),
                    ("column", terminal.column),
                    ("end_line", terminal.end_line),
                    ("end_column", terminal.end_column),
                ] {
                    if let Some(n) = field {
                        value[key] = n.into();
                    }
                }
                value
            }
        }
    }

    /// The pinned element the target lives in, for element and text
    /// targets.
    pub fn path(&self) -> Option<&str> {
        match self {
            Target::Element { path } | Target::Text { path, .. } => Some(path),
            _ => None,
        }
    }
}

/// A plain-text reference: `nextcode://ui/<path>` (an element), a code
/// reference (`file:line[:col[-[line:]col]]`, `file:line-line`, with an
/// optional `#cell=<n>` after the file), else an element path.
pub fn parse_reference(text: &str) -> Result<Target, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("empty target".into());
    }
    if let Some(path) = text.strip_prefix(UI_SCHEME) {
        let Some((path, fragment)) = path.split_once("#text=") else {
            return Ok(Target::Element {
                path: path.to_owned(),
            });
        };
        let (text, nth) = match fragment.rsplit_once("&nth=") {
            Some((text, nth)) => (
                text,
                nth.parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("bad \"&nth=\" in {text:?}"))?,
            ),
            None => (fragment, 1),
        };
        return Ok(Target::Text {
            path: path.to_owned(),
            select: TextSelect::Match {
                text: percent_decode(text),
                nth,
            },
        });
    }
    Ok(parse_code_reference(text)
        .map(Target::Code)
        .unwrap_or_else(|| Target::Element {
            path: text.to_owned(),
        }))
}

/// The reference of characters in an element's text:
/// `nextcode://ui/<path>#text=<text>&nth=<n>` (`&nth` omitted for the
/// first), escaping `%`, `#`, `&`, spaces and line breaks.
pub fn text_reference(path: &str, text: &str, nth: usize) -> String {
    let mut reference = format!("{UI_SCHEME}{path}#text=");
    for c in text.chars() {
        match c {
            '%' | '#' | '&' | ' ' | '\n' | '\r' | '\t' => {
                reference.push_str(&format!("%{:02X}", c as u32))
            }
            c => reference.push(c),
        }
    }
    if nth > 1 {
        reference.push_str(&format!("&nth={nth}"));
    }
    reference
}

/// Undoes `%XX` escapes (invalid ones stay as they are).
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = text
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `file:line[:col[-[line:]col]]` or `file:line-line`, with an optional
/// `#cell=<n>` after the file. `None` when `text` does not end like one.
pub fn parse_code_reference(text: &str) -> Option<CodeWant> {
    // From the right: the file may hold `:` (Windows drives, `kind:key`).
    let (head, range) = split_range(text)?;
    let (file, cell) = match head.rsplit_once("#cell=") {
        Some((file, cell)) => (file, Some(cell.parse::<usize>().ok().filter(|n| *n > 0)?)),
        None => (head, None),
    };
    if file.is_empty() {
        return None;
    }
    let (line, column, end_line, end_column) = range;
    Some(CodeWant {
        file: file.to_owned(),
        cell,
        line,
        column,
        end_line,
        end_column,
    })
}

type LineRange = (u32, Option<u32>, Option<u32>, Option<u32>);

/// Splits `head:<range>` where range is `L`, `L-L`, `L:C`, `L:C-C` or
/// `L:C-L:C`.
fn split_range(text: &str) -> Option<(&str, LineRange)> {
    let number = |s: &str| -> Option<u32> {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
            .filter(|n| *n > 0)
    };
    let (rest, last) = text.rsplit_once(':')?;
    // `L`, `L-L`, or the `C`, `C-C`, `C-L` tail of a longer form.
    if let Some(n) = number(last) {
        // `…:L:C` (or `…:L-L:C`? not a form) else `…:L`.
        if let Some((head, line)) = rest.rsplit_once(':')
            && let Some(line_n) = number(line)
            && !head.is_empty()
        {
            // `file:L:C`, unless `head:L` is itself `file:L:C-L` (handled
            // below through the dash form).
            return Some((head, (line_n, Some(n), None, None)));
        }
        if let Some((head, line)) = rest.rsplit_once(':')
            && let Some((start_column, end_line)) = line.split_once('-')
            && let (Some(_), Some(_)) = (number(start_column), number(end_line))
        {
            // `file:L:C-L:C`.
            let (head2, start_line) = head.rsplit_once(':')?;
            let start_line = number(start_line)?;
            return Some((
                head2,
                (start_line, number(start_column), number(end_line), Some(n)),
            ));
        }
        return Some((rest, (n, None, None, None)));
    }
    let (a, b) = last.split_once('-')?;
    let (a, b) = (number(a)?, number(b)?);
    // `file:L:C-C` or `file:L-L`.
    if let Some((head, line)) = rest.rsplit_once(':')
        && let Some(line) = number(line)
        && !head.is_empty()
    {
        return Some((head, (line, Some(a), Some(line), Some(b))));
    }
    Some((rest, (a, None, Some(b), None)))
}

/// The reference text of a code range (what pickers copy).
pub fn code_reference(code: &CodeWant) -> String {
    let mut text = code.file.clone();
    if let Some(cell) = code.cell {
        text.push_str(&format!("#cell={cell}"));
    }
    text.push_str(&format!(":{}", code.line));
    match (code.column, code.end_line, code.end_column) {
        (Some(column), end_line, Some(end_column)) => text.push_str(&format!(
            ":{column}-{}:{end_column}",
            end_line.unwrap_or(code.line)
        )),
        (Some(column), _, None) => text.push_str(&format!(":{column}")),
        (None, Some(end_line), _) if end_line != code.line => {
            text.push_str(&format!("-{end_line}"))
        }
        _ => {}
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(text: &str) -> Option<(String, Option<usize>, LineRange)> {
        parse_code_reference(text)
            .map(|c| (c.file, c.cell, (c.line, c.column, c.end_line, c.end_column)))
    }

    #[test]
    fn code_references() {
        assert_eq!(
            code("src/main.rs:12"),
            Some(("src/main.rs".into(), None, (12, None, None, None)))
        );
        assert_eq!(
            code("src/main.rs:12:5"),
            Some(("src/main.rs".into(), None, (12, Some(5), None, None)))
        );
        assert_eq!(
            code("src/main.rs:12:5-12:18"),
            Some((
                "src/main.rs".into(),
                None,
                (12, Some(5), Some(12), Some(18))
            ))
        );
        assert_eq!(
            code("src/main.rs:12:5-14:3"),
            Some(("src/main.rs".into(), None, (12, Some(5), Some(14), Some(3))))
        );
        assert_eq!(
            code("src/main.rs:12:5-9"),
            Some(("src/main.rs".into(), None, (12, Some(5), Some(12), Some(9))))
        );
        assert_eq!(
            code("src/main.rs:3-7"),
            Some(("src/main.rs".into(), None, (3, None, Some(7), None)))
        );
        assert_eq!(
            code(r"C:\work\a.rs:4:2"),
            Some((r"C:\work\a.rs".into(), None, (4, Some(2), None, None)))
        );
        assert_eq!(
            code("nb.ipynb#cell=3:2:5-2:9"),
            Some(("nb.ipynb".into(), Some(3), (2, Some(5), Some(2), Some(9))))
        );
        assert_eq!(code("src/main.rs"), None);
        assert_eq!(code("src/main.rs:0"), None);
        assert_eq!(code(":12"), None);
    }

    #[test]
    fn references_round_trip() {
        for text in [
            "src/main.rs:12",
            "src/main.rs:12:5",
            "src/main.rs:12:5-12:18",
            "src/main.rs:12:5-14:3",
            "src/a.rs:3-7",
            "nb.ipynb#cell=3:2:5-2:9",
        ] {
            assert_eq!(code_reference(&parse_code_reference(text).unwrap()), text);
        }
    }

    #[test]
    fn plain_references() {
        assert_eq!(
            parse_reference("nextcode://ui/workspace/title-bar/run"),
            Ok(Target::Element {
                path: "workspace/title-bar/run".into()
            })
        );
        // A path whose last key is a number reads as code unless marked:
        // agents use the scheme (or a JSON object) for those.
        assert!(matches!(
            parse_reference("notebook:a.ipynb/cell:3"),
            Ok(Target::Code(_))
        ));
        assert_eq!(
            parse_reference("workspace/left-dock/panel:files"),
            Ok(Target::Element {
                path: "workspace/left-dock/panel:files".into()
            })
        );
        assert!(parse_reference("  ").is_err());
        let reference = text_reference("a/b", "50% & #1 x", 2);
        assert_eq!(
            reference,
            "nextcode://ui/a/b#text=50%25%20%26%20%231%20x&nth=2"
        );
        assert_eq!(
            parse_reference(&reference),
            Ok(Target::Text {
                path: "a/b".into(),
                select: TextSelect::Match {
                    text: "50% & #1 x".into(),
                    nth: 2
                }
            })
        );
        assert_eq!(
            parse_reference(&text_reference("p", "héllo", 1)),
            Ok(Target::Text {
                path: "p".into(),
                select: TextSelect::Match {
                    text: "héllo".into(),
                    nth: 1
                }
            })
        );
    }

    #[test]
    fn json_targets() {
        let parse = |value: Value| Target::from_json(&value);
        assert_eq!(
            parse(json!({"path": "a/b", "text": "Run", "nth": 2})),
            Ok(Ok(Target::Text {
                path: "a/b".into(),
                select: TextSelect::Match {
                    text: "Run".into(),
                    nth: 2
                }
            }))
        );
        assert_eq!(
            parse(json!({"path": "a", "range": [1, 4]})),
            Ok(Ok(Target::Text {
                path: "a".into(),
                select: TextSelect::Range(1..4)
            }))
        );
        assert_eq!(parse(json!({"find": "Commit"})), Ok(Err("Commit".into())));
        assert!(
            parse(json!({"file": "a.rs"}))
                .unwrap_err()
                .contains("\"line\"")
        );
        assert!(
            parse(json!({"path": "a", "range": [4, 1]}))
                .unwrap_err()
                .contains("start < end")
        );
        assert!(parse(json!({"line": 3})).unwrap_err().contains("one of"));
        assert!(parse(json!(3)).is_err());
        let terminal = parse(json!({"terminal": "active", "text": "error"}));
        assert!(matches!(terminal, Ok(Ok(Target::Terminal(_)))));
        for value in [
            json!({"path": "a/b"}),
            json!({"path": "a", "text": "x", "nth": 1}),
            json!({"file": "a.rs", "line": 3, "column": 2, "end_line": 4, "end_column": 1}),
            json!({"terminal": 2, "line": 1}),
        ] {
            let target = parse(value.clone()).unwrap().unwrap();
            assert_eq!(target.to_json(), value);
        }
    }
}
