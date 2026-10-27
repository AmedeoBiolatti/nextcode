//! A language server for next_code workflow files (YAML), over any byte
//! stream: next_code runs it in-process for YAML documents, so it works
//! the same for local, WSL and dev-container projects. It knows the format,
//! not only its shape: next_code's own problems as diagnostics, completion
//! of keys for where the cursor is, values, step ids in `depends_on` and
//! `${steps.…}`/`${inputs.…}` variables, hovers with the schema's
//! descriptions and step and input summaries, go to definition, references
//! and rename of step and input ids, and the steps as document symbols.
//!
//! Only workflow files are served ([`is_workflow`]); other YAML documents
//! get nothing.

mod analysis;
mod context;
mod docs;
mod features;

use std::{
    collections::HashMap,
    io::{self, BufReader, Read, Write},
    ops::Range,
};

use lsp::codec;
use serde_json::{Value, json};

/// The name the server reports (`serverInfo`).
pub const NAME: &str = "next_code workflows";

/// Whether the document at `uri` with `text` is a workflow file: a YAML
/// file in a `next_code/workflows` or `.next_code/workflows` folder, or one
/// whose first lines point a YAML language server at the workflow schema.
pub fn is_workflow(uri: &str, text: &str) -> bool {
    let uri = uri.to_ascii_lowercase().replace("%2e", ".");
    let yaml = uri.ends_with(".yaml") || uri.ends_with(".yml");
    let folder = uri.contains("next_code/workflows/");
    let modeline = text
        .lines()
        .take(5)
        .any(|line| line.contains("yaml-language-server:") && line.contains(workflow::SCHEMA_FILE));
    (yaml && folder) || modeline
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Utf8,
    Utf16,
}

struct Document {
    text: String,
    workflow: bool,
}

struct Server<W: Write> {
    writer: W,
    encoding: Encoding,
    documents: HashMap<String, Document>,
}

/// Serves one client until it says `exit` or the stream ends.
pub fn serve(reader: impl Read, writer: impl Write) -> io::Result<()> {
    let mut reader = BufReader::new(reader);
    let mut server = Server {
        writer,
        encoding: Encoding::Utf16,
        documents: HashMap::new(),
    };
    while let Some(body) = codec::read_message(&mut reader)? {
        let Ok(message) = serde_json::from_slice::<Value>(&body) else {
            continue;
        };
        if !server.handle(&message)? {
            break;
        }
    }
    Ok(())
}

/// Byte offset of an LSP position in `text`, clamped to its line.
fn offset(text: &str, position: &Value, encoding: Encoding) -> usize {
    let line = position["line"].as_u64().unwrap_or(0) as usize;
    let character = position["character"].as_u64().unwrap_or(0) as usize;
    let mut start = 0;
    for _ in 0..line {
        match text[start..].find('\n') {
            Some(at) => start += at + 1,
            None => return text.len(),
        }
    }
    let end = text[start..].find('\n').map_or(text.len(), |at| start + at);
    let line_text = &text[start..end];
    match encoding {
        Encoding::Utf8 => {
            let mut at = character.min(line_text.len());
            while !line_text.is_char_boundary(at) {
                at -= 1;
            }
            start + at
        }
        Encoding::Utf16 => {
            let mut units = 0;
            for (at, c) in line_text.char_indices() {
                if units >= character {
                    return start + at;
                }
                units += c.len_utf16();
            }
            end
        }
    }
}

/// The LSP position of byte `offset` in `text`.
fn position(text: &str, offset: usize, encoding: Encoding) -> Value {
    let offset = offset.min(text.len());
    let before = &text[..offset];
    let line = before.matches('\n').count();
    let line_start = before.rfind('\n').map_or(0, |at| at + 1);
    let character = match encoding {
        Encoding::Utf8 => offset - line_start,
        Encoding::Utf16 => before[line_start..].encode_utf16().count(),
    };
    json!({ "line": line, "character": character })
}

impl<W: Write> Server<W> {
    fn send(&mut self, message: Value) -> io::Result<()> {
        codec::write_message(&mut self.writer, &serde_json::to_vec(&message)?)
    }

    fn range(&self, text: &str, range: &Range<usize>) -> Value {
        json!({
            "start": position(text, range.start, self.encoding),
            "end": position(text, range.end, self.encoding),
        })
    }

    /// Handles one message; `false` once the client said `exit`.
    fn handle(&mut self, message: &Value) -> io::Result<bool> {
        let Some(method) = message["method"].as_str() else {
            return Ok(true); // A response to nothing we asked.
        };
        let params = &message["params"];
        let Some(id) = message.get("id").cloned() else {
            return self.notification(method, params);
        };
        let result = self.request(method, params);
        let reply = match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, error)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": code, "message": error },
            }),
        };
        self.send(reply)?;
        Ok(true)
    }

    fn notification(&mut self, method: &str, params: &Value) -> io::Result<bool> {
        let uri = params["textDocument"]["uri"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        match method {
            "exit" => return Ok(false),
            "textDocument/didOpen" => {
                let text = params["textDocument"]["text"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned();
                let workflow = is_workflow(&uri, &text);
                self.documents
                    .insert(uri.clone(), Document { text, workflow });
                self.publish(&uri)?;
            }
            "textDocument/didChange" => {
                let encoding = self.encoding;
                if let Some(document) = self.documents.get_mut(&uri) {
                    for change in params["contentChanges"].as_array().into_iter().flatten() {
                        let new = change["text"].as_str().unwrap_or("");
                        match change.get("range") {
                            Some(range) => {
                                let start = offset(&document.text, &range["start"], encoding);
                                let end = offset(&document.text, &range["end"], encoding);
                                document.text.replace_range(start..end.max(start), new);
                            }
                            None => document.text = new.to_owned(),
                        }
                    }
                    document.workflow = is_workflow(&uri, &document.text);
                    self.publish(&uri)?;
                }
            }
            "textDocument/didClose" => {
                // Its problems go with it.
                let workflow = self
                    .documents
                    .remove(&uri)
                    .is_some_and(|document| document.workflow);
                if workflow {
                    self.send(json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/publishDiagnostics",
                        "params": { "uri": uri, "diagnostics": [] },
                    }))?;
                }
            }
            _ => {}
        }
        Ok(true)
    }

    /// Sends the document's problems (none for a file that is not a
    /// workflow).
    fn publish(&mut self, uri: &str) -> io::Result<()> {
        let Some(document) = self.documents.get(uri) else {
            return Ok(());
        };
        let diagnostics: Vec<Value> = if document.workflow {
            features::diagnostics(&document.text)
                .into_iter()
                .map(|(range, message)| {
                    json!({
                        "range": self.range(&document.text, &range),
                        "severity": 1,
                        "source": NAME,
                        "message": message,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": { "uri": uri, "diagnostics": diagnostics },
        }))
    }

    fn request(&mut self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        match method {
            "initialize" => {
                let utf8 = params["capabilities"]["general"]["positionEncodings"]
                    .as_array()
                    .is_some_and(|encodings| encodings.iter().any(|encoding| encoding == "utf-8"));
                self.encoding = if utf8 {
                    Encoding::Utf8
                } else {
                    Encoding::Utf16
                };
                return Ok(json!({
                    "capabilities": {
                        "positionEncoding": if utf8 { "utf-8" } else { "utf-16" },
                        "textDocumentSync": { "openClose": true, "change": 1 },
                        "completionProvider": { "triggerCharacters": [".", "{", "$", " ", "["] },
                        "hoverProvider": true,
                        "definitionProvider": true,
                        "referencesProvider": true,
                        "renameProvider": { "prepareProvider": true },
                        "documentSymbolProvider": true,
                    },
                    "serverInfo": { "name": NAME, "version": env!("CARGO_PKG_VERSION") },
                }));
            }
            "shutdown" => return Ok(Value::Null),
            _ => {}
        }
        let uri = params["textDocument"]["uri"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        let Some(document) = self.documents.get(&uri) else {
            return match method {
                "textDocument/completion"
                | "textDocument/references"
                | "textDocument/documentSymbol"
                | "textDocument/hover"
                | "textDocument/definition"
                | "textDocument/prepareRename"
                | "textDocument/rename" => Ok(Value::Null),
                _ => Err((-32601, format!("unknown method `{method}`"))),
            };
        };
        if !document.workflow {
            return Ok(Value::Null);
        }
        let text = document.text.as_str();
        let at = offset(text, &params["position"], self.encoding);
        let location =
            |range: &Range<usize>| json!({ "uri": uri, "range": self.range(text, range) });
        Ok(match method {
            "textDocument/completion" => {
                let items: Vec<Value> = features::complete(text, at)
                    .into_iter()
                    .enumerate()
                    .map(|(index, item)| {
                        json!({
                            "label": item.label,
                            "kind": match item.kind {
                                features::Kind::Key => 10,      // Property
                                features::Kind::Value => 12,    // Value
                                features::Kind::Step => 3,      // Function
                                features::Kind::Input => 6,     // Variable
                                features::Kind::Variable => 6,
                            },
                            "detail": item.detail,
                            "documentation": item.documentation.map(|text| json!({ "kind": "markdown", "value": text })),
                            "sortText": format!("{index:04}"),
                            "filterText": item.label,
                            "insertTextFormat": if item.snippet { 2 } else { 1 },
                            "textEdit": { "range": self.range(text, &item.replace), "newText": item.insert },
                        })
                    })
                    .collect();
                json!({ "isIncomplete": false, "items": items })
            }
            "textDocument/hover" => match features::hover(text, at) {
                Some((markdown, range)) => json!({
                    "contents": { "kind": "markdown", "value": markdown },
                    "range": self.range(text, &range),
                }),
                None => Value::Null,
            },
            "textDocument/definition" => {
                features::definition(text, at).map_or(Value::Null, |range| location(&range))
            }
            "textDocument/references" => {
                let declaration = params["context"]["includeDeclaration"]
                    .as_bool()
                    .unwrap_or(true);
                let found: Vec<Value> = features::references(text, at, declaration)
                    .iter()
                    .map(location)
                    .collect();
                json!(found)
            }
            "textDocument/prepareRename" => match features::prepare_rename(text, at) {
                Some((range, placeholder)) => json!({
                    "range": self.range(text, &range),
                    "placeholder": placeholder,
                }),
                None => Value::Null,
            },
            "textDocument/rename" => {
                let new_name = params["newName"].as_str().unwrap_or("");
                let edits = features::rename(text, at, new_name).map_err(|err| (-32803, err))?;
                let edits: Vec<Value> = edits
                    .iter()
                    .map(|(range, new)| json!({ "range": self.range(text, range), "newText": new }))
                    .collect();
                json!({ "changes": { uri.clone(): edits } })
            }
            "textDocument/documentSymbol" => {
                fn symbol<W: Write>(
                    server: &Server<W>,
                    text: &str,
                    found: &features::Symbol,
                ) -> Value {
                    json!({
                        "name": found.name,
                        "detail": found.detail,
                        "kind": match found.kind {
                            features::SymbolKind::Step => 12,   // Function
                            features::SymbolKind::Group => 3,   // Namespace
                            features::SymbolKind::Inputs => 19, // Object
                            features::SymbolKind::Input => 13,  // Variable
                        },
                        "range": server.range(text, &found.range),
                        "selectionRange": server.range(text, &found.selection),
                        "children": found.children.iter().map(|child| symbol(server, text, child)).collect::<Vec<_>>(),
                    })
                }
                let symbols: Vec<Value> = features::symbols(text)
                    .iter()
                    .map(|found| symbol(self, text, found))
                    .collect();
                json!(symbols)
            }
            _ => return Err((-32601, format!("unknown method `{method}`"))),
        })
    }
}

#[cfg(test)]
mod tests;
