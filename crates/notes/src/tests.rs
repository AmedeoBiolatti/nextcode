use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    sync::Arc,
    thread,
    time::Instant,
};

use super::*;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("next_code_notes_{name}_{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap();
    dir
}

const CODE: &str = "\
use std::io;

fn main() {
    let items = load();
    for item in items.clone() {
        println!(\"{item}\");
    }
}

fn load() -> Vec<String> {
    vec![]
}
";

fn anchored(start_line: usize, end_line: usize, state: AnchorState) -> Anchored {
    Anchored {
        start_line,
        end_line,
        state,
    }
}

#[test]
fn anchor_keeps_lines_and_context() {
    let anchor = anchor_from(CODE, 5, 7, None);
    assert_eq!(anchor.start_line, 5);
    assert_eq!(anchor.end_line, 7);
    assert_eq!(
        anchor.text,
        "    for item in items.clone() {\n        println!(\"{item}\");\n    }"
    );
    assert_eq!(anchor.before, ["fn main() {", "    let items = load();"]);
    assert_eq!(anchor.after, ["}", ""]);
    // Clamped to the text.
    let anchor = anchor_from(CODE, 40, 50, Some((1, 2)));
    assert_eq!((anchor.start_line, anchor.end_line), (12, 12));
    assert_eq!(anchor.start_column, Some(1));
    assert_eq!(anchor.after, Vec::<String>::new());
}

#[test]
fn reanchor_unchanged_is_exact() {
    let anchor = anchor_from(CODE, 5, 7, None);
    assert_eq!(reanchor(&anchor, CODE), anchored(5, 7, AnchorState::Exact));
    // Re-indented in place still counts.
    let indented = CODE.replace("    for item", "        for item");
    assert_eq!(
        reanchor(&anchor, &indented),
        anchored(5, 7, AnchorState::Exact)
    );
}

#[test]
fn reanchor_follows_lines_inserted_above() {
    let anchor = anchor_from(CODE, 5, 7, None);
    let edited = format!("// header\n// more\n\n{CODE}");
    assert_eq!(
        reanchor(&anchor, &edited),
        anchored(8, 10, AnchorState::Moved)
    );
    // And removed above.
    let edited = CODE.replacen("use std::io;\n\n", "", 1);
    assert_eq!(
        reanchor(&anchor, &edited),
        anchored(3, 5, AnchorState::Moved)
    );
}

#[test]
fn reanchor_finds_a_slightly_edited_snippet() {
    let anchor = anchor_from(CODE, 5, 5, None);
    let edited = format!(
        "// new first line\n{}",
        CODE.replace("for item in items.clone() {", "for item in items.iter() {")
    );
    assert_eq!(
        reanchor(&anchor, &edited),
        anchored(6, 6, AnchorState::Fuzzy)
    );
}

#[test]
fn reanchor_picks_the_nearest_duplicate() {
    let block = "fn helper() {\n    compute_the_thing(1, 2);\n}\n";
    let text = format!("{block}\n{block}\n{block}");
    // Line 6: the second copy's call.
    let anchor = anchor_from(&text, 6, 6, None);
    assert_eq!(anchor.text, "    compute_the_thing(1, 2);");
    // Three lines inserted at the top: the calls are at 5, 9 and 13; 5 is
    // nearest to the stored 6.
    let edited = format!("a\nb\nc\n{text}");
    assert_eq!(
        reanchor(&anchor, &edited),
        anchored(5, 5, AnchorState::Moved)
    );
    // One line inserted: copies at 3, 7, 11: 7 is nearest to 6.
    let edited = format!("a\n{text}");
    assert_eq!(
        reanchor(&anchor, &edited),
        anchored(7, 7, AnchorState::Moved)
    );
}

#[test]
fn reanchor_orphans_deleted_lines() {
    let anchor = anchor_from(CODE, 5, 7, None);
    let edited = "use std::io;\n\nfn main() {\n    run();\n}\n";
    assert_eq!(
        reanchor(&anchor, edited),
        anchored(5, 5, AnchorState::Orphaned)
    );
    assert_eq!(reanchor(&anchor, ""), anchored(1, 1, AnchorState::Orphaned));
}

#[test]
fn reanchor_handles_crlf() {
    let crlf = CODE.replace('\n', "\r\n");
    let anchor = anchor_from(&crlf, 5, 6, None);
    assert!(!anchor.text.contains('\r'));
    assert!(
        anchor
            .before
            .iter()
            .chain(&anchor.after)
            .all(|line| !line.contains('\r'))
    );
    assert_eq!(anchor, anchor_from(CODE, 5, 6, None));
    // Anchored on LF, read back from CRLF and the other way around.
    assert_eq!(reanchor(&anchor, &crlf), anchored(5, 6, AnchorState::Exact));
    let moved = format!("// x\r\n{crlf}");
    assert_eq!(
        reanchor(&anchor, &moved),
        anchored(6, 7, AnchorState::Moved)
    );
}

#[test]
fn reanchor_weak_snippet_uses_context() {
    // A lone `}` matches many lines: the context decides.
    let anchor = anchor_from(CODE, 8, 8, None);
    assert_eq!(anchor.text, "}");
    let edited = format!("fn other() {{\n}}\n\n{CODE}");
    assert_eq!(
        reanchor(&anchor, &edited),
        anchored(11, 11, AnchorState::Moved)
    );
}

#[test]
fn notes_round_trip_and_ops() {
    let mut notes = Notes::default();
    let anchor = anchor_from(CODE, 5, 5, None);
    let id = notes
        .add(
            "src/main.rs",
            None,
            anchor.clone(),
            "Why clone?",
            Author::user("me"),
            Some("agent".into()),
        )
        .id
        .clone();
    assert!(id.starts_with('n') && id.len() == 5, "{id}");
    notes
        .reply(&id, "To keep `items`.", Author::agent("claude"))
        .unwrap();
    notes
        .edit_message(&id, 1, "To keep `items` for later.")
        .unwrap();
    notes.resolve(&id, Author::user("me")).unwrap();
    let note = notes.get(&id).unwrap();
    assert!(!note.is_open());
    assert_eq!(note.thread.len(), 2);
    assert!(note.thread[1].edited.is_some());
    assert_eq!(note.text(), "Why clone?");
    let parsed = Notes::parse(&notes.to_json()).unwrap();
    assert_eq!(parsed, notes);
    assert!(notes.to_json().contains("\"status\": \"resolved\""));
    assert_eq!(notes.find(&id[..3]).unwrap().id, id);
    assert!(notes.reply("nope", "x", Author::user("me")).is_err());
    notes.reopen(&id).unwrap();
    assert!(notes.get(&id).unwrap().is_open());
    assert_eq!(notes.delete(&id).unwrap().id, id);
    assert!(notes.notes.is_empty());
    assert!(Notes::parse("").unwrap().notes.is_empty());
    assert!(Notes::parse("{not json").is_err());
    assert!(Notes::parse("{\"version\": 99, \"notes\": []}").is_err());
}

#[test]
fn addressing() {
    let agent = Author::agent("claude");
    assert!(agent.is_addressed_by(None));
    assert!(agent.is_addressed_by(Some("agent")));
    assert!(agent.is_addressed_by(Some("claude")));
    assert!(!agent.is_addressed_by(Some("user")));
}

fn add_note(store: &Store) -> String {
    store
        .add(
            "a.rs",
            None,
            anchor_from("x\n", 1, 1, None),
            "note",
            Author::user("me"),
            None,
        )
        .unwrap()
        .id
}

#[test]
fn store_writes_and_skips_unchanged() {
    let root = temp_dir("store");
    let store = Store::open(&root);
    assert!(store.load().unwrap().notes.is_empty());
    let id = add_note(&store);
    let path = root.join(NOTES_FILE);
    let written = fs::read_to_string(&path).unwrap();
    assert!(written.starts_with("{\n  \"version\": 1,"));
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    // Same anchor: no write.
    let anchor = store.load().unwrap().get(&id).unwrap().anchor.clone();
    thread::sleep(std::time::Duration::from_millis(20));
    store.update_anchor(&id, anchor).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    // An error writes nothing and releases the lock.
    assert!(store.reply("missing", "x", Author::agent("a")).is_err());
    assert!(!root.join(LOCK_FILE).exists());
    // Nothing but notes.json is left behind (no temp files).
    let names: Vec<String> = fs::read_dir(root.join(".next_code"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["notes.json"]);
    // A file that does not parse is never overwritten.
    fs::write(&path, "{broken").unwrap();
    assert!(store.reply(&id, "x", Author::agent("a")).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "{broken");
    fs::remove_dir_all(root).ok();
}

#[test]
fn concurrent_threads_lose_no_replies() {
    let root = temp_dir("threads");
    let id = add_note(&Store::open(&root));
    let writers: Vec<_> = (0..8)
        .map(|writer| {
            let (root, id) = (root.clone(), id.clone());
            thread::spawn(move || {
                // A store each, as separate processes would have.
                let store = Store::open(&root);
                for reply in 0..15 {
                    store
                        .reply(
                            &id,
                            &format!("{writer}/{reply}"),
                            Author::agent(format!("w{writer}")),
                        )
                        .unwrap();
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    let note = Store::open(&root).load().unwrap().get(&id).unwrap().clone();
    assert_eq!(note.thread.len(), 1 + 8 * 15);
    for writer in 0..8 {
        for reply in 0..15 {
            let text = format!("{writer}/{reply}");
            assert!(
                note.thread.iter().any(|message| message.text == text),
                "{text} lost"
            );
        }
    }
    fs::remove_dir_all(root).ok();
}

/// Child process for `concurrent_processes_lose_no_replies`.
#[test]
fn child_writer() {
    let (Ok(root), Ok(id), Ok(name)) = (
        std::env::var("NOTES_TEST_ROOT"),
        std::env::var("NOTES_TEST_ID"),
        std::env::var("NOTES_TEST_NAME"),
    ) else {
        return;
    };
    let store = Store::open(root);
    for reply in 0..20 {
        store
            .reply(&id, &format!("{name}/{reply}"), Author::agent(&name))
            .unwrap();
    }
}

#[test]
fn concurrent_processes_lose_no_replies() {
    let root = temp_dir("processes");
    let id = add_note(&Store::open(&root));
    let exe = std::env::current_exe().unwrap();
    let children: Vec<_> = (0..4)
        .map(|child| {
            Command::new(&exe)
                .args(["--exact", "tests::child_writer", "--test-threads=1"])
                .env("NOTES_TEST_ROOT", &root)
                .env("NOTES_TEST_ID", &id)
                .env("NOTES_TEST_NAME", format!("p{child}"))
                .stdout(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let note = Store::open(&root).load().unwrap().get(&id).unwrap().clone();
    assert_eq!(note.thread.len(), 1 + 4 * 20);
    fs::remove_dir_all(root).ok();
}

#[test]
fn stale_lock_is_taken_over() {
    let root = temp_dir("stale");
    let store = Store::open(&root);
    fs::create_dir_all(root.join(".next_code")).unwrap();
    // A writer that crashed a minute ago.
    let crashed = format!("4242 {}\n", now_ms() - 60_000);
    fs::write(root.join(LOCK_FILE), crashed).unwrap();
    let started = Instant::now();
    add_note(&store);
    assert!(started.elapsed().as_secs() < 2);
    assert!(!root.join(LOCK_FILE).exists());
    fs::remove_dir_all(root).ok();
}

#[test]
fn live_lock_is_waited_for() {
    let root = temp_dir("live");
    let store = Store::open(&root);
    fs::create_dir_all(root.join(".next_code")).unwrap();
    let lock = root.join(LOCK_FILE);
    fs::write(&lock, format!("4242 {}\n", now_ms())).unwrap();
    let releaser = {
        let lock = lock.clone();
        thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(300));
            fs::remove_file(lock).unwrap();
        })
    };
    let started = Instant::now();
    add_note(&store);
    assert!(started.elapsed().as_millis() >= 250);
    releaser.join().unwrap();
    fs::remove_dir_all(root).ok();
}

#[test]
fn exclude_is_added_once() {
    let root = temp_dir("exclude");
    fs::create_dir_all(root.join(".git/info")).unwrap();
    fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    // No final newline: the pattern still goes on a line of its own.
    fs::write(
        root.join(".git/info/exclude"),
        "# git ls-files --others\n*.log",
    )
    .unwrap();
    let store = Store::open(&root);
    add_note(&store);
    add_note(&store);
    store.ensure_excluded().unwrap();
    let exclude = fs::read_to_string(root.join(".git/info/exclude")).unwrap();
    assert!(
        exclude.starts_with("# git ls-files --others\n*.log\n"),
        "{exclude}"
    );
    assert_eq!(exclude.matches(EXCLUDE_PATTERN).count(), 1, "{exclude}");
    assert!(!root.join(".gitignore").exists());
    // Without `.git/info` yet.
    let fresh = temp_dir("exclude_fresh");
    fs::create_dir_all(fresh.join(".git")).unwrap();
    fs::write(fresh.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    add_note(&Store::open(&fresh));
    assert_eq!(
        fs::read_to_string(fresh.join(".git/info/exclude")).unwrap(),
        format!("# NextCode sticky notes (local, not committed)\n{EXCLUDE_PATTERN}\n")
    );
    // Not a repository: nothing.
    let plain = temp_dir("exclude_plain");
    add_note(&Store::open(&plain));
    assert!(!plain.join(".git").exists());
    for dir in [root, fresh, plain] {
        fs::remove_dir_all(dir).ok();
    }
}

/// The app reaches remote projects through its own [`NotesFs`].
#[test]
fn store_works_through_any_fs() {
    let root = temp_dir("custom_fs");
    let store = Store::with_fs(Arc::new(LocalFs::new(&root)));
    let id = add_note(&store);
    assert_eq!(
        Store::open(&root).load().unwrap().get(&id).unwrap().text(),
        "note"
    );
    fs::remove_dir_all(root).ok();
}
