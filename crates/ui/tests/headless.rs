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

/// Test tools are background processes too; keep them off the Windows desktop.
fn background_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    remote::launch::no_window(&mut command);
    command
}

/// Removes a temp project a language server worked in. On Windows the
/// server's own children (rust-analyzer's `cargo check`) can hold files in
/// it for a moment after the editor exits.
fn remove_project_dir(root: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while let Err(err) = fs::remove_dir_all(root) {
        if Instant::now() >= deadline {
            panic!("cannot remove {}: {err}", root.display());
        }
        thread::sleep(Duration::from_millis(250));
    }
}

/// `canonicalize`, spelled as the editor spells a project root: without
/// Windows' `\\?\` prefix.
fn canonical(path: &std::path::Path) -> PathBuf {
    let path = path.canonicalize().unwrap();
    match path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        Some(plain) if cfg!(windows) => PathBuf::from(plain),
        _ => path,
    }
}

struct Headless {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<Value>,
    /// Lines read while waiting for something else, in arrival order.
    backlog: Vec<Value>,
    next_id: u64,
}

impl Headless {
    fn start(root: &std::path::Path) -> Self {
        Self::start_with(root, &[])
    }

    fn start_with(root: &std::path::Path, env: &[(&str, &std::path::Path)]) -> Self {
        Self::launch(&[root.as_os_str()], env)
    }

    /// `next_code --headless <args>`.
    fn launch(args: &[&std::ffi::OsStr], env: &[(&str, &std::path::Path)]) -> Self {
        let mut child = background_command(env!("CARGO_BIN_EXE_next_code_editor"))
            .arg("--headless")
            .args(args)
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

/// The Files tree's operations over headless: create (with parents), copy
/// names, rename/move refusals, delete.
#[test]
fn headless_mode_creates_renames_copies_and_deletes_files() {
    let root = std::env::temp_dir().join(format!("next_code_headless_ops_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(&root).unwrap();
    let root = canonical(&root);
    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    let mut ok = |request: Value| {
        let reply = headless.call(request.clone());
        assert_eq!(reply["ok"], true, "{request} → {reply}");
        reply["result"].clone()
    };

    let created = ok(json!({ "cmd": "create_file", "path": "pkg/sub/mod.py" }));
    let module = root.join("pkg").join("sub").join("mod.py");
    assert_eq!(created["path"], module.to_string_lossy().as_ref());
    assert!(module.is_file());
    ok(json!({ "cmd": "create_dir", "path": "empty" }));
    assert!(root.join("empty").is_dir());
    fs::write(&module, "x = 1\n").unwrap();

    // New File's types: extensions, fixed names, template text, roots.
    let notebook = ok(json!({ "cmd": "create_file", "path": "pkg/analysis", "type": "jupyter" }));
    let notebook_path = root.join("pkg").join("analysis.ipynb");
    assert_eq!(notebook["path"], notebook_path.to_string_lossy().as_ref());
    let notebook_text = fs::read_to_string(&notebook_path).unwrap();
    assert!(
        notebook_text.contains("\"cell_type\": \"code\""),
        "{notebook_text}"
    );
    let docker = ok(json!({ "cmd": "create_file", "path": "pkg", "type": "dockerfile" }));
    assert_eq!(
        docker["path"],
        root.join("pkg")
            .join("Dockerfile")
            .to_string_lossy()
            .as_ref()
    );
    let workflow = ok(json!({ "cmd": "create_file", "path": "pkg/deploy", "type": "workflow" }));
    let workflow_path = root
        .join(".next_code")
        .join("workflows")
        .join("deploy.yaml");
    assert_eq!(workflow["path"], workflow_path.to_string_lossy().as_ref());
    assert!(
        fs::read_to_string(&workflow_path)
            .unwrap()
            .starts_with("# yaml-language-server")
    );
    let ignore = ok(json!({ "cmd": "create_file", "type": "gitignore" }));
    assert_eq!(
        ignore["path"],
        root.join(".gitignore").to_string_lossy().as_ref()
    );

    // Replies spell paths natively (`\` on Windows).
    let native = |relative: &str| {
        root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR))
            .to_string_lossy()
            .into_owned()
    };
    // Copies keep the name where it is free, else ` copy`, ` copy 2`.
    let copy = ok(json!({ "cmd": "copy_path", "path": "pkg/sub/mod.py" }));
    assert_eq!(copy["path"], native("pkg/sub/mod copy.py"));
    let copy = ok(json!({ "cmd": "copy_path", "path": "pkg/sub/mod.py" }));
    assert_eq!(copy["path"], native("pkg/sub/mod copy 2.py"));
    let copy = ok(json!({ "cmd": "copy_path", "path": "pkg", "dir": "empty" }));
    assert_eq!(copy["path"], native("empty/pkg"));
    assert_eq!(
        fs::read_to_string(root.join("empty/pkg/sub/mod.py")).unwrap(),
        "x = 1\n"
    );

    // Rename and move; taken targets and moves into itself are refused.
    let renamed = ok(
        json!({ "cmd": "rename_path", "path": "pkg/sub/mod.py", "new_path": "pkg/sub/main.py" }),
    );
    assert_eq!(renamed["path"], native("pkg/sub/main.py"));
    assert_eq!(renamed["edits"], json!([]));
    assert!(!module.exists());
    let mut fails = |request: Value| {
        let reply = headless.call(request.clone());
        assert_eq!(reply["ok"], false, "{request} → {reply}");
        reply["error"].as_str().unwrap().to_owned()
    };
    let taken = fails(
        json!({ "cmd": "rename_path", "path": "pkg/sub/main.py", "new_path": "pkg/sub/mod copy.py" }),
    );
    assert!(taken.contains("`mod copy.py` already exists"), "{taken}");
    let into_itself =
        fails(json!({ "cmd": "rename_path", "path": "pkg", "new_path": "pkg/sub/pkg" }));
    assert_eq!(into_itself, "Cannot move `pkg` into itself");
    let exists = fails(json!({ "cmd": "create_file", "path": "pkg/sub/main.py" }));
    assert!(exists.contains("already exists"), "{exists}");
    let unknown = fails(json!({ "cmd": "create_file", "path": "x", "type": "cobol" }));
    assert!(unknown.contains("Unknown file type `cobol`"), "{unknown}");
    let empty = fails(json!({ "cmd": "create_dir", "path": "a/.." }));
    assert!(!empty.is_empty());
    let moved =
        headless.call(json!({ "cmd": "rename_path", "path": "pkg", "new_path": "empty/moved" }));
    assert_eq!(moved["ok"], true, "{moved}");
    assert!(root.join("empty/moved/sub/main.py").is_file());

    // Delete files and folders (with contents); missing paths fail.
    let deleted = headless
        .call(json!({ "cmd": "delete_path", "paths": ["empty/moved/sub/main.py", "empty"] }));
    assert_eq!(deleted["ok"], true, "{deleted}");
    assert!(!root.join("empty").exists());
    let missing = headless.call(json!({ "cmd": "delete_path", "path": "empty" }));
    assert_eq!(missing["ok"], false, "{missing}");

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).unwrap();
}

/// Renaming a module asks rust-analyzer (`workspace/willRenameFiles`) for
/// its edits: `mod util;` and `util::` follow the new name.
#[test]
fn headless_mode_renames_a_rust_module_with_the_servers_edits() {
    if background_command("rust-analyzer")
        .arg("--version")
        .output()
        .is_err()
    {
        println!("skipped: rust-analyzer is not on PATH");
        return;
    }
    let root = std::env::temp_dir().join(format!("next_code_headless_mv_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"moves\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    fs::write(
        root.join("src").join("main.rs"),
        "mod util;\n\nfn main() {\n    util::helper();\n}\n",
    )
    .unwrap();
    fs::write(root.join("src").join("util.rs"), "pub fn helper() {}\n").unwrap();
    let root = root.canonicalize().unwrap();

    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    let open =
        headless.call(json!({ "cmd": "open", "path": "src/main.rs", "timeout_ms": 120_000 }));
    assert_eq!(open["ok"], true, "{open}");
    // Until rust-analyzer has loaded the crate, it has no module to rename.
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let reply = headless.call(
            json!({ "cmd": "definition", "path": "src/main.rs", "line": 3, "character": 10 }),
        );
        if reply["result"].as_array().is_some_and(|l| !l.is_empty()) || Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
    let renamed = headless
        .call(json!({ "cmd": "rename_path", "path": "src/util.rs", "new_path": "src/tools.rs" }));
    assert_eq!(renamed["ok"], true, "{renamed}");
    let edits = renamed["result"]["edits"].as_array().unwrap();
    assert_eq!(edits.len(), 1, "{renamed}");
    assert_eq!(edits[0]["applied"], true, "{renamed}");
    assert_eq!(
        fs::read_to_string(root.join("src").join("main.rs")).unwrap(),
        "mod tools;\n\nfn main() {\n    tools::helper();\n}\n"
    );
    assert!(root.join("src").join("tools.rs").is_file());
    assert!(!root.join("src").join("util.rs").exists());

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    remove_project_dir(&root);
}

#[test]
fn headless_mode_replaces_in_files() {
    let root = std::env::temp_dir().join(format!("next_code_headless_rp_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("other")).unwrap();
    fs::write(root.join("src/a.py"), "name = 'old'\r\nold_value = old\r\n").unwrap();
    fs::write(root.join("src/b.py"), "OLD old é old\n").unwrap();
    fs::write(root.join("other/c.py"), "old\n").unwrap();
    let root = root.canonicalize().unwrap();
    let read = |rel: &str| fs::read_to_string(root.join(rel)).unwrap();

    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");

    // Whole words, case-sensitive, only under `src`.
    let reply = headless.call(json!({
        "cmd": "replace", "pattern": "old", "replacement": "new",
        "whole_word": true, "case_sensitive": true, "paths": ["src"],
    }));
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["result"]["replacements"], 4, "{reply}");
    let mut counts: Vec<(String, u64)> = reply["result"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| {
            let path = PathBuf::from(file["path"].as_str().unwrap());
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            (name, file["replacements"].as_u64().unwrap())
        })
        .collect();
    counts.sort();
    assert_eq!(counts, [("a.py".into(), 2), ("b.py".into(), 2)]);
    assert_eq!(read("src/a.py"), "name = 'new'\r\nold_value = new\r\n");
    assert_eq!(read("src/b.py"), "OLD new é new\n");
    assert_eq!(read("other/c.py"), "old\n");

    // Regex with numbered and named groups.
    let reply = headless.call(json!({
        "cmd": "replace", "pattern": r"(?P<word>\w+)_(value)", "replacement": "${word}.$2",
        "regex": true, "case_sensitive": true,
    }));
    assert_eq!(reply["result"]["replacements"], 1, "{reply}");
    assert_eq!(read("src/a.py"), "name = 'new'\r\nold.value = new\r\n");

    // Errors: no replacement, a bad pattern.
    let missing = headless.call(json!({ "cmd": "replace", "pattern": "x" }));
    assert_eq!(missing["ok"], false, "{missing}");
    let bad = headless.call(json!({
        "cmd": "replace", "pattern": "(", "replacement": "", "regex": true,
    }));
    assert_eq!(bad["ok"], false, "{bad}");
    let none = headless.call(json!({ "cmd": "replace", "pattern": "zzz", "replacement": "y" }));
    assert_eq!(none["result"]["replacements"], 0, "{none}");

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
        let status = background_command("git")
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

/// A multi-root workspace over headless: two git repositories as the
/// folders of a `.next_code-workspace` file (one relative, one absolute
/// with a name, plus settings), then a VS Code `.code-workspace` naming a
/// folder by `file:` URI.
#[test]
fn headless_mode_opens_multi_root_workspaces() {
    let base = std::env::temp_dir().join(format!("next_code_headless_ws_{}", std::process::id()));
    fs::remove_dir_all(&base).ok();
    for dir in ["api/src", "web", "ws", "config"] {
        fs::create_dir_all(base.join(dir)).unwrap();
    }
    // Windows' canonical form (`\\?\C:\…`) is not how users write folders.
    let base = if cfg!(windows) {
        base
    } else {
        base.canonicalize().unwrap()
    };
    let (api, web) = (base.join("api"), base.join("web"));
    let git = |dir: &std::path::Path, args: &[&str]| {
        let status = background_command("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    fs::write(
        api.join("src").join("main.rs"),
        "fn main() {} // needle api\n",
    )
    .unwrap();
    fs::write(api.join("notes.md"), "# notes\n").unwrap();
    fs::write(web.join("index.js"), "// needle web\n").unwrap();
    for dir in [&api, &web] {
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.name", "Test"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "commit.gpgsign", "false"]);
        git(dir, &["config", "core.autocrlf", "false"]);
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", "init"]);
    }
    fs::write(web.join("index.js"), "// needle web\nconsole.log(1);\n").unwrap();
    // The workspace's settings format Markdown with a program that prints
    // a hash of the text, which shows they apply.
    let file = base.join("ws").join("team.next_code-workspace");
    let workspace = json!({
        "folders": [
            { "path": "../api" },
            { "path": web.to_string_lossy(), "name": "frontend" },
        ],
        "settings": {
            "languages": {
                "markdown": {
                    "formatter": { "command": "git", "args": ["hash-object", "--stdin"] }
                }
            }
        },
    });
    fs::write(&file, format!("// Two repositories.\n{workspace:#}\n")).unwrap();
    let config = base.join("config");

    let mut headless = Headless::launch(
        &[std::ffi::OsStr::new("--workspace"), file.as_os_str()],
        &[("XDG_CONFIG_HOME", &config)],
    );
    let ready = headless.next(|line| line["event"].is_string());
    assert_eq!(ready["event"], "ready", "{ready}");

    let info = headless.call(json!({ "cmd": "info" }))["result"].clone();
    let roots = info["roots"].as_array().unwrap_or_else(|| panic!("{info}"));
    let names: Vec<&str> = roots
        .iter()
        .map(|root| root["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["api", "frontend"], "{info}");
    assert_eq!(roots[1]["index"], 1);
    assert_eq!(
        PathBuf::from(roots[0]["path"].as_str().unwrap()),
        api,
        "{info}"
    );
    assert_eq!(
        PathBuf::from(roots[1]["path"].as_str().unwrap()),
        web,
        "{info}"
    );
    assert_eq!(
        info["root"], roots[0]["path"],
        "`root` stays the first folder"
    );
    assert_eq!(info["workspace_file"], file.to_string_lossy().as_ref());

    // `list_files` answers in the platform's separator (`src\main.rs` on
    // Windows); `PathBuf::from("src/main.rs")` would keep the `/`.
    let native = |path: &str| {
        path.split('/')
            .collect::<PathBuf>()
            .to_string_lossy()
            .into_owned()
    };
    let files = headless.call(json!({ "cmd": "list_files" }));
    assert_eq!(
        files["result"],
        json!([
            { "root": 0, "path": "notes.md" },
            { "root": 0, "path": native("src/main.rs") },
            { "root": 1, "path": "index.js" },
        ]),
        "{files}"
    );
    let one = headless.call(json!({ "cmd": "list_files", "root": "frontend" }));
    assert_eq!(one["result"], json!(["index.js"]), "{one}");

    let read = headless.call(json!({ "cmd": "read", "root": 1, "path": "index.js" }));
    assert!(
        read["result"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("console.log")),
        "{read}"
    );
    let bad = headless.call(json!({ "cmd": "read", "root": 2, "path": "index.js" }));
    assert!(
        bad["error"].as_str().unwrap().contains("no folder 2"),
        "{bad}"
    );
    let bad = headless.call(json!({ "cmd": "read", "root": "nope", "path": "index.js" }));
    assert_eq!(bad["ok"], false, "{bad}");

    let search = headless.call(json!({ "cmd": "search", "pattern": "needle" }));
    let mut found: Vec<(u64, String)> = search["result"]["files"]
        .as_array()
        .unwrap_or_else(|| panic!("{search}"))
        .iter()
        .map(|file| {
            (
                file["root"].as_u64().unwrap(),
                file["relative"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    found.sort();
    assert_eq!(
        found,
        [(0, "src/main.rs".to_owned()), (1, "index.js".to_owned())],
        "{search}"
    );

    let status = headless.call(json!({ "cmd": "git_status", "root": 1 }));
    assert_eq!(status["result"]["branch"], "main", "{status}");
    assert_eq!(
        status["result"]["entries"],
        json!([{ "path": "index.js", "orig_path": null, "staged": null, "unstaged": "modified" }]),
        "{status}"
    );
    let first = headless.call(json!({ "cmd": "git_status" }));
    assert_eq!(
        first["result"]["entries"],
        json!([]),
        "the first folder: {first}"
    );
    let all = headless.call(json!({ "cmd": "git_status", "all": true }));
    let repositories: Vec<(u64, usize)> = all["result"]
        .as_array()
        .unwrap_or_else(|| panic!("{all}"))
        .iter()
        .map(|entry| {
            (
                entry["root"].as_u64().unwrap(),
                entry["status"]["entries"].as_array().unwrap().len(),
            )
        })
        .collect();
    assert_eq!(repositories, [(0, 0), (1, 1)], "{all}");
    // An absolute path picks its folder's repository.
    let diff = headless.call(json!({
        "cmd": "git_diff", "path": web.join("index.js").to_string_lossy(),
    }));
    assert_eq!(diff["result"]["added"], 1, "{diff}");

    let format = headless.call(json!({ "cmd": "format", "path": "notes.md" }));
    let hash = format["result"]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{format}"));
    assert!(
        hash.trim().len() == 40 && hash.trim().bytes().all(|b| b.is_ascii_hexdigit()),
        "the workspace's formatter ran: {format}"
    );

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());

    // VS Code's file, named as a plain path: a `file:` URI folder first.
    let web_text = web.to_string_lossy().replace('\\', "/").replace(' ', "%20");
    let uri = match web_text.strip_prefix('/') {
        Some(rest) => format!("file:///{rest}"),
        None => format!("file:///{web_text}"),
    };
    let legacy = base.join("ws").join("legacy.code-workspace");
    fs::write(
        &legacy,
        format!(
            "{{\n  \"folders\": [{{ \"uri\": \"{uri}\" }}, {{ \"path\": \"../api\" }},],\n}}\n"
        ),
    )
    .unwrap();
    let mut headless = Headless::launch(&[legacy.as_os_str()], &[("XDG_CONFIG_HOME", &config)]);
    let ready = headless.next(|line| line["event"].is_string());
    assert_eq!(ready["event"], "ready", "{ready}");
    let info = headless.call(json!({ "cmd": "info" }))["result"].clone();
    let names: Vec<&str> = info["roots"]
        .as_array()
        .unwrap_or_else(|| panic!("{info}"))
        .iter()
        .map(|root| root["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["web", "api"], "{info}");
    assert_eq!(PathBuf::from(info["root"].as_str().unwrap()), web, "{info}");
    let files = headless.call(json!({ "cmd": "list_files", "root": 1 }));
    assert_eq!(
        files["result"],
        json!(["notes.md", native("src/main.rs")]),
        "{files}"
    );
    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&base).unwrap();
}

/// The editor's change markers, Stage Hunk and blame over headless.
#[test]
fn headless_mode_reports_line_changes_stages_one_and_blames() {
    let root =
        std::env::temp_dir().join(format!("next_code_headless_blame_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(&root).unwrap();
    let git = |args: &[&str]| {
        let output = background_command("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "commit.gpgsign", "false"]);
    git(&["config", "core.autocrlf", "false"]);
    fs::write(root.join("f.txt"), "one\ntwo\nthree\n").unwrap();
    git(&["add", "f.txt"]);
    git(&["commit", "-q", "-m", "first\n\nthe body"]);
    let first = git(&["rev-parse", "HEAD"]).trim().to_owned();
    fs::write(root.join("f.txt"), "one\nTWO\nthree\n").unwrap();
    git(&["commit", "-q", "-am", "second"]);
    let second = git(&["rev-parse", "HEAD"]).trim().to_owned();
    fs::write(root.join("f.txt"), "zero\none\nTWO\nthree\nfour\n").unwrap();
    fs::write(root.join("free.txt"), "untracked\n").unwrap();
    fs::write(root.join("added.txt"), "a\nb\n").unwrap();
    git(&["add", "added.txt"]);
    let root = root.canonicalize().unwrap();

    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    let changes = |headless: &mut Headless, request: Value| -> Value {
        let reply = headless.call(request);
        assert_eq!(reply["ok"], true, "{reply}");
        reply["result"].clone()
    };

    // The working-tree file against HEAD: lines added at both ends.
    let result = changes(
        &mut headless,
        json!({ "cmd": "git_line_changes", "path": "f.txt" }),
    );
    assert_eq!(
        result,
        json!({ "base": "blob", "changes": [
            { "kind": "added", "new": { "start": 0, "end": 1 }, "old": { "start": 0, "end": 0 } },
            { "kind": "added", "new": { "start": 4, "end": 5 }, "old": { "start": 3, "end": 3 } },
        ] })
    );
    // Unsaved text: a deleted last line; CRLF of the same lines changes nothing.
    let result = changes(
        &mut headless,
        json!({ "cmd": "git_line_changes", "path": "f.txt", "contents": "one\nTWO" }),
    );
    assert_eq!(
        result["changes"],
        json!([{ "kind": "deleted", "new": { "start": 2, "end": 2 }, "old": { "start": 2, "end": 4 } }])
    );
    let result = changes(
        &mut headless,
        json!({ "cmd": "git_line_changes", "path": "f.txt", "contents": "one\r\nTWO\r\nthree\r\n" }),
    );
    assert_eq!(result["changes"], json!([]));
    // Untracked: no markers; staged but never committed: all added.
    let result = changes(
        &mut headless,
        json!({ "cmd": "git_line_changes", "path": "free.txt" }),
    );
    assert_eq!(result, json!({ "base": "none", "changes": [] }));
    let result = changes(
        &mut headless,
        json!({ "cmd": "git_line_changes", "path": "added.txt" }),
    );
    assert_eq!(result["base"], "new_file");
    assert_eq!(result["changes"][0]["new"], json!({ "start": 0, "end": 3 }));

    // Stage only the second change (the added last line).
    let staged = headless.call(json!({
        "cmd": "git_stage_line_change", "path": "f.txt", "index": true, "change": 1,
    }));
    assert_eq!(staged["ok"], true, "{staged}");
    assert_eq!(git(&["show", ":f.txt"]), "one\nTWO\nthree\nfour\n");
    assert_eq!(
        fs::read_to_string(root.join("f.txt")).unwrap(),
        "zero\none\nTWO\nthree\nfour\n"
    );
    let result = changes(
        &mut headless,
        json!({ "cmd": "git_line_changes", "path": "f.txt", "index": true }),
    );
    assert_eq!(
        result["changes"],
        json!([{ "kind": "added", "new": { "start": 0, "end": 1 }, "old": { "start": 0, "end": 0 } }])
    );
    let refused = headless.call(json!({
        "cmd": "git_stage_line_change", "path": "f.txt", "change": 0,
    }));
    assert_eq!(refused["ok"], false, "{refused}");

    // Blame of the working tree: uncommitted lines are null.
    let blame = changes(
        &mut headless,
        json!({ "cmd": "git_blame", "path": "f.txt" }),
    );
    let ids: Vec<Value> = blame["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| match line.as_u64() {
            Some(index) => blame["commits"][index as usize]["id"].clone(),
            None => Value::Null,
        })
        .collect();
    assert_eq!(
        ids,
        [
            Value::Null,
            json!(first),
            json!(second),
            json!(first),
            Value::Null
        ]
    );
    let oldest = blame["commits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|commit| commit["id"] == json!(first))
        .unwrap();
    assert_eq!(oldest["author"], "Test");
    assert_eq!(oldest["message"], "first\n\nthe body");
    // Unsaved text with an edited line.
    let blame = changes(
        &mut headless,
        json!({ "cmd": "git_blame", "path": "f.txt", "contents": "one\nTWO!\nthree\n" }),
    );
    assert_eq!(blame["lines"][1], Value::Null);
    assert!(
        blame["lines"][0].is_u64() && blame["lines"][2].is_u64(),
        "{blame}"
    );
    let untracked = headless.call(json!({ "cmd": "git_blame", "path": "free.txt" }));
    assert_eq!(untracked["ok"], false, "{untracked}");

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).unwrap();
}

/// PyCharm's workflow over headless: commit a checked subset, roll back,
/// ignore, merge with a conflict, rename, push to a bare remote.
#[test]
fn headless_mode_commits_chosen_files_merges_and_pushes() {
    let base = std::env::temp_dir().join(format!("next_code_headless_flow_{}", std::process::id()));
    fs::remove_dir_all(&base).ok();
    fs::create_dir_all(&base).unwrap();
    // The remote's URL keeps the plain path: on Windows `canonicalize`
    // gives `\\?\C:\…`, which git reads as an ssh host.
    let remote = base.join("remote.git");
    let base = base.canonicalize().unwrap();
    let root = base.join("repo");
    fs::create_dir_all(&root).unwrap();
    let run = |dir: &std::path::Path, args: &[&str]| {
        let status = background_command("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    let git = |args: &[&str]| run(&root, args);
    run(&base, &["init", "-q", "--bare", "-b", "main", "remote.git"]);
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "commit.gpgsign", "false"]);
    git(&["config", "core.autocrlf", "false"]);
    git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    fs::write(root.join("a.txt"), "a\n").unwrap();
    fs::write(root.join("b.txt"), "b\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "init"]);

    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    let ok = |reply: &Value| assert_eq!(reply["ok"], true, "{reply}");

    // Modified a and b, an untracked c: commit only b and c.
    fs::write(root.join("a.txt"), "a2\n").unwrap();
    fs::write(root.join("b.txt"), "b2\n").unwrap();
    fs::write(root.join("c.txt"), "c\n").unwrap();
    let commit = headless.call(json!({
        "cmd": "git_commit_paths", "paths": ["b.txt", "c.txt"], "message": "b and c",
    }));
    ok(&commit);
    let status = headless.call(json!({ "cmd": "git_status" }));
    assert_eq!(
        status["result"]["entries"],
        json!([{ "path": "a.txt", "orig_path": null, "staged": null, "unstaged": "modified" }]),
        "{status}"
    );

    // Roll back a; ignore a new untracked file.
    ok(&headless.call(json!({ "cmd": "git_rollback", "paths": ["a.txt"] })));
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "a\n");
    fs::write(root.join("out.log"), "x\n").unwrap();
    ok(&headless.call(json!({ "cmd": "git_ignore", "paths": ["out.log"] })));
    assert_eq!(
        fs::read_to_string(root.join(".gitignore")).unwrap(),
        "/out.log\n"
    );
    ok(&headless.call(json!({
        "cmd": "git_commit_paths", "paths": [".gitignore"], "message": "ignore",
    })));

    // First push sets the upstream.
    let target = headless.call(json!({ "cmd": "git_push_target" }));
    assert_eq!(
        target["result"]["target"],
        json!({ "remote": "origin", "branch": "main", "new": true }),
        "{target}"
    );
    assert_eq!(target["result"]["outgoing"].as_array().unwrap().len(), 3);
    ok(&headless.call(json!({ "cmd": "git_push_branch" })));
    let status = headless.call(json!({ "cmd": "git_status" }));
    assert_eq!(status["result"]["upstream"], "origin/main", "{status}");

    // A branch that conflicts with main: merging stops, then aborts.
    ok(&headless.call(json!({ "cmd": "git_branch_create", "name": "topic" })));
    fs::write(root.join("b.txt"), "topic\n").unwrap();
    git(&["commit", "-q", "-am", "topic"]);
    ok(&headless.call(json!({ "cmd": "git_switch", "name": "main" })));
    fs::write(root.join("b.txt"), "main\n").unwrap();
    git(&["commit", "-q", "-am", "main"]);
    let merge = headless.call(json!({ "cmd": "git_merge", "name": "topic" }));
    assert_eq!(merge["result"], "conflicts", "{merge}");
    let state = headless.call(json!({ "cmd": "git_operation_state" }));
    assert_eq!(state["result"], "merging");
    ok(&headless.call(json!({ "cmd": "git_abort" })));
    let state = headless.call(json!({ "cmd": "git_operation_state" }));
    assert_eq!(state["result"], "none");

    // Rename; tracking shows main one commit ahead of origin/main.
    ok(&headless.call(json!({
        "cmd": "git_branch_rename", "name": "topic", "new_name": "feature",
    })));
    let branches = headless.call(json!({ "cmd": "git_branches" }));
    assert_eq!(
        branches["result"]["tracking"],
        json!([
            { "name": "feature", "upstream": null, "ahead": 0, "behind": 0 },
            { "name": "main", "upstream": "origin/main", "ahead": 1, "behind": 0 },
        ]),
        "{branches}"
    );

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&base).unwrap();
}

/// Stash (untracked files, a pop that conflicts and keeps the stash, drop)
/// and the shelf (shelve modified/new/deleted files, view, unshelve keeping
/// then removing the shelf, a conflicting unshelve, delete).
#[test]
fn headless_mode_stashes_and_shelves_changes() {
    let root =
        std::env::temp_dir().join(format!("next_code_headless_stash_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let git = |args: &[&str]| {
        let status = background_command("git")
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
    fs::write(root.join("a.txt"), "a\n").unwrap();
    fs::write(root.join("b.txt"), "b\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "init"]);

    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    let ok = |reply: &Value| assert_eq!(reply["ok"], true, "{reply}");
    let read = |name: &str| fs::read_to_string(root.join(name)).ok();

    // Stash a modified file and an untracked one.
    fs::write(root.join("a.txt"), "a2\n").unwrap();
    fs::write(root.join("new.txt"), "new\n").unwrap();
    ok(&headless.call(json!({
        "cmd": "git_stash_push", "message": "wip", "include_untracked": true,
    })));
    let status = headless.call(json!({ "cmd": "git_status" }));
    assert_eq!(status["result"]["entries"], json!([]), "{status}");
    let list = headless.call(json!({ "cmd": "git_stash_list" }));
    let stash = &list["result"][0];
    assert_eq!(
        (&stash["index"], &stash["message"], &stash["branch"]),
        (&json!(0), &json!("wip"), &json!("main")),
        "{list}"
    );
    let changes = headless.call(json!({ "cmd": "git_stash_changes", "index": 0 }));
    let files: Vec<(&str, &str, &Value)> = changes["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| {
            (
                file["path"].as_str().unwrap(),
                file["change"].as_str().unwrap(),
                &file["old"],
            )
        })
        .collect();
    assert_eq!(files.len(), 2, "{changes}");
    assert_eq!((files[0].0, files[0].1), ("a.txt", "modified"));
    assert_eq!(
        (files[1].0, files[1].1, files[1].2),
        ("new.txt", "untracked", &json!("empty"))
    );

    // A commit to a.txt makes the pop conflict: the stash stays.
    fs::write(root.join("a.txt"), "committed\n").unwrap();
    git(&["commit", "-q", "-am", "a"]);
    let pop = headless.call(json!({ "cmd": "git_stash_apply", "index": 0, "pop": true }));
    assert_eq!(
        pop["result"],
        json!({ "result": "conflicts", "paths": ["a.txt"] }),
        "{pop}"
    );
    let list = headless.call(json!({ "cmd": "git_stash_list" }));
    assert_eq!(list["result"].as_array().unwrap().len(), 1, "{list}");
    git(&["reset", "-q", "--hard"]);
    git(&["clean", "-q", "-f"]);
    ok(&headless.call(json!({ "cmd": "git_stash_drop", "index": 0 })));
    let list = headless.call(json!({ "cmd": "git_stash_list" }));
    assert_eq!(list["result"], json!([]), "{list}");
    let drop = headless.call(json!({ "cmd": "git_stash_drop", "index": 0 }));
    assert_eq!(drop["ok"], false, "{drop}");

    // Shelve a deletion, a modification and a new file: all rolled back.
    fs::remove_file(root.join("a.txt")).unwrap();
    fs::write(root.join("b.txt"), "b2\n").unwrap();
    fs::write(root.join("n.txt"), "n\n").unwrap();
    let shelve = headless.call(json!({
        "cmd": "git_shelve", "paths": ["a.txt", "b.txt", "n.txt"], "message": "my shelf",
    }));
    ok(&shelve);
    let id = shelve["result"]["id"].as_str().unwrap().to_owned();
    assert_eq!(shelve["result"]["files"].as_array().unwrap().len(), 3);
    let status = headless.call(json!({ "cmd": "git_status" }));
    assert_eq!(status["result"]["entries"], json!([]), "{status}");
    assert_eq!(read("a.txt").as_deref(), Some("committed\n"));
    assert_eq!(read("b.txt").as_deref(), Some("b\n"));
    assert_eq!(read("n.txt"), None);
    assert!(
        root.join(".next_code/shelf")
            .join(&id)
            .join("shelf.patch")
            .exists()
    );
    let list = headless.call(json!({ "cmd": "git_shelf_list" }));
    assert_eq!(list["result"][0]["message"], "my shelf", "{list}");
    let diff = headless.call(json!({ "cmd": "git_shelf_diff", "shelf": id, "path": "b.txt" }));
    assert_eq!(
        (&diff["result"]["added"], &diff["result"]["removed"]),
        (&json!(1), &json!(1)),
        "{diff}"
    );

    // Unshelve keeping the shelf, roll back, unshelve removing it.
    let unshelve = headless.call(json!({ "cmd": "git_unshelve", "shelf": id }));
    assert_eq!(unshelve["result"]["result"], "done", "{unshelve}");
    assert_eq!(read("a.txt"), None);
    assert_eq!(read("b.txt").as_deref(), Some("b2\n"));
    assert_eq!(read("n.txt").as_deref(), Some("n\n"));
    let list = headless.call(json!({ "cmd": "git_shelf_list" }));
    assert_eq!(list["result"].as_array().unwrap().len(), 1, "{list}");
    ok(&headless.call(json!({ "cmd": "git_rollback", "paths": ["a.txt", "b.txt", "n.txt"] })));
    let unshelve = headless.call(json!({ "cmd": "git_unshelve", "shelf": id, "remove": true }));
    assert_eq!(unshelve["result"]["result"], "done", "{unshelve}");
    assert_eq!(read("b.txt").as_deref(), Some("b2\n"));
    let list = headless.call(json!({ "cmd": "git_shelf_list" }));
    assert_eq!(list["result"], json!([]), "{list}");

    // b.txt changes again after shelving: unshelving conflicts, the shelf stays.
    let shelve = headless.call(json!({
        "cmd": "git_shelve", "paths": ["b.txt"], "message": "conflicting",
    }));
    let id = shelve["result"]["id"].as_str().unwrap().to_owned();
    fs::write(root.join("b.txt"), "b3\n").unwrap();
    git(&["commit", "-q", "-am", "b3"]);
    let unshelve = headless.call(json!({ "cmd": "git_unshelve", "shelf": id, "remove": true }));
    assert_eq!(
        unshelve["result"],
        json!({ "result": "conflicts", "paths": ["b.txt"] }),
        "{unshelve}"
    );
    let list = headless.call(json!({ "cmd": "git_shelf_list" }));
    assert_eq!(list["result"].as_array().unwrap().len(), 1, "{list}");
    ok(&headless.call(json!({ "cmd": "git_shelf_delete", "shelf": id })));
    let list = headless.call(json!({ "cmd": "git_shelf_list" }));
    assert_eq!(list["result"], json!([]), "{list}");

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).unwrap();
}

/// The Git window's commands: filtered and paged log, selection changes,
/// commit details, cherry-pick (incl. a conflict), revert, reset, undo,
/// reword, tag, detached checkout and patches.
#[test]
fn headless_mode_filters_the_log_and_operates_on_commits() {
    let root = std::env::temp_dir().join(format!("next_code_headless_log_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let git = |args: &[&str]| {
        let status = background_command("git")
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
    let commit = |file: &str, text: &str, message: &str| {
        fs::write(root.join(file), text).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", message]);
    };
    for n in 1..=4 {
        commit("main.txt", &format!("{n}\n"), &format!("main {n}"));
    }
    git(&["switch", "-q", "-c", "topic"]);
    commit("t.txt", "t\n", "topic adds t");
    commit("main.txt", "topic\n", "topic edits main");
    git(&["switch", "-q", "main"]);
    commit("main.txt", "5\n", "main 5");

    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    let ok = |reply: &Value| assert_eq!(reply["ok"], true, "{reply}");
    let subjects = |reply: &Value| -> Vec<String> {
        reply["result"]["commits"]
            .as_array()
            .unwrap_or_else(|| panic!("{reply}"))
            .iter()
            .map(|c| c["subject"].as_str().unwrap().to_owned())
            .collect()
    };

    // Pages of 3 over all branches.
    let first = headless.call(json!({ "cmd": "git_log", "limit": 3 }));
    assert_eq!(first["result"]["next"], 3, "{first}");
    let rest = headless.call(json!({ "cmd": "git_log", "limit": 10, "skip": 3 }));
    assert_eq!(rest["result"]["next"], Value::Null, "{rest}");
    assert_eq!(subjects(&first).len() + subjects(&rest).len(), 7);
    // Filters.
    let topic = headless.call(json!({ "cmd": "git_log", "text": "TOPIC" }));
    assert_eq!(subjects(&topic), ["topic edits main", "topic adds t"]);
    let main_only = headless.call(json!({ "cmd": "git_log", "branches": ["main"] }));
    assert_eq!(subjects(&main_only).len(), 5);
    let t_file = headless.call(json!({ "cmd": "git_log", "paths": ["t.txt"] }));
    assert_eq!(subjects(&t_file), ["topic adds t"]);
    let nobody = headless.call(json!({ "cmd": "git_log", "authors": ["nobody"] }));
    assert_eq!(subjects(&nobody), Vec::<String>::new());
    let future = headless.call(json!({ "cmd": "git_log", "since": 4_000_000_000_i64 }));
    assert_eq!(subjects(&future), Vec::<String>::new());

    // Two topic commits together, and one commit's details.
    let span = headless.call(json!({
        "cmd": "git_selection_changes", "newest": "topic", "oldest": "topic~1",
    }));
    let paths: Vec<&str> = span["result"]["changes"]
        .as_array()
        .unwrap_or_else(|| panic!("{span}"))
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["main.txt", "t.txt"]);
    let details = headless.call(json!({ "cmd": "git_commit_details", "commit": "main~1" }));
    assert_eq!(
        details["result"],
        json!({ "message": "main 4", "branches": ["main", "topic"], "pushed": false }),
        "{details}"
    );

    // Cherry-pick "topic adds t" cleanly; "topic edits main" conflicts.
    let id = |revision: &str| {
        let output = background_command("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", revision])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let picked = headless.call(json!({ "cmd": "git_cherry_pick", "commits": [id("topic~1")] }));
    assert_eq!(
        picked["result"],
        json!({ "result": "done", "empty": 0 }),
        "{picked}"
    );
    assert!(root.join("t.txt").exists());
    let conflict = headless.call(json!({ "cmd": "git_cherry_pick", "commits": [id("topic")] }));
    assert_eq!(
        conflict["result"],
        json!({ "result": "conflicts", "paths": ["main.txt"] }),
        "{conflict}"
    );
    ok(&headless.call(json!({ "cmd": "git_abort" })));

    // Revert the cherry-pick, then reset away the revert (hard).
    let reverted = headless.call(json!({ "cmd": "git_revert", "commits": [id("HEAD")] }));
    assert_eq!(reverted["result"]["result"], "done", "{reverted}");
    assert!(!root.join("t.txt").exists());
    ok(&headless.call(json!({ "cmd": "git_reset", "commit": "HEAD~1", "mode": "hard" })));
    assert!(root.join("t.txt").exists());

    // Undo the cherry-pick (kept staged), reword HEAD, tag it.
    ok(&headless.call(json!({ "cmd": "git_undo_commit" })));
    let status = headless.call(json!({ "cmd": "git_status" }));
    assert_eq!(
        status["result"]["entries"][0]["staged"], "added",
        "{status}"
    );
    let reworded = headless.call(json!({ "cmd": "git_reword", "message": "main five" }));
    assert!(
        reworded["result"]["summary"]
            .as_str()
            .is_some_and(|s| s.ends_with(" main five")),
        "{reworded}"
    );
    ok(&headless.call(json!({ "cmd": "git_tag", "name": "v5", "message": "five" })));
    let refs = headless.call(json!({ "cmd": "git_refs" }));
    assert!(
        refs["result"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == "v5" && r["kind"] == "tag"),
        "{refs}"
    );

    // A patch of two commits, oldest first; a detached checkout.
    let patch = headless.call(json!({
        "cmd": "git_patch", "commits": [id("main~1"), id("main")],
    }));
    let text = patch["result"]["patch"]
        .as_str()
        .unwrap_or_else(|| panic!("{patch}"));
    assert!(text.find("main 4").unwrap() < text.find("main five").unwrap());
    git(&["stash", "-q"]);
    ok(&headless.call(json!({ "cmd": "git_checkout_revision", "commit": "main~2" })));
    let status = headless.call(json!({ "cmd": "git_status" }));
    assert_eq!(status["result"]["branch"], Value::Null, "{status}");

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
    if background_command("rust-analyzer")
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
    if background_command("rust-analyzer")
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
    if background_command("rust-analyzer")
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
    if background_command("rust-analyzer")
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
    if background_command("rust-analyzer")
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
    if background_command("rustfmt")
        .arg("--version")
        .output()
        .is_ok()
    {
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
    if background_command("uv").arg("--version").output().is_err() {
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
    if background_command("uv").arg("--version").output().is_err() {
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
    let built = background_command(env!("CARGO"))
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

/// Removes the dev containers created for `folder` (by VS Code's label).
struct DevContainers(String);

impl Drop for DevContainers {
    fn drop(&mut self) {
        let label = format!("label=devcontainer.local_folder={}", self.0);
        let Ok(output) = background_command("docker")
            .args(["ps", "-aq", "--filter", &label])
            .output()
        else {
            return;
        };
        for id in String::from_utf8_lossy(&output.stdout).split_whitespace() {
            background_command("docker")
                .args(["rm", "-f", id])
                .output()
                .ok();
        }
    }
}

/// A plain HTTP GET through `localhost:port`; the whole response.
fn http_get(port: u16) -> String {
    use std::io::Read;
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    stream
        .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    String::from_utf8_lossy(&response).into_owned()
}

/// Port forwarding against a real dev container: `forwardPorts` on
/// connect, auto-forwarding of a server started inside, HTTP through the
/// local port, manual forward/stop, release when the server exits.
#[test]
#[ignore = "needs Docker (python:3.12-slim) and a built Linux agent (NEXT_CODE_WSL_AGENT)"]
fn headless_mode_forwards_dev_container_ports() {
    let root = std::env::temp_dir().join(format!("next_code_ports_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(root.join(".devcontainer")).unwrap();
    fs::write(root.join("hello.txt"), "hello\n").unwrap();
    fs::write(
        root.join(".devcontainer/devcontainer.json"),
        r#"{ "image": "python:3.12-slim",
             "forwardPorts": [9100],
             "portsAttributes": { "8000": { "label": "Web" }, "8001": { "onAutoForward": "silent" } } }"#,
    )
    .unwrap();
    let root = root.canonicalize().unwrap();
    let containers = DevContainers(root.to_string_lossy().into_owned());
    let state = root.join(".state");
    let mut headless = Headless::launch(
        &["--devcontainer".as_ref(), root.as_os_str()],
        &[("XDG_DATA_HOME", &state)],
    );
    let ready = headless.next(|line| matches!(line["event"].as_str(), Some("ready" | "error")));
    assert_eq!(ready["event"], "ready", "{ready}");

    // `forwardPorts`, with nothing listening yet.
    let configured =
        headless.next(|line| line["event"] == "port_forwarded" && line["port"] == 9100);
    assert_eq!(configured["origin"], "dev_container", "{configured}");

    let started = headless.call(json!({ "cmd": "exec", "command":
        "setsid python3 -m http.server 8000 >/tmp/http8000.log 2>&1 </dev/null & \
         setsid python3 -m http.server 8001 >/tmp/http8001.log 2>&1 </dev/null & echo $! >/tmp/http8001.pid" }));
    assert_eq!(started["result"]["code"], 0, "{started}");

    let auto = headless.next(|line| line["event"] == "port_forwarded" && line["port"] == 8000);
    assert_eq!(auto["origin"], "auto", "{auto}");
    let local = auto["local_port"].as_u64().unwrap() as u16;
    let available = headless.next(|line| line["event"] == "port_available" && line["port"] == 8000);
    assert_eq!(available["action"], "notify", "{available}");
    assert_eq!(available["label"], "Web", "{available}");
    assert_eq!(available["url"], format!("http://localhost:{local}"));
    let silent = headless.next(|line| line["event"] == "port_available" && line["port"] == 8001);
    assert_eq!(silent["action"], "silent", "{silent}");

    let page = http_get(local);
    assert!(page.starts_with("HTTP/1.0 200"), "{page}");
    assert!(page.contains("hello.txt"), "{page}");

    let list = headless.call(json!({ "cmd": "ports_list" }));
    let result = &list["result"];
    assert_eq!(result["available"], true, "{list}");
    let listening = result["listening"].as_array().unwrap();
    let server = listening
        .iter()
        .find(|l| l["port"] == 8000)
        .expect("8000 listens");
    assert!(
        server["command"]
            .as_str()
            .unwrap_or_default()
            .contains("http.server 8000"),
        "{server}"
    );
    let forwarded = result["forwarded"].as_array().unwrap();
    let web = forwarded
        .iter()
        .find(|f| f["port"] == 8000)
        .expect("8000 forwarded");
    assert_eq!(web["label"], "Web");
    assert_eq!(web["local_port"], local);
    assert!(
        forwarded
            .iter()
            .any(|f| f["port"] == 9100 && f["origin"] == "dev_container")
    );

    // A user port, by hand, then stopped.
    let manual = headless.call(json!({ "cmd": "ports_forward", "port": 9200 }));
    let manual_port = manual["result"]["local_port"].as_u64().expect("local port") as u16;
    assert!(std::net::TcpStream::connect(("127.0.0.1", manual_port)).is_ok());
    let stop = headless.call(json!({ "cmd": "ports_stop", "port": 9200 }));
    assert_eq!(stop["ok"], true, "{stop}");
    // One that stays, to be restored on reopen.
    let kept = headless.call(json!({ "cmd": "ports_forward", "port": "localhost:9201" }));
    assert!(kept["result"]["local_port"].is_u64(), "{kept}");

    // Stopping the automatic port closes its listener; it is not
    // forwarded again while the server keeps listening.
    let stop = headless.call(json!({ "cmd": "ports_stop", "port": 8000 }));
    assert_eq!(stop["ok"], true, "{stop}");
    headless.next(|line| line["event"] == "port_released" && line["port"] == 8000);
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", local)).is_ok() {
        assert!(Instant::now() < deadline, "localhost:{local} still accepts");
        thread::sleep(Duration::from_millis(100));
    }
    thread::sleep(Duration::from_secs(3));
    let list = headless.call(json!({ "cmd": "ports_list" }));
    assert!(
        !list["result"]["forwarded"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["port"] == 8000),
        "{list}"
    );

    // A server that exits releases its automatic port.
    let killed =
        headless.call(json!({ "cmd": "exec", "command": "kill $(cat /tmp/http8001.pid)" }));
    assert_eq!(killed["result"]["code"], 0, "{killed}");
    headless.next(|line| line["event"] == "port_released" && line["port"] == 8001);

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());

    // Reopened (once the detached `docker stop` of the first session is
    // done): the user port is forwarded again.
    let label = format!("label=devcontainer.local_folder={}", root.to_string_lossy());
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let running = background_command("docker")
            .args(["ps", "-q", "--filter", &label])
            .output()
            .unwrap();
        if running.stdout.trim_ascii().is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
    let mut headless = Headless::launch(
        &["--devcontainer".as_ref(), root.as_os_str()],
        &[("XDG_DATA_HOME", &state)],
    );
    let restored = headless.next(|line| line["event"] == "port_forwarded" && line["port"] == 9201);
    assert_eq!(restored["origin"], "user", "{restored}");
    let stop = headless.call(json!({ "cmd": "ports_stop", "port": 9201 }));
    assert_eq!(stop["ok"], true, "{stop}");
    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    drop(containers);
    fs::remove_dir_all(&root).ok();
}

/// A workflow file's text from `json!` written the old way, with `steps`
/// and `inputs` as lists of `{"id": …}` objects: they become mappings
/// keyed by id in list order (`json!` objects sort their keys), nested
/// groups too. JSON is YAML.
fn workflow_file(workflow: &Value) -> String {
    fn keyed(list: &Value) -> String {
        let entries: Vec<String> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                let mut settings = item.as_object().unwrap().clone();
                let id = settings.remove("id").unwrap();
                let steps = settings.remove("steps");
                format!("{id}: {}", with_steps(settings, steps))
            })
            .collect();
        format!("{{{}}}", entries.join(", "))
    }
    fn with_steps(map: serde_json::Map<String, Value>, steps: Option<Value>) -> String {
        let mut text = Value::Object(map).to_string();
        if let Some(steps) = steps {
            text.pop();
            if !text.ends_with('{') {
                text.push_str(", ");
            }
            text.push_str(&format!("\"steps\": {}}}", keyed(&steps)));
        }
        text
    }
    let mut top = workflow.as_object().unwrap().clone();
    let steps = top.remove("steps");
    let inputs = top.remove("inputs").map(|inputs| keyed(&inputs));
    let mut text = with_steps(top, steps);
    if let Some(inputs) = inputs {
        text.pop();
        text.push_str(&format!(", \"inputs\": {inputs}}}"));
    }
    text
}

/// The next `workflow_event` about step `step` reaching `status`.
fn step_event(headless: &mut Headless, step: &str, status: &str) -> Value {
    headless.next(|line| {
        line["event"] == "workflow_event"
            && line["type"] == "step"
            && line["step"] == step
            && (line["status"] == status || line["status"] == "failed")
    })
}

/// A workflow over two local projects: a check on this machine, a server
/// in the other project (output readiness, a printed URL), a manual gate,
/// a port readiness, and a last step in the headless project; then
/// status, Stop All, and a check failing with `onFail` until Retry.
#[test]
fn headless_mode_runs_a_workflow_across_projects() {
    let root = std::env::temp_dir().join(format!("next_code_workflow_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    for dir in ["main/.next_code/workflows", "other", "config"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    let root = root.canonicalize().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let flag = root.join("vpn-up");
    // `echo`, `sleep`, `exit` and `''` read the same in sh and PowerShell.
    let vpn_check = if cfg!(windows) {
        format!(
            "if (Test-Path {}) {{ exit 0 }} else {{ exit 1 }}",
            flag.display()
        )
    } else {
        format!("test -e '{}'", flag.display())
    };
    let workflow = json!({
        "name": "Dev environment",
        "steps": [
            { "id": "check", "check": "exit 0" },
            { "id": "vpn", "check": vpn_check, "on_fail": "Connect the VPN" },
            {
                "id": "server", "title": "Server", "depends_on": ["check"],
                "project": { "local": root.join("other") },
                "run": "echo http://localhost:1234/login; echo serv''ing; sleep 60",
                "ready": { "output": "serving" },
                "open_urls": true
            },
            { "id": "gate", "depends_on": ["server"], "ready": { "manual": "Log in" } },
            { "id": "port", "depends_on": ["gate"], "ready": { "port": port } },
            {
                "id": "last", "depends_on": ["port", "vpn"],
                "project": { "local": root.join("main") },
                "run": "echo ${env:WORKFLOW_WORD}-done", "cwd": ".next_code",
                "env": { "UNUSED": "1" }
            }
        ]
    });
    fs::write(
        root.join("main/.next_code/workflows/dev.yaml"),
        format!("# Started by the test.\n{}\n", workflow_file(&workflow)),
    )
    .unwrap();
    fs::write(
        root.join("main/.next_code/workflows/broken.yaml"),
        "steps: {a: {depends_on: [a], run: x}}",
    )
    .unwrap();
    // A file of the old JSON format, converted below.
    fs::write(
        root.join("main/.next_code/workflows/old.jsonc"),
        "// Old.\n{\"name\": \"Old\", \"steps\": [{\"id\": \"a\", \"check\": \"exit 0\", \"timeout_s\": 60}]}",
    )
    .unwrap();

    let config = root.join("config");
    let mut headless = Headless::launch(
        &[root.join("main").as_os_str()],
        &[
            ("XDG_CONFIG_HOME", &config),
            ("WORKFLOW_WORD", std::path::Path::new("all")),
        ],
    );
    let ready = headless.next(|line| line["event"].is_string());
    assert_eq!(ready["event"], "ready", "{ready}");

    let list = headless.call(json!({ "cmd": "workflows_list" }));
    let list = list["result"].as_array().unwrap().clone();
    let broken = list.iter().find(|w| w["name"] == "broken").unwrap();
    assert_eq!(broken["error"], "dependency cycle: a → a", "{broken}");
    let dev = list
        .iter()
        .find(|w| w["name"] == "Dev environment")
        .unwrap();
    assert_eq!(dev["steps"].as_array().unwrap().len(), 6, "{dev}");
    assert_eq!(dev["steps"][4]["ready"]["port"], port);
    let refused = headless.call(json!({ "cmd": "workflow_run", "name": "broken" }));
    assert!(
        refused["error"].as_str().unwrap().contains("cannot run"),
        "{refused}"
    );
    // The format's schema, for editors and agents.
    let schema = headless.call(json!({ "cmd": "workflow_schema" }));
    assert_eq!(schema["result"]["schema"]["title"], "next_code workflow", "{schema}");
    assert_eq!(schema["result"]["file"], "workflow.schema.json");
    // The old file: listed with a problem until converted.
    let old = list.iter().find(|w| w["name"] == "old").unwrap();
    assert!(
        old["error"].as_str().unwrap().contains("old JSON workflow format"),
        "{old}"
    );
    let converted = headless.call(json!({ "cmd": "workflow_convert", "path": "old.jsonc" }));
    let result = &converted["result"];
    assert!(result["path"].as_str().unwrap().ends_with("old.yaml"), "{converted}");
    assert!(result["backup"].as_str().unwrap().ends_with("old.jsonc.bak"), "{converted}");
    assert_eq!(result["problems"], json!([]));
    let text = fs::read_to_string(root.join("main/.next_code/workflows/old.yaml")).unwrap();
    assert_eq!(text, "# Old.\nname: Old\n\nsteps:\n  a: {check: exit 0, timeout: 1m}\n");
    let list = headless.call(json!({ "cmd": "workflows_list" }));
    let old = list["result"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["name"] == "Old")
        .unwrap()
        .clone();
    assert_eq!(old["error"], Value::Null, "{old}");
    assert_eq!(old["steps"][0]["timeout"], "1m");
    let again = headless.call(json!({ "cmd": "workflow_convert", "name": "Old" }));
    assert!(
        again["error"].as_str().unwrap().contains("not in the old JSON format"),
        "{again}"
    );

    let run = headless.call(json!({ "cmd": "workflow_run", "path": "dev.yaml" }));
    assert_eq!(run["ok"], true, "{run}");
    let vpn = step_event(&mut headless, "vpn", "failed");
    assert_eq!(vpn["reason"], "Connect the VPN", "{vpn}");
    let url = headless.next(|line| line["event"] == "workflow_event" && line["type"] == "url");
    assert_eq!(url["url"], "http://localhost:1234/login", "{url}");
    assert_eq!(url["step"], "server");
    let server = step_event(&mut headless, "server", "ready");
    assert_eq!(server["status"], "ready", "{server}");
    let gate = step_event(&mut headless, "gate", "running");
    assert_eq!(gate["detail"], "Log in", "{gate}");
    let early = headless
        .call(json!({ "cmd": "workflow_continue", "name": "Dev environment", "step": "port" }));
    assert_eq!(early["ok"], false, "{early}");
    let resumed = headless
        .call(json!({ "cmd": "workflow_continue", "name": "Dev environment", "step": "gate" }));
    assert_eq!(resumed["ok"], true, "{resumed}");
    let waiting = step_event(&mut headless, "port", "running");
    assert_eq!(waiting["detail"], format!("waiting for port {port}"));
    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    step_event(&mut headless, "port", "ready");

    // `last` waits for the VPN: Retry once it is up.
    let status = headless.call(json!({ "cmd": "workflow_status", "name": "Dev environment" }));
    let steps = status["result"]["steps"].as_array().unwrap().clone();
    assert_eq!(steps[5]["status"], "waiting", "{status}");
    assert_eq!(steps[5]["detail"], "blocked: vpn did not get ready");
    fs::write(&flag, "").unwrap();
    let retried = headless.call(json!({ "cmd": "workflow_retry", "name": "Dev environment" }));
    assert_eq!(retried["result"], true, "{retried}");
    let last = step_event(&mut headless, "last", "ready");
    assert_eq!(last["status"], "ready", "{last}");
    let outcome = headless.next(|line| line["type"] == "outcome" && line["outcome"] == "ready");
    assert_eq!(outcome["workflow"], "Dev environment");

    let status = headless.call(json!({ "cmd": "workflow_status", "name": "Dev environment" }));
    let result = &status["result"];
    assert_eq!(result["outcome"], "ready", "{status}");
    let steps = result["steps"].as_array().unwrap();
    assert_eq!(steps[2]["last_line"], "serving", "{status}");
    assert_eq!(steps[2]["terminal"], "Server");
    assert_eq!(steps[5]["last_line"], "all-done", "{status}");
    assert_eq!(steps[5]["exit_code"], 0);
    assert!(
        steps[2]["location"].as_str().unwrap().ends_with("other"),
        "{status}"
    );

    // Stop All interrupts the server (still running after it got ready).
    let stop = headless.call(json!({ "cmd": "workflow_stop", "name": "Dev environment" }));
    assert_eq!(stop["result"], true, "{stop}");
    step_event(&mut headless, "server", "stopped");
    headless.next(|line| line["type"] == "outcome" && line["outcome"] == "stopped");

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    drop(listener);
    fs::remove_dir_all(&root).ok();
}

/// `workflow_status` of `path`, polled until `done` holds for it.
fn status_until(headless: &mut Headless, path: &str, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let status = headless.call(json!({ "cmd": "workflow_status", "path": path }));
        if done(&status["result"]) {
            return status["result"].clone();
        }
        assert!(Instant::now() < deadline, "timed out: {status}");
        thread::sleep(Duration::from_millis(100));
    }
}

/// An evaluation that fails (exit 1) after writing an artifact, and a
/// report that runs `after` it with `${steps.eval.exit_code}` and the
/// artifact's path: the run finishes with failures, the report ready.
/// Then Stop during an evaluation that checkpoints on Ctrl+C: it gets its
/// `stop_grace_period` (longer than the default 3s) and writes its checkpoint
/// before it exits; a report after it is blocked.
#[test]
fn headless_mode_reports_after_a_failed_evaluation_and_stops_with_grace() {
    let root = std::env::temp_dir().join(format!("next_code_wf_after_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    for dir in ["main/.next_code/workflows", "config"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    // Windows' canonical form (`\\?\C:\…`) trips PowerShell's `Set-Content`.
    let root = if cfg!(windows) {
        root
    } else {
        root.canonicalize().unwrap()
    };
    let main = root.join("main");
    // The report line reads the same in sh and PowerShell.
    let report = "echo 'code=${steps.eval.exit_code} status=${steps.eval.status} \
                  metrics=${steps.eval.artifacts.metrics}' | tee report.txt";
    let (eval, checkpointing) = if cfg!(windows) {
        (
            "New-Item -ItemType Directory -Force out | Out-Null; Set-Content out/metrics.json '{}'; \
             'evaluated'; exit 1",
            "New-Item -ItemType Directory -Force out | Out-Null; try { 'evaluating'; Start-Sleep 60 } \
             finally { Start-Sleep 4; Set-Content out/checkpoint.txt saved; 'checkpoint written' }",
        )
    } else {
        fs::write(
            main.join("eval.sh"),
            "mkdir -p out\n\
             trap 'sleep 4; echo saved > out/checkpoint.txt; echo checkpoint written; exit 3' INT\n\
             echo evaluating\n\
             sleep 60\n",
        )
        .unwrap();
        (
            "mkdir -p out && echo '{}' > out/metrics.json && echo evaluated && exit 1",
            "sh eval.sh",
        )
    };
    let workflows = main.join(".next_code/workflows");
    let evaluate = json!({
        "name": "Evaluate",
        "steps": [
            {
                "id": "eval", "project": { "local": main }, "run": eval,
                "stop_grace_period": "20s",
                "artifacts": [
                    { "label": "metrics", "path": "out/metrics.json" },
                    { "label": "missing", "path": "out/none.txt" },
                    "https://example.com/runs/1"
                ]
            },
            {
                "id": "report", "depends_on": { "eval": "completed" }, "project": { "local": main },
                "run": report,
                "artifacts": ["report.txt"]
            }
        ]
    });
    fs::write(workflows.join("evaluate.yaml"), workflow_file(&evaluate)).unwrap();
    let grace = json!({
        "name": "Grace",
        "steps": [
            {
                "id": "eval", "project": { "local": main }, "run": checkpointing,
                "stop_grace_period": "20s",
                "artifacts": [{ "label": "checkpoint", "path": "out/checkpoint.txt" }]
            },
            {
                "id": "report", "depends_on": { "eval": "completed" }, "project": { "local": main },
                "run": "echo ${steps.eval.artifacts.checkpoint}"
            }
        ]
    });
    fs::write(workflows.join("grace.yaml"), workflow_file(&grace)).unwrap();

    let mut headless = Headless::launch(
        &[main.as_os_str()],
        &[("XDG_CONFIG_HOME", &root.join("config"))],
    );
    let ready = headless.next(|line| line["event"].is_string());
    assert_eq!(ready["event"], "ready", "{ready}");
    let list = headless.call(json!({ "cmd": "workflows_list" }));
    let listed = list["result"]
        .as_array()
        .unwrap()
        .iter()
        .find(|found| found["path"].as_str().unwrap().ends_with("evaluate.yaml"))
        .unwrap()
        .clone();
    assert_eq!(listed["error"], Value::Null, "{listed}");
    assert_eq!(
        listed["steps"][1]["depends_on"],
        json!({ "eval": "completed" })
    );
    assert_eq!(listed["steps"][0]["stop_grace_period"], "20s");
    assert_eq!(
        listed["steps"][0]["artifacts"][0],
        json!({ "label": "metrics", "path": "out/metrics.json" })
    );
    assert_eq!(
        listed["steps"][0]["artifacts"][2],
        json!({ "label": null, "url": "https://example.com/runs/1" })
    );

    // The evaluation fails; the report runs after it anyway.
    let run = headless.call(json!({ "cmd": "workflow_run", "path": "evaluate.yaml" }));
    assert_eq!(run["ok"], true, "{run}");
    let failed = step_event(&mut headless, "eval", "failed");
    assert_eq!(failed["reason"], "failed (exit code 1)", "{failed}");
    let reported = step_event(&mut headless, "report", "ready");
    assert_eq!(reported["status"], "ready", "{reported}");
    let outcome = headless.next(|line| {
        line["type"] == "outcome" && line["workflow"] == "Evaluate" && line["outcome"] != "running"
    });
    assert_eq!(
        outcome["outcome"], "failed",
        "finished with failures: {outcome}"
    );
    let status = status_until(&mut headless, "evaluate.yaml", |status| {
        status["steps"][1]["artifacts"][0]["exists"] == true
    });
    assert_eq!(status["outcome"], "failed", "{status}");
    let eval = &status["steps"][0];
    assert_eq!(eval["status"], "failed");
    assert_eq!(eval["exit_code"], 1);
    let metrics = &eval["artifacts"][0];
    assert_eq!(metrics["label"], "metrics");
    assert_eq!(metrics["exists"], true, "{status}");
    let metrics_path = metrics["resolved"].as_str().unwrap().to_owned();
    assert!(
        metrics_path.ends_with(if cfg!(windows) {
            "out\\metrics.json"
        } else {
            "out/metrics.json"
        }),
        "{metrics_path}"
    );
    assert!(fs::metadata(&metrics_path).is_ok(), "{metrics_path}");
    assert_eq!(eval["artifacts"][1]["exists"], false, "missing: {status}");
    assert_eq!(
        eval["artifacts"][2]["resolved"],
        "https://example.com/runs/1"
    );
    assert_eq!(eval["artifacts"][2]["exists"], true);
    let report = &status["steps"][1];
    assert_eq!(report["status"], "ready");
    assert_eq!(
        report["last_line"],
        format!("code=1 status=failed metrics={metrics_path}"),
        "{status}"
    );

    // Stop while evaluating: Ctrl+C, then the step's grace to checkpoint.
    let run = headless.call(json!({ "cmd": "workflow_run", "path": "grace.yaml" }));
    assert_eq!(run["ok"], true, "{run}");
    let run_id = run["result"]["run"].clone();
    status_until(&mut headless, "grace.yaml", |status| {
        status["steps"][0]["last_line"] == "evaluating"
    });
    let stopped_at = Instant::now();
    let stop = headless.call(json!({ "cmd": "workflow_stop", "path": "grace.yaml" }));
    assert_eq!(stop["result"], true, "{stop}");
    let stopping = headless.next(|line| {
        line["type"] == "step"
            && line["run"] == run_id
            && line["step"] == "eval"
            && line["status"] == "stopping"
    });
    assert!(
        stopping["detail"]
            .as_str()
            .unwrap()
            .starts_with("stopping ("),
        "{stopping}"
    );
    let stopped = headless.next(|line| {
        line["type"] == "step"
            && line["run"] == run_id
            && line["step"] == "eval"
            && (line["status"] == "stopped" || line["status"] == "failed")
    });
    assert_eq!(stopped["status"], "stopped", "{stopped}");
    let took = stopped_at.elapsed();
    assert!(
        took >= Duration::from_millis(3500) && took < Duration::from_secs(20),
        "the checkpoint took {took:?}"
    );
    assert_eq!(
        fs::read_to_string(main.join("out/checkpoint.txt"))
            .unwrap()
            .trim(),
        "saved"
    );
    let status = status_until(&mut headless, "grace.yaml", |status| {
        status["steps"][0]["artifacts"][0]["exists"] == true
    });
    assert_eq!(status["outcome"], "stopped", "{status}");
    if cfg!(unix) {
        assert_eq!(status["steps"][0]["exit_code"], 3, "{status}");
        // The terminal echoes the Ctrl+C (`^C`) before it.
        let last = status["steps"][0]["last_line"].as_str().unwrap();
        assert!(last.ends_with("checkpoint written"), "{last}");
    }
    assert_eq!(status["steps"][1]["status"], "stopped");
    // A report after a stopped evaluation can never run.
    let restart = headless
        .call(json!({ "cmd": "workflow_restart", "path": "grace.yaml", "step": "report" }));
    assert_eq!(restart["result"], true, "{restart}");
    let status = status_until(&mut headless, "grace.yaml", |status| {
        status["steps"][1]["status"] == "waiting"
    });
    assert_eq!(
        status["steps"][1]["detail"], "blocked: eval was stopped",
        "{status}"
    );
    assert_eq!(status["steps"][1]["blocked"], true);
    assert_eq!(status["outcome"], "failed");

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).ok();
}

/// A group of two steps, one failing (exit 1), and a step after the
/// group: the steps inherit the group's project, events and status name
/// them by qualified id (`svc/bad`) and include the group's own state,
/// the step after it reads its status; Retry on the group reruns its
/// failed step.
#[test]
fn headless_mode_runs_a_workflow_with_a_group() {
    let root = std::env::temp_dir().join(format!("next_code_wf_group_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    for dir in ["main/.next_code/workflows", "config"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    let root = if cfg!(windows) {
        root
    } else {
        root.canonicalize().unwrap()
    };
    let main = root.join("main");
    let grouped = json!({
        "name": "Grouped",
        "steps": [
            {
                "id": "svc", "title": "Service", "project": { "local": main },
                "steps": [
                    { "id": "ok", "run": "echo fine" },
                    { "id": "bad", "run": "echo broken; exit 1" }
                ]
            },
            {
                "id": "report", "depends_on": { "svc": "completed" }, "project": { "local": main },
                "run": "echo report-${steps.svc.status}"
            }
        ]
    });
    fs::write(
        main.join(".next_code/workflows/grouped.yaml"),
        workflow_file(&grouped),
    )
    .unwrap();
    let mut headless = Headless::launch(
        &[main.as_os_str()],
        &[("XDG_CONFIG_HOME", &root.join("config"))],
    );
    let ready = headless.next(|line| line["event"].is_string());
    assert_eq!(ready["event"], "ready", "{ready}");

    let list = headless.call(json!({ "cmd": "workflows_list" }));
    let listed = list["result"]
        .as_array()
        .unwrap()
        .iter()
        .find(|found| found["name"] == "Grouped")
        .unwrap()
        .clone();
    assert_eq!(listed["error"], Value::Null, "{listed}");
    let svc = &listed["steps"][0];
    assert_eq!(svc["kind"], "group", "{listed}");
    assert_eq!(svc["steps"][1]["path"], "svc/bad");
    assert_eq!(svc["steps"][1]["kind"], "step");
    assert!(
        svc["steps"][0]["project"]
            .as_str()
            .unwrap()
            .ends_with("main"),
        "inherited: {listed}"
    );
    assert_eq!(
        listed["steps"][1]["depends_on"],
        json!({ "svc": "completed" })
    );

    let run = headless.call(json!({ "cmd": "workflow_run", "name": "Grouped" }));
    assert_eq!(run["ok"], true, "{run}");
    let bad = step_event(&mut headless, "svc/bad", "failed");
    assert_eq!(bad["reason"], "failed (exit code 1)", "{bad}");
    let group = step_event(&mut headless, "svc", "failed");
    assert_eq!(group["reason"], "1 of 2 steps failed", "{group}");
    let report = step_event(&mut headless, "report", "ready");
    assert_eq!(report["status"], "ready", "{report}");
    let outcome = headless.next(|line| {
        line["type"] == "outcome" && line["workflow"] == "Grouped" && line["outcome"] != "running"
    });
    assert_eq!(outcome["outcome"], "failed", "{outcome}");

    let status = status_until(&mut headless, "grouped.yaml", |status| {
        status["steps"][3]["last_line"] == "report-failed"
    });
    let rows = status["steps"].as_array().unwrap();
    let ids: Vec<&str> = rows.iter().map(|row| row["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["svc", "svc/ok", "svc/bad", "report"], "{status}");
    let kinds: Vec<&str> = rows
        .iter()
        .map(|row| row["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["group", "step", "step", "step"]);
    assert_eq!(rows[0]["status"], "failed");
    assert_eq!(rows[0]["title"], "Service");
    assert_eq!(rows[0]["reason"], "1 of 2 steps failed");
    assert_eq!(rows[0]["steps"], json!(["svc/ok", "svc/bad"]));
    assert_eq!(rows[0]["parent"], Value::Null);
    assert_eq!(rows[1]["parent"], "svc");
    assert_eq!(rows[1]["status"], "ready");
    assert_eq!(rows[1]["last_line"], "fine");
    assert!(
        rows[1]["location"].as_str().unwrap().ends_with("main"),
        "{status}"
    );
    assert_eq!(rows[2]["exit_code"], 1);
    assert_eq!(status["outcome"], "failed");

    // Steps go by qualified id; Retry on the group reruns its failed step.
    let unknown =
        headless.call(json!({ "cmd": "workflow_restart", "name": "Grouped", "step": "bad" }));
    assert_eq!(unknown["ok"], false, "{unknown}");
    let retried =
        headless.call(json!({ "cmd": "workflow_retry", "name": "Grouped", "step": "svc" }));
    assert_eq!(retried["result"], true, "{retried}");
    let again = step_event(&mut headless, "svc/bad", "waiting");
    assert_eq!(again["status"], "waiting", "{again}");
    step_event(&mut headless, "svc/bad", "failed");
    let retried =
        headless.call(json!({ "cmd": "workflow_retry", "name": "Grouped", "step": "svc/ok" }));
    assert_eq!(retried["result"], false, "`ok` did not fail: {retried}");

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).ok();
}

/// A workflow with inputs: listed with their specs, run with values (a
/// step in a group echoes them), defaults for the others, every problem
/// with the values reported.
#[test]
fn headless_mode_runs_a_workflow_with_inputs() {
    let root = std::env::temp_dir().join(format!("next_code_wf_inputs_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    for dir in ["main/.next_code/workflows", "config"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    let root = if cfg!(windows) {
        root
    } else {
        root.canonicalize().unwrap()
    };
    let main = root.join("main");
    let deploy = json!({
        "name": "Deploy",
        "inputs": [
            { "id": "env", "label": "Environment", "type": "choice",
              "options": ["dev", { "value": "prod", "label": "Production" }], "default": "dev" },
            { "id": "branch", "type": "string", "required": true, "placeholder": "branch name" },
            { "id": "workers", "type": "number", "default": 4, "min": 1, "max": 64, "integer": true },
            { "id": "clean", "type": "boolean" }
        ],
        "steps": [{
            "id": "svc", "project": { "local": main }, "env": { "BRANCH": "${inputs.branch}" },
            "steps": [{ "id": "show", "run": "echo ${inputs.env}-${inputs.workers}-${inputs.clean}" }]
        }]
    });
    fs::write(
        main.join(".next_code/workflows/deploy.yaml"),
        workflow_file(&deploy),
    )
    .unwrap();
    let mut headless = Headless::launch(
        &[main.as_os_str()],
        &[("XDG_CONFIG_HOME", &root.join("config"))],
    );
    let ready = headless.next(|line| line["event"].is_string());
    assert_eq!(ready["event"], "ready", "{ready}");

    let list = headless.call(json!({ "cmd": "workflows_list" }));
    let listed = list["result"]
        .as_array()
        .unwrap()
        .iter()
        .find(|found| found["name"] == "Deploy")
        .unwrap()
        .clone();
    assert_eq!(listed["error"], Value::Null, "{listed}");
    let inputs = &listed["inputs"];
    assert_eq!(inputs[0]["label"], "Environment");
    assert_eq!(inputs[0]["type"], "choice");
    assert_eq!(
        inputs[0]["options"],
        json!([{ "value": "dev", "label": "dev" }, { "value": "prod", "label": "Production" }])
    );
    assert_eq!(inputs[1]["required"], true);
    assert_eq!(inputs[1]["placeholder"], "branch name");
    assert_eq!(inputs[2]["default"], 4);
    assert_eq!(inputs[2]["max"], 64.0);
    assert_eq!(inputs[3]["default"], false);

    // Every problem at once; nothing runs.
    let refused = headless.call(json!({
        "cmd": "workflow_run", "name": "Deploy",
        "inputs": { "env": "qa", "workers": 100, "colour": "red" }
    }));
    assert_eq!(refused["ok"], false, "{refused}");
    let error = refused["error"].as_str().unwrap();
    for problem in [
        "unknown input `colour`",
        "input `env`: `qa` is not one of dev, prod",
        "input `branch`: required",
        "input `workers`: must be ≤ 64",
    ] {
        assert!(error.contains(problem), "{problem}: {error}");
    }
    let not_object =
        headless.call(json!({ "cmd": "workflow_run", "name": "Deploy", "inputs": [1] }));
    assert_eq!(
        not_object["error"], "`inputs` must be an object",
        "{not_object}"
    );

    // Strings parse for numbers and booleans; the rest get defaults.
    let run = headless.call(json!({
        "cmd": "workflow_run", "name": "Deploy",
        "inputs": { "branch": "feature", "workers": "8", "clean": "true" }
    }));
    assert_eq!(run["ok"], true, "{run}");
    let show = step_event(&mut headless, "svc/show", "ready");
    assert_eq!(show["status"], "ready", "{show}");
    let status = status_until(&mut headless, "deploy.yaml", |status| {
        status["steps"][1]["last_line"] == "dev-8-true"
    });
    assert_eq!(
        status["inputs"],
        json!({ "env": "dev", "branch": "feature", "workers": 8, "clean": true }),
        "{status}"
    );

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).ok();
}

/// A workflow step in a dev container: the container is brought up for
/// the step, its command runs in a pty inside, and its port is seen
/// through the agent.
#[test]
#[ignore = "needs Docker (python:3.12-slim) and a built Linux agent (NEXT_CODE_WSL_AGENT)"]
fn headless_mode_runs_a_workflow_step_in_a_dev_container() {
    let root = std::env::temp_dir().join(format!("next_code_wf_dc_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    for dir in ["main/.next_code/workflows", "web/.devcontainer"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    let root = root.canonicalize().unwrap();
    fs::write(
        root.join("web/.devcontainer/devcontainer.json"),
        r#"{ "image": "python:3.12-slim" }"#,
    )
    .unwrap();
    fs::write(root.join("web/index.html"), "workflow web\n").unwrap();
    let containers = DevContainers(root.join("web").to_string_lossy().into_owned());
    let workflow = json!({
        "name": "Container",
        "steps": [
            {
                "id": "web", "project": { "devcontainer": root.join("web") },
                "run": "python3 -m http.server 8765", "ready": { "port": 8765 },
                "timeout": "5m"
            },
            {
                "id": "home", "depends_on": ["web"], "project": { "devcontainer": root.join("web") },
                "check": "test \"${userHome}\" = \"$HOME\" && test -f index.html"
            }
        ]
    });
    fs::write(
        root.join("main/.next_code/workflows/container.yaml"),
        workflow_file(&workflow),
    )
    .unwrap();
    let state = root.join("state");
    let mut headless = Headless::launch(
        &[root.join("main").as_os_str()],
        &[("XDG_DATA_HOME", &state)],
    );
    let ready = headless.next(|line| line["event"].is_string());
    assert_eq!(ready["event"], "ready", "{ready}");
    let run = headless.call(json!({ "cmd": "workflow_run", "name": "Container" }));
    assert_eq!(run["ok"], true, "{run}");
    let web = step_event(&mut headless, "web", "ready");
    assert_eq!(web["status"], "ready", "{web}");
    let home = step_event(&mut headless, "home", "ready");
    assert_eq!(home["status"], "ready", "{home}");
    let status = headless.call(json!({ "cmd": "workflow_status", "name": "Container" }));
    let location = status["result"]["steps"][0]["location"].as_str().unwrap();
    assert!(location.ends_with("[Dev Container]"), "{status}");
    let stop = headless.call(json!({ "cmd": "workflow_stop", "name": "Container" }));
    assert_eq!(stop["result"], true, "{stop}");
    step_event(&mut headless, "web", "stopped");
    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    drop(containers);
    fs::remove_dir_all(&root).ok();
}

/// `spell_check` on files and on text: comment/string/docstring scoping,
/// corrections, the project's `.next_code/words.txt` and (where the config
/// dir can be redirected) the user's words, settings and extra
/// dictionaries.
#[test]
fn headless_mode_spell_checks_files_and_text() {
    let root =
        std::env::temp_dir().join(format!("next_code_headless_spell_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    let project = root.join("project");
    fs::create_dir_all(project.join(".next_code")).unwrap();
    fs::write(
        project.join("a.py"),
        "\"\"\"Modul for frobnitz zorblax.\"\"\"\n\n\
         def recieve_data(argumnt):\n    \
         # Retreive the data\n    \
         return f\"Helo {argumnt}\"\n",
    )
    .unwrap();
    fs::write(project.join(".next_code").join("words.txt"), "frobnitz\n").unwrap();
    let config = root.join("config");
    let dictionaries = config.join("next_code").join("dictionaries");
    fs::create_dir_all(&dictionaries).unwrap();
    fs::write(config.join("next_code").join("words.txt"), "Zorblax\n").unwrap();
    fs::write(dictionaries.join("xx_TEST.aff"), "SET UTF-8\n").unwrap();
    fs::write(dictionaries.join("xx_TEST.dic"), "1\nhelo\n").unwrap();
    let project = project.canonicalize().unwrap();
    let redirected = cfg!(target_os = "linux");

    let mut headless = Headless::start_with(&project, &[("XDG_CONFIG_HOME", &config)]);
    let ready = headless.next(|line| line["event"].is_string());
    assert_eq!(ready["event"], "ready", "{ready}");
    let words = |reply: &Value| -> Vec<String> {
        reply["result"]["misspellings"]
            .as_array()
            .unwrap_or_else(|| panic!("{reply}"))
            .iter()
            .map(|m| m["word"].as_str().unwrap().to_owned())
            .collect()
    };

    let file = headless.call(json!({ "cmd": "spell_check", "path": "a.py" }));
    let mut expected = vec!["Modul", "Retreive", "Helo"];
    if !redirected {
        expected.insert(1, "zorblax");
    }
    assert_eq!(
        words(&file),
        expected,
        "identifiers are not checked: {file}"
    );
    let modul = &file["result"]["misspellings"][0];
    assert_eq!(
        (modul["start"].as_u64(), modul["end"].as_u64()),
        (Some(3), Some(8))
    );
    let retreive = &file["result"]["misspellings"][expected.len() - 2];
    assert_eq!(
        (retreive["line"].as_u64(), retreive["column"].as_u64()),
        (Some(3), Some(6))
    );
    assert_eq!(retreive["suggestions"][0], "Retrieve", "{retreive}");

    // Unsaved text checked as the file; Markdown and prose without a path.
    let text = headless.call(json!({
        "cmd": "spell_check", "path": "a.py", "text": "x = 1  # an exampel\nmispeled = 2\n",
    }));
    assert_eq!(words(&text), ["exampel"]);
    let markdown = headless.call(json!({
        "cmd": "spell_check", "language": "markdown",
        "text": "# Titel\n\nSee `fooBarr` and [docs](https://exmaple.com).\n\n```\nmispeled\n```\n",
    }));
    assert_eq!(words(&markdown), ["Titel"]);
    let prose = headless.call(json!({ "cmd": "spell_check", "text": "Fix teh bug" }));
    assert_eq!(words(&prose), ["teh"]);
    let unknown = headless.call(json!({ "cmd": "spell_check", "text": "x", "language": "cobol" }));
    assert_eq!(unknown["ok"], false, "{unknown}");

    if redirected {
        // An extra dictionary from the config dir, chosen in settings.
        fs::write(
            config.join("next_code").join("settings.json"),
            r#"{ "spell": { "languages": ["en_US", "xx_TEST"], "check_code": false } }"#,
        )
        .unwrap();
        assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
        assert!(headless.child.wait().unwrap().success());
        headless = Headless::start_with(&project, &[("XDG_CONFIG_HOME", &config)]);
        headless.next(|line| line["event"].is_string());
        let code = headless.call(json!({ "cmd": "spell_check", "path": "a.py" }));
        assert_eq!(words(&code), Vec::<String>::new(), "check_code off: {code}");
        let notes = headless.call(json!({
            "cmd": "spell_check", "path": "notes.md", "text": "Helo Modul",
        }));
        assert_eq!(words(&notes), ["Modul"]);
    }

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).ok();
}

/// The interpreter for notebook tests, when it can import `modules`:
/// 1. `NEXT_CODE_NOTEBOOK_PYTHON` when set (and nothing else);
/// 2. a virtualenv with ipykernel and marimo kept under cargo's target tmp
///    dir, created once with `uv` (`uv venv` + `uv pip install`) when uv is
///    installed; only that first creation needs the network, and it gives
///    up after [`NOTEBOOK_ENV_TIMEOUT`];
/// 3. `python3`/`python` from PATH when it already imports them.
///
/// `None` (after printing why: the test then passes as skipped) otherwise.
fn notebook_python(modules: &[&str]) -> Option<PathBuf> {
    let imports = modules.join(", ");
    if let Some(python) = std::env::var_os("NEXT_CODE_NOTEBOOK_PYTHON") {
        let python = PathBuf::from(python);
        if python_imports(&python, &imports) {
            return Some(python);
        }
        println!(
            "skipped: NEXT_CODE_NOTEBOOK_PYTHON ({}) cannot import {imports}",
            python.display()
        );
        return None;
    }
    let found = NOTEBOOK_ENV
        .iter()
        .cloned()
        .chain(["python3", "python"].map(PathBuf::from))
        .find(|python| python_imports(python, &imports));
    if found.is_none() {
        println!(
            "skipped: no interpreter with {imports} (set NEXT_CODE_NOTEBOOK_PYTHON, or install \
             uv so the tests create one)"
        );
    }
    found
}

/// How long creating the cached notebook env may take.
const NOTEBOOK_ENV_TIMEOUT: Duration = Duration::from_secs(600);

/// The cached notebook env's interpreter, created on first use (once per
/// test process; other processes find it done).
static NOTEBOOK_ENV: std::sync::LazyLock<Option<PathBuf>> =
    std::sync::LazyLock::new(cached_notebook_env);

fn venv_python(venv: &std::path::Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    }
}

fn python_imports(python: &std::path::Path, imports: &str) -> bool {
    background_command(python)
        .args(["-c", &format!("import {imports}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn cached_notebook_env() -> Option<PathBuf> {
    const MODULES: &str = "ipykernel, jupyter_client, marimo";
    let venv = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("notebook-env");
    let python = venv_python(&venv);
    if python_imports(&python, MODULES) {
        return Some(python);
    }
    if !background_command("uv")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        println!(
            "no notebook env at {} and no uv to create it",
            venv.display()
        );
        return None;
    }
    // Built next to it and renamed into place, so a half-made env is never
    // taken for a ready one (also by a concurrent test process).
    let building = venv.with_file_name(format!("notebook-env-{}", std::process::id()));
    fs::remove_dir_all(&building).ok();
    let log = venv.with_file_name(format!("notebook-env-{}.log", std::process::id()));
    println!(
        "creating the notebook test env {} (uv venv; uv pip install ipykernel marimo)",
        venv.display()
    );
    let deadline = Instant::now() + NOTEBOOK_ENV_TIMEOUT;
    let built = run_until(
        background_command("uv")
            .args(["venv", "--quiet", "--relocatable"])
            .arg(&building),
        &log,
        deadline,
    )
    .and_then(|()| {
        run_until(
            background_command("uv")
                // Bytecode now: the tests' first imports run in parallel.
                .args([
                    "pip",
                    "install",
                    "--quiet",
                    "--compile-bytecode",
                    "--python",
                ])
                .arg(venv_python(&building))
                .args(["ipykernel", "marimo"]),
            &log,
            deadline,
        )
    });
    if let Err(err) = built {
        let output = fs::read_to_string(&log).unwrap_or_default();
        println!(
            "could not create the notebook test env (offline?): {err}\n{}",
            output.trim()
        );
        fs::remove_dir_all(&building).ok();
        fs::remove_file(&log).ok();
        return None;
    }
    fs::remove_file(&log).ok();
    // A broken env from an earlier run makes way; if another process just
    // put its env there, ours goes.
    if !python_imports(&python, MODULES) {
        fs::remove_dir_all(&venv).ok();
    }
    if fs::rename(&building, &venv).is_err() {
        fs::remove_dir_all(&building).ok();
    }
    python_imports(&python, MODULES).then_some(python)
}

/// Runs `command` (output appended to `log`) until it exits or `deadline`
/// passes (then it is killed).
fn run_until(
    command: &mut Command,
    log: &std::path::Path,
    deadline: Instant,
) -> Result<(), String> {
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|err| err.to_string())?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(file.try_clone().map_err(|err| err.to_string())?)
        .stderr(file)
        .spawn()
        .map_err(|err| err.to_string())?;
    loop {
        if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
            return if status.success() {
                Ok(())
            } else {
                Err(format!("{command:?} failed ({status})"))
            };
        }
        if Instant::now() >= deadline {
            child.kill().ok();
            child.wait().ok();
            return Err(format!("{command:?} timed out"));
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// A notebook run's cells by index.
fn ran_cells(reply: &Value) -> Vec<u64> {
    reply["result"]["cells"]
        .as_array()
        .unwrap_or_else(|| panic!("{reply}"))
        .iter()
        .map(|cell| cell["index"].as_u64().unwrap())
        .collect()
}

const JUPYTER_CELL_INTRO: &str = r##"  {
   "cell_type": "markdown",
   "id": "intro",
   "metadata": {},
   "source": [
    "# Doubling\n",
    "A *small* notebook."
   ]
  },"##;

const JUPYTER_CELL_SKIPPED: &str = r##"  {
   "cell_type": "code",
   "execution_count": null,
   "id": "skipped",
   "metadata": {},
   "outputs": [],
   "source": [
    "print('never')"
   ]
  }"##;

const JUPYTER_METADATA: &str = r##" "metadata": {
  "custom": {
   "keep": true
  },
  "kernelspec": {
   "display_name": "Python 3",
   "language": "python",
   "name": "python3"
  },
  "language_info": {
   "name": "python"
  }
 },
 "nbformat": 4,
 "nbformat_minor": 5
}
"##;

/// A Jupyter notebook in Jupyter's own layout: run (outputs, counts, the
/// stop at the first error), save (outputs written, the rest unchanged
/// byte for byte), close (the kernel process ends).
#[test]
fn headless_mode_runs_saves_and_closes_a_jupyter_notebook() {
    let Some(python) = notebook_python(&["ipykernel", "jupyter_client"]) else {
        return;
    };
    let root =
        std::env::temp_dir().join(format!("next_code_headless_ipynb_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(root.join("nb")).unwrap();
    let code_cell = |id: &str, lines: &[&str]| {
        let source: Vec<String> = lines.iter().map(|line| json!(line).to_string()).collect();
        format!(
            "  {{\n   \"cell_type\": \"code\",\n   \"execution_count\": null,\n   \"id\": \"{id}\",\n   \
             \"metadata\": {{}},\n   \"outputs\": [],\n   \"source\": [\n    {}\n   ]\n  }},",
            source.join(",\n    ")
        )
    };
    let fixture = [
        "{\n \"cells\": [".to_owned(),
        JUPYTER_CELL_INTRO.to_owned(),
        code_cell("define", &["x = 21"]),
        code_cell("double", &["print(x * 2)"]),
        code_cell("pid", &["import os\n", "print(os.getpid())\n", "x"]),
        code_cell("fail", &["1 / 0"]),
        JUPYTER_CELL_SKIPPED.to_owned(),
        " ],".to_owned(),
        JUPYTER_METADATA.to_owned(),
    ]
    .join("\n");
    let file = root.join("nb").join("double.ipynb");
    fs::write(&file, &fixture).unwrap();
    let root = root.canonicalize().unwrap();

    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    let path = "nb/double.ipynb";
    let open = headless.call(json!({
        "cmd": "notebook_open", "path": path, "python": python.to_string_lossy(),
    }));
    assert_eq!(open["ok"], true, "{open}");
    assert_eq!(open["result"]["kind"], "ipynb");
    let cells = open["result"]["cells"].as_array().unwrap();
    assert_eq!(cells.len(), 6);
    assert_eq!(cells[0]["kind"], "markdown");
    assert_eq!(cells[3]["source"], "import os\nprint(os.getpid())\nx");

    let run = headless.call(json!({ "cmd": "notebook_run", "path": path }));
    assert_eq!(run["ok"], true, "{run}");
    assert_eq!(
        ran_cells(&run),
        [1, 2, 3, 4],
        "markdown never runs; the error stops the rest: {run}"
    );
    let ran = run["result"]["cells"].as_array().unwrap();
    for (cell, count) in ran.iter().zip(1..) {
        assert_eq!(cell["execution_count"], count, "{cell}");
    }
    assert_eq!(ran[0]["status"], "ok");
    assert_eq!(ran[1]["outputs"][0]["output_type"], "stream");
    assert_eq!(ran[1]["outputs"][0]["text"], "42\n", "{run}");
    let pid: u32 = ran[2]["outputs"][0]["text"]
        .as_str()
        .unwrap()
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("{run}"));
    assert_eq!(ran[2]["outputs"][1]["output_type"], "execute_result");
    assert_eq!(ran[2]["outputs"][1]["text_plain"], "21");
    let error = &ran[3];
    assert_eq!(error["status"], "error");
    assert_eq!(error["outputs"][0]["ename"], "ZeroDivisionError", "{error}");
    assert!(
        !error["outputs"][0]["traceback"]
            .to_string()
            .contains("\\u001b"),
        "tracebacks lose their colours: {error}"
    );
    let status = headless.call(json!({ "cmd": "notebook_status", "path": path }));
    assert_eq!(status["result"]["cells"][5]["status"], "idle", "{status}");
    assert!(
        status["result"]["interpreter"].as_str().is_some(),
        "{status}"
    );

    let save = headless.call(json!({ "cmd": "notebook_save", "path": path }));
    assert_eq!(save["ok"], true, "{save}");
    let saved = fs::read_to_string(root.join(path)).unwrap();
    for unchanged in [JUPYTER_CELL_INTRO, JUPYTER_CELL_SKIPPED, JUPYTER_METADATA] {
        assert!(
            saved.contains(unchanged),
            "kept byte for byte:\n{unchanged}\nin\n{saved}"
        );
    }
    let notebook: Value = serde_json::from_str(&saved).unwrap();
    let double = &notebook["cells"][2];
    assert_eq!(double["execution_count"], 2);
    assert_eq!(
        double["outputs"],
        json!([{ "name": "stdout", "output_type": "stream", "text": ["42\n"] }])
    );
    assert_eq!(
        notebook["cells"][4]["outputs"][0]["ename"],
        "ZeroDivisionError"
    );
    assert_eq!(notebook["cells"][5]["execution_count"], Value::Null);

    let close = headless.call(json!({ "cmd": "notebook_close", "path": path }));
    assert_eq!(close["ok"], true, "{close}");
    if cfg!(target_os = "linux") {
        let process = PathBuf::from(format!("/proc/{pid}"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while process.exists() {
            assert!(
                Instant::now() < deadline,
                "kernel {pid} still runs after notebook_close"
            );
            thread::sleep(Duration::from_millis(100));
        }
    }
    let closed = headless.call(json!({ "cmd": "notebook_run", "path": path }));
    assert_eq!(closed["ok"], false, "{closed}");

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).ok();
}

const MARIMO_NOTEBOOK: &str = "import marimo\n\n__generated_with = \"0.25.1\"\napp = marimo.App(width=\"medium\")\n\n\n\
    @app.cell\ndef _():\n    a = 1\n    return (a,)\n\n\n\
    @app.cell\ndef _(a):\n    b = a + 1\n    return (b,)\n\n\n\
    @app.cell\ndef _(b):\n    print(b)\n    return\n\n\n\
    if __name__ == \"__main__\":\n    app.run()\n";

/// A marimo notebook: reactive re-runs after an edit, multiple
/// definitions reported on both cells, saved in marimo's layout.
#[test]
fn headless_mode_runs_marimo_cells_reactively_and_saves_them() {
    let Some(python) = notebook_python(&["ipykernel", "jupyter_client", "marimo"]) else {
        return;
    };
    let root =
        std::env::temp_dir().join(format!("next_code_headless_marimo_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("reactive.py"), MARIMO_NOTEBOOK).unwrap();
    let root = root.canonicalize().unwrap();

    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    let path = "reactive.py";
    let mut ok = |request: Value| {
        let reply = headless.call(request.clone());
        assert_eq!(reply["ok"], true, "{request} → {reply}");
        reply
    };
    let open = ok(json!({
        "cmd": "notebook_open", "path": path, "python": python.to_string_lossy(),
    }));
    assert_eq!(open["result"]["kind"], "marimo");
    assert_eq!(open["result"]["cells"][1]["source"], "b = a + 1");

    let run = ok(json!({ "cmd": "notebook_run", "path": path }));
    assert_eq!(ran_cells(&run), [0, 1, 2]);
    assert_eq!(
        run["result"]["cells"][2]["outputs"][0]["text"], "2\n",
        "{run}"
    );

    let edit = ok(json!({ "cmd": "notebook_edit", "path": path, "cell": 0, "source": "a = 10" }));
    assert_eq!(edit["result"]["stale"], true, "{edit}");
    let run = ok(json!({ "cmd": "notebook_run", "path": path, "cells": [0] }));
    assert_eq!(ran_cells(&run), [0, 1, 2], "descendants re-run: {run}");
    assert_eq!(
        run["result"]["cells"][2]["outputs"][0]["text"], "11\n",
        "{run}"
    );
    assert_eq!(run["result"]["cells"][0]["stale"], false);

    let insert =
        ok(json!({ "cmd": "notebook_insert", "path": path, "index": 3, "source": "a = 2" }));
    assert!(insert["result"]["error"].is_string(), "{insert}");
    let status = ok(json!({ "cmd": "notebook_status", "path": path }));
    let error = status["result"]["cells"][0]["error"]
        .as_str()
        .unwrap_or_else(|| panic!("{status}"));
    println!("multiple definitions: {error}");
    let run = ok(json!({ "cmd": "notebook_run", "path": path, "cells": [3] }));
    assert_eq!(ran_cells(&run), [1, 2, 3], "{run}");
    let cells = run["result"]["cells"].as_array().unwrap();
    assert_eq!(cells[2]["outputs"][0]["ename"], "MarimoError", "{run}");
    for skipped in &cells[..2] {
        assert_eq!(skipped["status"], "error");
        assert!(
            skipped["outputs"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("An ancestor raised an exception"),
            "{skipped}"
        );
    }

    ok(json!({ "cmd": "notebook_delete", "path": path, "cell": 3 }));
    ok(json!({ "cmd": "notebook_save", "path": path }));
    let saved = fs::read_to_string(root.join(path)).unwrap();
    for expected in [
        "__generated_with = \"0.25.1\"\napp = marimo.App(width=\"medium\")\n",
        "@app.cell\ndef _():\n    a = 10\n    return (a,)\n",
        "@app.cell\ndef _(a):\n    b = a + 1\n    return (b,)\n",
        "@app.cell\ndef _(b):\n    print(b)\n    return\n",
    ] {
        assert!(saved.contains(expected), "{expected:?} in\n{saved}");
    }
    marimo_check(&python, &root.join(path));

    ok(json!({ "cmd": "notebook_close", "path": path }));
    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).ok();
}

/// `marimo check` of `file` with the marimo CLI next to `python` (skipped
/// with a note when there is none).
fn marimo_check(python: &std::path::Path, file: &std::path::Path) {
    let marimo = python.with_file_name(format!("marimo{}", std::env::consts::EXE_SUFFIX));
    if !marimo.exists() {
        println!("marimo check skipped: no marimo CLI next to the interpreter");
        return;
    }
    let check = background_command(&marimo)
        .arg("check")
        .arg(file)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "marimo check: {}{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr)
    );
}

const MARIMO_ASYNC: &str = "import marimo\n\n__generated_with = \"0.25.1\"\napp = marimo.App()\n\n\n\
    @app.cell\nasync def _():\n    import asyncio\n    await asyncio.sleep(0.01)\n    a = 1\n    \
    await asyncio.sleep(0, result=\"slept\")\n    return (a,)\n\n\n\
    @app.cell\ndef _(a):\n    b = a + 1\n    return (b,)\n\n\n\
    @app.cell\ndef _(b):\n    print(b)\n    return\n\n\n\
    if __name__ == \"__main__\":\n    app.run()\n";

/// A marimo cell with top-level `await` runs on the kernel's loop; an
/// external edit reloads with the edited cell and its descendants stale
/// (outputs kept) and a run brings them up to date; `async def` is saved.
#[test]
fn headless_mode_awaits_marimo_cells_and_marks_reloaded_cells_stale() {
    let Some(python) = notebook_python(&["ipykernel", "jupyter_client", "marimo"]) else {
        return;
    };
    let root = std::env::temp_dir().join(format!(
        "next_code_headless_mo_async_{}",
        std::process::id()
    ));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("async_nb.py"), MARIMO_ASYNC).unwrap();
    let root = root.canonicalize().unwrap();

    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    let path = "async_nb.py";
    let mut ok = |request: Value| {
        let reply = headless.call(request.clone());
        assert_eq!(reply["ok"], true, "{request} → {reply}");
        reply
    };
    ok(json!({ "cmd": "notebook_open", "path": path, "python": python.to_string_lossy() }));
    let run = ok(json!({ "cmd": "notebook_run", "path": path }));
    assert_eq!(ran_cells(&run), [0, 1, 2]);
    let cells = run["result"]["cells"].as_array().unwrap();
    assert_eq!(cells[0]["status"], "ok", "{run}");
    assert_eq!(cells[0]["outputs"][0]["text_plain"], "'slept'", "{run}");
    assert_eq!(cells[2]["outputs"][0]["text"], "2\n", "{run}");

    // Unchanged on disk: nothing to reload.
    let same = ok(json!({ "cmd": "notebook_reload", "path": path }));
    assert_eq!(same["result"]["reloaded"], false);
    fs::write(
        root.join(path),
        MARIMO_ASYNC.replace("    a = 1\n", "    a = 5\n"),
    )
    .unwrap();
    let reload = ok(json!({ "cmd": "notebook_reload", "path": path }));
    assert_eq!(reload["result"]["reloaded"], true);
    let cells = reload["result"]["cells"].as_array().unwrap();
    let stale: Vec<&Value> = cells.iter().map(|cell| &cell["stale"]).collect();
    assert_eq!(stale, [true, true, true], "{reload}");
    assert_eq!(
        cells[2]["outputs"][0]["text"], "2\n",
        "outputs kept: {reload}"
    );
    let run = ok(json!({ "cmd": "notebook_run", "path": path, "cells": [2] }));
    assert_eq!(ran_cells(&run), [0, 1, 2], "stale ancestors first: {run}");
    assert_eq!(run["result"]["cells"][2]["outputs"][0]["text"], "6\n");
    let status = ok(json!({ "cmd": "notebook_status", "path": path }));
    assert!(
        status["result"]["cells"]
            .as_array()
            .unwrap()
            .iter()
            .all(|cell| cell["stale"] == false),
        "{status}"
    );

    ok(json!({ "cmd": "notebook_edit", "path": path, "cell": 1, "source": "b = a + 2" }));
    ok(json!({ "cmd": "notebook_save", "path": path }));
    let saved = fs::read_to_string(root.join(path)).unwrap();
    assert!(
        saved.contains("@app.cell\nasync def _():\n    import asyncio\n"),
        "{saved}"
    );
    marimo_check(&python, &root.join(path));

    ok(json!({ "cmd": "notebook_close", "path": path }));
    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).ok();
}

/// Collapsed outputs/sources are saved in the `.ipynb` (JupyterLab's and
/// nbformat's keys) and read back on reopening; a Jupyter cell may
/// `await`; a reload after an output-stripping rewrite keeps the outputs
/// of unchanged cells.
#[test]
fn headless_mode_keeps_collapsed_cells_and_reloads_jupyter_notebooks() {
    let Some(python) = notebook_python(&["ipykernel", "jupyter_client"]) else {
        return;
    };
    let root =
        std::env::temp_dir().join(format!("next_code_headless_nb_fold_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(&root).unwrap();
    let notebook = |cells: Value| {
        serde_json::to_string_pretty(&json!({
            "cells": cells, "metadata": {}, "nbformat": 4, "nbformat_minor": 5,
        }))
        .unwrap()
    };
    let cell = |id: &str, source: &str, metadata: Value| {
        json!({
            "cell_type": "code", "execution_count": null, "id": id, "metadata": metadata,
            "outputs": [], "source": source,
        })
    };
    let awaited = "import asyncio\nawaited = await asyncio.sleep(0.01, result=21)\nprint(awaited)";
    fs::write(
        root.join("fold.ipynb"),
        notebook(json!([
            cell("first", awaited, json!({ "collapsed": true })),
            cell("second", "awaited * 2", json!({})),
        ])),
    )
    .unwrap();
    let root = root.canonicalize().unwrap();

    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    let path = "fold.ipynb";
    let mut ok = |request: Value| {
        let reply = headless.call(request.clone());
        assert_eq!(reply["ok"], true, "{request} → {reply}");
        reply
    };
    let open =
        ok(json!({ "cmd": "notebook_open", "path": path, "python": python.to_string_lossy() }));
    assert_eq!(open["result"]["cells"][0]["outputs_hidden"], true, "{open}");
    assert_eq!(open["result"]["cells"][1]["outputs_hidden"], false);
    let run = ok(json!({ "cmd": "notebook_run", "path": path }));
    let cells = run["result"]["cells"].as_array().unwrap();
    assert_eq!(cells[0]["outputs"][0]["text"], "21\n", "{run}");
    assert_eq!(cells[1]["outputs"][0]["text_plain"], "42", "{run}");

    let edit = ok(json!({
        "cmd": "notebook_edit", "path": path, "cell": 1,
        "outputs_hidden": true, "source_hidden": true,
    }));
    assert_eq!(edit["result"]["outputs_hidden"], true);
    ok(json!({ "cmd": "notebook_edit", "path": path, "cell": 0, "outputs_hidden": false }));
    ok(json!({ "cmd": "notebook_save", "path": path }));
    let saved: Value = serde_json::from_str(&fs::read_to_string(root.join(path)).unwrap()).unwrap();
    assert_eq!(saved["cells"][0]["metadata"], json!({ "collapsed": false }));
    assert_eq!(
        saved["cells"][1]["metadata"],
        json!({ "collapsed": true, "jupyter": { "outputs_hidden": true, "source_hidden": true } })
    );
    ok(json!({ "cmd": "notebook_close", "path": path }));
    let reopened =
        ok(json!({ "cmd": "notebook_open", "path": path, "python": python.to_string_lossy() }));
    let cells = reopened["result"]["cells"].as_array().unwrap();
    assert_eq!(
        (&cells[1]["outputs_hidden"], &cells[1]["source_hidden"]),
        (&json!(true), &json!(true)),
        "{reopened}"
    );
    assert_eq!(cells[1]["outputs"][0]["text_plain"], "42", "outputs saved");

    // A tool strips the outputs and changes the second cell's source.
    let mut stripped = saved.clone();
    for cell in stripped["cells"].as_array_mut().unwrap() {
        cell["outputs"] = json!([]);
        cell["execution_count"] = Value::Null;
    }
    stripped["cells"][1]["source"] = json!("awaited * 3");
    fs::write(
        root.join(path),
        serde_json::to_string_pretty(&stripped).unwrap(),
    )
    .unwrap();
    let reload = ok(json!({ "cmd": "notebook_reload", "path": path }));
    let cells = reload["result"]["cells"].as_array().unwrap();
    assert_eq!(cells[0]["outputs"][0]["text"], "21\n", "kept: {reload}");
    assert_eq!(cells[1]["outputs"], json!([]), "changed: {reload}");
    assert_eq!(cells[1]["source_hidden"], true);

    ok(json!({ "cmd": "notebook_close", "path": path }));
    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).ok();
}

/// marimo UI elements: described with their arguments and value; setting
/// one runs the cells using it (not the one defining it) with the new
/// value, as marimo does.
#[test]
fn headless_mode_sets_marimo_ui_elements_and_reruns_their_users() {
    let Some(python) = notebook_python(&["ipykernel", "jupyter_client", "marimo"]) else {
        return;
    };
    let root =
        std::env::temp_dir().join(format!("next_code_headless_mo_ui_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(&root).unwrap();
    let notebook = "import marimo\n\n__generated_with = \"0.25.1\"\napp = marimo.App()\n\n\n\
        @app.cell\ndef _():\n    import marimo as mo\n    return (mo,)\n\n\n\
        @app.cell\ndef _(mo):\n    size = mo.ui.slider(1, 10, value=3, label=\"Size\")\n    \
        loud = mo.ui.checkbox(label=\"Loud\")\n    mo.md(f\"Pick {size} and {loud}\")\n    \
        return loud, size\n\n\n\
        @app.cell\ndef _(loud, size):\n    word = \"hi\" * size.value\n    \
        print(word.upper() if loud.value else word)\n    return\n\n\n\
        if __name__ == \"__main__\":\n    app.run()\n";
    fs::write(root.join("ui.py"), notebook).unwrap();
    let root = root.canonicalize().unwrap();

    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    let path = "ui.py";
    let mut ok = |request: Value| {
        let reply = headless.call(request.clone());
        assert_eq!(reply["ok"], true, "{request} → {reply}");
        reply
    };
    ok(json!({ "cmd": "notebook_open", "path": path, "python": python.to_string_lossy() }));
    let run = ok(json!({ "cmd": "notebook_run", "path": path }));
    let cells = run["result"]["cells"].as_array().unwrap();
    assert_eq!(cells[2]["outputs"][0]["text"], "hihihi\n", "{run}");
    let parts = &cells[1]["outputs"][0]["marimo_ui"];
    assert_eq!(parts[0], json!({ "markdown": "Pick " }), "{run}");
    let size = &parts[1]["element"];
    assert_eq!(
        (&size["kind"], &size["label"], &size["value"]),
        (&json!("slider"), &json!("Size"), &json!(3))
    );
    let loud = &parts[3]["element"];
    assert_eq!(
        (&loud["kind"], &loud["value"]),
        (&json!("checkbox"), &json!(false))
    );
    let set = |id: &Value, value: Value| json!({ "cmd": "notebook_set_ui", "path": path, "cell": 1, "element": id, "value": value });

    let changed = ok(set(&size["id"], json!(2)));
    assert_eq!(ran_cells(&changed), [2], "only the user re-runs: {changed}");
    assert_eq!(
        changed["result"]["cells"][0]["outputs"][0]["text"],
        "hihi\n"
    );
    let changed = ok(set(&loud["id"], json!(true)));
    assert_eq!(
        changed["result"]["cells"][0]["outputs"][0]["text"],
        "HIHI\n"
    );
    // The element's output shows the value it was set to.
    let status = ok(json!({ "cmd": "notebook_status", "path": path }));
    let parts = &status["result"]["cells"][1]["outputs"][0]["marimo_ui"];
    assert_eq!(
        (&parts[1]["element"]["value"], &parts[3]["element"]["value"]),
        (&json!(2), &json!(true))
    );
    let missing = headless.call(set(&json!("nope"), json!(1)));
    assert_eq!(missing["ok"], false, "{missing}");

    headless.call(json!({ "cmd": "notebook_close", "path": path }));
    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).ok();
}

/// Windows → WSL, run by hand (`cargo test --test headless -- --ignored
/// --nocapture notebooks_in_wsl`): a project in the distro named by
/// `NEXT_CODE_TEST_WSL_DISTRO` (default `Ubuntu`, which needs `uv` and the
/// Linux agent next to `next_code.exe`, see `scripts\build-wsl-agent.ps1`)
/// with a `.venv` holding ipykernel and marimo. Notebooks open without an
/// interpreter named (the project's `.venv` is found in the distro), run
/// in the notebook's Linux folder (Jupyter and marimo, `await`, a UI
/// element), save with LF line endings, and their kernels' ports are not
/// forwarded as the user's.
#[cfg(windows)]
#[test]
#[ignore]
fn headless_mode_runs_notebooks_in_wsl() {
    let distro = std::env::var("NEXT_CODE_TEST_WSL_DISTRO").unwrap_or_else(|_| "Ubuntu".into());
    let bash = |script: &str, input: &str| {
        let mut child = background_command("wsl.exe")
            .args(["-d", &distro, "--", "bash", "-lc", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{script}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let project = format!("/tmp/next_code_wsl_notebooks_{}", std::process::id());
    bash(
        &format!(
            "rm -rf {project} && mkdir -p {project}/nb && cd {project} && uv venv -q .venv && \
             uv pip install -q --python .venv/bin/python ipykernel marimo"
        ),
        "",
    );
    let ipynb = json!({
        "cells": [
            { "cell_type": "code", "execution_count": null, "id": "env", "metadata": {},
              "outputs": [], "source": "import os, sys\nprint(sys.platform, os.getcwd())\nsys.executable" },
            { "cell_type": "code", "execution_count": null, "id": "await", "metadata": {},
              "outputs": [], "source": "import asyncio\nawait asyncio.sleep(0.01, result='é ✓')" },
        ],
        "metadata": {}, "nbformat": 4, "nbformat_minor": 5,
    });
    bash(
        &format!("cat > {project}/nb/demo.ipynb"),
        &format!("{ipynb:#}\n"),
    );
    bash(
        &format!("cat > {project}/nb/reactive.py"),
        &MARIMO_ASYNC.replace(
            "\n\n\nif __name__",
            "\n\n\n@app.cell\ndef _():\n    import marimo as mo\n    n = mo.ui.slider(1, 10, value=3)\n    \
             n\n    return (n,)\n\n\n@app.cell\ndef _(n):\n    print(n.value * 2)\n    return\n\n\nif __name__",
        ),
    );

    let mut headless = Headless::launch(
        &[
            "--wsl".as_ref(),
            std::ffi::OsStr::new(&distro),
            std::ffi::OsStr::new(&project),
        ],
        &[],
    );
    headless.next(|line| line["event"] == "ready");
    let mut ok = |request: Value| {
        let reply = headless.call(request.clone());
        println!("{request}\n→ {reply}");
        assert_eq!(reply["ok"], true, "{request} → {reply}");
        reply
    };
    ok(json!({ "cmd": "notebook_open", "path": "nb/demo.ipynb" }));
    let run = ok(json!({ "cmd": "notebook_run", "path": "nb/demo.ipynb" }));
    let cells = run["result"]["cells"].as_array().unwrap();
    assert_eq!(
        cells[0]["outputs"][0]["text"],
        format!("linux {project}/nb\n"),
        "{run}"
    );
    assert_eq!(
        cells[0]["outputs"][1]["text_plain"],
        format!("'{project}/.venv/bin/python'")
    );
    assert_eq!(cells[1]["outputs"][0]["text_plain"], "'é ✓'");
    ok(json!({ "cmd": "notebook_save", "path": "nb/demo.ipynb" }));

    ok(json!({ "cmd": "notebook_open", "path": "nb/reactive.py" }));
    let run = ok(json!({ "cmd": "notebook_run", "path": "nb/reactive.py" }));
    let cells = run["result"]["cells"].as_array().unwrap();
    assert_eq!(cells[2]["outputs"][0]["text"], "2\n", "{run}");
    assert_eq!(cells[4]["outputs"][0]["text"], "6\n", "{run}");
    let slider = cells[3]["outputs"][0]["marimo_ui"][0]["element"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("{run}"))
        .to_owned();
    let set = ok(json!({
        "cmd": "notebook_set_ui", "path": "nb/reactive.py", "cell": 3, "element": slider, "value": 7,
    }));
    assert_eq!(ran_cells(&set), [4], "{set}");
    assert_eq!(set["result"]["cells"][0]["outputs"][0]["text"], "14\n");
    ok(
        json!({ "cmd": "notebook_edit", "path": "nb/reactive.py", "cell": 1, "source": "b = a + 2" }),
    );
    ok(json!({ "cmd": "notebook_save", "path": "nb/reactive.py" }));
    let ports = ok(json!({ "cmd": "ports_list" }));
    assert!(
        ports["result"]["forwarded"]
            .as_array()
            .is_none_or(|forwarded| forwarded.is_empty()),
        "kernel ports are not forwarded: {ports}"
    );
    ok(json!({ "cmd": "notebook_close", "path": "nb/demo.ipynb" }));
    ok(json!({ "cmd": "notebook_close", "path": "nb/reactive.py" }));
    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());

    let saved = bash(
        &format!("cat {project}/nb/demo.ipynb {project}/nb/reactive.py"),
        "",
    );
    assert!(!saved.contains('\r'), "LF line endings");
    assert!(
        saved.contains(&format!("\"linux {project}/nb\\n\"")),
        "outputs saved: {saved}"
    );
    assert!(saved.contains("    b = a + 2\n"), "{saved}");
    bash(
        &format!("{project}/.venv/bin/marimo check {project}/nb/reactive.py"),
        "",
    );
    // `[l]`: not this script's own command line.
    let kernels = bash(
        &format!("pgrep -f 'next_code-kerne[l]-' | wc -l; rm -rf {project}"),
        "",
    );
    assert_eq!(kernels.trim(), "0", "no kernel left running");
}

/// A workflow file through the language-server commands: next_code's
/// built-in workflow server (in-process) diagnoses, navigates, outlines
/// and renames it.
#[test]
fn headless_mode_serves_workflow_files_with_the_built_in_server() {
    let root = std::env::temp_dir().join(format!("next_code_wf_lsp_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(root.join(".next_code/workflows")).unwrap();
    // `\\?\C:\…` on Windows: the server's `file:///C:/…` must still match.
    let root = root.canonicalize().unwrap();
    let file = root.join(".next_code/workflows/dev.yaml");
    fs::write(
        &file,
        "steps:\n  db: {run: d}\n  api: {depends_on: [db, zz], run: 'a ${steps.db.status}'}\n",
    )
    .unwrap();
    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    let path = ".next_code/workflows/dev.yaml";
    let open = headless.call(json!({ "cmd": "open", "path": path, "timeout_ms": 30_000 }));
    assert_eq!(open["result"]["language"], "yaml", "{open}");
    assert_eq!(open["result"]["server"], "next_code workflows", "{open}");
    let diagnostics =
        headless.call(json!({ "cmd": "diagnostics", "path": path, "wait_ms": 5_000 }));
    let found = &diagnostics["result"][0]["diagnostics"];
    assert_eq!(found[0]["message"], "`api` depends on unknown step `zz`", "{diagnostics}");
    // From `db` in api's depends_on to db's key.
    let definition = headless.call(
        json!({ "cmd": "definition", "path": path, "line": 2, "character": 21 }),
    );
    assert_eq!(
        definition["result"][0]["range"]["start"],
        json!({ "line": 1, "character": 2 }),
        "{definition}"
    );
    let symbols = headless.call(json!({ "cmd": "document_symbols", "path": path }));
    let names: Vec<&str> = symbols["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|symbol| symbol["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["db", "api"], "{symbols}");
    let renamed = headless.call(json!({
        "cmd": "rename", "path": path, "line": 1, "character": 3, "new_name": "postgres"
    }));
    assert_eq!(renamed["result"]["applied"], true, "{renamed}");
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "steps:\n  postgres: {run: d}\n  api: {depends_on: [postgres, zz], run: 'a ${steps.postgres.status}'}\n"
    );
    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).ok();
}

/// Restart policies, HTTP readiness, health checks, `if` and env files
/// with real processes: a flaky command restarts until it gives up, a
/// step gets ready once a URL answers, a health check makes a step
/// unhealthy, a false `if` skips a step and what needs it, and a `.env`
/// reaches a command.
#[test]
fn headless_mode_restarts_probes_skips_and_reads_env_files() {
    let root = std::env::temp_dir().join(format!("next_code_wf_policies_{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    fs::create_dir_all(root.join(".next_code/workflows")).unwrap();
    let root = if cfg!(windows) {
        root
    } else {
        root.canonicalize().unwrap()
    };
    fs::write(root.join(".env"), "GREETING=hello-from-file\n").unwrap();
    // A server answering every request with 200.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut request = [0; 1024];
            let _ = std::io::Read::read(&mut stream, &mut request);
            let _ = std::io::Write::write_all(
                &mut stream,
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    });
    let greet = if cfg!(windows) {
        "echo $env:GREETING"
    } else {
        "echo $GREETING"
    };
    let text = format!(
        r#"name: Policies
inputs:
  mode: {{type: choice, options: [quick, full], default: quick}}
steps:
  flaky:
    run: exit 3
    restart: {{policy: on_failure, max: 2, backoff: 200ms}}
  web:
    ready: {{http: 'http://127.0.0.1:{port}/health'}}
  sick:
    check: exit 0
    healthcheck: {{test: exit 1, interval: 300ms, retries: 2}}
  full:
    if: inputs.mode == 'full'
    run: echo full
  after_full: {{depends_on: [full], run: echo never}}
  greet:
    project: {{local: {root}}}
    env_file: .env
    run: {greet}
"#,
        root = serde_json::to_string(&root).unwrap(),
    );
    fs::write(root.join(".next_code/workflows/policies.yaml"), text).unwrap();
    let mut headless = Headless::start(&root);
    headless.next(|line| line["event"] == "ready");
    let list = headless.call(json!({ "cmd": "workflows_list" }));
    // By name: on Windows the user's own workflows (in the real config
    // dir, which `XDG_CONFIG_HOME` doesn't move) are listed too.
    let listed = list["result"]
        .as_array()
        .and_then(|list| list.iter().find(|workflow| workflow["name"] == "Policies"))
        .unwrap_or_else(|| panic!("{list}"));
    assert_eq!(listed["error"], Value::Null, "{listed}");
    assert_eq!(
        listed["steps"][0]["policies"]["restart"]["policy"], "on_failure",
        "{listed}"
    );
    assert_eq!(
        listed["steps"][2]["policies"]["healthcheck"]["probe"]["test"], "exit 1",
        "{listed}"
    );
    assert_eq!(
        listed["steps"][1]["ready"]["http"]["url"],
        format!("http://127.0.0.1:{port}/health"),
        "{listed}"
    );
    let run = headless.call(json!({ "cmd": "workflow_run", "name": "Policies" }));
    assert_eq!(run["ok"], true, "{run}");
    let status = status_until(&mut headless, "policies.yaml", |status| {
        let steps = &status["steps"];
        steps[0]["status"] == "failed"
            && steps[1]["status"] == "ready"
            && steps[2]["status"] == "failed"
            && steps[5]["status"] == "ready"
    });
    let steps = status["steps"].as_array().unwrap();
    assert_eq!(steps[0]["reason"], "failed (exit code 3) (gave up after 2 restarts)", "{status}");
    assert_eq!(steps[0]["restarts"], 2);
    assert_eq!(steps[2]["reason"], "unhealthy: exit code 1", "{status}");
    assert_eq!(steps[3]["status"], "skipped");
    assert_eq!(steps[3]["reason"], "if inputs.mode == 'full'");
    assert_eq!(steps[4]["status"], "skipped");
    assert_eq!(steps[4]["reason"], "full was skipped");
    assert_eq!(steps[5]["last_line"], "hello-from-file", "{status}");
    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).ok();
}
