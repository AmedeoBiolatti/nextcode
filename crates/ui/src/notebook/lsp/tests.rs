//! The virtual document against a scripted language server (a real
//! `LanguageServer` client over pipes): masking, position mapping, sync
//! after typing and structural edits, diagnostic filtering and routing to
//! cell editors, edits applied to cells, merged completions.

use std::{
    io::{self, BufRead, BufReader, Write},
    sync::mpsc,
    thread,
    time::Duration,
};

use gpui::{AppContext, TestAppContext};
use lsp::{
    LanguageServer,
    types::{
        CompletionOptions, DiagnosticRelatedInformation, DiagnosticSeverity, Location,
        ServerCapabilities,
    },
};
use serde_json::{Value, json};

use super::*;
use crate::{editor::Editor, project::Project};

/// A server whose traffic the test reads; requests are answered by
/// `respond(method, params)` (`null` when it has no answer).
struct FakeServer {
    server: Arc<RunningServer>,
    messages: mpsc::Receiver<Value>,
    _incoming: futures::channel::mpsc::UnboundedReceiver<lsp::Incoming>,
}

fn read_frame(reader: &mut impl BufRead) -> Option<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = value.trim().parse().ok();
        }
    }
    let mut body = vec![0; length?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

fn fake_server(
    sync: TextDocumentSyncKind,
    respond: impl Fn(&str, &Value) -> Value + Send + 'static,
) -> FakeServer {
    let (client_reads, mut server_writes) = io::pipe().unwrap();
    let (server_reads, client_writes) = io::pipe().unwrap();
    let (connection, incoming) = LanguageServer::new(client_reads, client_writes, None);
    let (sender, messages) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(server_reads);
        while let Some(message) = read_frame(&mut reader) {
            if let (Some(id), Some(method)) = (message.get("id"), message["method"].as_str()) {
                let reply = json!({
                    "jsonrpc": "2.0", "id": id, "result": respond(method, &message["params"]),
                })
                .to_string();
                write!(
                    server_writes,
                    "Content-Length: {}\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
                server_writes.flush().unwrap();
            }
            if sender.send(message).is_err() {
                return;
            }
        }
    });
    let server = RunningServer {
        connection: Arc::new(connection),
        encoding: Encoding::Utf16,
        sync,
        save_includes_text: false,
        capabilities: ServerCapabilities {
            completion_provider: Some(CompletionOptions::default()),
            ..ServerCapabilities::default()
        },
        semantic_legend: None,
    };
    FakeServer {
        server: Arc::new(server),
        messages,
        _incoming: incoming,
    }
}

impl FakeServer {
    /// The next `count` notifications about documents, applied to `text`.
    fn apply(&self, count: usize, text: &mut String) {
        let mut seen = 0;
        while seen < count {
            let message = self
                .messages
                .recv_timeout(Duration::from_secs(5))
                .expect("a message from the client");
            let params = &message["params"];
            match message["method"].as_str() {
                Some("textDocument/didOpen") => {
                    *text = params["textDocument"]["text"].as_str().unwrap().to_owned();
                }
                Some("textDocument/didChange") => {
                    for change in params["contentChanges"].as_array().unwrap() {
                        let new = change["text"].as_str().unwrap();
                        match change.get("range") {
                            Some(range) => {
                                let start = offset(text, &range["start"]);
                                let end = offset(text, &range["end"]);
                                text.replace_range(start..end, new);
                            }
                            None => *text = new.to_owned(),
                        }
                    }
                }
                _ => continue,
            }
            seen += 1;
        }
    }

    /// Whether the client sent anything more (waits a little).
    fn quiet(&self) -> bool {
        self.messages
            .recv_timeout(Duration::from_millis(100))
            .is_err()
    }
}

/// Byte offset of an LSP position (UTF-16 columns) in `text`.
fn offset(text: &str, position: &Value) -> usize {
    let line = position["line"].as_u64().unwrap() as usize;
    let character = position["character"].as_u64().unwrap() as usize;
    let start: usize = text.split('\n').take(line).map(|l| l.len() + 1).sum();
    let mut units = 0;
    for (index, c) in text[start..].char_indices() {
        if units >= character || c == '\n' {
            return start + index;
        }
        units += c.len_utf16();
    }
    text.len()
}

fn ipynb(cells: &[(&str, &str)]) -> String {
    let cells: Vec<Value> = cells
        .iter()
        .map(|(kind, source)| match *kind {
            "code" => json!({
                "cell_type": "code", "execution_count": null, "metadata": {},
                "outputs": [], "source": source,
            }),
            kind => json!({ "cell_type": kind, "metadata": {}, "source": source }),
        })
        .collect();
    json!({ "cells": cells, "metadata": {}, "nbformat": 4, "nbformat_minor": 4 }).to_string()
}

/// A folder whose Python server is disabled (the tests bring their own).
fn folder(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "next_code_notebook_lsp_{}_{name}",
        std::process::id()
    ));
    std::fs::create_dir_all(dir.join(".next_code")).unwrap();
    std::fs::write(
        dir.join(".next_code").join("settings.json"),
        r#"{ "languages": { "python": { "server": { "enabled": false } } } }"#,
    )
    .unwrap();
    dir
}

fn notebook(name: &str, text: &str, cx: &mut TestAppContext) -> Entity<NotebookDocument> {
    let dir = folder(name);
    let path = dir.join(name);
    cx.new(|cx| {
        let notebook = NotebookDocument::parse(&path, text).unwrap();
        NotebookDocument::from_notebook(Project::local(dir), path, notebook, text.to_owned(), cx)
    })
}

/// `document`'s language-server document, open on `server`.
fn open(
    document: &Entity<NotebookDocument>,
    server: &FakeServer,
    cx: &mut TestAppContext,
) -> Entity<NotebookLsp> {
    let project = document.read_with(cx, |document, _| document.project().clone());
    let store = cx.new(|_| LspStore::new(project, Arc::new(Settings::default())));
    let lsp = cx.new(|cx| NotebookLsp::new(document.clone(), store, cx));
    lsp.update(cx, |lsp, cx| lsp.open_on(server.server.clone(), cx));
    lsp
}

fn ids(document: &Entity<NotebookDocument>, cx: &mut TestAppContext) -> Vec<CellId> {
    document.read_with(cx, |document, _| {
        document.cells().iter().map(|cell| cell.id).collect()
    })
}

fn virtual_text(lsp: &Entity<NotebookLsp>, cx: &mut TestAppContext) -> String {
    lsp.read_with(cx, |lsp, _| lsp.virtual_document().borrow().text())
}

fn version(lsp: &Entity<NotebookLsp>, cx: &mut TestAppContext) -> i32 {
    lsp.read_with(cx, |lsp, _| lsp.virtual_document().borrow().version())
}

fn diagnostic(line: u32, start: u32, end: u32, code: Option<&str>, message: &str) -> Diagnostic {
    Diagnostic {
        range: lsp::types::Range::new(Position::new(line, start), Position::new(line, end)),
        severity: Some(DiagnosticSeverity::WARNING),
        code: code.map(|code| NumberOrString::String(code.into())),
        message: message.into(),
        ..Diagnostic::default()
    }
}

#[test]
fn ipython_syntax_is_replaced_on_its_own_lines() {
    let cases: &[(&str, &str, &[u32])] = &[
        (
            "%matplotlib inline\nx = 1",
            "...               \nx = 1",
            &[0],
        ),
        ("!pip install numpy", "...               ", &[0]),
        (
            "files = !ls -la\nlen(files)",
            "files = ...    \nlen(files)",
            &[0],
        ),
        ("t = %timeit -o f()", "t = ...           ", &[0]),
        (
            "for i in x:\n    !echo $i",
            "for i in x:\n    ...     ",
            &[1],
        ),
        ("df?", "...", &[0]),
        ("x?", "0 ", &[0]),
        ("np.sum??", "...     ", &[0]),
        ("!echo é", "...    ", &[0]),
        ("%ls\r\nx = 1", "...\r\nx = 1", &[0]),
        // Python, untouched.
        (
            "a != b\nx = 1  # really?\ny = x % 2",
            "a != b\nx = 1  # really?\ny = x % 2",
            &[],
        ),
        // Cell magics: Python bodies stay, others go.
        ("%%time\nx = 1\ny = 2", "\nx = 1\ny = 2", &[0]),
        ("%%bash\necho hi\nls", "\n\n", &[0, 1, 2]),
        ("", "", &[]),
    ];
    for (source, expected, masked) in cases {
        let (text, lines) = python_text(source, true);
        assert_eq!(
            (text.as_str(), lines.as_slice()),
            (*expected, *masked),
            "{source:?}"
        );
        assert_eq!(line_count(&text), line_count(source), "{source:?}");
    }
    // marimo cells are Python as written.
    assert_eq!(
        python_text("%time x", false),
        ("%time x".to_owned(), vec![])
    );
}

#[gpui::test]
fn positions_map_between_cells_and_the_virtual_document(cx: &mut TestAppContext) {
    let document = notebook(
        "map.ipynb",
        &ipynb(&[
            ("code", "s = \"héllo 👋\"\nprint(s)"),
            ("markdown", "# Notes"),
            ("code", ""),
            ("code", "%matplotlib inline\ny = len(s)"),
        ]),
        cx,
    );
    let ids = ids(&document, cx);
    let virtual_document = document.read_with(cx, |_, cx| VirtualDocument::new(&document, cx));
    let prelude = IPYTHON_PRELUDE.lines().count() as u32;
    assert_eq!(virtual_document.start_of(ids[0]), Some(prelude));
    assert_eq!(
        virtual_document.start_of(ids[1]),
        None,
        "Markdown is not code"
    );
    // An empty cell still has its line.
    assert_eq!(virtual_document.start_of(ids[2]), Some(prelude + 2));
    assert_eq!(virtual_document.start_of(ids[3]), Some(prelude + 3));
    assert_eq!(
        virtual_document.text(),
        format!("{IPYTHON_PRELUDE}s = \"héllo 👋\"\nprint(s)\n\n...               \ny = len(s)\n")
    );
    // Columns are the cell's own (UTF-16 here): only lines move.
    let at = |line, character| virtual_document.cell_position(Position::new(line, character));
    assert_eq!(
        at(prelude, 12),
        Some(CellPosition {
            cell: ids[0],
            number: 1,
            position: Position::new(0, 12),
        })
    );
    assert_eq!(
        at(prelude + 2, 0).map(|p| (p.cell, p.number)),
        Some((ids[2], 3))
    );
    assert_eq!(
        at(prelude + 4, 4),
        Some(CellPosition {
            cell: ids[3],
            number: 4,
            position: Position::new(1, 4),
        })
    );
    assert_eq!(at(0, 4), None, "the prelude is no cell");
    assert_eq!(at(prelude + 5, 0), None, "past the last cell");
    assert_eq!(
        virtual_document.line_text(prelude + 3),
        Some("...               ")
    );
}

#[gpui::test]
fn the_server_follows_typing_and_structural_edits(cx: &mut TestAppContext) {
    let server = fake_server(TextDocumentSyncKind::INCREMENTAL, |_, _| Value::Null);
    let document = notebook(
        "sync.ipynb",
        &ipynb(&[
            ("code", "import os"),
            ("markdown", "Text"),
            ("code", "x = 1\ny = 2"),
            ("code", "%time f()"),
        ]),
        cx,
    );
    let lsp = open(&document, &server, cx);
    let mut text = String::new();
    server.apply(1, &mut text);
    assert_eq!(text, virtual_text(&lsp, cx));

    // Each step sends one change; the server's text always matches.
    let ids = ids(&document, cx);
    let mut check =
        |step: &str,
         cx: &mut TestAppContext,
         run: &dyn Fn(&mut NotebookDocument, &mut Context<NotebookDocument>)| {
            let before = version(&lsp, cx);
            document.update(cx, |document, cx| run(document, cx));
            let sent = (version(&lsp, cx) - before) as usize;
            server.apply(sent, &mut text);
            assert_eq!(text, virtual_text(&lsp, cx), "after {step}");
            sent
        };
    let typed = |source: &'static str, id: CellId| {
        move |document: &mut NotebookDocument, cx: &mut Context<NotebookDocument>| {
            document.set_source(id, source.to_owned(), None, cx)
        }
    };
    assert_eq!(check("typing", cx, &typed("x = 10\ny = 2", ids[2])), 1);
    assert_eq!(
        check("a new line", cx, &typed("x = 10\nz = 3\ny = 2", ids[2])),
        1
    );
    assert_eq!(check("a magic", cx, &typed("%%bash\nls", ids[3])), 1);
    assert_eq!(check("Markdown text", cx, &typed("Other text", ids[1])), 0);
    check("insert", cx, &|document, cx| {
        document.insert(1, CellKind::Code, "a = [\n  1,\n]".into(), cx);
    });
    check("delete", cx, &|document, cx| document.delete(&[ids[0]], cx));
    check("move", cx, &|document, cx| {
        document.move_cells(&[ids[3]], -2, cx);
    });
    check("to Markdown", cx, &|document, cx| {
        document.set_kind(ids[2], CellKind::Markdown, cx)
    });
    check("to code", cx, &|document, cx| {
        document.set_kind(ids[1], CellKind::Code, cx)
    });
    check("split", cx, &|document, cx| {
        document.split(ids[3], 7, cx);
    });
    for _ in 0..3 {
        check("undo", cx, &|document, cx| {
            document.undo_structure(cx);
        });
    }
    check("redo", cx, &|document, cx| {
        document.redo_structure(cx);
    });
    let reloaded = ipynb(&[("code", "b = 2"), ("code", "c = b")]);
    check("reload", cx, &|document, cx| {
        document.reload(&reloaded, cx).unwrap()
    });

    // An editor sends its edit at once; the document's echo sends nothing.
    let id = ids_now(&document, cx)[1];
    let link = lsp.read_with(cx, |lsp, _| lsp.link(id).unwrap());
    let before = version(&lsp, cx);
    link.sync("c = b * 2");
    server.apply(1, &mut text);
    document.update(cx, |document, cx| {
        document.set_source(id, "c = b * 2".into(), None, cx)
    });
    assert_eq!(version(&lsp, cx), before + 1);
    assert!(server.quiet());
    assert_eq!(text, virtual_text(&lsp, cx));
    assert!(text.ends_with("b = 2\nc = b * 2\n"), "{text}");
}

fn ids_now(document: &Entity<NotebookDocument>, cx: &mut TestAppContext) -> Vec<CellId> {
    ids(document, cx)
}

#[gpui::test]
fn full_sync_servers_get_the_whole_text(cx: &mut TestAppContext) {
    let server = fake_server(TextDocumentSyncKind::FULL, |_, _| Value::Null);
    let document = notebook(
        "full.ipynb",
        &ipynb(&[("code", "a = 1"), ("code", "b = a")]),
        cx,
    );
    let lsp = open(&document, &server, cx);
    let mut text = String::new();
    server.apply(1, &mut text);
    let id = ids(&document, cx)[0];
    document.update(cx, |document, cx| {
        document.set_source(id, "a = 2\n".into(), None, cx)
    });
    server.apply(1, &mut text);
    assert_eq!(text, format!("{IPYTHON_PRELUDE}a = 2\n\nb = a\n"));
    assert_eq!(text, virtual_text(&lsp, cx));
}

#[gpui::test]
fn diagnostics_are_filtered_and_routed_to_their_cells(cx: &mut TestAppContext) {
    let document = notebook(
        "diagnostics.ipynb",
        &ipynb(&[
            ("code", "def f():\n    return 1\nf"),
            ("markdown", "Text"),
            (
                "code",
                "%matplotlib inline\ns = \"é👋\"; print(undefined)\nawait g()\ndef h():\n    await g()\ndef f():\n    pass\nx\ny",
            ),
        ]),
        cx,
    );
    let ids = ids(&document, cx);
    let virtual_document = Rc::new(RefCell::new(
        document.read_with(cx, |_, cx| VirtualDocument::new(&document, cx)),
    ));
    let (first, third) = {
        let doc = virtual_document.borrow();
        (doc.start_of(ids[0]).unwrap(), doc.start_of(ids[2]).unwrap())
    };
    let redeclared = |line, related_line| Diagnostic {
        related_information: Some(vec![DiagnosticRelatedInformation {
            location: Location::new(
                lsp::path_to_uri(virtual_document.borrow().path()).unwrap(),
                lsp::types::Range::new(
                    Position::new(related_line, 4),
                    Position::new(related_line, 5),
                ),
            ),
            message: "See function declaration".into(),
        }]),
        ..diagnostic(line, 4, 5, Some("reportRedeclaration"), "obscured")
    };
    let published = vec![
        diagnostic(
            1,
            0,
            3,
            Some("reportAttributeAccessIssue"),
            "in the prelude",
        ),
        diagnostic(
            first + 2,
            0,
            1,
            Some("reportUnusedExpression"),
            "displayed value",
        ),
        redeclared(first, third + 5),
        diagnostic(third, 0, 3, Some("reportUndefinedVariable"), "on a magic"),
        diagnostic(
            third + 1,
            17,
            26,
            Some("reportUndefinedVariable"),
            "undefined",
        ),
        diagnostic(
            third + 2,
            0,
            5,
            None,
            "\"await\" allowed only within async function",
        ),
        diagnostic(
            third + 4,
            4,
            9,
            None,
            "\"await\" allowed only within async function",
        ),
        redeclared(third + 3, third + 5),
        diagnostic(
            third + 7,
            0,
            1,
            Some("reportUnusedExpression"),
            "unused, mid-cell",
        ),
        diagnostic(
            third + 8,
            0,
            1,
            Some("reportUnusedExpression"),
            "displayed value",
        ),
        diagnostic(
            third + 9,
            0,
            1,
            Some("reportUndefinedVariable"),
            "past the cells",
        ),
    ];
    let kept = virtual_document.borrow().filter_diagnostics(published);
    let messages: Vec<(u32, &str)> = kept
        .iter()
        .map(|d| (d.range.start.line, d.message.as_str()))
        .collect();
    assert_eq!(
        messages,
        [
            (third + 1, "undefined"),
            (third + 4, "\"await\" allowed only within async function"),
            (third + 3, "obscured"),
            (third + 7, "unused, mid-cell"),
        ]
    );
    let by_cell = virtual_document.borrow().split_diagnostics(&kept);
    assert_eq!(by_cell.len(), 1);
    assert_eq!(by_cell[&ids[2]].len(), 4);

    // The cell's editor shows them on its own lines (UTF-16 columns).
    let server = fake_server(TextDocumentSyncKind::INCREMENTAL, |_, _| Value::Null);
    let source = document.read_with(cx, |document, _| document.cells()[2].cell.source.clone());
    let editor = cx.new(|cx| Editor::notebook_cell(&source, Some(Language::Python), cx));
    let link = CellLink {
        document: virtual_document.clone(),
        cell: ids[2],
    };
    editor.update(cx, |editor, cx| {
        editor.attach_cell_server(server.server.clone(), link, cx);
        editor.set_diagnostics(&kept, cx);
    });
    let shown = editor.read_with(cx, |editor, _| editor.diagnostic_ranges());
    let line_start =
        |line: usize| -> usize { source.split('\n').take(line).map(|l| l.len() + 1).sum() };
    let undefined = line_start(1)
        + source
            .split('\n')
            .nth(1)
            .unwrap()
            .find("undefined")
            .unwrap();
    assert_eq!(shown[0], (undefined..undefined + 9, "undefined".to_owned()));
    assert_eq!(shown.len(), 4);
    // Another cell's editor shows none of them.
    let other =
        cx.new(|cx| Editor::notebook_cell("def f():\n    return 1\nf", Some(Language::Python), cx));
    other.update(cx, |editor, cx| {
        let link = CellLink {
            document: virtual_document.clone(),
            cell: ids[0],
        };
        editor.attach_cell_server(server.server.clone(), link, cx);
        editor.set_diagnostics(&kept, cx);
    });
    assert!(
        other
            .read_with(cx, |editor, _| editor.diagnostic_ranges())
            .is_empty()
    );
}

#[gpui::test]
fn workspace_edits_rename_across_cells(cx: &mut TestAppContext) {
    let server = fake_server(TextDocumentSyncKind::INCREMENTAL, |_, _| Value::Null);
    let document = notebook(
        "rename.ipynb",
        &ipynb(&[
            ("code", "valeur = \"é👋\"; valeur += \"!\""),
            ("markdown", "Uses valeur"),
            ("code", "%time print(valeur)\nprint(valeur)"),
        ]),
        cx,
    );
    let lsp = open(&document, &server, cx);
    let ids = ids(&document, cx);
    let doc = lsp.read_with(cx, |lsp, _| lsp.virtual_document().clone());
    let (first, third) = (
        doc.borrow().start_of(ids[0]).unwrap(),
        doc.borrow().start_of(ids[2]).unwrap(),
    );
    let edit = |line, start, end| TextEdit {
        range: lsp::types::Range::new(Position::new(line, start), Position::new(line, end)),
        new_text: "value".into(),
    };
    // `valeur += …` starts after `"é👋"; ` (👋 is two UTF-16 units).
    let edits = [
        edit(first, 0, 6),
        edit(first, 16, 22),
        edit(third + 1, 6, 12),
    ];
    let applied = cx.update(|cx| doc.borrow().apply_edits(&edits, Encoding::Utf16, cx));
    assert_eq!(applied, Ok(()));
    let sources: Vec<String> = document.read_with(cx, |document, _| {
        document
            .cells()
            .iter()
            .map(|c| c.cell.source.clone())
            .collect()
    });
    assert_eq!(
        sources,
        [
            "value = \"é👋\"; value += \"!\"",
            "Uses valeur",
            "%time print(valeur)\nprint(value)",
        ]
    );
    assert!(virtual_text(&lsp, cx).contains("print(value)\n"));

    // A range ending where the next cell starts ends with its cell.
    let whole = TextEdit {
        range: lsp::types::Range::new(Position::new(first, 0), Position::new(first + 1, 0)),
        new_text: "v = 0".into(),
    };
    let applied = cx.update(|cx| doc.borrow().apply_edits(&[whole], Encoding::Utf16, cx));
    assert_eq!(applied, Ok(()));
    assert_eq!(
        document.read_with(cx, |d, _| d.cells()[0].cell.source.clone()),
        "v = 0"
    );
    // Edits across cells or outside them change nothing.
    let across = TextEdit {
        range: lsp::types::Range::new(Position::new(first, 0), Position::new(third + 1, 0)),
        new_text: String::new(),
    };
    let outside = edit(0, 0, 1);
    for edits in [[across], [outside]] {
        let applied = cx.update(|cx| doc.borrow().apply_edits(&edits, Encoding::Utf16, cx));
        assert!(applied.is_err());
    }
    assert_eq!(
        document.read_with(cx, |d, _| d.cells()[0].cell.source.clone()),
        "v = 0"
    );
}

#[gpui::test]
fn cell_editors_ask_at_their_place_in_the_notebook(cx: &mut TestAppContext) {
    let server = fake_server(TextDocumentSyncKind::INCREMENTAL, |_, _| Value::Null);
    let document = notebook(
        "positions.ipynb",
        &ipynb(&[("code", "a = 1"), ("code", "é = \"👋\"; b = a")]),
        cx,
    );
    let lsp = open(&document, &server, cx);
    let ids = ids(&document, cx);
    let source = "é = \"👋\"; b = a";
    let editor = cx.new(|cx| Editor::notebook_cell(source, Some(Language::Python), cx));
    let link = lsp.read_with(cx, |lsp, _| lsp.link(ids[1]).unwrap());
    editor.update(cx, |editor, cx| {
        editor.attach_cell_server(server.server.clone(), link, cx)
    });
    let prelude = IPYTHON_PRELUDE.lines().count() as u32;
    let b = source.find('b').unwrap();
    assert_eq!(
        editor.read_with(cx, |e, _| e.server_position_of(b)),
        Position::new(prelude + 1, 10)
    );
    // Lines added above (in a cell, or a new cell) move it down.
    document.update(cx, |document, cx| {
        document.set_source(ids[0], "a = 1\n\n".into(), None, cx)
    });
    assert_eq!(
        editor.read_with(cx, |e, _| e.server_position_of(b)),
        Position::new(prelude + 3, 10)
    );
    document.update(cx, |document, cx| {
        document.insert(0, CellKind::Code, "import os\nimport re".into(), cx);
    });
    assert_eq!(
        editor.read_with(cx, |e, _| e.server_position_of(b)),
        Position::new(prelude + 5, 10)
    );
    document.update(cx, |document, cx| {
        document.move_cells(&[ids[1]], -2, cx);
    });
    assert_eq!(
        editor.read_with(cx, |e, _| e.server_position_of(b)),
        Position::new(prelude, 10)
    );
}

#[gpui::test]
fn completions_list_the_servers_items_then_the_kernels(cx: &mut TestAppContext) {
    // The server completes `val` with an edit in its document's lines.
    let server = fake_server(TextDocumentSyncKind::INCREMENTAL, |method, params| {
        if method != "textDocument/completion" {
            return Value::Null;
        }
        let position = &params["position"];
        let (line, end) = (
            position["line"].clone(),
            position["character"].as_u64().unwrap(),
        );
        let edit = |label: &str| {
            json!({
                "label": label,
                "textEdit": {
                    "range": {
                        "start": { "line": line, "character": end - 3 },
                        "end": { "line": line, "character": end },
                    },
                    "newText": label,
                },
            })
        };
        json!({ "isIncomplete": false, "items": [edit("value_server"), edit("value_both")] })
    });
    let document = notebook(
        "complete.ipynb",
        &ipynb(&[("code", "value_server = 1"), ("code", "x = 1\nval")]),
        cx,
    );
    let lsp = open(&document, &server, cx);
    let id = ids(&document, cx)[1];
    let editor = cx.new(|cx| {
        let mut editor = Editor::notebook_cell("x = 1\nval", Some(Language::Python), cx);
        let kernel: crate::editor::CompletionProvider =
            Rc::new(|_: String, offset: usize, _: &mut App| {
                let names = vec!["value_both".to_owned(), "value_runtime".to_owned()];
                Task::ready(Some((names, offset - 3..offset)))
            });
        editor.set_completion_provider(Some(kernel));
        editor
    });
    let link = lsp.read_with(cx, |lsp, _| lsp.link(id).unwrap());
    editor.update(cx, |editor, cx| {
        editor.attach_cell_server(server.server.clone(), link, cx);
        editor.set_cursor(editor.buffer().len(), cx);
        editor.complete_at_cursor(cx);
    });
    let mut labels = Vec::new();
    for _ in 0..500 {
        cx.run_until_parked();
        labels = editor.read_with(cx, |editor, _| editor.completion_labels());
        if labels.len() == 3 {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(labels, ["value_both", "value_server", "value_runtime"]);
    // The server's edit lands in the cell, not at the document's line.
    editor.update(cx, |editor, cx| editor.accept_completion_at(1, cx));
    cx.run_until_parked();
    assert_eq!(editor.read_with(cx, |e, _| e.text()), "x = 1\nvalue_server");
}

#[gpui::test]
fn typing_in_a_large_notebook_sends_only_the_edited_line(cx: &mut TestAppContext) {
    let server = fake_server(TextDocumentSyncKind::INCREMENTAL, |_, _| Value::Null);
    let cells: Vec<(String, String)> = (0..500)
        .map(|i| {
            (
                "code".to_owned(),
                format!("import math\nvalue_{i} = math.sqrt({i})\nvalue_{i}"),
            )
        })
        .collect();
    let cells: Vec<(&str, &str)> = cells
        .iter()
        .map(|(k, s)| (k.as_str(), s.as_str()))
        .collect();
    let document = notebook("large.ipynb", &ipynb(&cells), cx);
    let lsp = open(&document, &server, cx);
    let mut text = String::new();
    server.apply(1, &mut text);
    let id = ids(&document, cx)[250];
    let link = lsp.read_with(cx, |lsp, _| lsp.link(id).unwrap());
    let mut source = cells[250].1.to_owned();
    let started = std::time::Instant::now();
    for _ in 0..200 {
        // As a cell editor does: its own sync, then the document's echo.
        source.push('x');
        link.sync(&source);
        document.update(cx, |document, cx| {
            document.set_source(id, source.clone(), None, cx)
        });
    }
    let elapsed = started.elapsed();
    eprintln!("200 keystrokes in cell 251 of 500: {elapsed:?}");
    let mut changes = Vec::new();
    while changes.len() < 200 {
        let message = server
            .messages
            .recv_timeout(Duration::from_secs(5))
            .expect("a change per keystroke");
        if message["method"] == "textDocument/didChange" {
            changes.push(message);
        }
    }
    assert!(server.quiet(), "one change per keystroke");
    let last = &changes[199]["params"]["contentChanges"][0];
    let start = lsp.read_with(cx, |lsp, _| {
        lsp.virtual_document().borrow().start_of(id).unwrap()
    });
    assert_eq!(last["text"], format!("value_250{}\n", "x".repeat(200)));
    assert_eq!(last["range"]["start"]["line"], start + 2);
    assert_eq!(last["range"]["end"]["line"], start + 3);
}

/// Open notebook documents for `workspace_edit::apply`, as the workspace
/// has them.
struct NotebookDocuments<'a> {
    document: Rc<RefCell<VirtualDocument>>,
    cx: &'a mut App,
}

impl workspace_edit::Documents for NotebookDocuments<'_> {
    fn version(&mut self, path: &Path) -> Option<i32> {
        let document = self.document.borrow();
        (document.path() == path).then(|| document.version())
    }

    fn text(&mut self, path: &Path) -> Option<String> {
        let document = self.document.borrow();
        (document.path() == path).then(|| document.text())
    }

    fn edit(
        &mut self,
        path: &Path,
        edits: &[TextEdit],
        encoding: Encoding,
    ) -> Option<Result<(), String>> {
        let document = self.document.borrow();
        (document.path() == path).then(|| document.apply_edits(edits, encoding, self.cx))
    }

    fn renamed(&mut self, _: &Path, _: &Path) {}

    fn deleted(&mut self, _: &Path) {}
}

/// Runs the app until `done` (a real server answers on its own threads).
fn wait_for(cx: &mut TestAppContext, what: &str, done: impl Fn(&mut TestAppContext) -> bool) {
    for _ in 0..3000 {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

/// The real basedpyright through the project's `LspStore` (skipped when
/// `basedpyright-langserver` is not installed): a name defined in a later
/// edit of cell 1 stops being undefined in cell 3, the displayed last
/// expression is not "unused", go to definition lands in cell 1 and a
/// rename edits both cells.
#[gpui::test]
fn basedpyright_reads_the_cells_as_one_module(cx: &mut TestAppContext) {
    let dir = std::env::temp_dir().join(format!(
        "next_code_notebook_lsp_{}_basedpyright",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let project = Project::local(dir.clone());
    if !matches!(
        project.find_executable_in("basedpyright-langserver", &dir),
        Ok(Some(_))
    ) {
        eprintln!("skipped: basedpyright-langserver is not installed");
        return;
    }
    let text = ipynb(&[
        ("code", "import math\nradius = 2.0"),
        ("markdown", "# Area"),
        (
            "code",
            "%matplotlib inline\narea = math.pi * radius ** 2 * rate\narea",
        ),
    ]);
    let path = dir.join("real.ipynb");
    let document = cx.new(|cx| {
        let notebook = NotebookDocument::parse(&path, &text).unwrap();
        NotebookDocument::from_notebook(project.clone(), path, notebook, text.clone(), cx)
    });
    let store = cx.new(|_| LspStore::new(project.clone(), Arc::new(Settings::default())));
    let lsp = cx.new(|cx| NotebookLsp::new(document.clone(), store.clone(), cx));
    let ids = ids(&document, cx);
    let (watched, third) = (lsp.clone(), ids[2]);
    let codes = move |cx: &mut TestAppContext| -> Vec<(String, String)> {
        watched.read_with(cx, |lsp, _| {
            lsp.diagnostics(third)
                .iter()
                .map(|d| {
                    let code = match &d.code {
                        Some(NumberOrString::String(code)) => code.clone(),
                        _ => String::new(),
                    };
                    (code, d.message.clone())
                })
                .collect()
        })
    };
    let undefined = |cx: &mut TestAppContext| {
        codes(cx)
            .iter()
            .any(|(_, message)| message.contains("\"rate\" is not defined"))
    };
    wait_for(cx, "`rate` reported undefined", undefined);
    let reported = codes(cx);
    assert!(
        !reported
            .iter()
            .any(|(code, _)| code == "reportUnusedExpression"),
        "{reported:?}"
    );
    document.update(cx, |document, cx| {
        document.set_source(
            ids[0],
            "import math\nradius = 2.0\nrate = 0.5".into(),
            None,
            cx,
        )
    });
    wait_for(cx, "`rate` defined", |cx| !undefined(cx));

    let (server, virtual_document) = lsp.read_with(cx, |lsp, _| {
        (lsp.server().unwrap(), lsp.virtual_document().clone())
    });
    let uri = lsp::path_to_uri(virtual_document.borrow().path()).unwrap();
    let start = virtual_document.borrow().start_of(ids[2]).unwrap();
    let at_rate = lsp::types::TextDocumentPositionParams::new(
        TextDocumentIdentifier::new(uri),
        Position::new(start + 1, 31),
    );
    let definition = futures::executor::block_on(
        server
            .connection
            .request::<lsp::types::request::GotoDefinition>(lsp::types::GotoDefinitionParams {
                text_document_position_params: at_rate.clone(),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            }),
    )
    .unwrap()
    .unwrap();
    let target = language::definition_locations(definition).remove(0);
    let target = virtual_document
        .borrow()
        .cell_position(target.range.start)
        .unwrap();
    assert_eq!(
        (target.cell, target.number, target.position.line),
        (ids[0], 1, 2)
    );

    let edit =
        futures::executor::block_on(server.connection.request::<lsp::types::request::Rename>(
            lsp::types::RenameParams {
                text_document_position: at_rate,
                new_name: "growth".into(),
                work_done_progress_params: Default::default(),
            },
        ))
        .unwrap()
        .unwrap();
    let operations = workspace_edit::operations(edit).unwrap();
    let report = cx.update(|cx| {
        let mut documents = NotebookDocuments {
            document: virtual_document.clone(),
            cx,
        };
        workspace_edit::apply(&project, &operations, server.encoding, &mut documents)
    });
    assert_eq!(report.failure, None);
    let sources: Vec<String> = document.read_with(cx, |document, _| {
        document
            .cells()
            .iter()
            .map(|c| c.cell.source.clone())
            .collect()
    });
    assert_eq!(
        sources,
        [
            "import math\nradius = 2.0\ngrowth = 0.5",
            "# Area",
            "%matplotlib inline\narea = math.pi * radius ** 2 * growth\narea",
        ]
    );
    store
        .update(cx, |store, cx| {
            store.shutdown_all(Duration::from_secs(2), cx)
        })
        .detach();
    cx.run_until_parked();
}
