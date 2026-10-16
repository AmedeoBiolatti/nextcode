use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use gpui::{Entity, TestAppContext};

use super::{actions::CommitAction, *};

/// Test tools are background processes too; keep them off the Windows desktop.
fn background_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    remote::launch::no_window(&mut command);
    command
}

fn git(dir: &Path, args: &[&str]) {
    let output = background_command("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

fn temp_repo(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("next_code_log_{name}_{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("user.name", "Test"),
        ("user.email", "test@example.com"),
        ("commit.gpgsign", "false"),
        ("core.autocrlf", "false"),
    ] {
        git(&dir, &["config", key, value]);
    }
    dir
}

fn commit(dir: &Path, file: &str, text: &str, message: &str) {
    std::fs::write(dir.join(file), text).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
}

/// main: a, b, merge of side (s), c (tagged v1); side: s.
fn branchy_repo(name: &str) -> PathBuf {
    let dir = temp_repo(name);
    commit(&dir, "a.txt", "a\n", "a");
    commit(&dir, "b.txt", "b\n", "b");
    git(&dir, &["switch", "-q", "-c", "side"]);
    commit(&dir, "s.txt", "s\n", "s");
    git(&dir, &["switch", "-q", "main"]);
    git(
        &dir,
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
    );
    commit(&dir, "c.txt", "c\n", "c");
    git(&dir, &["tag", "v1"]);
    dir
}

/// `count` linear commits on main, made at once with `git fast-import`.
fn long_repo(name: &str, count: usize) -> PathBuf {
    let dir = temp_repo(name);
    let mut stream = String::new();
    for n in 1..=count {
        let message = format!("commit {n}");
        stream.push_str(&format!(
            "commit refs/heads/main\nmark :{n}\ncommitter Test <test@example.com> {} +0000\ndata {}\n{message}\n",
            1_600_000_000 + n,
            message.len()
        ));
        if n > 1 {
            stream.push_str(&format!("from :{}\n", n - 1));
        }
        let content = format!("{n}\n");
        stream.push_str(&format!(
            "M 644 inline f.txt\ndata {}\n{content}\n",
            content.len()
        ));
    }
    let mut child = background_command("git")
        .arg("-C")
        .arg(&dir)
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stream.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
    git(&dir, &["reset", "-q", "--hard"]);
    dir
}

fn open(dir: &Path, cx: &mut TestAppContext) -> Entity<GitLogPanel> {
    let panel = cx.new(|cx| GitLogPanel::new(Project::local(dir.to_path_buf()), cx));
    panel.update(cx, |panel, cx| panel.start(cx));
    cx.run_until_parked();
    panel
}

fn subjects(panel: &GitLogPanel) -> Vec<&str> {
    panel.commits.iter().map(|c| c.subject.as_str()).collect()
}

fn changed_paths(panel: &GitLogPanel) -> Vec<String> {
    match &panel.details.as_ref().unwrap().changes {
        Some(Ok(span)) => span.changes.iter().map(|c| c.path.clone()).collect(),
        other => panic!(
            "changes not loaded: {:?}",
            other.as_ref().map(|r| r.is_ok())
        ),
    }
}

#[gpui::test]
fn the_log_lists_branches_and_details_of_the_first_commit(cx: &mut TestAppContext) {
    let dir = branchy_repo("details");
    let panel = open(&dir, cx);
    panel.read_with(cx, |panel, _| {
        assert_eq!(subjects(panel), ["c", "merge side", "s", "b", "a"]);
        assert_eq!(panel.next, None);
        assert_eq!(panel.commits[0].refs, ["HEAD -> main", "tag: v1"]);
        assert_eq!(panel.graph.iter().map(|row| row.width).max(), Some(2));
        assert_eq!(panel.selected, [0]);
        assert_eq!(changed_paths(panel), ["c.txt"]);
        let details = panel.details.as_ref().unwrap();
        assert_eq!(details.message.as_deref(), Some("c"));
        assert_eq!(details.branches.as_deref(), Some(&["main".to_owned()][..]));
        let labels: Vec<String> = panel
            .branch_rows
            .iter()
            .map(|row| match row {
                BranchRow::Head { label } => label.clone(),
                BranchRow::Group { label, .. } => format!("{label}/"),
                BranchRow::Ref { label, .. } => label.clone(),
            })
            .collect();
        // Tags start collapsed.
        assert_eq!(
            labels,
            [
                "HEAD (Current Branch: main)",
                "Local/",
                "main",
                "side",
                "Tags/"
            ]
        );
        assert_eq!(panel.me.as_deref(), Some("Test"));
    });
    std::fs::remove_dir_all(dir).ok();
}

#[gpui::test]
fn several_selected_commits_show_their_combined_changes(cx: &mut TestAppContext) {
    let dir = branchy_repo("multi");
    let panel = open(&dir, cx);
    panel.update(cx, |panel, cx| {
        // c, then Shift+click b: c, merge side, s, b.
        panel.click_commit(3, Modifiers::shift(), cx);
    });
    cx.run_until_parked();
    panel.read_with(cx, |panel, _| {
        assert_eq!(panel.selected, [0, 1, 2, 3]);
        // From a (b's parent) to c.
        assert_eq!(changed_paths(panel), ["b.txt", "c.txt", "s.txt"]);
        assert!(panel.details.as_ref().unwrap().message.is_none());
    });
    panel.update(cx, |panel, cx| {
        // Ctrl+click toggles: drop merge side and s.
        panel.click_commit(1, Modifiers::secondary_key(), cx);
        panel.click_commit(2, Modifiers::secondary_key(), cx);
    });
    cx.run_until_parked();
    panel.read_with(cx, |panel, _| {
        assert_eq!(panel.selected, [0, 3]);
        // Still from the oldest selected commit's parent to the newest.
        assert_eq!(changed_paths(panel), ["b.txt", "c.txt", "s.txt"]);
        assert!(
            !panel.enabled(CommitAction::CheckoutRevision),
            "single only"
        );
        assert!(panel.enabled(CommitAction::CherryPick));
    });
    panel.update(cx, |panel, cx| panel.click_commit(1, Modifiers::none(), cx));
    cx.run_until_parked();
    panel.read_with(cx, |panel, _| {
        assert_eq!(panel.selected, [1]);
        // A merge commit: against its first parent.
        assert_eq!(changed_paths(panel), ["s.txt"]);
        assert!(!panel.enabled(CommitAction::CherryPick), "merge");
        assert!(panel.enabled(CommitAction::Revert));
    });
    std::fs::remove_dir_all(dir).ok();
}

#[gpui::test]
fn branch_text_and_path_filters_reload_the_log(cx: &mut TestAppContext) {
    let dir = branchy_repo("filters");
    let panel = open(&dir, cx);
    panel.update(cx, |panel, cx| {
        let side = panel
            .branch_rows
            .iter()
            .position(|row| row.revision() == Some("side"))
            .unwrap();
        panel.selected_branch = Some(side);
        let revision = panel.branch_rows[side].revision().unwrap().to_owned();
        panel.set_filter(|filter| filter.revisions = vec![revision], cx);
    });
    cx.run_until_parked();
    panel.read_with(cx, |panel, _| assert_eq!(subjects(panel), ["s", "b", "a"]));

    panel.update(cx, |panel, cx| {
        panel.set_filter(
            |filter| {
                filter.revisions.clear();
                filter.text = "SIDE".into();
            },
            cx,
        )
    });
    cx.run_until_parked();
    panel.read_with(cx, |panel, _| {
        assert_eq!(subjects(panel), ["merge side"]);
        assert!(panel.graph.iter().all(|row| row.column == 0));
    });

    panel.update(cx, |panel, cx| {
        panel.set_filter(|filter| filter.text.clear(), cx);
        panel.show_path_history("b.txt".into(), cx);
    });
    cx.run_until_parked();
    panel.read_with(cx, |panel, cx| {
        assert_eq!(subjects(panel), ["b"]);
        assert_eq!(panel.path_input.read(cx).text(), "b.txt");
    });
    std::fs::remove_dir_all(dir).ok();
}

#[gpui::test]
fn long_histories_load_page_by_page(cx: &mut TestAppContext) {
    let dir = long_repo("pages", 2 * PAGE + 500);
    let panel = open(&dir, cx);
    panel.read_with(cx, |panel, _| {
        assert_eq!(panel.commits.len(), PAGE);
        assert_eq!(panel.next, Some(PAGE));
        assert_eq!(
            panel.commits[0].subject,
            format!("commit {}", 2 * PAGE + 500)
        );
    });
    for expected in [2 * PAGE, 2 * PAGE + 500] {
        panel.update(cx, |panel, cx| panel.load_more(cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.commits.len(), expected);
            assert_eq!(panel.graph.len(), expected);
        });
    }
    panel.read_with(cx, |panel, _| {
        assert_eq!(panel.next, None);
        assert_eq!(panel.commits.last().unwrap().subject, "commit 1");
    });
    std::fs::remove_dir_all(dir).ok();
}

#[gpui::test]
fn menu_operations_change_the_repository_and_report(cx: &mut TestAppContext) {
    let dir = branchy_repo("operations");
    let panel = open(&dir, cx);
    let changed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let events = changed.clone();
    cx.update(|cx| {
        cx.subscribe(&panel, move |_, event: &GitLogEvent, _| {
            if let GitLogEvent::Changed(message) = event {
                events.borrow_mut().push(message.clone());
            }
        })
        .detach()
    });
    panel.read_with(cx, |panel, _| {
        assert!(panel.enabled(CommitAction::UndoCommit), "HEAD, not pushed");
        assert!(panel.enabled(CommitAction::EditMessage));
    });
    panel.update(cx, |panel, cx| panel.run_action(CommitAction::Revert, cx));
    cx.run_until_parked();
    panel.update(cx, |panel, cx| panel.reload(cx));
    cx.run_until_parked();
    panel.read_with(cx, |panel, _| {
        assert_eq!(panel.commits[0].subject, "Revert \"c\"");
        assert_eq!(
            panel.report.as_ref().map(|(m, e)| (m.to_string(), *e)),
            Some(("Reverted 1 commit".into(), false))
        );
    });
    assert_eq!(changed.borrow().as_slice(), ["Reverted 1 commit"]);
    assert!(!dir.join("c.txt").exists());

    // Cherry-pick c (index 1 now) onto side.
    git(&dir, &["switch", "-q", "side"]);
    panel.update(cx, |panel, cx| {
        panel.reload(cx);
    });
    cx.run_until_parked();
    panel.update(cx, |panel, cx| {
        let c = panel.commits.iter().position(|c| c.subject == "c").unwrap();
        panel.click_commit(c, Modifiers::none(), cx);
        assert!(!panel.enabled(CommitAction::UndoCommit), "not HEAD");
        panel.run_action(CommitAction::CherryPick, cx);
    });
    cx.run_until_parked();
    assert!(dir.join("c.txt").exists());
    assert_eq!(changed.borrow().last().unwrap(), "Cherry-picked 1 commit");

    // Undo the cherry-pick, then edit HEAD's message and tag it.
    panel.update(cx, |panel, cx| panel.reload(cx));
    cx.run_until_parked();
    let head_index = |panel: &GitLogPanel| {
        let head = panel.head.as_ref().unwrap();
        panel.commits.iter().position(|c| c.id == head.id).unwrap()
    };
    panel.update(cx, |panel, cx| {
        let head = head_index(panel);
        assert_eq!(panel.commits[head].subject, "c");
        panel.click_commit(head, Modifiers::none(), cx);
        panel.run_action(CommitAction::UndoCommit, cx);
    });
    cx.run_until_parked();
    panel.update(cx, |panel, cx| panel.reload(cx));
    cx.run_until_parked();
    panel.update(cx, |panel, cx| {
        let head = head_index(panel);
        assert_eq!(
            panel.commits[head].subject, "s",
            "the cherry-pick is undone"
        );
        panel.reword_head("s, reworded".into(), cx);
    });
    cx.run_until_parked();
    panel.update(cx, |panel, cx| panel.reload(cx));
    cx.run_until_parked();
    panel.update(cx, |panel, cx| {
        let at = panel.head.as_ref().unwrap().id.clone();
        panel.create_tag("v2".into(), at, cx);
    });
    cx.run_until_parked();
    panel.update(cx, |panel, cx| panel.reload(cx));
    cx.run_until_parked();
    panel.read_with(cx, |panel, _| {
        let head = panel
            .commits
            .iter()
            .find(|c| c.refs.iter().any(|r| r == "HEAD -> side"));
        assert_eq!(head.map(|c| c.subject.as_str()), Some("s, reworded"));
        assert!(panel.refs.iter().any(|r| r.name == "v2"));
    });
    std::fs::remove_dir_all(dir).ok();
}
