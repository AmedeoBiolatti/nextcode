//! `next_code --headless` end to end on a local project: the JSON-lines
//! contract scripts rely on. Language servers depend on the machine: tests
//! that need one are skipped when it is not on PATH.

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

struct Headless {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<Value>,
    /// Lines read while waiting for something else, in arrival order.
    backlog: Vec<Value>,
    next_id: u64,
}

impl Headless {
    fn start(root: &PathBuf) -> Self {
        Self::start_with(root, &[])
    }

    fn start_with(root: &PathBuf, env: &[(&str, &std::path::Path)]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_next_code"))
            .arg("--headless")
            .arg(root)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                tx.send(serde_json::from_str(&line).unwrap()).ok();
            }
        });
        Self {
            child,
            stdin,
            lines,
            backlog: Vec::new(),
            next_id: 0,
        }
    }

    /// The first line (earlier ones included) that `matches`.
    fn next(&mut self, matches: impl Fn(&Value) -> bool) -> Value {
        if let Some(index) = self.backlog.iter().position(&matches) {
            return self.backlog.remove(index);
        }
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(120))
                .expect("no output from next_code --headless");
            if matches(&line) {
                return line;
            }
            self.backlog.push(line);
        }
    }

    fn call(&mut self, mut request: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        request["id"] = id.into();
        writeln!(self.stdin, "{request}").unwrap();
        self.next(|line| line["id"] == id)
    }
}

#[test]
fn headless_mode_serves_files_search_watch_and_commands() {
    let root = std::env::temp_dir().join(format!("next_code_headless_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join(".gitignore"), "*.log\n").unwrap();
    fs::write(root.join("debug.log"), "needle\n").unwrap();
    fs::write(root.join("src").join("a.txt"), "one\nneedle two\n").unwrap();
    let root = root.canonicalize().unwrap();

    let mut headless = Headless::start(&root);
    let ready = headless.next(|line| line["event"].is_string());
    assert_eq!(ready["event"], "ready", "{ready}");
    assert_eq!(ready["remote"], false);

    let files = headless.call(json!({ "cmd": "list_files" }));
    let a = PathBuf::from("src")
        .join("a.txt")
        .to_string_lossy()
        .into_owned();
    assert_eq!(files["result"], json!([".gitignore", a]), "{files}");

    let search = headless.call(json!({ "cmd": "search", "pattern": "needle" }));
    let found = &search["result"]["files"];
    assert_eq!(
        found.as_array().unwrap().len(),
        1,
        "ignored files are not searched: {search}"
    );
    assert_eq!(found[0]["lines"][0]["line"], 1);

    assert_eq!(headless.call(json!({ "cmd": "watch" }))["ok"], true);
    let write = headless.call(json!({ "cmd": "write", "path": "src/b.txt", "text": "héllo\n" }));
    assert_eq!(write["ok"], true, "{write}");
    let changed = headless.next(|line| line["event"] == "files_changed");
    assert!(
        changed["changes"].to_string().contains("b.txt"),
        "{changed}"
    );
    let read = headless.call(json!({ "cmd": "read", "path": "src/b.txt" }));
    assert_eq!(read["result"]["text"], "héllo\n");
    assert_eq!(
        headless.call(json!({ "cmd": "stat", "path": "src" }))["result"]["kind"],
        "dir"
    );

    let exec = headless.call(json!({ "cmd": "exec", "command": "echo from-shell" }));
    assert_eq!(exec["result"]["code"], 0, "{exec}");
    assert!(
        exec["result"]["stdout"]
            .as_str()
            .unwrap()
            .contains("from-shell")
    );

    let missing = headless.call(json!({ "cmd": "read", "path": "missing.txt" }));
    assert_eq!(missing["ok"], false);
    let unknown = headless.call(json!({ "cmd": "frobnicate" }));
    assert!(unknown["error"].as_str().unwrap().contains("frobnicate"));

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn headless_mode_stages_diffs_and_commits_with_git() {
    let root = std::env::temp_dir().join(format!("next_code_headless_git_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(&root).unwrap();
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "commit.gpgsign", "false"]);
    git(&["config", "core.autocrlf", "false"]);
    fs::write(root.join("a.txt"), "one\n").unwrap();
    git(&["add", "a.txt"]);
    git(&["commit", "-q", "-m", "init"]);
    let root = root.canonicalize().unwrap();

    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    assert_eq!(headless.call(json!({ "cmd": "watch" }))["ok"], true);

    fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();
    fs::write(root.join("new.txt"), "new\n").unwrap();
    let status = headless.call(json!({ "cmd": "git_status" }));
    assert_eq!(status["result"]["branch"], "main", "{status}");
    assert_eq!(
        status["result"]["entries"],
        json!([
            { "path": "a.txt", "orig_path": null, "staged": null, "unstaged": "modified" },
            { "path": "new.txt", "orig_path": null, "staged": null, "unstaged": "untracked" },
        ])
    );

    let diff = headless.call(json!({ "cmd": "git_diff", "path": "a.txt" }));
    assert_eq!(
        (
            diff["result"]["added"].clone(),
            diff["result"]["removed"].clone()
        ),
        (json!(1), json!(0)),
        "{diff}"
    );

    let stage = headless.call(json!({ "cmd": "git_stage", "paths": ["a.txt"] }));
    assert_eq!(stage["ok"], true, "{stage}");
    let staged = headless.call(json!({ "cmd": "git_diff", "path": "a.txt", "staged": true }));
    assert_eq!(staged["result"]["added"], 1, "{staged}");

    // Forget the staging's own repository events before committing.
    thread::sleep(Duration::from_millis(800));
    while let Ok(line) = headless.lines.try_recv() {
        headless.backlog.push(line);
    }
    headless
        .backlog
        .retain(|line| line["event"] != "files_changed");
    let commit = headless.call(json!({ "cmd": "git_commit", "message": "add two" }));
    assert!(
        commit["result"]["summary"]
            .as_str()
            .unwrap()
            .ends_with(" add two"),
        "{commit}"
    );
    // The commit moves the branch ref: watchers learn the status changed.
    headless.next(|line| line["event"] == "files_changed" && line["repository"] == true);

    // History: two commits on main, newest first, with the branch label.
    let log = headless.call(json!({ "cmd": "git_log", "limit": 10 }));
    let commits = log["result"]["commits"].as_array().unwrap();
    assert_eq!(commits.len(), 2, "{log}");
    assert_eq!(commits[0]["subject"], "add two");
    assert_eq!(commits[0]["refs"], json!(["HEAD -> main"]));
    assert_eq!(log["result"]["graph"][0]["column"], 0);
    let refs = headless.call(json!({ "cmd": "git_refs" }));
    assert_eq!(refs["result"][0]["name"], "main", "{refs}");

    // The last commit, and comparing its parent with the working tree.
    let shown = headless.call(json!({ "cmd": "git_commit_changes", "commit": "HEAD" }));
    assert_eq!(
        shown["result"]["changes"],
        json!([{ "path": "a.txt", "orig_path": null, "change": "modified" }]),
        "{shown}"
    );
    fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let changes = headless.call(json!({ "cmd": "git_changes", "base": "HEAD~1" }));
    assert_eq!(changes["result"][0]["path"], "a.txt", "{changes}");
    let against = headless.call(json!({
        "cmd": "git_diff", "path": "a.txt", "old": "HEAD~1", "whole_file": true,
    }));
    assert_eq!(
        (
            against["result"]["added"].clone(),
            against["result"]["removed"].clone()
        ),
        (json!(2), json!(0)),
        "{against}"
    );
    let between = headless.call(json!({
        "cmd": "git_diff", "path": "a.txt", "old": "HEAD~1", "new": "HEAD",
    }));
    assert_eq!(between["result"]["added"], 1, "{between}");
    git(&["checkout", "-q", "--", "a.txt"]);

    let discard = headless.call(json!({ "cmd": "git_discard", "paths": ["new.txt"] }));
    assert_eq!(discard["ok"], true, "{discard}");
    assert!(!root.join("new.txt").exists());
    let clean = headless.call(json!({ "cmd": "git_status" }));
    assert_eq!(clean["result"]["entries"], json!([]), "{clean}");

    // Hunks: stage one of two edits, then discard the other.
    let lines: String = (1..=20).map(|n| format!("line {n}\n")).collect();
    fs::write(root.join("b.txt"), &lines).unwrap();
    git(&["add", "b.txt"]);
    git(&["commit", "-q", "-m", "add b"]);
    fs::write(
        root.join("b.txt"),
        lines
            .replace("line 2\n", "line two\n")
            .replace("line 19\n", "line nineteen\n"),
    )
    .unwrap();
    let hunks = |diff: &Value| -> Vec<Value> {
        diff["result"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|row| row.get("hunk").cloned())
            .collect()
    };
    let diff = headless.call(json!({ "cmd": "git_diff", "path": "b.txt" }));
    assert_eq!(hunks(&diff).len(), 2, "{diff}");
    let staged = headless.call(json!({
        "cmd": "git_apply_hunk", "path": "b.txt", "hunk": 1, "action": "stage",
    }));
    assert_eq!(staged["ok"], true, "{staged}");
    let diff = headless.call(json!({ "cmd": "git_diff", "path": "b.txt", "staged": true }));
    let staged_hunks = hunks(&diff);
    assert_eq!(staged_hunks.len(), 1, "{diff}");
    let header = staged_hunks[0]["header"].as_str().unwrap();
    assert!(header.starts_with("@@ -16"), "{header}");
    let discarded = headless.call(json!({
        "cmd": "git_apply_hunk", "path": "b.txt", "hunk": 0, "action": "discard",
    }));
    assert_eq!(discarded["ok"], true, "{discarded}");
    assert_eq!(
        fs::read_to_string(root.join("b.txt")).unwrap(),
        lines.replace("line 19\n", "line nineteen\n")
    );
    let amend = headless.call(json!({
        "cmd": "git_commit", "message": "add b, edited", "amend": true,
    }));
    assert!(
        amend["result"]["summary"]
            .as_str()
            .unwrap()
            .ends_with(" add b, edited"),
        "{amend}"
    );
    let log = headless.call(json!({ "cmd": "git_log", "limit": 10 }));
    assert_eq!(
        log["result"]["commits"].as_array().unwrap().len(),
        3,
        "{log}"
    );

    // Branches: create and switch, switch back, delete.
    let created = headless.call(json!({ "cmd": "git_branch_create", "name": "topic" }));
    assert_eq!(created["ok"], true, "{created}");
    let status = headless.call(json!({ "cmd": "git_status" }));
    assert_eq!(status["result"]["branch"], "topic");
    let back = headless.call(json!({ "cmd": "git_switch", "name": "main" }));
    assert_eq!(back["ok"], true, "{back}");
    let deleted = headless.call(json!({ "cmd": "git_branch_delete", "name": "topic" }));
    assert_eq!(deleted["ok"], true, "{deleted}");
    let bad = headless.call(json!({ "cmd": "git_branch_create", "name": "a..b" }));
    assert_eq!(bad["ok"], false, "{bad}");

    // Worktrees: add one next to the repository, list, remove.
    let added = headless.call(json!({
        "cmd": "git_worktree_add", "name": "side", "create": true,
    }));
    let path = added["result"]["path"]
        .as_str()
        .expect("worktree path")
        .to_owned();
    let worktrees = headless.call(json!({ "cmd": "git_worktrees" }));
    let worktrees = worktrees["result"].as_array().unwrap();
    assert_eq!(worktrees.len(), 2);
    assert_eq!(worktrees[0]["current"], true);
    assert_eq!(worktrees[1]["branch"], "side");
    let removed = headless.call(json!({ "cmd": "git_worktree_remove", "path": path }));
    assert_eq!(removed["ok"], true, "{removed}");
    assert!(!std::path::Path::new(&path).exists());
    // No remote: pushing fails with git's message rather than prompting.
    let push = headless.call(json!({ "cmd": "git_push" }));
    assert_eq!(push["ok"], false, "{push}");

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn headless_mode_reads_python_projects_and_dev_container_configs() {
    let root = std::env::temp_dir().join(format!("next_code_headless_py_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::create_dir_all(root.join(".devcontainer")).unwrap();
    fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"demo\"\nversion = \"0.2.0\"\nrequires-python = \">=3.12\"\n\
         dependencies = [\"httpx>=0.28\"]\n\n[project.scripts]\ndemo = \"demo:main\"\n\n\
         [dependency-groups]\ndev = [\"pytest\"]\n",
    )
    .unwrap();
    fs::write(
        root.join("uv.lock"),
        "version = 1\n[[package]]\nname = \"httpx\"\nversion = \"0.28.1\"\n",
    )
    .unwrap();
    fs::write(root.join(".python-version"), "3.12\n").unwrap();
    fs::write(
        root.join("tests/test_a.py"),
        "class TestA:\n    def test_ok(self):\n        assert True\n",
    )
    .unwrap();
    fs::write(
        root.join(".devcontainer/devcontainer.json"),
        "{ // comment\n \"image\": \"python:3.12\" }\n",
    )
    .unwrap();

    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    let info = headless.call(json!({ "cmd": "python_info" }));
    let info = &info["result"];
    assert_eq!(
        (&info["name"], &info["version"], &info["pinned"]),
        (&json!("demo"), &json!("0.2.0"), &json!("3.12")),
        "{info}"
    );
    assert_eq!(
        info["dependencies"],
        json!([
            { "name": "httpx", "specifier": ">=0.28", "kind": "dependencies", "locked": "0.28.1" },
            { "name": "pytest", "specifier": "", "kind": "group: dev", "locked": null },
        ])
    );
    assert_eq!(info["scripts"], json!({ "demo": "demo:main" }));
    assert_eq!(info["locked_packages"], 1);

    let test =
        headless.call(json!({ "cmd": "python_test_at", "path": "tests/test_a.py", "line": 2 }));
    assert_eq!(test["result"], "TestA::test_ok", "{test}");
    let outside =
        headless.call(json!({ "cmd": "python_test_at", "path": "tests/test_a.py", "line": 0 }));
    assert_eq!(outside["result"], "TestA", "{outside}");

    let configs = headless.call(json!({ "cmd": "devcontainer_configs" }));
    let configs = configs["result"].as_array().unwrap();
    assert_eq!(configs.len(), 1);
    assert!(configs[0].as_str().unwrap().ends_with("devcontainer.json"));

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn headless_mode_decodes_rust_analyzer_semantic_tokens() {
    if Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_err()
    {
        println!("skipped: rust-analyzer is not on PATH");
        return;
    }
    let root = std::env::temp_dir().join(format!("next_code_headless_rs_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    fs::write(
        root.join("src").join("main.rs"),
        "struct Point {\n    x: i32,\n}\n\n\
         fn shift(point: Point, by: i32) -> i32 {\n    let mut total = point.x;\n    total += by;\n    total\n}\n\n\
         fn main() {\n    let p = Point { x: 1 };\n    println!(\"{}\", shift(p, 2));\n}\n",
    )
    .unwrap();
    let root = root.canonicalize().unwrap();

    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    let open =
        headless.call(json!({ "cmd": "open", "path": "src/main.rs", "timeout_ms": 120_000 }));
    assert_eq!(open["ok"], true, "{open}");

    let token = |tokens: &[Value], line: u64, text: &str| {
        tokens
            .iter()
            .find(|token| token["line"] == line && token["text"] == text)
            .cloned()
    };
    let has_mutable = |token: &Option<Value>| {
        token.as_ref().is_some_and(|token| {
            token["modifiers"]
                .as_array()
                .is_some_and(|modifiers| modifiers.contains(&json!("mutable")))
        })
    };
    // Until rust-analyzer has loaded the crate, its tokens lack semantics.
    let deadline = Instant::now() + Duration::from_secs(300);
    let tokens = loop {
        let reply = headless.call(json!({ "cmd": "semantic_tokens", "path": "src/main.rs" }));
        assert_eq!(reply["ok"], true, "{reply}");
        let tokens = reply["result"].as_array().unwrap().clone();
        let ready = token(&tokens, 4, "by").is_some_and(|t| t["scope"] == "variable.parameter")
            && token(&tokens, 0, "Point").is_some_and(|t| t["scope"] == "type.struct")
            && has_mutable(&token(&tokens, 5, "total"));
        if ready || Instant::now() >= deadline {
            break tokens;
        }
        thread::sleep(Duration::from_secs(1));
    };
    let excerpt: Vec<&Value> = tokens
        .iter()
        .filter(|token| token["line"].as_u64().is_some_and(|line| line <= 5))
        .collect();
    println!("semantic_tokens (lines 0-5): {}", json!(excerpt));

    let by = token(&tokens, 4, "by").expect("token for `by`");
    assert_eq!(by["scope"], "variable.parameter", "{by}");
    assert_eq!((&by["start"], &by["end"]), (&json!(23), &json!(25)), "{by}");
    let point = token(&tokens, 0, "Point").expect("token for `Point`");
    assert_eq!(point["scope"], "type.struct", "{point}");
    let total = token(&tokens, 5, "total");
    assert!(has_mutable(&total), "{total:?}");

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).ok();
}

#[test]
fn headless_mode_shows_rust_analyzer_call_hierarchy() {
    if Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_err()
    {
        println!("skipped: rust-analyzer is not on PATH");
        return;
    }
    let root =
        std::env::temp_dir().join(format!("next_code_headless_calls_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"calls\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    fs::write(
        root.join("src").join("main.rs"),
        "fn leaf() {}\n\nfn caller() {\n    leaf();\n}\n\nfn main() {\n    caller();\n}\n",
    )
    .unwrap();
    let root = root.canonicalize().unwrap();

    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    let open =
        headless.call(json!({ "cmd": "open", "path": "src/main.rs", "timeout_ms": 120_000 }));
    assert_eq!(open["ok"], true, "{open}");

    // Until rust-analyzer has loaded the crate, it may find no calls.
    let mut calls = |cmd: &str, line: u64| {
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let reply = headless
                .call(json!({ "cmd": cmd, "path": "src/main.rs", "line": line, "character": 4 }));
            assert_eq!(reply["ok"], true, "{reply}");
            let calls = reply["result"].as_array().unwrap().clone();
            if !calls.is_empty() || Instant::now() >= deadline {
                break calls;
            }
            thread::sleep(Duration::from_secs(1));
        }
    };

    let incoming = calls("incoming_calls", 0);
    assert_eq!(incoming.len(), 1, "{incoming:?}");
    let caller = &incoming[0];
    assert_eq!(caller["name"], "caller", "{caller}");
    assert_eq!(caller["kind"], 12, "a function: {caller}");
    assert!(
        caller["path"].as_str().unwrap().ends_with("main.rs"),
        "{caller}"
    );
    assert_eq!(caller["selection_range"]["start"]["line"], 2, "{caller}");
    assert_eq!(
        caller["from_ranges"],
        json!([{ "start": { "line": 3, "character": 4 }, "end": { "line": 3, "character": 8 } }]),
        "the call site inside `caller`"
    );

    let outgoing = calls("outgoing_calls", 2);
    assert_eq!(outgoing.len(), 1, "{outgoing:?}");
    assert_eq!(outgoing[0]["name"], "leaf", "{}", outgoing[0]);
    assert_eq!(outgoing[0]["selection_range"]["start"]["line"], 0);

    let nothing = headless
        .call(json!({ "cmd": "incoming_calls", "path": "src/main.rs", "line": 1, "character": 0 }));
    assert_eq!(nothing["result"], json!([]), "{nothing}");

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).ok();
}

#[test]
fn headless_mode_lists_rust_analyzer_symbols() {
    if Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_err()
    {
        println!("skipped: rust-analyzer is not on PATH");
        return;
    }
    let root =
        std::env::temp_dir().join(format!("next_code_headless_symbols_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"symbols\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    fs::write(
        root.join("src").join("main.rs"),
        "struct Point {\n    x: i32,\n}\n\n\
         impl Point {\n    fn new(x: i32) -> Self {\n        Point { x }\n    }\n}\n\n\
         fn farthest_distance(point: Point) -> i32 {\n    point.x\n}\n\n\
         fn main() {\n    println!(\"{}\", farthest_distance(Point::new(1)));\n}\n",
    )
    .unwrap();
    let root = root.canonicalize().unwrap();

    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    let open =
        headless.call(json!({ "cmd": "open", "path": "src/main.rs", "timeout_ms": 120_000 }));
    assert_eq!(open["ok"], true, "{open}");

    let reply = headless.call(json!({ "cmd": "document_symbols", "path": "src/main.rs" }));
    assert_eq!(reply["ok"], true, "{reply}");
    let symbols = reply["result"].as_array().unwrap().clone();
    println!("document_symbols: {}", json!(symbols));
    let find = |name: &str| {
        symbols
            .iter()
            .position(|symbol| symbol["name"] == name)
            .unwrap_or_else(|| panic!("no symbol {name}: {symbols:?}"))
    };
    let point = &symbols[find("Point")];
    assert_eq!(point["kind"], 23, "a struct: {point}");
    assert_eq!(point["kind_name"], "struct");
    assert_eq!(point["depth"], 0);
    assert_eq!(point["selection_range"]["start"]["line"], 0);
    let x = &symbols[find("x")];
    assert_eq!(
        (&x["depth"], &x["parent"]),
        (&json!(1), &json!(find("Point")))
    );
    // The method sits one level down, under its impl block.
    let new = &symbols[find("new")];
    assert_eq!(new["depth"], 1, "{new}");
    let parent = &symbols[new["parent"].as_u64().unwrap() as usize];
    assert_eq!(parent["depth"], 0, "{parent}");
    assert!(
        parent["name"].as_str().unwrap().contains("Point"),
        "{parent}"
    );
    assert_eq!(new["selection_range"]["start"]["line"], 5, "{new}");
    // Pre-order by position: the struct, its field, the impl, its method.
    assert!(find("Point") < find("x") && find("x") < find("new"));
    assert!(find("new") < find("farthest_distance") && find("farthest_distance") < find("main"));

    // Until rust-analyzer has indexed the crate, workspace search may be
    // empty.
    let deadline = Instant::now() + Duration::from_secs(300);
    let found = loop {
        let reply = headless.call(json!({ "cmd": "workspace_symbols", "query": "farthest_dist" }));
        assert_eq!(reply["ok"], true, "{reply}");
        let found = reply["result"].as_array().unwrap().clone();
        if !found.is_empty() || Instant::now() >= deadline {
            break found;
        }
        thread::sleep(Duration::from_secs(1));
    };
    let function = found
        .iter()
        .find(|symbol| symbol["name"] == "farthest_distance")
        .unwrap_or_else(|| panic!("farthest_distance not found: {found:?}"));
    assert_eq!(function["kind"], 12, "a function: {function}");
    assert!(
        function["path"].as_str().unwrap().ends_with("main.rs"),
        "{function}"
    );
    assert_eq!(function["range"]["start"]["line"], 10, "{function}");

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).ok();
}

#[test]
fn headless_mode_shows_rust_analyzer_signatures_hints_and_implementations() {
    if Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_err()
    {
        println!("skipped: rust-analyzer is not on PATH");
        return;
    }
    let root = std::env::temp_dir().join(format!("next_code_headless_nav_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"nav\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    let source = "trait Shape {\n    fn area(&self) -> f64;\n}\n\n\
                  struct Square(f64);\n\n\
                  impl Shape for Square {\n    fn area(&self) -> f64 {\n        self.0 * self.0\n    }\n}\n\n\
                  fn scale(shape: &dyn Shape, factor: f64) -> f64 {\n    shape.area() * factor\n}\n\n\
                  fn main() {\n    let square = Square(2.0);\n    let total = scale(&square, 3.0);\n    println!(\"{total}\");\n}\n";
    fs::write(root.join("src").join("main.rs"), source).unwrap();
    let root = root.canonicalize().unwrap();

    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    let open =
        headless.call(json!({ "cmd": "open", "path": "src/main.rs", "timeout_ms": 120_000 }));
    assert_eq!(open["ok"], true, "{open}");

    // Until rust-analyzer has loaded the crate, answers may be empty.
    let mut until = |request: Value, done: &dyn Fn(&Value) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let reply = headless.call(request.clone());
            assert_eq!(reply["ok"], true, "{reply}");
            if done(&reply["result"]) || Instant::now() >= deadline {
                break reply["result"].clone();
            }
            thread::sleep(Duration::from_secs(1));
        }
    };
    let file = |cmd: &str, line: u64, character: u64| json!({ "cmd": cmd, "path": "src/main.rs", "line": line, "character": character });

    // Type hints after `let` names, parameter names before arguments.
    let hints = until(
        json!({ "cmd": "inlay_hints", "path": "src/main.rs", "start_line": 16 }),
        &|hints| hints.as_array().is_some_and(|hints| hints.len() >= 3),
    );
    println!("inlay_hints: {hints}");
    let hint = |line: u64, label: &str| {
        hints
            .as_array()
            .unwrap()
            .iter()
            .find(|hint| hint["line"] == line && hint["label"].as_str().unwrap().contains(label))
            .cloned()
            .unwrap_or_else(|| panic!("no `{label}` hint on line {line}: {hints}"))
    };
    let square = hint(17, "Square");
    assert_eq!(square["kind"], "type", "{square}");
    assert_eq!(square["character"], 14, "after `square`: {square}");
    let factor = hint(18, "factor");
    assert_eq!(factor["kind"], "parameter", "{factor}");
    assert_eq!(factor["character"], 31, "before `3.0`: {factor}");

    // Inside `scale(&square, 3.0)`, at the second argument.
    let help = until(file("signature_help", 18, 31), &|help| !help.is_null());
    println!("signature_help: {help}");
    let signature = &help["signatures"][0];
    let label = signature["label"].as_str().unwrap();
    assert!(label.contains("fn scale("), "{help}");
    let active = &signature["active"];
    let (start, end) = (
        active[0].as_u64().unwrap() as usize,
        active[1].as_u64().unwrap() as usize,
    );
    assert_eq!(&label[start..end], "factor: f64", "{help}");
    assert_eq!(
        signature["parameters"].as_array().unwrap().len(),
        2,
        "{help}"
    );
    let outside = until(file("signature_help", 16, 0), &|_| true);
    assert_eq!(outside, Value::Null, "outside a call");

    let line_of = |locations: &Value| locations[0]["range"]["start"]["line"].clone();
    let found = |locations: &Value| locations.as_array().is_some_and(|l| !l.is_empty());
    // The trait method's implementation, the `square` variable's type.
    let implementations = until(file("implementation", 1, 7), &found);
    assert_eq!(line_of(&implementations), 7, "{implementations}");
    let type_definition = until(file("type_definition", 17, 8), &found);
    assert_eq!(line_of(&type_definition), 4, "{type_definition}");
    let declaration = until(file("declaration", 13, 11), &found);
    assert!(
        [json!(1), json!(7)].contains(&line_of(&declaration)),
        "{declaration}"
    );

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).ok();
}

#[test]
fn headless_mode_renames_fixes_and_formats_with_rust_analyzer() {
    if Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_err()
    {
        println!("skipped: rust-analyzer is not on PATH");
        return;
    }
    let root = std::env::temp_dir().join(format!("next_code_headless_edit_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"edits\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    let main = "mod util;\n\nfn main() {\n    util::helper();\n    println!(\"{}\", name(Color::Red));\n}\n\n\
                enum Color {\n    Red,\n    Green,\n}\n\n\
                fn name(color: Color) -> &'static str {\n    match color {\n    }\n}\n";
    fs::write(root.join("src").join("main.rs"), main).unwrap();
    fs::write(root.join("src").join("util.rs"), "pub fn helper() {}\n").unwrap();
    let root = root.canonicalize().unwrap();
    let read = |name: &str| fs::read_to_string(root.join("src").join(name)).unwrap();

    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    let open =
        headless.call(json!({ "cmd": "open", "path": "src/main.rs", "timeout_ms": 120_000 }));
    assert_eq!(open["ok"], true, "{open}");
    let at = |cmd: &str, line: u64, character: u64| json!({ "cmd": cmd, "path": "src/main.rs", "line": line, "character": character });
    // Until rust-analyzer has loaded the crate, `helper` leads nowhere.
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let reply = headless.call(at("definition", 3, 10));
        let found = reply["result"].as_array().is_some_and(|l| !l.is_empty());
        if found || Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_secs(1));
    }

    // Rename from the call site: the open file and the unopened module.
    let mut rename = at("rename", 3, 10);
    rename["new_name"] = "assist".into();
    let renamed = headless.call(rename);
    assert_eq!(renamed["ok"], true, "{renamed}");
    assert_eq!(renamed["result"]["applied"], true, "{renamed}");
    assert_eq!(
        renamed["result"]["changed"].as_array().map(Vec::len),
        Some(2),
        "{renamed}"
    );
    assert_eq!(read("util.rs"), "pub fn assist() {}\n");
    assert!(
        read("main.rs").contains("    util::assist();\n"),
        "{}",
        read("main.rs")
    );

    // A quick fix on the empty `match` ("Add 2 missing match arms" from the
    // diagnostic, or the "Fill match arms" assist).
    let is_fill = |action: &Value| {
        action["title"]
            .as_str()
            .is_some_and(|title| title.contains("match arms"))
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    let actions = loop {
        let reply = headless.call(at("code_actions", 13, 6));
        assert_eq!(reply["ok"], true, "{reply}");
        let actions = reply["result"].as_array().unwrap().clone();
        if actions.iter().any(is_fill) || Instant::now() >= deadline {
            break actions;
        }
        thread::sleep(Duration::from_secs(1));
    };
    println!("code_actions: {}", json!(actions));
    let fill = actions
        .iter()
        .find(|a| is_fill(a))
        .expect("a match-arms fix");
    assert_eq!(fill["kind"], "quickfix", "{fill}");
    let applied = headless.call(json!({ "cmd": "apply_code_action", "index": fill["index"] }));
    assert_eq!(applied["result"]["applied"], true, "{applied}");
    let fixed = read("main.rs");
    assert!(
        fixed.contains("Color::Red =>") && fixed.contains("Color::Green =>"),
        "{fixed}"
    );

    // Format Document through rust-analyzer (rustfmt) fixes indentation.
    if Command::new("rustfmt").arg("--version").output().is_ok() {
        let open = headless.call(json!({ "cmd": "open", "path": "src/util.rs" }));
        assert_eq!(open["ok"], true, "{open}");
        let messy = "pub fn assist() {\nlet x = 1;\n      println!(\"{}\", x);\n}\n";
        let write = headless.call(json!({ "cmd": "write", "path": "src/util.rs", "text": messy }));
        assert_eq!(write["ok"], true, "{write}");
        let formatted = headless.call(json!({ "cmd": "format", "path": "src/util.rs" }));
        assert_eq!(formatted["ok"], true, "{formatted}");
        let expected = "pub fn assist() {\n    let x = 1;\n    println!(\"{}\", x);\n}\n";
        assert_eq!(formatted["result"]["text"], expected, "{formatted}");
        assert_eq!(formatted["result"]["changed"], true);
        assert_eq!(read("util.rs"), expected);
    } else {
        println!("format skipped: rustfmt is not on PATH");
    }

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).ok();
}

/// `{id: status}` of a `tests_run` reply.
fn statuses(reply: &Value) -> std::collections::BTreeMap<String, String> {
    reply["result"]["results"]
        .as_array()
        .unwrap_or_else(|| panic!("{reply}"))
        .iter()
        .map(|row| {
            (
                row["id"].as_str().unwrap().to_owned(),
                row["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn result_row<'a>(reply: &'a Value, id: &str) -> &'a Value {
    reply["result"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == id)
        .unwrap_or_else(|| panic!("no {id} in {reply}"))
}

#[test]
fn headless_mode_discovers_runs_and_configures_pytest_tests() {
    if Command::new("uv").arg("--version").output().is_err() {
        println!("skipped: uv is not on PATH");
        return;
    }
    let root =
        std::env::temp_dir().join(format!("next_code_headless_pytest_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(root.join("tests").join("sub")).unwrap();
    fs::create_dir_all(root.join(".vscode")).unwrap();
    fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"demo\"\nversion = \"0.1.0\"\nrequires-python = \">=3.9\"\n\
         dependencies = []\n\n[dependency-groups]\ndev = [\"pytest\"]\n\n[tool.uv]\npackage = false\n",
    )
    .unwrap();
    fs::write(
        root.join("tests").join("test_a.py"),
        "import pytest\n\n\
         def test_pass():\n    assert 1 + 1 == 2\n\n\
         def test_fail():\n    assert 1 + 1 == 3, \"math is broken\"\n\n\
         @pytest.mark.parametrize(\"x,y\", [(1, 2), (3, 7)])\n\
         def test_double(x, y):\n    assert x * 2 == y\n\n\
         class TestGroup:\n    def test_inside(self):\n        print(\"inside out\")\n\n\
         \x20   @pytest.mark.skip(reason=\"not today\")\n    def test_skipped(self):\n        pass\n",
    )
    .unwrap();
    fs::write(
        root.join("tests").join("sub").join("test_b.py"),
        "def test_b():\n    pass\n",
    )
    .unwrap();
    fs::write(
        root.join(".vscode").join("launch.json"),
        "{\n  // VS Code's file\n  \"configurations\": [\n    { \"name\": \"Main\", \"type\": \"debugpy\", \
         \"request\": \"launch\", \"program\": \"${workspaceFolder}/main.py\", \"justMyCode\": false },\n    \
         { \"name\": \"Web\", \"type\": \"node\", \"request\": \"launch\" },\n  ],\n}\n",
    )
    .unwrap();

    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");

    let discovered = headless.call(json!({ "cmd": "tests_discover" }));
    let nodes = discovered["result"]["nodes"]
        .as_array()
        .unwrap_or_else(|| panic!("{discovered}"));
    let find = |id: &str| {
        nodes
            .iter()
            .find(|node| node["id"] == id)
            .unwrap_or_else(|| panic!("no {id} in {discovered}"))
    };
    assert_eq!(find("tests/test_a.py::test_fail")["line"], 5);
    assert_eq!(find("tests/test_a.py::test_double")["kind"], "function");
    assert_eq!(find("tests/test_a.py::test_double[3-7]")["kind"], "test");
    assert_eq!(find("tests/test_a.py::TestGroup")["kind"], "class");
    assert_eq!(find("tests/test_a.py::TestGroup::test_inside")["line"], 13);
    assert_eq!(find("tests/sub")["kind"], "folder");

    let all = headless.call(json!({ "cmd": "tests_run" }));
    assert_eq!(
        statuses(&all),
        [
            ("tests/sub/test_b.py::test_b", "passed"),
            ("tests/test_a.py::TestGroup::test_inside", "passed"),
            ("tests/test_a.py::TestGroup::test_skipped", "skipped"),
            ("tests/test_a.py::test_double[1-2]", "passed"),
            ("tests/test_a.py::test_double[3-7]", "failed"),
            ("tests/test_a.py::test_fail", "failed"),
            ("tests/test_a.py::test_pass", "passed"),
        ]
        .map(|(id, status)| (id.to_owned(), status.to_owned()))
        .into(),
        "{all}"
    );
    let fail = result_row(&all, "tests/test_a.py::test_fail");
    assert!(
        fail["message"].as_str().unwrap().contains("math is broken"),
        "{fail}"
    );
    assert!(
        fail["output"]
            .as_str()
            .unwrap()
            .contains("assert 1 + 1 == 3"),
        "{fail}"
    );
    let inside = result_row(&all, "tests/test_a.py::TestGroup::test_inside");
    assert!(
        inside["output"].as_str().unwrap().contains("inside out"),
        "{inside}"
    );
    assert_eq!(
        result_row(&all, "tests/test_a.py::TestGroup::test_skipped")["message"],
        "not today"
    );
    assert_eq!(all["result"]["roots"][0]["summary"]["failed"], 2, "{all}");

    let class = headless.call(json!({ "cmd": "tests_run", "ids": ["tests/test_a.py::TestGroup"] }));
    assert_eq!(statuses(&class).len(), 2, "{class}");

    let configs = headless.call(json!({ "cmd": "run_configurations", "file": "tests/test_a.py" }));
    let configs = configs["result"]["configurations"].as_array().unwrap();
    let config = |name: &str| {
        configs
            .iter()
            .find(|config| config["name"] == name)
            .unwrap()
    };
    assert_eq!(config("Main")["source"], "launch_json");
    assert!(
        config("Web")["disabled"]
            .as_str()
            .unwrap()
            .contains("`node`")
    );
    assert_eq!(config("pytest: All Tests")["command"], "uv run pytest");
    let current = config("Python: Current File")["command"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        current.starts_with("uv run python ") && current.contains("test_a.py"),
        "{current}"
    );

    let debug = headless.call(json!({ "cmd": "debug_request", "name": "Main" }));
    let debug = &debug["result"];
    assert_eq!(debug["adapter"]["type"], "python", "{debug}");
    assert_eq!(debug["configuration"]["justMyCode"], false);
    assert!(
        debug["configuration"]["program"]
            .as_str()
            .unwrap()
            .ends_with("main.py")
    );
    let pytest = headless.call(json!({ "cmd": "debug_request", "name": "pytest: All Tests" }));
    assert_eq!(
        pytest["result"]["configuration"]["module"], "pytest",
        "{pytest}"
    );

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).ok();
}

#[test]
fn headless_mode_discovers_and_runs_cargo_tests() {
    let root =
        std::env::temp_dir().join(format!("next_code_headless_cargo_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(root.join("src").join("bin")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    fs::write(
        root.join("src").join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n\
         #[cfg(test)]\nmod tests {\n    use super::*;\n\n\
         \x20   #[test]\n    fn adds() {\n        assert_eq!(add(1, 2), 3);\n    }\n\n\
         \x20   #[test]\n    fn fails() {\n        assert_eq!(add(1, 2), 4, \"bad math\");\n    }\n\n\
         \x20   #[test]\n    #[ignore = \"slow\"]\n    fn ignored() {}\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("src").join("bin").join("tool.rs"),
        "fn main() {\n    println!(\"tool\");\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("tests").join("it.rs"),
        "#[test]\nfn integration() {\n    assert_eq!(demo::add(2, 2), 4);\n}\n",
    )
    .unwrap();
    // Its own target dir: the outer `cargo test` holds the workspace's.
    let target = root.join("target");
    let mut headless = Headless::start_with(&root, &[("CARGO_TARGET_DIR", &target)]);
    headless.next(|line| line["event"] == "ready");

    let discovered = headless.call(json!({ "cmd": "tests_discover" }));
    let nodes = discovered["result"]["nodes"]
        .as_array()
        .unwrap_or_else(|| panic!("{discovered}"));
    let adds = nodes
        .iter()
        .find(|node| node["id"] == "demo/lib::tests::adds")
        .unwrap_or_else(|| panic!("{discovered}"));
    assert_eq!(adds["line"], 9, "{adds}");
    assert!(adds["path"].as_str().unwrap().ends_with("lib.rs"));
    assert!(
        nodes
            .iter()
            .any(|node| node["id"] == "demo/test:it::integration")
    );

    let all = headless.call(json!({ "cmd": "tests_run" }));
    assert_eq!(
        statuses(&all),
        [
            ("demo/lib::tests::adds", "passed"),
            ("demo/lib::tests::fails", "failed"),
            ("demo/lib::tests::ignored", "skipped"),
            ("demo/test:it::integration", "passed"),
        ]
        .map(|(id, status)| (id.to_owned(), status.to_owned()))
        .into(),
        "{all}"
    );
    let fails = result_row(&all, "demo/lib::tests::fails");
    assert!(
        fails["message"].as_str().unwrap().contains("bad math"),
        "{fails}"
    );
    let one = headless.call(json!({ "cmd": "tests_run", "ids": ["demo/lib::tests::adds"] }));
    assert_eq!(statuses(&one).len(), 1, "{one}");

    let debug = headless.call(json!({ "cmd": "debug_request", "name": "cargo run: tool" }));
    let debug = &debug["result"];
    assert_eq!(debug["adapter"]["type"], "lldb", "{debug}");
    let program = debug["configuration"]["program"].as_str().unwrap();
    assert!(
        program.ends_with("tool") || program.ends_with("tool.exe"),
        "{program}"
    );

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).ok();
}

/// The next `debug_event` of `kind` (other events stay in the backlog).
fn debug_event(headless: &mut Headless, kind: &str) -> Value {
    headless.next(|line| line["event"] == "debug_event" && line["type"] == kind)
}

#[test]
fn headless_mode_debugs_python_with_debugpy() {
    if Command::new("uv").arg("--version").output().is_err() {
        println!("skipped: uv is not on PATH (debugpy comes through `uv run --with debugpy`)");
        return;
    }
    let root =
        std::env::temp_dir().join(format!("next_code_headless_dap_py_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(&root).unwrap();
    let main = root.join("main.py");
    fs::write(
        &main,
        "def compute(n):\n    total = 0\n    for i in range(n):\n        total += i\n    return total\n\n\
         result = compute(4)\nprint(\"result\", result)\n",
    )
    .unwrap();

    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    let set = headless.call(json!({
        "cmd": "debug_set_breakpoints", "path": "main.py", "breakpoints": [{ "line": 3 }]
    }));
    assert_eq!(set["ok"], true, "{set}");
    let started = headless.call(json!({ "cmd": "debug_start", "request": {
        "name": "main.py",
        "adapter": { "type": "python" },
        "request": "launch",
        "configuration": { "program": main.to_string_lossy() }
    } }));
    assert_eq!(started["ok"], true, "{started}");

    // Stops in the loop with the frame on line 3 (0-based).
    let stopped = debug_event(&mut headless, "stopped");
    assert_eq!(stopped["frame"]["line"], 3, "{stopped}");
    assert!(
        stopped["frame"]["path"]
            .as_str()
            .unwrap()
            .ends_with("main.py")
    );
    let verified = debug_event(&mut headless, "breakpoints");
    assert_eq!(verified["breakpoints"][0]["verified"], true, "{verified}");
    let stack = headless.call(json!({ "cmd": "debug_stack" }));
    let frames = stack["result"]["frames"].as_array().unwrap();
    assert_eq!(frames[0]["name"], "compute", "{stack}");
    assert_eq!(stack["result"]["state"], "paused");

    let locals = headless.call(json!({ "cmd": "debug_variables" }));
    let variables = &locals["result"]["variables"];
    let value = |name: &str| {
        variables
            .as_array()
            .unwrap()
            .iter()
            .find(|var| var["name"] == name)
            .map(|var| var["value"].clone())
    };
    assert_eq!(value("i"), Some(json!("0")), "{locals}");
    assert_eq!(value("total"), Some(json!("0")), "{locals}");
    let evaluated = headless.call(json!({ "cmd": "debug_evaluate", "expression": "n * 100 + i" }));
    assert_eq!(evaluated["result"]["result"], "400", "{evaluated}");

    // Step over the loop body: back at the `for` (line 2).
    let step = headless.call(json!({ "cmd": "debug_step", "kind": "over" }));
    assert_eq!(step["ok"], true, "{step}");
    let stopped = debug_event(&mut headless, "stopped");
    assert_eq!(stopped["frame"]["line"], 2, "{stopped}");

    // Without breakpoints the program runs to its end.
    headless.call(json!({ "cmd": "debug_set_breakpoints", "path": "main.py", "breakpoints": [] }));
    let resumed = headless.call(json!({ "cmd": "debug_continue" }));
    assert_eq!(resumed["ok"], true, "{resumed}");
    let ended = debug_event(&mut headless, "terminated");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let output = headless.call(json!({ "cmd": "debug_output" }));
    assert!(
        output["result"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line["category"] == "stdout" && line["text"] == "result 6"),
        "{output}"
    );

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).ok();
}

/// An LLDB adapter: `NEXT_CODE_TEST_LLDB` (the adapter's path, put first on
/// `PATH`), else `codelldb` or `lldb-dap` on `PATH`.
fn lldb_adapter() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NEXT_CODE_TEST_LLDB") {
        return Some(PathBuf::from(path));
    }
    let names: &[&str] = if cfg!(windows) {
        &["codelldb.exe", "lldb-dap.exe"]
    } else {
        &["codelldb", "lldb-dap"]
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|path| path.is_file())
}

#[test]
fn headless_mode_debugs_a_cargo_binary_with_lldb() {
    let Some(adapter) = lldb_adapter() else {
        println!("skipped: no codelldb/lldb-dap on PATH and NEXT_CODE_TEST_LLDB is not set");
        return;
    };
    let root =
        std::env::temp_dir().join(format!("next_code_headless_dap_rs_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    fs::write(
        root.join("src").join("main.rs"),
        "fn main() {\n    let numbers = vec![1, 2, 3];\n    let total: i32 = numbers.iter().sum();\n    \
         println!(\"total {total}\");\n}\n",
    )
    .unwrap();
    let built = Command::new(env!("CARGO"))
        .args(["build", "--quiet"])
        .current_dir(&root)
        .env_remove("CARGO_TARGET_DIR")
        .status()
        .unwrap();
    assert!(built.success());
    let program =
        root.join("target")
            .join("debug")
            .join(if cfg!(windows) { "demo.exe" } else { "demo" });

    // The adapter's directory first on PATH, so it is the one found.
    let mut path = vec![adapter.parent().unwrap().to_path_buf()];
    path.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(path).unwrap();
    let mut headless = Headless::start_with(&root, &[("PATH", std::path::Path::new(&path))]);
    headless.next(|line| line["event"] == "ready");
    headless.call(json!({
        "cmd": "debug_set_breakpoints", "path": "src/main.rs", "breakpoints": [{ "line": 3 }]
    }));
    let started = headless.call(json!({ "cmd": "debug_start", "request": {
        "name": "demo",
        "adapter": { "type": "lldb" },
        "request": "launch",
        "configuration": { "program": program.to_string_lossy() }
    } }));
    assert_eq!(started["ok"], true, "{started}");
    let stopped = debug_event(&mut headless, "stopped");
    assert_eq!(stopped["frame"]["line"], 3, "{stopped}");
    let locals = headless.call(json!({ "cmd": "debug_variables" }));
    let total = locals["result"]["variables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|var| var["name"] == "total")
        .map(|var| var["value"].clone());
    assert_eq!(total, Some(json!("6")), "{locals}");
    let stopping = headless.call(json!({ "cmd": "debug_stop" }));
    assert_eq!(stopping["ok"], true, "{stopping}");

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).ok();
}
