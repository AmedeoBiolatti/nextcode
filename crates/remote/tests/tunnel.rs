//! TCP tunnels (port forwarding) through an in-process agent over pipes:
//! echo with half-close, many concurrent connections, large transfers both
//! ways, flow control keeping other requests responsive, errors, and the
//! listening-port scan.

use std::{
    fs,
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use remote::{AgentClient, Transport, TunnelHandle};

fn connect(name: &str) -> (Arc<AgentClient>, PathBuf) {
    let root = std::env::temp_dir().join(format!("next_code_tunnel_{name}_{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let (client_reader, agent_writer) = io::pipe().unwrap();
    let (agent_reader, client_writer) = io::pipe().unwrap();
    thread::spawn(move || next_code_agent::serve(agent_reader, agent_writer));
    let transport = Transport {
        reader: Box::new(client_reader),
        writer: Box::new(client_writer),
        child: None,
        stderr: None,
    };
    let (client, _) = AgentClient::connect(
        transport,
        "test agent",
        root.to_str().unwrap(),
        Duration::from_secs(10),
    )
    .unwrap();
    (client, root)
}

/// A server on the agent's side: echoes each connection, then half-closes
/// after its client did.
fn echo_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                io::copy(&mut &stream, &mut &stream).ok();
                stream.shutdown(Shutdown::Write).ok();
            });
        }
    });
    port
}

/// A connected pair: the application's end and the end the tunnel serves
/// (what the editor's forwarder accepted).
fn pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let app = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (served, _) = listener.accept().unwrap();
    (app, served)
}

/// A tunnel to `host:port` and the application's end; the receiver gets
/// the tunnel's close.
fn open(
    client: &Arc<AgentClient>,
    host: &str,
    port: u16,
) -> (TcpStream, TunnelHandle, mpsc::Receiver<Option<String>>) {
    let (app, served) = pair();
    let (tx, closed) = mpsc::channel();
    let handle = client
        .tunnel(host, port, served, move |error| {
            tx.send(error).ok();
        })
        .unwrap();
    (app, handle, closed)
}

/// Deterministic bytes that differ between connections.
fn payload(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u32).wrapping_mul(31).wrapping_add(u32::from(seed)) as u8)
        .collect()
}

/// Sends `data` while reading the echo concurrently; the echo.
fn round_trip(app: TcpStream, data: Vec<u8>) -> Vec<u8> {
    thread::scope(|scope| {
        let app = &app;
        let sending = scope.spawn(move || {
            (&*app).write_all(&data).unwrap();
            app.shutdown(Shutdown::Write).unwrap();
        });
        let mut echoed = Vec::new();
        (&*app).read_to_end(&mut echoed).unwrap();
        sending.join().unwrap();
        echoed
    })
}

#[test]
fn echo_with_half_close_then_the_tunnel_reports_its_close() {
    let (client, root) = connect("echo");
    let port = echo_server();
    let (app, _handle, closed) = open(&client, "127.0.0.1", port);
    assert_eq!(round_trip(app, b"hello".to_vec()), b"hello");
    assert_eq!(closed.recv_timeout(Duration::from_secs(10)).unwrap(), None);
    fs::remove_dir_all(root).ok();
}

#[test]
fn many_concurrent_connections_keep_their_own_bytes() {
    let (client, root) = connect("many");
    let port = echo_server();
    let connections: Vec<_> = (0..40u8)
        .map(|seed| {
            let client = client.clone();
            thread::spawn(move || {
                let (app, handle, _closed) = open(&client, "127.0.0.1", port);
                let data = payload(seed, 200_000 + usize::from(seed) * 1000);
                assert!(round_trip(app, data.clone()) == data, "connection {seed}");
                drop(handle);
            })
        })
        .collect();
    for connection in connections {
        connection.join().unwrap();
    }
    fs::remove_dir_all(root).ok();
}

#[test]
fn twenty_megabytes_flow_both_ways_at_once() {
    let (client, root) = connect("large");
    let port = echo_server();
    let (app, _handle, closed) = open(&client, "127.0.0.1", port);
    let data = payload(7, 20 * 1024 * 1024);
    let started = Instant::now();
    let echoed = round_trip(app, data.clone());
    assert_eq!(echoed.len(), data.len());
    assert!(echoed == data, "the echo differs");
    assert_eq!(closed.recv_timeout(Duration::from_secs(10)).unwrap(), None);
    println!("20 MiB echoed in {:?}", started.elapsed());
    fs::remove_dir_all(root).ok();
}

/// A server flooding a connection nobody reads fills only the tunnel's
/// window: requests on the same agent connection still answer at once.
#[test]
fn a_stalled_download_does_not_block_other_requests() {
    let (client, root) = connect("stalled");
    fs::write(root.join("a.txt"), "still here").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sent_tx, sent) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let chunk = vec![b'x'; 64 * 1024];
        let mut total = 0usize;
        // Ends when the tunnel closes the connection.
        while stream.write_all(&chunk).is_ok() {
            total += chunk.len();
            sent_tx.send(total).ok();
        }
    });
    let (app, handle, closed) = open(&client, "127.0.0.1", port);
    // Let the server fill every buffer up to the window.
    thread::sleep(Duration::from_millis(500));
    let started = Instant::now();
    for _ in 0..20 {
        let text = client
            .read_file(root.join("a.txt").to_str().unwrap())
            .unwrap();
        assert_eq!(text, b"still here");
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "requests took {:?}",
        started.elapsed()
    );
    // The window bounds what left the server: socket buffers plus 1 MiB,
    // far below what an unbounded relay would have pulled by now.
    let total = sent.try_iter().last().unwrap_or_default();
    assert!(total < 32 * 1024 * 1024, "{total} bytes sent");
    // The relay is stuck writing to an application that never reads; on
    // Windows `shutdown` does not wake a blocked `send`, so its thread ends
    // when the application closes its end (Linux ends it at once).
    drop(handle);
    drop(app);
    closed.recv_timeout(Duration::from_secs(10)).unwrap();
    fs::remove_dir_all(root).ok();
}

#[test]
fn the_server_closing_first_ends_the_applications_stream() {
    let (client, root) = connect("server_close");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.write_all(b"HTTP/1.0 200 OK\r\n\r\nbye").unwrap();
    });
    let (mut app, _handle, closed) = open(&client, "127.0.0.1", port);
    let mut received = Vec::new();
    app.read_to_end(&mut received).unwrap();
    assert_eq!(received, b"HTTP/1.0 200 OK\r\n\r\nbye");
    // The application closing its side completes the tunnel.
    drop(app);
    closed.recv_timeout(Duration::from_secs(10)).unwrap();
    fs::remove_dir_all(root).ok();
}

#[test]
fn closing_the_handle_ends_both_sides() {
    let (client, root) = connect("close");
    let port = echo_server();
    let (mut app, handle, closed) = open(&client, "127.0.0.1", port);
    app.write_all(b"ping").unwrap();
    let mut buffer = [0; 4];
    app.read_exact(&mut buffer).unwrap();
    drop(handle);
    closed.recv_timeout(Duration::from_secs(10)).unwrap();
    app.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    // End of stream or a reset, never a hang.
    assert!(matches!(app.read(&mut buffer), Ok(0) | Err(_)));
    fs::remove_dir_all(root).ok();
}

#[test]
fn refused_connections_fail_the_request() {
    let (client, root) = connect("refused");
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let (_app, served) = pair();
    let err = client
        .tunnel("127.0.0.1", port, served, |_| {})
        .err()
        .unwrap();
    assert!(err.to_string().contains("cannot connect"), "{err}");
    fs::remove_dir_all(root).ok();
}

/// A server bound to IPv6 loopback only is still reached for 127.0.0.1.
#[test]
fn loopback_falls_back_to_the_other_family() {
    let Ok(listener) = TcpListener::bind("[::1]:0") else {
        println!("skipped: no IPv6 loopback");
        return;
    };
    let (client, root) = connect("v6");
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.write_all(b"v6").unwrap();
    });
    let (mut app, _handle, _closed) = open(&client, "127.0.0.1", port);
    let mut received = Vec::new();
    app.read_to_end(&mut received).unwrap();
    assert_eq!(received, b"v6");
    fs::remove_dir_all(root).ok();
}

#[test]
#[cfg(target_os = "linux")]
fn listening_ports_include_a_server_of_this_process() {
    let (client, root) = connect("ports");
    let port = echo_server();
    let ports = client.listening_ports().unwrap();
    let ours = ports
        .iter()
        .find(|entry| entry.port == port)
        .unwrap_or_else(|| panic!("{port} not in {ports:?}"));
    assert_eq!(ours.pid, Some(std::process::id()));
    fs::remove_dir_all(root).ok();
}
