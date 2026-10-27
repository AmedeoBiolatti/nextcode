//! `.ipynb` (nbformat 4.x) reading and writing.
//!
//! The writer reproduces nbformat's `writes`: `split_lines` (sources,
//! stream text and text-like mime values as lists of lines), then
//! `json.dumps(nb, sort_keys=True, indent=1, ensure_ascii=False)`, so a
//! notebook saved by Jupyter (or VS Code, which writes the same layout)
//! reads and writes back byte for byte.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Number, Value, json};

use crate::model::{Cell, CellKind, Format, IpynbInfo, Metadata, Notebook, Output};
use crate::outputs::{bundle_from_json, is_json_mime};

/// Reads an nbformat 4 notebook. Empty (or whitespace-only) text is a new
/// notebook ([`new_notebook`]).
pub fn parse(text: &str) -> Result<Notebook, String> {
    if text.trim().is_empty() {
        return Ok(new_notebook());
    }
    let root: Value =
        serde_json::from_str(text).map_err(|error| format!("Invalid notebook JSON: {error}"))?;
    let Value::Object(mut root) = root else {
        return Err("Invalid notebook: the JSON is not an object".into());
    };
    let nbformat = root
        .remove("nbformat")
        .and_then(|v| v.as_u64())
        .unwrap_or(4);
    if nbformat != 4 {
        return Err(format!(
            "Unsupported notebook format {nbformat} (only nbformat 4 is supported)"
        ));
    }
    let nbformat_minor = root
        .remove("nbformat_minor")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let metadata = match root.remove("metadata") {
        Some(Value::Object(metadata)) => metadata,
        _ => Metadata::new(),
    };
    let cells = match root.remove("cells") {
        Some(Value::Array(cells)) => cells
            .into_iter()
            .enumerate()
            .map(|(index, cell)| parse_cell(cell).map_err(|e| format!("Cell {}: {e}", index + 1)))
            .collect::<Result<Vec<_>, _>>()?,
        None => Vec::new(),
        Some(_) => return Err("Invalid notebook: `cells` is not a list".into()),
    };
    let info = IpynbInfo {
        nbformat,
        nbformat_minor,
        metadata,
        extra: root,
        indent: detect_indent(text),
        trailing_newline: text.ends_with('\n'),
    };
    Ok(Notebook {
        cells,
        format: Format::Ipynb(info),
    })
}

fn detect_indent(text: &str) -> usize {
    text.lines()
        .find(|line| line.starts_with(' '))
        .map_or(1, |line| line.len() - line.trim_start_matches(' ').len())
}

/// Joins nbformat's multi-line string (a string or a list of strings).
fn join_text(value: Option<Value>) -> String {
    match value {
        Some(Value::String(text)) => text,
        Some(Value::Array(lines)) => lines.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

fn parse_cell(cell: Value) -> Result<Cell, String> {
    let Value::Object(mut cell) = cell else {
        return Err("not an object".into());
    };
    let kind = match cell.remove("cell_type").as_ref().and_then(Value::as_str) {
        Some(name) => {
            CellKind::from_name(name).ok_or_else(|| format!("unknown cell type `{name}`"))?
        }
        None => return Err("missing `cell_type`".into()),
    };
    let source = join_text(cell.remove("source"));
    let id = match cell.remove("id") {
        Some(Value::String(id)) => Some(id),
        _ => None,
    };
    let metadata = match cell.remove("metadata") {
        Some(Value::Object(metadata)) => metadata,
        _ => Metadata::new(),
    };
    let attachments = cell.remove("attachments");
    let (execution_count, outputs) = if kind == CellKind::Code {
        let count = cell.remove("execution_count").and_then(|v| v.as_u64());
        let outputs = match cell.remove("outputs") {
            Some(Value::Array(outputs)) => outputs.iter().filter_map(parse_output).collect(),
            _ => Vec::new(),
        };
        (count, outputs)
    } else {
        (None, Vec::new())
    };
    Ok(Cell {
        kind,
        source,
        id,
        execution_count,
        outputs,
        metadata,
        attachments,
        extra: cell,
        marimo: None,
    })
}

/// An nbformat output object (also the `outputs` state of ipywidgets'
/// Output widget); `None` for unknown output types.
pub fn parse_output(output: &Value) -> Option<Output> {
    let str_field = |key: &str| {
        output
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let metadata = || {
        output
            .get("metadata")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    };
    let data = || bundle_from_json(output.get("data").unwrap_or(&Value::Null));
    Some(match output.get("output_type")?.as_str()? {
        "stream" => Output::Stream {
            name: str_field("name"),
            text: join_text(output.get("text").cloned()),
        },
        "display_data" => Output::DisplayData {
            data: data(),
            metadata: metadata(),
            display_id: None,
        },
        "execute_result" => Output::ExecuteResult {
            execution_count: output.get("execution_count").and_then(Value::as_u64),
            data: data(),
            metadata: metadata(),
        },
        "error" => Output::Error {
            ename: str_field("ename"),
            evalue: str_field("evalue"),
            traceback: output
                .get("traceback")
                .and_then(Value::as_array)
                .map(|lines| {
                    lines
                        .iter()
                        .filter_map(|line| line.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        },
        _ => return None,
    })
}

/// Writes the notebook as Jupyter does: keys sorted, `info.indent` spaces
/// per level, multi-line strings as lists of lines, non-ASCII unescaped,
/// a trailing newline when the file had one.
pub fn to_text(info: &IpynbInfo, cells: &[Cell]) -> String {
    let mut root = info.extra.clone();
    root.insert(
        "cells".into(),
        Value::Array(cells.iter().map(|cell| cell_json(info, cell)).collect()),
    );
    root.insert("metadata".into(), Value::Object(info.metadata.clone()));
    root.insert("nbformat".into(), info.nbformat.into());
    root.insert("nbformat_minor".into(), info.nbformat_minor.into());
    let mut out = String::new();
    write_json(&mut out, &Value::Object(root), info.indent.max(1), 0);
    if info.trailing_newline {
        out.push('\n');
    }
    out
}

fn cell_json(info: &IpynbInfo, cell: &Cell) -> Value {
    let mut map = cell.extra.clone();
    map.insert("cell_type".into(), cell.kind.name().into());
    match (&cell.id, info.nbformat_minor >= 5) {
        (Some(id), _) => {
            map.insert("id".into(), id.clone().into());
        }
        (None, true) => {
            map.insert("id".into(), new_cell_id().into());
        }
        (None, false) => {}
    }
    map.insert("metadata".into(), Value::Object(cell.metadata.clone()));
    map.insert("source".into(), split_lines(&cell.source));
    if let Some(attachments) = &cell.attachments {
        let mut attachments = attachments.clone();
        if let Value::Object(attachments) = &mut attachments {
            for bundle in attachments.values_mut() {
                if let Value::Object(bundle) = bundle {
                    split_bundle(bundle);
                }
            }
        }
        map.insert("attachments".into(), attachments);
    }
    if cell.kind == CellKind::Code {
        map.insert(
            "execution_count".into(),
            cell.execution_count.map_or(Value::Null, Value::from),
        );
        map.insert(
            "outputs".into(),
            Value::Array(cell.outputs.iter().map(output_json).collect()),
        );
    }
    Value::Object(map)
}

/// An output as nbformat writes it.
pub fn output_json(output: &Output) -> Value {
    let bundle = |data: &crate::model::MimeBundle| {
        let mut map: Map<String, Value> =
            data.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        split_bundle(&mut map);
        Value::Object(map)
    };
    match output {
        Output::Stream { name, text } => json!({
            "name": name,
            "output_type": "stream",
            "text": split_lines(text),
        }),
        Output::DisplayData { data, metadata, .. } => json!({
            "data": bundle(data),
            "metadata": metadata,
            "output_type": "display_data",
        }),
        Output::ExecuteResult {
            execution_count,
            data,
            metadata,
        } => json!({
            "data": bundle(data),
            "execution_count": execution_count,
            "metadata": metadata,
            "output_type": "execute_result",
        }),
        Output::Error {
            ename,
            evalue,
            traceback,
        } => json!({
            "ename": ename,
            "evalue": evalue,
            "output_type": "error",
            "traceback": traceback,
        }),
    }
}

/// nbformat's `_split_mimebundle`: text-like string values become lists of
/// lines.
fn split_bundle(bundle: &mut Map<String, Value>) {
    for (mime, value) in bundle.iter_mut() {
        if let Value::String(text) = value
            && !is_json_mime(mime)
            && (mime.starts_with("text/")
                || mime == "application/javascript"
                || mime == "image/svg+xml")
        {
            *value = split_lines(text);
        }
    }
}

/// Python's `str.splitlines(True)` as a JSON list.
fn split_lines(text: &str) -> Value {
    Value::Array(
        splitlines_keepends(text)
            .map(|line| Value::String(line.to_owned()))
            .collect(),
    )
}

/// Python's `str.splitlines(keepends=True)`.
pub fn splitlines_keepends(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let mut end = rest.len();
        for (i, c) in rest.char_indices() {
            let len = match c {
                '\r' if rest[i + 1..].starts_with('\n') => 2,
                '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}'
                | '\u{2028}' | '\u{2029}' => c.len_utf8(),
                _ => continue,
            };
            end = i + len;
            break;
        }
        let (line, tail) = rest.split_at(end);
        rest = tail;
        Some(line)
    })
}

/// `json.dumps(value, sort_keys=True, indent=indent, ensure_ascii=False)`.
fn write_json(out: &mut String, value: &Value, indent: usize, level: usize) {
    let newline = |out: &mut String, level: usize| {
        out.push('\n');
        out.extend(std::iter::repeat_n(' ', indent * level));
    };
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => write_number(out, n),
        Value::String(s) => write_string(out, s),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, level + 1);
                write_json(out, item, indent, level + 1);
            }
            newline(out, level);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            // Byte order of UTF-8 is code point order, as Python sorts.
            entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, level + 1);
                write_string(out, key);
                out.push_str(": ");
                write_json(out, item, indent, level + 1);
            }
            newline(out, level);
            out.push('}');
        }
    }
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    let mut start = 0;
    for (i, c) in s.char_indices() {
        let escape = match c {
            '"' => "\\\"",
            '\\' => "\\\\",
            '\n' => "\\n",
            '\r' => "\\r",
            '\t' => "\\t",
            '\x08' => "\\b",
            '\x0c' => "\\f",
            c if (c as u32) < 0x20 => "",
            _ => continue,
        };
        out.push_str(&s[start..i]);
        if escape.is_empty() {
            out.push_str(&format!("\\u{:04x}", c as u32));
        } else {
            out.push_str(escape);
        }
        start = i + c.len_utf8();
    }
    out.push_str(&s[start..]);
    out.push('"');
}

fn write_number(out: &mut String, n: &Number) {
    if let Some(i) = n.as_i64() {
        out.push_str(&i.to_string());
    } else if let Some(u) = n.as_u64() {
        out.push_str(&u.to_string());
    } else if let Some(f) = n.as_f64() {
        out.push_str(&python_float_repr(f));
    } else {
        out.push_str(&n.to_string());
    }
}

/// Python's `repr(float)`: the shortest round-tripping digits, scientific
/// notation outside `1e-4 <= |x| < 1e16`.
pub fn python_float_repr(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    // Rust's `{:e}` gives the shortest round-tripping digits.
    let sci = format!("{f:e}");
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(m) => ("-", m),
        None => ("", mantissa),
    };
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let decpt = exponent + 1;
    let mut out = String::from(sign);
    if !(-4 < decpt && decpt <= 16) {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let exp = decpt - 1;
        out.push_str(&format!(
            "e{}{:02}",
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        ));
    } else if decpt <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-decpt) as usize));
        out.push_str(&digits);
    } else {
        let decpt = decpt as usize;
        if decpt >= digits.len() {
            out.push_str(&digits);
            out.extend(std::iter::repeat_n('0', decpt - digits.len()));
            out.push_str(".0");
        } else {
            out.push_str(&digits[..decpt]);
            out.push('.');
            out.push_str(&digits[decpt..]);
        }
    }
    out
}

/// An empty nbformat 4.5 notebook (Python 3 kernelspec and
/// language_info) with one empty code cell.
pub fn new_notebook() -> Notebook {
    let metadata = json!({
        "kernelspec": {
            "display_name": "Python 3 (ipykernel)",
            "language": "python",
            "name": "python3",
        },
        "language_info": {"name": "python"},
    });
    let Value::Object(metadata) = metadata else {
        unreachable!()
    };
    let mut cell = Cell::new(CellKind::Code, "");
    cell.id = Some(new_cell_id());
    Notebook {
        cells: vec![cell],
        format: Format::Ipynb(IpynbInfo {
            nbformat: 4,
            nbformat_minor: 5,
            metadata,
            extra: Metadata::new(),
            indent: 1,
            trailing_newline: true,
        }),
    }
}

/// A fresh nbformat cell id (8 lowercase hex chars... unique per process run).
pub fn new_cell_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    static SEED: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        nanos ^ (u64::from(std::process::id()) << 32)
    });
    // The low 32 bits of a bijective mix of a per-process counter: unique
    // for the first 2^32 ids of a run.
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let seed = *SEED;
    let x = (n.wrapping_add(seed) as u32) ^ ((seed >> 32) as u32);
    format!("{:08x}", mix32(x))
}

/// A bijection on `u32` (lowbias32).
fn mix32(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    x
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn float_repr_matches_python() {
        // `python3 -c 'print(json.dumps([...]))'`
        let cases = [
            (1e-05, "1e-05"),
            (0.1, "0.1"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (123456789.123, "123456789.123"),
            (1.5e-4, "0.00015"),
            (2.0, "2.0"),
            (-0.0, "-0.0"),
            (0.0, "0.0"),
            (1e300, "1e+300"),
            (5e-324, "5e-324"),
            (0.0001, "0.0001"),
            (-2.5e-7, "-2.5e-07"),
            (1.2345678901234567e19, "1.2345678901234567e+19"),
        ];
        for (f, expected) in cases {
            assert_eq!(python_float_repr(f), expected, "{f}");
        }
    }

    #[test]
    fn splitlines_like_python() {
        let lines: Vec<&str> = splitlines_keepends("a\nb\r\nc\rd\u{2028}e\x0cf").collect();
        assert_eq!(lines, ["a\n", "b\r\n", "c\r", "d\u{2028}", "e\x0c", "f"]);
        assert_eq!(splitlines_keepends("").count(), 0);
        assert_eq!(splitlines_keepends("x\n").collect::<Vec<_>>(), ["x\n"]);
    }

    #[test]
    fn escapes_like_python() {
        let mut out = String::new();
        write_string(&mut out, "q\" b\\ \n\r\t\x08\x0c\x01\x1b\x7f é 🎉");
        assert_eq!(
            out,
            "\"q\\\" b\\\\ \\n\\r\\t\\b\\f\\u0001\\u001b\x7f é 🎉\""
        );
    }

    #[test]
    fn cell_ids_are_unique_hex() {
        let ids: HashSet<String> = (0..10_000).map(|_| new_cell_id()).collect();
        assert_eq!(ids.len(), 10_000);
        assert!(ids.iter().all(|id| {
            id.len() == 8
                && id
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        }));
    }

    #[test]
    fn empty_text_is_a_new_notebook() {
        let notebook = parse("  \n").unwrap();
        assert_eq!(notebook.cells.len(), 1);
        assert_eq!(notebook.cells[0].kind, CellKind::Code);
        let text = notebook.to_text();
        let id = notebook.cells[0].id.as_deref().unwrap();
        assert_eq!(
            text,
            format!(
                "{{\n \"cells\": [\n  {{\n   \"cell_type\": \"code\",\n   \"execution_count\": null,\n   \"id\": \"{id}\",\n   \"metadata\": {{}},\n   \"outputs\": [],\n   \"source\": []\n  }}\n ],\n \"metadata\": {{\n  \"kernelspec\": {{\n   \"display_name\": \"Python 3 (ipykernel)\",\n   \"language\": \"python\",\n   \"name\": \"python3\"\n  }},\n  \"language_info\": {{\n   \"name\": \"python\"\n  }}\n }},\n \"nbformat\": 4,\n \"nbformat_minor\": 5\n}}\n"
            )
        );
        assert_eq!(parse(&text).unwrap(), notebook);
    }

    #[test]
    fn errors() {
        assert!(parse("{").unwrap_err().contains("Invalid notebook JSON"));
        assert!(parse("[]").is_err());
        assert!(
            parse(r#"{"nbformat": 3, "cells": []}"#)
                .unwrap_err()
                .contains("format 3")
        );
    }
}
