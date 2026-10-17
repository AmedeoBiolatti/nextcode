//! `.ipynb` round trips against files written by Jupyter (nbformat 5.11)
//! and VS Code.

use notebook::{Cell, CellKind, Format, Kind, Notebook, Output, outputs};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn round_trip(name: &str) -> Notebook {
    let text = fixture(name);
    let notebook = Notebook::parse(Kind::Ipynb, &text).unwrap();
    let written = notebook.to_text();
    if written != text {
        let line = written
            .lines()
            .zip(text.lines())
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        panic!(
            "{name}: first difference at line {}:\nwritten:  {:?}\noriginal: {:?}",
            line + 1,
            written.lines().nth(line),
            text.lines().nth(line)
        );
    }
    notebook
}

#[test]
fn jupyter_saved_notebook_round_trips() {
    let notebook = round_trip("jupyter_v45.ipynb");
    let Format::Ipynb(info) = &notebook.format else {
        panic!()
    };
    assert_eq!((info.nbformat, info.nbformat_minor, info.indent), (4, 5, 1));
    assert!(info.trailing_newline);
    assert!(info.metadata.contains_key("widgets"));
    let cells = &notebook.cells;
    assert!(cells.iter().all(|cell| cell.id.is_some()));

    assert_eq!(cells[0].kind, CellKind::Markdown);
    assert!(cells[0].source.starts_with("# Analysis — résumé ✓\n"));
    assert!(cells[0].attachments.as_ref().unwrap()["logo.png"]["image/png"].is_string());

    // Stream lines joined.
    assert_eq!(
        cells[2].outputs,
        vec![Output::Stream {
            name: "stdout".into(),
            text: "line 0\nline 1\nline 2\ntab\there \"quoted\" \\ back\n".into()
        }]
    );
    // Widget view.
    let Output::DisplayData { data, .. } = &cells[3].outputs[0] else {
        panic!()
    };
    assert!(matches!(
        outputs::richest(data),
        outputs::Rich::Widget {
            plain: Some("IntSlider(value=0)")
        }
    ));
    // pandas: html + plain.
    let Output::ExecuteResult {
        data,
        execution_count,
        ..
    } = &cells[4].outputs[0]
    else {
        panic!()
    };
    assert_eq!(*execution_count, Some(3));
    let outputs::Rich::Html { html, plain } = outputs::richest(data) else {
        panic!()
    };
    assert!(html.contains("<table"));
    assert!(plain.unwrap().contains("1e-05") || plain.unwrap().contains("0.00001"));
    // matplotlib png.
    let Output::DisplayData { data, .. } = &cells[5].outputs[0] else {
        panic!()
    };
    assert!(matches!(outputs::richest(data), outputs::Rich::Png(png) if png.starts_with("iVBOR")));
    // ANSI traceback.
    let Output::Error {
        ename, traceback, ..
    } = &cells[6].outputs[0]
    else {
        panic!()
    };
    assert_eq!(ename, "ZeroDivisionError");
    assert!(traceback.iter().any(|line| line.contains('\x1b')));
    assert!(cells[8].outputs.len() == 2);
    // Not run: null count, unknown metadata kept.
    assert_eq!(cells[9].execution_count, None);
    assert_eq!(cells[9].metadata["custom"]["tiny"], 1e-07);
    // Raw with CRLF and an empty code cell.
    assert_eq!(cells[10].kind, CellKind::Raw);
    assert_eq!(cells[10].source, "raw\ttext\r\nwith CRLF");
    assert_eq!(cells[11].source, "");
}

#[test]
fn nbformat_4_4_round_trips_without_ids() {
    let notebook = round_trip("jupyter_v44.ipynb");
    assert!(notebook.cells.iter().all(|cell| cell.id.is_none()));
}

#[test]
fn vscode_notebook_round_trips() {
    let notebook = round_trip("vscode.ipynb");
    let Format::Ipynb(info) = &notebook.format else {
        panic!()
    };
    assert_eq!(info.nbformat_minor, 2);
    assert!(info.metadata.contains_key("vscode"));
    assert!(notebook.cells[0].source.contains("日本語"));
    assert!(notebook.cells[0].source.contains('\x1b'));
    // JSON mime kept as JSON.
    let Output::ExecuteResult { data, .. } = &notebook.cells[1].outputs[1] else {
        panic!()
    };
    assert!(data["application/json"].is_object());
    // svg lines joined.
    let Output::DisplayData { data, .. } = &notebook.cells[1].outputs[2] else {
        panic!()
    };
    assert_eq!(
        outputs::richest(data),
        outputs::Rich::Svg("<svg xmlns=\"http://www.w3.org/2000/svg\">\n</svg>")
    );
}

#[test]
fn indent_and_missing_newline_are_kept() {
    let notebook = round_trip("indent2_no_newline.ipynb");
    let Format::Ipynb(info) = &notebook.format else {
        panic!()
    };
    assert_eq!(info.indent, 2);
    assert!(!info.trailing_newline);
}

#[test]
fn editing_one_cell_changes_only_its_lines() {
    let text = fixture("jupyter_v45.ipynb");
    let mut notebook = Notebook::parse(Kind::Ipynb, &text).unwrap();
    notebook.cells[7].source = "x = 43\nprint(x)".into();
    let written = notebook.to_text();
    let old: Vec<&str> = text.lines().collect();
    let new: Vec<&str> = written.lines().collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    assert_eq!(old[prefix..old.len() - suffix], ["    \"x = 42\""]);
    assert_eq!(
        new[prefix..new.len() - suffix],
        ["    \"x = 43\\n\",", "    \"print(x)\""]
    );
}

#[test]
fn new_cells_get_ids_only_from_4_5() {
    for (name, expect_id) in [("jupyter_v45.ipynb", true), ("jupyter_v44.ipynb", false)] {
        let mut notebook = Notebook::parse(Kind::Ipynb, &fixture(name)).unwrap();
        notebook.cells.push(Cell::new(CellKind::Markdown, "new"));
        let reread = Notebook::parse(Kind::Ipynb, &notebook.to_text()).unwrap();
        assert_eq!(
            reread.cells.last().unwrap().id.is_some(),
            expect_id,
            "{name}"
        );
        assert_eq!(reread.cells.last().unwrap().source, "new");
    }
}

#[test]
fn unknown_keys_are_kept() {
    let text = "{\n \"cells\": [\n  {\n   \"cell_type\": \"code\",\n   \"execution_count\": 2,\n   \"metadata\": {},\n   \"outputs\": [],\n   \"source\": [\n    \"a\"\n   ],\n   \"zz_custom\": [\n    1.5\n   ]\n  }\n ],\n \"metadata\": {},\n \"nbformat\": 4,\n \"nbformat_minor\": 4,\n \"zz_top\": {\n  \"k\": null\n }\n}\n";
    let notebook = Notebook::parse(Kind::Ipynb, text).unwrap();
    assert!(notebook.cells[0].extra.contains_key("zz_custom"));
    assert_eq!(notebook.to_text(), text);
}
