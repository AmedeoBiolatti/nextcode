//! `next_code --headless` end to end on a local project: the JSON-lines
//! contract scripts rely on (language servers are left out: they depend on
//! the machine).

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
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
        let mut child = Command::new(env!("CARGO_BIN_EXE_next_code"))
            .arg("--headless")
            .arg(root)
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
                .recv_timeout(Duration::from_secs(30))
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

    assert_eq!(headless.call(json!({ "cmd": "shutdown" }))["ok"], true);
    assert!(headless.child.wait().unwrap().success());
    fs::remove_dir_all(&root).unwrap();
}
