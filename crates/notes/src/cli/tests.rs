use std::fs;

use super::*;

fn parsed(line: &str) -> Result<Invocation, String> {
    let args: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
    parse(&args)
}

fn command(line: &str) -> Command {
    parsed(line).unwrap().command
}

#[test]
fn parses_commands() {
    assert_eq!(command(""), Command::Help);
    assert_eq!(command("--help"), Command::Help);
    assert_eq!(
        command("list src --all --to agent"),
        Command::List {
            file: Some("src".into()),
            status: None,
            to: Some("agent".into()),
        }
    );
    assert_eq!(
        command("list"),
        Command::List {
            file: None,
            status: Some(Status::Open),
            to: None,
        }
    );
    assert_eq!(
        command("add src/main.rs:12-14 why clone here? --to user"),
        Command::Add {
            file: "src/main.rs".into(),
            start: 12,
            end: 14,
            columns: None,
            cell: None,
            text: "why clone here?".into(),
            to: Some("user".into()),
            color: None,
        }
    );
    let Command::Add {
        file,
        start,
        end,
        columns,
        cell,
        ..
    } = command(r"add C:\p\a.rs:3:5-4:9 x --cell 2")
    else {
        panic!("not add");
    };
    assert_eq!(
        (file.as_str(), start, end, columns, cell),
        (r"C:\p\a.rs", 3, 4, Some((5, 9)), Some(2))
    );
    assert_eq!(
        command("reply n1a2b sounds good"),
        Command::Reply {
            id: "n1a2b".into(),
            text: "sounds good".into(),
        }
    );
    assert_eq!(command("resolve n1"), Command::Resolve { id: "n1".into() });
    assert_eq!(
        command("wait --to agent --timeout 1.5 --since 2026-10-18T12:00:00Z"),
        Command::Wait {
            to: Some("agent".into()),
            timeout: Some(Duration::from_millis(1500)),
            since: parse_time("2026-10-18T12:00:00Z"),
        }
    );
    // Common options anywhere.
    let invocation = parsed("reply --as claude n1 ok --format json --root /p --from user").unwrap();
    assert_eq!(invocation.name.as_deref(), Some("claude"));
    assert_eq!(invocation.kind, AuthorKind::User);
    assert!(invocation.json);
    assert_eq!(invocation.root, Some(PathBuf::from("/p")));
    assert_eq!(invocation.author(), Author::user("claude"));
}

#[test]
fn rejects_bad_arguments() {
    for line in [
        "frobnicate",
        "add src/main.rs why",
        "add src/main.rs:0 why",
        "add src/main.rs:5-3 why",
        "add src/main.rs:5",
        "show",
        "show a b",
        "reply n1",
        "wait --timeout soon",
        "wait --since yesterday",
        "list --bogus",
        "list --format yaml",
        "list --from robot",
    ] {
        assert!(parsed(line).is_err(), "{line}");
    }
}

fn project(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("next_code_notes_cli_{name}_{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(dir.join(".git")).unwrap();
    fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("src/main.rs"),
        "fn main() {\n    let a = 1;\n    let b = 2;\n}\n",
    )
    .unwrap();
    dir
}

/// Runs `line` in `cwd`; exit code and output.
fn run_in(cwd: &Path, line: &str) -> (i32, String) {
    let invocation = parsed(line).unwrap();
    let mut out = Vec::new();
    let code = execute(&invocation, cwd, &mut out, Duration::from_millis(20)).unwrap();
    (code, String::from_utf8(out).unwrap())
}

#[test]
fn add_list_reply_resolve() {
    let root = project("flow");
    let src = root.join("src");
    // From a subfolder: the root is found, the file is relative to cwd.
    let (code, out) = run_in(&src, "add main.rs:2-3 Why two lets? --as claude");
    assert_eq!(code, 0, "{out}");
    let id = out.split_whitespace().next().unwrap().to_owned();
    assert!(
        out.starts_with(&format!("{id}  src/main.rs:2-3  open  to user\n")),
        "{out}"
    );
    assert!(out.contains("[agent claude, "), "{out}");
    assert!(out.contains("    Why two lets?"), "{out}");

    // Lines inserted above: the note follows.
    fs::write(
        root.join("src/main.rs"),
        "// top\nfn main() {\n    let a = 1;\n    let b = 2;\n}\n",
    )
    .unwrap();
    let (_, out) = run_in(&root, "list");
    assert!(
        out.starts_with(&format!("{id}  src/main.rs:3-4  open  to user  (moved)")),
        "{out}"
    );

    let (_, out) = run_in(
        &root,
        &format!("reply {} Fine. --from user --as amedeo", &id[..3]),
    );
    assert!(out.contains("[user amedeo, "), "{out}");
    let (_, out) = run_in(&root, &format!("show {id}"));
    assert!(out.contains("      3 |     let a = 1;"), "{out}");

    let (_, out) = run_in(&root, &format!("resolve {id} --format json --as claude"));
    let note: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(note["status"], "resolved");
    assert_eq!(note["at"]["state"], "moved");
    assert_eq!(note["thread"].as_array().unwrap().len(), 2);
    assert_eq!(run_in(&root, "list").1, "no notes\n");
    assert!(
        run_in(&root, "list --all src")
            .1
            .contains("resolved by claude")
    );
    assert!(run_in(&root, "list --all lib").1.contains("no notes"));

    let excluded = fs::read_to_string(root.join(".git/info/exclude")).unwrap();
    assert!(excluded.contains(crate::EXCLUDE_PATTERN));
    let (_, out) = run_in(&root, &format!("delete {id}"));
    assert!(out.starts_with(&format!("deleted {id}")), "{out}");
    fs::remove_dir_all(root).ok();
}

#[test]
fn add_checks_the_file() {
    let root = project("missing");
    let invocation = parsed("add nope.rs:1 x").unwrap();
    let err = execute(&invocation, &root, &mut Vec::new(), Duration::ZERO).unwrap_err();
    assert!(err.to_string().contains("no file nope.rs"), "{err}");
    let invocation = parsed("add src/main.rs:99 x").unwrap();
    assert!(execute(&invocation, &root, &mut Vec::new(), Duration::ZERO).is_err());
    fs::remove_dir_all(root).ok();
}

#[test]
fn adds_notes_on_notebook_cells() {
    let root = project("cells");
    let notebook = json!({
        "cells": [
            { "cell_type": "markdown", "source": ["# Title\n"] },
            { "cell_type": "code", "source": ["import os\n", "print(os.getcwd())\n"] },
        ],
        "metadata": {}, "nbformat": 4, "nbformat_minor": 5,
    });
    fs::write(root.join("a.ipynb"), notebook.to_string()).unwrap();
    let (code, out) = run_in(&root, "add a.ipynb:2 cwd? --cell 2 --format json");
    assert_eq!(code, 0, "{out}");
    let note: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(note["cell"], 2);
    assert_eq!(note["anchor"]["text"], "print(os.getcwd())");
    assert_eq!(note["at"]["state"], "exact");
    fs::remove_dir_all(root).ok();
}

#[test]
fn wait_returns_on_a_new_user_message() {
    let root = project("wait");
    let (_, out) = run_in(&root, "add src/main.rs:1 Look here --as claude");
    let id = out.split_whitespace().next().unwrap().to_owned();
    let writer = {
        let root = root.clone();
        let id = id.clone();
        thread::spawn(move || {
            let store = Store::open(&root);
            thread::sleep(Duration::from_millis(150));
            // Another agent's message on a note to the user: not for us.
            store.reply(&id, "me too", Author::agent("other")).unwrap();
            // Our own message never wakes us.
            store
                .reply(&id, "still here", Author::agent("claude"))
                .unwrap();
            thread::sleep(Duration::from_millis(150));
            store
                .reply(&id, "Done, have a look", Author::user("amedeo"))
                .unwrap();
        })
    };
    let started = Instant::now();
    let (code, out) = run_in(&root, "wait --as claude --timeout 10");
    writer.join().unwrap();
    assert_eq!(code, 0, "{out}");
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert!(out.contains("* [user amedeo, "), "{out}");
    assert!(out.contains("  [agent other, "), "{out}");
    let since = out
        .lines()
        .last()
        .unwrap()
        .strip_prefix("since: ")
        .unwrap()
        .to_owned();
    assert!(parse_time(&since).is_some());

    // Nothing newer than `since`: times out.
    let (code, _) = run_in(
        &root,
        &format!("wait --as claude --timeout 0.2 --since {since}"),
    );
    assert_eq!(code, 1);
    // A note addressed to agents wakes `--to agent`, whoever wrote it.
    let (_, out) = run_in(&root, "add src/main.rs:2 for you --as other --to agent");
    let to_agent = out.split_whitespace().next().unwrap().to_owned();
    let (code, out) = run_in(
        &root,
        &format!("wait --as claude --to agent --timeout 2 --since {since} --format json"),
    );
    assert_eq!(code, 0, "{out}");
    let reply: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(reply["new"], json!([{ "note": to_agent, "index": 0 }]));
    fs::remove_dir_all(root).ok();
}
