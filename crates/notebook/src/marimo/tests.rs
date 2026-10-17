use super::*;

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

/// Expectations from marimo 0.25.1:
/// `compile_cell(code)` → `sorted(defs | unmangled temporaries)`, `sorted(refs)`.
#[test]
fn analysis_matches_marimo() {
    let cases: &[(&str, &[&str], &[&str])] = &[
        ("x = 1", &["x"], &[]),
        ("a, (b, *c) = f()", &["a", "b", "c"], &["f"]),
        ("x += y", &["x"], &["y"]),
        ("y: int = z", &["y"], &["int", "z"]),
        ("z: list[T]", &["z"], &["T", "list"]),
        (
            "import a.b.c\nimport d as e\nfrom f import g, h as i",
            &["a", "e", "g", "i"],
            &[],
        ),
        (
            "def f(p, q=default, *args, k: K = kd, **kw) -> R:\n    return p + q + free + f",
            &["f"],
            &["K", "R", "default", "free", "kd"],
        ),
        (
            "class C(Base, metaclass=Meta):\n    attr = value\n    def m(self):\n        return attr2 + C",
            &["C"],
            &["Base", "Meta", "attr2", "value"],
        ),
        (
            "for i in items:\n    total = i\nelse:\n    done = True",
            &["done", "i", "total"],
            &["items"],
        ),
        (
            "with open(path) as fh, ctx() as (u, v):\n    data = fh.read()",
            &["data", "fh", "u", "v"],
            &["ctx", "open", "path"],
        ),
        (
            "try:\n    risky()\nexcept ValueError as err:\n    print(err)\nfinally:\n    cleanup()",
            &[],
            &["ValueError", "cleanup", "print", "risky"],
        ),
        (
            "if (n := len(xs)) > 10:\n    print(n)",
            &["n"],
            &["len", "print", "xs"],
        ),
        ("[w for w in words if w]", &[], &["words"]),
        ("[(lambda q: q + r)(1) for r in rs]", &[], &["rs"]),
        ("{k: v for k, v in pairs.items()}", &[], &["pairs"]),
        ("[y := f(x) for x in data]", &["y"], &["data", "f"]),
        ("print(x)\nx = 1", &["x"], &["print", "x"]),
        (
            "def g():\n    return later\nlater = 1",
            &["g", "later"],
            &[],
        ),
        ("def h():\n    global gl\n    gl = 1", &["gl", "h"], &[]),
        ("del old", &[], &["old"]),
        (
            "_private = 1\nprint(_private, _other)",
            &["_private"],
            &["print"],
        ),
        (
            "match cmd:\n    case [\"go\", direction]:\n        pass\n    case Point(x=px, y=0) as pt:\n        pass\n    case {\"k\": kv, **rest}:\n        pass\n    case [first, *others]:\n        pass\n    case Color.RED:\n        pass\n    case _:\n        pass",
            &["direction", "first", "kv", "others", "pt", "px", "rest"],
            &["Color", "Point", "cmd"],
        ),
        ("lambda a, b=dflt: a + b + c", &[], &["c", "dflt"]),
        (
            "f\"{name!r:>{width}} {obj.attr}\"",
            &[],
            &["name", "obj", "width"],
        ),
        (
            "async def af():\n    await thing()\nresult = await other()",
            &["af", "result"],
            &["other", "thing"],
        ),
        ("x = x + 1", &["x"], &["x"]),
        (
            "def rec(n):\n    return rec(n - 1) if n else base",
            &["rec"],
            &["base"],
        ),
        (
            "nums = [i for i in range(n)]\nprint(i)",
            &["nums"],
            &["i", "n", "print", "range"],
        ),
        (
            "@decorator(arg)\ndef decorated():\n    pass",
            &["decorated"],
            &["arg", "decorator"],
        ),
        ("obj.attr = 1\nseq[idx] = 2", &[], &["idx", "obj", "seq"]),
        (
            "def outer():\n    def inner():\n        return captured\n    captured = 1\n    return inner",
            &["outer"],
            &[],
        ),
        ("type Alias = list[Elem]", &["Alias"], &["Elem", "list"]),
        ("def gen[T](v: T) -> T:\n    return v", &["gen"], &[]),
        (
            "print(__name__, __file__, len)",
            &[],
            &["__file__", "__name__", "len", "print"],
        ),
        ("__dunder = 1", &["__dunder"], &[]),
        (
            "from . import sibling\nfrom ..pkg import thing as other",
            &["other", "sibling"],
            &[],
        ),
        (
            "class K:\n    x = 1\n    y = x + 1\n    z = [x for _ in range(3)]",
            &["K"],
            &["range"],
        ),
        ("while cond:\n    step()\n    break", &[], &["cond", "step"]),
        (
            "assert check, message\nraise Error from cause",
            &[],
            &["Error", "cause", "check", "message"],
        ),
        ("x: int", &["x"], &["int"]),
        ("a = b = c", &["a", "b"], &["c"]),
        ("for k in d: pass\nprint(k)", &["k"], &["d", "print"]),
        ("def f(x=x): pass", &["f"], &["x"]),
        ("import os.path as osp", &["osp"], &[]),
        (
            "print(e)\ntry:\n    pass\nexcept Exception as e:\n    pass",
            &[],
            &["Exception", "e", "print"],
        ),
    ];
    for (code, defs, refs) in cases {
        let analysis = analyze(code);
        assert_eq!(analysis.syntax_error, None, "{code}");
        assert_eq!(analysis.defs, set(defs), "defs of {code:?}");
        assert_eq!(analysis.refs, set(refs), "refs of {code:?}");
    }
}

#[test]
fn syntax_errors() {
    for code in [
        "x = (1,",
        "def f(:\n    pass",
        "return 1",
        "break",
        "from os import *",
    ] {
        let analysis = analyze(code);
        assert!(analysis.syntax_error.is_some(), "{code}");
        assert!(analysis.defs.is_empty() && analysis.refs.is_empty());
    }
    assert_eq!(analyze("for x in y:\n    break").syntax_error, None);
    assert_eq!(analyze("def f():\n    return 1").syntax_error, None);
}

#[test]
fn builtins() {
    assert!(is_builtin("print"));
    assert!(is_builtin("__file__"));
    assert!(is_builtin("ExceptionGroup"));
    assert!(!is_builtin("mo"));
}

#[test]
fn public_defs() {
    let analysis = analyze("_a = 1\nb = 2");
    assert_eq!(analysis.public_defs().collect::<Vec<_>>(), ["b"]);
}

fn graph(codes: &[&str]) -> Graph {
    let analyses: Vec<Analysis> = codes.iter().map(|c| analyze(c)).collect();
    Graph::new(&analyses)
}

#[test]
fn graph_edges_and_order() {
    // 0 defines a, 1 uses a and defines b, 2 uses b, 3 independent, 4 uses a and b.
    let g = graph(&[
        "c = b + 1",
        "a = 1",
        "b = a * 2",
        "print('hi')",
        "print(a, b, c)",
    ]);
    assert_eq!(g.parents(0), [2]);
    assert_eq!(g.children(1), [2, 4]);
    assert_eq!(g.topological(), [1, 2, 0, 3, 4]);
    assert_eq!(g.run_order(&[1]), [1, 2, 0, 4]);
    assert_eq!(g.run_order(&[3]), [3]);
    assert_eq!(g.run_order(&[0, 2]), [2, 0, 4]);
    assert_eq!(g.ancestors(4), [1, 2, 0]);
    assert_eq!(g.ancestors(1), Vec::<usize>::new());
    assert!((0..5).all(|c| g.error(c).is_none()));
    // Private names make no edges.
    let g = graph(&["_x = 1", "print(_x)"]);
    assert!(g.children(0).is_empty());
}

#[test]
fn graph_errors() {
    let g = graph(&[
        "x = 1",
        "x = 2\ny = 0",
        "print(x)",
        "p = q",
        "q = p",
        "z = (",
    ]);
    assert_eq!(
        g.error(0),
        Some(&CellError::MultipleDefinitions {
            names: vec!["x".into()],
            cells: vec![1]
        })
    );
    assert_eq!(
        g.error(1).unwrap().to_string(),
        "The variable 'x' was defined by another cell (also defined by cell 1)"
    );
    assert_eq!(g.error(2), None);
    assert_eq!(g.error(3), Some(&CellError::Cycle { cells: vec![3, 4] }));
    assert_eq!(
        g.error(4).unwrap().to_string(),
        "This cell is in a cycle (cells 4, 5)"
    );
    assert!(matches!(g.error(5), Some(CellError::Syntax(_))));
    // Cycle members run together, in notebook order.
    assert_eq!(g.run_order(&[4]), [3, 4]);
    assert_eq!(g.topological(), [0, 1, 2, 3, 4, 5]);
    assert_eq!(
        CellError::MultipleDefinitions {
            names: vec!["a".into(), "b".into()],
            cells: vec![0, 2]
        }
        .to_string(),
        "The variables 'a', 'b' were defined by another cell (also defined by cells 1, 3)"
    );
}

#[test]
fn markdown_extraction() {
    let md = extract_markdown("mo.md(r\"\"\"\n# Title\n\nSome *text*.\n\"\"\")").unwrap();
    assert_eq!(md.text, "# Title\n\nSome *text*.");
    assert_eq!(md.tag, "r");
    let md = extract_markdown("mo.md(\"\"\"\n    Indented\n    text\\n\n\"\"\")").unwrap();
    assert_eq!(md.text, "Indented\ntext");
    assert!(extract_markdown("mo.md(f\"{x}\")").is_none());
    assert!(extract_markdown("mo.md(f\"plain\")").is_none());
    assert!(extract_markdown("title = mo.md(\"x\")").is_none());
    assert!(extract_markdown("mo.md(\"a\")\nmo.md(\"b\")").is_none());
    assert_eq!(
        extract_markdown("# comment\nmo.md(r'''x''')  # trailing")
            .unwrap()
            .text,
        "x"
    );
}

#[test]
fn markdown_cells_write_back() {
    let mut cell = Cell::new(CellKind::Markdown, "# Hi");
    assert_eq!(cell_code(&cell), "mo.md(r\"\"\"\n# Hi\n\"\"\")");
    cell.marimo = Some(MarimoCell {
        name: "_".into(),
        original: Some(("# Hi".into(), "mo.md(\"# Hi\")".into())),
        ..MarimoCell::default()
    });
    assert_eq!(cell_code(&cell), "mo.md(\"# Hi\")");
    cell.source = "# Hello \"\"quoted\"\"".into();
    assert_eq!(
        cell_code(&cell),
        "mo.md(\"\"\"\n# Hello \"\\\"quoted\"\\\"\n\"\"\")"
    );
}

#[test]
fn tuple_formatting() {
    let elems = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
    assert_eq!(format_tuple("return (...)", &[], true, true), "    return");
    assert_eq!(
        format_tuple("return (...)", &elems(&["x"]), true, true),
        "    return (x,)"
    );
    assert_eq!(
        format_tuple("return (...)", &elems(&["a", "b"]), true, true),
        "    return a, b"
    );
    assert_eq!(format_tuple("def _(...):", &[], false, false), "def _():");
    let long = elems(&[
        "a_very_long_name_number_one",
        "a_very_long_name_number_two",
        "xyz_abcdefghi",
    ]);
    // 79 characters: one line; 80: one name per line.
    assert_eq!(
        format_tuple("def _(...):", &long, false, false),
        "def _(a_very_long_name_number_one, a_very_long_name_number_two, xyz_abcdefghi):"
    );
    let long = elems(&[
        "a_very_long_name_number_one",
        "a_very_long_name_number_two",
        "xyz_abcdefghij",
    ]);
    assert_eq!(
        format_tuple("def _(...):", &long, false, false),
        "def _(\n    a_very_long_name_number_one,\n    a_very_long_name_number_two,\n    xyz_abcdefghij,\n):"
    );
}

#[test]
fn is_marimo_detection() {
    assert!(is_marimo(
        "import marimo\n\napp = marimo.App()\n\n@app.cell\ndef _():\n    return\n"
    ));
    assert!(!is_marimo("import marimo\nprint(1)\n"));
    assert!(!is_marimo("app = marimo.App()\n"));
}
