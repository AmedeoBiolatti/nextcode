use std::{
    io::{BufReader, PipeReader, PipeWriter},
    thread,
};

use super::*;

/// A client talking to the server on a thread over pipes.
struct Client {
    writer: PipeWriter,
    reader: BufReader<PipeReader>,
    next_id: i64,
    server: Option<thread::JoinHandle<io::Result<()>>>,
}

impl Client {
    fn start() -> Self {
        let (server_reader, writer) = io::pipe().unwrap();
        let (reader, server_writer) = io::pipe().unwrap();
        let server = thread::spawn(move || serve(server_reader, server_writer));
        Self {
            writer,
            reader: BufReader::new(reader),
            next_id: 1,
            server: Some(server),
        }
    }

    fn send(&mut self, message: Value) {
        codec::write_message(&mut self.writer, &serde_json::to_vec(&message).unwrap()).unwrap();
    }

    fn receive(&mut self) -> Value {
        let body = codec::read_message(&mut self.reader)
            .unwrap()
            .expect("a message");
        serde_json::from_slice(&body).unwrap()
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// The response to a request (notifications before it are skipped).
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let message = self.receive();
            if message["id"] == id {
                return message;
            }
        }
    }

    /// The next diagnostics published.
    fn diagnostics(&mut self) -> Value {
        loop {
            let message = self.receive();
            if message["method"] == "textDocument/publishDiagnostics" {
                return message["params"].clone();
            }
        }
    }
}

const URI: &str = "file:///home/me/.config/next_code/workflows/dev.yaml";

#[test]
fn a_client_gets_diagnostics_completion_and_a_rename() {
    let mut client = Client::start();
    let init = client.request(
        "initialize",
        json!({ "capabilities": { "general": { "positionEncodings": ["utf-8", "utf-16"] } } }),
    );
    assert_eq!(init["result"]["capabilities"]["positionEncoding"], "utf-8");
    assert_eq!(init["result"]["serverInfo"]["name"], NAME);
    client.notify("initialized", json!({}));

    let text = "steps:\n  db: {run: d}\n  api: {depends_on: [db, zz], run: s}\n";
    client.notify(
        "textDocument/didOpen",
        json!({ "textDocument": { "uri": URI, "languageId": "yaml", "version": 1, "text": text } }),
    );
    let published = client.diagnostics();
    assert_eq!(published["uri"], URI);
    let diagnostic = &published["diagnostics"][0];
    assert_eq!(diagnostic["message"], "`api` depends on unknown step `zz`");
    assert_eq!(
        diagnostic["range"],
        json!({ "start": { "line": 2, "character": 25 }, "end": { "line": 2, "character": 27 } })
    );

    // Fixed with a full-text change: no problems.
    let fixed = "steps:\n  db: {run: d}\n  api: {depends_on: [db], run: s}\n  web: {depends_on: [], run: w}\n";
    client.notify(
        "textDocument/didChange",
        json!({ "textDocument": { "uri": URI, "version": 2 }, "contentChanges": [{ "text": fixed }] }),
    );
    assert_eq!(client.diagnostics()["diagnostics"], json!([]));

    let completion = client.request(
        "textDocument/completion",
        json!({ "textDocument": { "uri": URI }, "position": { "line": 3, "character": 22 } }),
    );
    let labels: Vec<&str> = completion["result"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["db", "api"]);

    let rename = client.request(
        "textDocument/rename",
        json!({ "textDocument": { "uri": URI }, "position": { "line": 1, "character": 3 }, "newName": "postgres" }),
    );
    let edits = rename["result"]["changes"][URI].as_array().unwrap();
    assert_eq!(edits.len(), 2, "{rename}");
    assert_eq!(
        edits[1]["range"]["start"],
        json!({ "line": 2, "character": 21 })
    );
    let refused = client.request(
        "textDocument/rename",
        json!({ "textDocument": { "uri": URI }, "position": { "line": 1, "character": 3 }, "newName": "api" }),
    );
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("exists already")
    );

    let unknown = client.request("workspace/symbol", json!({ "query": "" }));
    assert_eq!(unknown["error"]["code"], -32601);
    assert_eq!(
        client.request("shutdown", Value::Null)["result"],
        Value::Null
    );
    client.notify("exit", Value::Null);
    client.server.take().unwrap().join().unwrap().unwrap();
}

#[test]
fn other_yaml_files_get_nothing() {
    let mut client = Client::start();
    client.request("initialize", json!({ "capabilities": {} }));
    let uri = "file:///src/app/docker-compose.yaml";
    client.notify(
        "textDocument/didOpen",
        json!({ "textDocument": { "uri": uri, "languageId": "yaml", "version": 1, "text": "services:\n  db: {}\n" } }),
    );
    assert_eq!(client.diagnostics()["diagnostics"], json!([]));
    let hover = client.request(
        "textDocument/hover",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 0, "character": 2 } }),
    );
    assert_eq!(hover["result"], Value::Null);
    client.notify("exit", Value::Null);
    client.server.take().unwrap().join().unwrap().unwrap();
}

#[test]
fn workflow_files_are_recognized_by_folder_or_schema_line() {
    assert!(is_workflow(
        "file:///c%3A/Users/me/AppData/Roaming/next_code/workflows/dev.yaml",
        ""
    ));
    assert!(is_workflow(
        "file:///home/me/p/.next_code/workflows/dev.yml",
        ""
    ));
    assert!(!is_workflow(
        "file:///home/me/p/.next_code/workflows/notes.md",
        ""
    ));
    assert!(!is_workflow("file:///home/me/p/ci.yaml", "on: push\n"));
    assert!(is_workflow(
        "file:///home/me/p/stack.yaml",
        "# yaml-language-server: $schema=./workflow.schema.json\nsteps: {}\n"
    ));
}

#[test]
fn utf16_positions_count_code_units() {
    let text = "a: é𝄞x\n";
    let at = offset(text, &json!({ "line": 0, "character": 6 }), Encoding::Utf16);
    assert_eq!(&text[at..], "x\n");
    assert_eq!(
        position(text, at, Encoding::Utf16),
        json!({ "line": 0, "character": 6 })
    );
    assert_eq!(
        position(text, at, Encoding::Utf8),
        json!({ "line": 0, "character": 9 })
    );
}
