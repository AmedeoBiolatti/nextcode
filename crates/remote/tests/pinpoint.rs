//! `next_code_agent pinpoint` calls relayed by a real agent (over pipes) to
//! a fake editor and back. Its own test binary: it sets the socket
//! directory in the environment before any thread starts.
#![cfg(unix)]

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use next_code_agent::pinpoint;
use parking_lot::Mutex;
use remote::{AgentClient, Transport};
use serde_json::{Value, json};

fn temp_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("next_code_pinpoint_{name}_{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn connect(root: &Path) -> Arc<AgentClient> {
    let (client_reader, agent_writer) = io::pipe().unwrap();
    let (agent_reader, client_writer) = io::pipe().unwrap();
    thread::spawn(move || next_code_agent::serve(agent_reader, agent_writer));
    let transport = Transport {
        reader: Box::new(client_reader),
        writer: Box::new(client_writer),
        child: None,
        stderr: None,
    };
    AgentClient::connect(
        transport,
        "test agent",
        root.to_str().unwrap(),
        Duration::from_secs(10),
    )
    .unwrap()
    .0
}

fn sockets(dir: &Path) -> usize {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "sock"))
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn calls_reach_the_editor_through_the_agent_and_get_its_reply() {
    let sockets_dir = temp_dir("sockets").join("run");
    // SAFETY: set before this binary's only test starts any thread.
    unsafe { std::env::set_var(pinpoint::DIR_ENV, &sockets_dir) };
    let root = temp_dir("project");
    fs::create_dir_all(root.join("src")).unwrap();
    let other = temp_dir("other");
    let client = connect(&root);
    let other_client = connect(&other);

    // The fake editor answers from another thread, as the UI thread would.
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let handler = |name: &'static str| {
        let seen = seen.clone();
        move |request: String, reply: remote::PinpointReply| {
            let request: Value = serde_json::from_str(&request).unwrap();
            seen.lock().push(request.clone());
            thread::spawn(move || match request["command"].as_str() {
                Some("drop") => drop(reply),
                _ => reply.send(json!({ "ok": true, "editor": name, "echo": request }).to_string()),
            });
        }
    };
    let root_str = root.to_str().unwrap().to_owned();
    let serving = client
        .serve_pinpoint(vec![root_str.clone()], handler("project"))
        .unwrap();
    let _other_serving = other_client
        .serve_pinpoint(vec![other.to_str().unwrap().into()], handler("other"))
        .unwrap();
    assert_eq!(sockets(&sockets_dir), 2);
    let mode =
        std::os::unix::fs::PermissionsExt::mode(&fs::metadata(&sockets_dir).unwrap().permissions());
    assert_eq!(mode & 0o777, 0o700);

    // The caller's working directory picks the editor of its project.
    let request = json!({
        "command": "highlight",
        "targets": [{ "file": format!("{root_str}/src/main.rs"), "line": 3 }],
    });
    let reply = pinpoint::call(&request, &root.join("src")).unwrap();
    assert_eq!(reply["editor"], "project");
    assert_eq!(reply["echo"], request);
    let reply = pinpoint::call(&json!({ "command": "list" }), &other).unwrap();
    assert_eq!(reply["editor"], "other");

    // An editor that drops a call still answers it.
    let reply = pinpoint::call(&json!({ "command": "drop" }), &root).unwrap();
    assert_eq!(reply["ok"], false);
    assert_eq!(seen.lock().len(), 3);

    // Dropping the stream stops serving: its socket goes away and calls
    // from its project fall back to the other editor.
    drop(serving);
    let deadline = Instant::now() + Duration::from_secs(10);
    while sockets(&sockets_dir) != 1 {
        assert!(Instant::now() < deadline, "the socket was not removed");
        thread::sleep(Duration::from_millis(10));
    }
    let reply = pinpoint::call(&json!({ "command": "list" }), &root).unwrap();
    assert_eq!(reply["editor"], "other");

    // A disconnected editor's agent removes its socket too.
    drop(_other_serving);
    drop(other_client);
    let deadline = Instant::now() + Duration::from_secs(10);
    while sockets(&sockets_dir) != 0 {
        assert!(Instant::now() < deadline, "the socket was not removed");
        thread::sleep(Duration::from_millis(10));
    }
    let err = pinpoint::call(&json!({ "command": "list" }), &root).unwrap_err();
    assert!(err.contains("no NextCode editor"), "{err}");
    drop(client);
    for dir in [&root, &other, &sockets_dir] {
        fs::remove_dir_all(dir).ok();
    }
}
