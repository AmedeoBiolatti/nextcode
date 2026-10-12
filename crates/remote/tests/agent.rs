//! The client against a real agent (`next_code_agent::serve`) over pipes, and
//! against scripted agents for handshake and disconnect failures.

use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use proto::{
    AgentMessage, ClientMessage, ErrorKind, FileKind, PROTOCOL_VERSION, Request, Response,
    SearchEvent, SearchLimits, SearchQuery,
};
use remote::{AgentClient, Initialized, Transport};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("next_code_remote_{name}_{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn path_str(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// An agent serving on pipes; its thread ends when the client goes away.
fn start_agent() -> (Transport, thread::JoinHandle<io::Result<()>>) {
    let (client_reader, agent_writer) = io::pipe().unwrap();
    let (agent_reader, client_writer) = io::pipe().unwrap();
    let agent = thread::spawn(move || next_code_agent::serve(agent_reader, agent_writer));
    let transport = Transport {
        reader: Box::new(client_reader),
        writer: Box::new(client_writer),
        child: None,
        stderr: None,
    };
    (transport, agent)
}

fn connect(
    root: &Path,
) -> (
    Arc<AgentClient>,
    Initialized,
    thread::JoinHandle<io::Result<()>>,
) {
    let (transport, agent) = start_agent();
    let (client, info) = AgentClient::connect(
        transport,
        "test agent",
        path_str(root),
        Duration::from_secs(10),
    )
    .unwrap();
    (client, info, agent)
}

/// The stream's next item; `None` if it ended or nothing came for 10 s.
fn next<T>(stream: &mut futures::channel::mpsc::UnboundedReceiver<T>) -> Option<T> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match stream.try_recv() {
            Ok(item) => return Some(item),
            Err(err) if err.is_closed() => return None,
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Err(_) => return None,
        }
    }
}

#[test]
fn files_are_listed_read_written_and_stated_through_the_agent() {
    let root = temp_dir("files");
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join(".gitignore"), "*.log\n").unwrap();
    fs::write(root.join("debug.log"), "").unwrap();
    fs::write(root.join("src").join("main.rs"), "fn main() {}\n").unwrap();

    let (client, info, agent) = connect(&root);
    assert_eq!(info.root, path_str(&root));
    assert_eq!(info.agent_version, env!("CARGO_PKG_VERSION"));

    let entries = client.read_dir(path_str(&root)).unwrap();
    let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, ["src", ".gitignore"]);
    let main = Path::new("src").join("main.rs");
    assert_eq!(
        client.list_files(path_str(&root)).unwrap(),
        [".gitignore", path_str(&main)]
    );

    let file = root.join("src").join("new é.txt");
    client
        .write_file(path_str(&file), "héllo\r\n".as_bytes().to_vec())
        .unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "héllo\r\n");
    assert_eq!(
        client.read_file(path_str(&file)).unwrap(),
        "héllo\r\n".as_bytes()
    );
    assert_eq!(client.stat(path_str(&file)).unwrap(), Some(FileKind::File));
    assert_eq!(client.stat(path_str(&root)).unwrap(), Some(FileKind::Dir));
    assert_eq!(client.stat(path_str(&root.join("missing"))).unwrap(), None);

    let missing = client
        .read_file(path_str(&root.join("missing")))
        .unwrap_err();
    assert_eq!(missing.kind(), io::ErrorKind::NotFound);

    drop(client);
    agent.join().unwrap().unwrap();
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn opening_a_missing_root_fails_with_not_found() {
    let (transport, _agent) = start_agent();
    let missing = std::env::temp_dir().join("next_code_remote_missing_root");
    let err = AgentClient::connect(
        transport,
        "test agent",
        path_str(&missing),
        Duration::from_secs(10),
    )
    .err()
    .unwrap();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

#[test]
fn the_agent_requires_initialize_with_its_protocol_first() {
    let root = temp_dir("handshake");
    let (mut agent_out, agent_writer) = io::pipe().unwrap();
    let (agent_reader, mut agent_in) = io::pipe().unwrap();
    let agent = thread::spawn(move || next_code_agent::serve(agent_reader, agent_writer));

    let mut banner = Vec::new();
    let mut byte = [0];
    while byte[0] != b'\n' {
        agent_out.read_exact(&mut byte).unwrap();
        banner.push(byte[0]);
    }
    assert_eq!(String::from_utf8(banner).unwrap(), proto::banner());

    let mut ask = |id, request| {
        proto::write_frame(&mut agent_in, &ClientMessage::Request { id, request }).unwrap();
        match proto::read_frame::<AgentMessage>(&mut agent_out).unwrap() {
            Some(AgentMessage::Response {
                id: answered,
                result,
            }) if answered == id => result,
            other => panic!("unexpected {other:?}"),
        }
    };
    let early = ask(
        1,
        Request::Stat {
            path: path_str(&root).into(),
        },
    )
    .unwrap_err();
    assert_eq!(early.kind, ErrorKind::InvalidInput);
    let wrong = ask(
        2,
        Request::Initialize {
            protocol: PROTOCOL_VERSION + 1,
            root: path_str(&root).into(),
        },
    )
    .unwrap_err();
    assert_eq!(wrong.kind, ErrorKind::Unsupported);
    assert!(matches!(
        ask(
            3,
            Request::Initialize {
                protocol: PROTOCOL_VERSION,
                root: path_str(&root).into(),
            }
        ),
        Ok(Response::Initialized { .. })
    ));
    assert_eq!(
        ask(
            4,
            Request::Stat {
                path: path_str(&root).into()
            }
        ),
        Ok(Response::Kind(Some(FileKind::Dir)))
    );

    drop(agent_in);
    agent.join().unwrap().unwrap();
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn watching_reports_files_created_on_the_agent_side() {
    let root = temp_dir("watch");
    let (client, _, _agent) = connect(&root);
    let (_watch, mut batches) = client
        .watch(path_str(&root), Duration::from_millis(50))
        .unwrap();
    let file = root.join("created.txt");
    fs::write(&file, "x").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = false;
    while !seen && Instant::now() < deadline {
        let batch = next(&mut batches).expect("no watch batch");
        seen = batch.events.iter().any(|event| event.path == file);
    }
    assert!(seen, "creation of {} was not reported", file.display());
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn searches_stream_matches_and_finish_with_done() {
    let root = temp_dir("search");
    fs::write(root.join("a.txt"), "alpha\nneedle here\n").unwrap();
    fs::write(root.join("b.txt"), "nothing\n").unwrap();
    let (client, _, _agent) = connect(&root);
    let query = SearchQuery {
        pattern: "needle".into(),
        ..SearchQuery::default()
    };
    let (_search, mut events) = client
        .search(path_str(&root), &query, SearchLimits::default())
        .unwrap();
    let mut files = Vec::new();
    loop {
        match next(&mut events).expect("search stream ended without Done") {
            SearchEvent::File(file) => files.push(file),
            SearchEvent::Done { matches, .. } => {
                assert_eq!(matches, 1);
                break;
            }
        }
    }
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, root.join("a.txt"));
    assert_eq!(files[0].lines[0].line, 1);
    assert!(next(&mut events).is_none(), "events after Done");

    let bad = SearchQuery {
        pattern: "(".into(),
        regex: true,
        ..SearchQuery::default()
    };
    let err = client
        .search(path_str(&root), &bad, SearchLimits::default())
        .err()
        .unwrap();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn scripted_agent_with_another_protocol_is_refused() {
    let (client_reader, mut agent_writer) = io::pipe().unwrap();
    let (_agent_reader, client_writer) = io::pipe().unwrap();
    // Login scripts may print before the agent starts.
    write!(
        agent_writer,
        "Welcome to Linux\nNEXT_CODE_AGENT 999 9.9.9\n"
    )
    .unwrap();
    let transport = Transport {
        reader: Box::new(client_reader),
        writer: Box::new(client_writer),
        child: None,
        stderr: None,
    };
    let err = AgentClient::connect(transport, "test agent", "/", Duration::from_secs(10))
        .err()
        .unwrap();
    assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    assert!(err.to_string().contains("protocol 999"), "{err}");
}

#[test]
fn a_dying_agent_fails_requests_and_reports_the_disconnect() {
    let (client_reader, mut agent_writer) = io::pipe().unwrap();
    let (mut agent_reader, client_writer) = io::pipe().unwrap();
    let agent = thread::spawn(move || {
        agent_writer.write_all(proto::banner().as_bytes()).unwrap();
        let Some(ClientMessage::Request { id, .. }) = proto::read_frame(&mut agent_reader).unwrap()
        else {
            panic!("expected Initialize");
        };
        let initialized = AgentMessage::Response {
            id,
            result: Ok(Response::Initialized {
                root: "/p".into(),
                home: "/home/me".into(),
            }),
        };
        proto::write_frame(&mut agent_writer, &initialized).unwrap();
        // Read one more request, then die without answering it.
        proto::read_frame::<ClientMessage>(&mut agent_reader).unwrap();
    });
    let transport = Transport {
        reader: Box::new(client_reader),
        writer: Box::new(client_writer),
        child: None,
        stderr: None,
    };
    let (client, _) =
        AgentClient::connect(transport, "test agent", "/p", Duration::from_secs(10)).unwrap();
    let mut disconnects = client.disconnects();

    let err = client.stat("/p/file").unwrap_err();
    agent.join().unwrap();
    assert_eq!(err.kind(), io::ErrorKind::ConnectionAborted);
    assert!(err.to_string().contains("test agent disconnected"), "{err}");
    let reason = next(&mut disconnects).expect("no disconnect notice");
    assert!(reason.contains("test agent disconnected"), "{reason}");
    assert_eq!(
        client.read_dir("/p").unwrap_err().kind(),
        io::ErrorKind::ConnectionAborted
    );
}

#[cfg(target_os = "linux")]
mod processes {
    use std::sync::mpsc;

    use proto::Event;

    use super::*;

    fn collect_until_exit(events: &mpsc::Receiver<Event>) -> (Vec<u8>, Vec<u8>, Option<i32>) {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        loop {
            match events
                .recv_timeout(Duration::from_secs(10))
                .expect("no exit")
            {
                Event::Stdout(data) => stdout.extend(data),
                Event::Stderr(data) => stderr.extend(data),
                Event::Exited { code } => return (stdout, stderr, code),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    fn spawn(
        client: &Arc<AgentClient>,
        request: proto::SpawnRequest,
    ) -> (remote::StreamHandle, mpsc::Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        let handle = client
            .start_stream(Request::Spawn(request), move |event| {
                tx.send(event).ok();
            })
            .unwrap();
        (handle, rx)
    }

    #[test]
    fn piped_processes_take_input_and_report_output_and_exit_code() {
        let root = temp_dir("piped");
        let (client, _, _agent) = connect(&root);
        let (process, events) = spawn(
            &client,
            proto::SpawnRequest {
                program: Some("/bin/sh".into()),
                args: vec![
                    "-c".into(),
                    r#"read line; echo "got $line in $PWD"; echo oops >&2; exit 3"#.into(),
                ],
                cwd: path_str(&root).into(),
                env: Vec::new(),
                pty: None,
            },
        );
        process.input(b"hi\n".to_vec()).unwrap();
        let (stdout, stderr, code) = collect_until_exit(&events);
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            format!("got hi in {}\n", root.display())
        );
        assert_eq!(stderr, b"oops\n");
        assert_eq!(code, Some(3));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn language_server_traffic_crosses_the_agent_both_ways() {
        use lsp::{Incoming, types::request::Shutdown};

        let root = temp_dir("lsp");
        let (client, _, _agent) = connect(&root);
        // `cat` echoes the client's request back, so it arrives as a
        // server-initiated request with the same method.
        let (server, mut incoming) = client
            .spawn_language_server("cat", &[], path_str(&root), None)
            .unwrap();
        let pending = server.request::<Shutdown>(());
        match next(&mut incoming) {
            Some(Incoming::Request { method, .. }) => assert_eq!(method, "shutdown"),
            other => panic!("unexpected {other:?}"),
        }
        drop(pending);
        drop(server);
        // Cancelling closes cat's stdin, so it exits and the stream ends
        // (after echoing the `$/cancelRequest` sent for the dropped request).
        let mut last = None;
        while let Some(message) = next(&mut incoming) {
            last = Some(message);
        }
        assert_eq!(last, Some(Incoming::Exited));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn shells_run_in_a_pty_in_the_requested_directory() {
        let root = temp_dir("pty");
        let (client, _, _agent) = connect(&root);
        let (shell, events) = spawn(
            &client,
            proto::SpawnRequest {
                program: Some("/bin/sh".into()),
                args: Vec::new(),
                cwd: path_str(&root).into(),
                env: vec![("PS1".into(), "$ ".into())],
                pty: Some(proto::PtySize {
                    columns: 80,
                    rows: 24,
                }),
            },
        );
        shell
            .resize(proto::PtySize {
                columns: 100,
                rows: 30,
            })
            .unwrap();
        shell
            .input(b"[ -t 0 ] && echo tty''yes; stty size; pwd; exit 7\r".to_vec())
            .unwrap();
        let (stdout, _, code) = collect_until_exit(&events);
        let output = String::from_utf8_lossy(&stdout);
        assert!(output.contains("ttyyes"), "{output}");
        assert!(output.contains("30 100"), "{output}");
        assert!(output.contains(&root.display().to_string()), "{output}");
        assert_eq!(code, Some(7));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn closing_the_connection_stops_the_agent_and_its_processes() {
        let root = temp_dir("shutdown");
        let (client, _, agent) = connect(&root);
        let (process, events) = spawn(
            &client,
            proto::SpawnRequest {
                program: Some("/bin/sh".into()),
                args: vec!["-c".into(), "echo $$; exec sleep 60".into()],
                cwd: path_str(&root).into(),
                env: Vec::new(),
                pty: None,
            },
        );
        let Ok(Event::Stdout(pid)) = events.recv_timeout(Duration::from_secs(10)) else {
            panic!("no pid");
        };
        let pid = String::from_utf8(pid).unwrap().trim().to_owned();
        let proc_dir = PathBuf::from(format!("/proc/{pid}"));
        assert!(proc_dir.exists());

        drop(process);
        drop(client);
        agent.join().unwrap().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let gone =
            || fs::read_to_string(proc_dir.join("stat")).map_or(true, |stat| stat.contains(") Z "));
        while !gone() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(gone(), "process {pid} survived the agent's shutdown");
        fs::remove_dir_all(&root).unwrap();
    }
}
