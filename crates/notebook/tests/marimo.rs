//! marimo round trips against files written by marimo 0.25.1
//! (`marimo._ast.codegen.generate_filecontents`).

use notebook::{
    CellKind, Format, Kind, MarimoCellKind, Notebook,
    marimo::{self, Analysis, Graph},
};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

#[track_caller]
fn assert_text(written: &str, expected: &str) {
    if written != expected {
        let line = written
            .lines()
            .zip(expected.lines())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| written.lines().count().min(expected.lines().count()));
        panic!(
            "first difference at line {}:\nwritten:  {:?}\nexpected: {:?}\n--- written ---\n{written}",
            line + 1,
            written.lines().nth(line),
            expected.lines().nth(line),
        );
    }
}

fn round_trip(name: &str) -> Notebook {
    let text = fixture(name);
    assert!(marimo::is_marimo(&text));
    let notebook = Notebook::parse(Kind::Marimo, &text).unwrap();
    assert_text(&notebook.to_text(), &text);
    notebook
}

#[test]
fn marimo_generated_file_round_trips() {
    let notebook = round_trip("marimo_basic.py");
    let Format::Marimo(info) = &notebook.format else {
        panic!()
    };
    assert!(info.header.starts_with("# /// script\n"));
    assert_eq!(info.generated_with.as_deref(), Some("0.25.1"));
    assert_eq!(info.app, "app = marimo.App()");
    assert_eq!(info.footer, "if __name__ == \"__main__\":\n    app.run()\n");
    let cells = &notebook.cells;
    assert_eq!(cells.len(), 17);
    assert_eq!(cells[0].source, "import marimo as mo\nimport numpy as np");
    assert_eq!(cells[1].kind, CellKind::Markdown);
    assert_eq!(cells[1].source, "# Title\n\nSome *text*.");
    assert_eq!(
        marimo::cell_code(&cells[1]),
        "mo.md(r\"\"\"\n# Title\n\nSome *text*.\n\"\"\")"
    );
    let defs = cells[2].marimo.as_ref().unwrap();
    assert_eq!(
        (defs.name.as_str(), defs.config.as_str()),
        ("defs", "hide_code=True")
    );
    assert_eq!(cells[3].source, "");
    let add = cells[4].marimo.as_ref().unwrap();
    assert_eq!(
        (add.kind, add.name.as_str()),
        (MarimoCellKind::Function, "add")
    );
    assert_eq!(cells[4].source, "def add(a, b):\n    return a + b");
    assert_eq!(
        cells[5].marimo.as_ref().unwrap().kind,
        MarimoCellKind::Class
    );
    assert_eq!(cells[8].marimo.as_ref().unwrap().config, "disabled=True");
    let unparsable = cells[9].marimo.as_ref().unwrap();
    assert_eq!(unparsable.kind, MarimoCellKind::Unparsable);
    assert_eq!(cells[9].source, "z = (1,\n");
    assert_eq!(cells[10].source, "# only comment");
    assert_eq!(cells[14].source, "async_res = await something()");
}

#[test]
fn setup_cell_and_markdown_variants_round_trip() {
    let notebook = round_trip("marimo_setup.py");
    let cells = &notebook.cells;
    let setup = cells[0].marimo.as_ref().unwrap();
    assert_eq!(setup.kind, MarimoCellKind::Setup);
    assert_eq!(cells[0].source, "import marimo as mo\nCONST = 3");
    assert_eq!(
        cells[1].marimo.as_ref().unwrap().kind,
        MarimoCellKind::Function
    );
    // An f-string with placeholders stays code.
    assert_eq!(cells[2].kind, CellKind::Code);
    assert_eq!(cells[3].kind, CellKind::Markdown);
    assert_eq!(cells[3].source, "plain *md*");
    assert_eq!(cells[4].source, "Indented\ntext with \"\"quotes");
    assert_eq!(cells[5].kind, CellKind::Markdown);
    assert_eq!(cells[5].marimo.as_ref().unwrap().name, "named_md");
}

#[test]
fn edits_recompute_signatures_as_marimo() {
    let mut notebook = Notebook::parse(Kind::Marimo, &fixture("marimo_basic.py")).unwrap();
    notebook.cells[1].source = "# New title".into();
    notebook.cells[2].source = "x = 2\nz = x * np.e".into();
    notebook.cells[7].source = "print(z, y_missing)".into();
    notebook.cells[12].source = "def helper():\n    return 42".into();
    assert_text(&notebook.to_text(), &fixture("marimo_basic_edited.py"));
}

#[test]
fn older_marimo_files_read_and_write_the_current_layout() {
    let text = fixture("marimo_old.py");
    let notebook = Notebook::parse(Kind::Marimo, &text).unwrap();
    assert_eq!(notebook.cells.len(), 5);
    assert_eq!(notebook.cells[0].source, "import marimo as mo");
    assert_eq!(notebook.cells[1].kind, CellKind::Markdown);
    assert_eq!(notebook.cells[1].source, "# Old style");
    assert_eq!(notebook.cells[4].marimo.as_ref().unwrap().name, "total");
    // marimo 0.25 writes the same, except that it bumps the version and
    // normalises the untouched Markdown cell's call.
    let expected = fixture("marimo_old_expected.py")
        .replace("\"0.25.1\"", "\"0.1.0\"")
        .replace(
            "mo.md(\"\"\"\n    # Old style\n    \"\"\")",
            "mo.md(\"# Old style\")",
        );
    assert_text(&notebook.to_text(), &expected);
}

#[test]
fn new_notebook_is_what_marimo_writes() {
    let notebook = marimo::new_notebook();
    assert_text(
        &notebook.to_text(),
        "import marimo\n\n__generated_with = \"0.25.1\"\napp = marimo.App()\n\n\n@app.cell\ndef _():\n    return\n\n\nif __name__ == \"__main__\":\n    app.run()\n",
    );
    let reread = Notebook::parse(Kind::Marimo, &notebook.to_text()).unwrap();
    assert_eq!(reread.cells.len(), 1);
}

#[test]
fn graph_of_a_notebook() {
    let notebook = Notebook::parse(Kind::Marimo, &fixture("marimo_basic.py")).unwrap();
    let analyses: Vec<Analysis> = notebook
        .cells
        .iter()
        .map(|cell| marimo::analyze(&marimo::cell_code(cell)))
        .collect();
    let graph = Graph::new(&analyses);
    // `import marimo as mo, numpy as np` feeds the markdown cell and `defs`,
    // whose `y` feeds the long print and `helper`, used by cell 13.
    assert_eq!(graph.children(0), [1, 2]);
    assert_eq!(graph.run_order(&[0]), [0, 1, 2, 7, 12, 13]);
    assert!(matches!(graph.error(9), Some(marimo::CellError::Syntax(_))));
}

#[test]
fn not_marimo() {
    assert!(Notebook::parse(Kind::Marimo, "print(1)\n").is_err());
    assert!(Notebook::parse(Kind::Marimo, "import marimo\nprint(1)\n").is_err());
}
