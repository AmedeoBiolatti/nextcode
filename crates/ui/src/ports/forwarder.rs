//! One forwarded port: a local listener on 127.0.0.1 whose connections are
//! each relayed through an agent tunnel, on background threads.

use std::{
    collections::HashMap,
    io,
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use parking_lot::Mutex;
use remote::{AgentClient, TunnelHandle};

/// Ports above the requested one tried before letting the system choose.
const NEXT_PORTS: u16 = 100;

/// Binds 127.0.0.1:`preferred`, else (unless `require`) the next free port
/// above it, else one the system picks (VS Code behaves the same way).
pub fn bind_local(preferred: u16, require: bool) -> io::Result<TcpListener> {
    let bind = |port: u16| TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
    match bind(preferred) {
        Ok(listener) => return Ok(listener),
        Err(err) if require => {
            return Err(io::Error::new(
                err.kind(),
                format!("local port {preferred} is not available: {err}"),
            ));
        }
        Err(_) => {}
    }
    let next = preferred.saturating_add(1);
    (next..=preferred.saturating_add(NEXT_PORTS))
        .filter(|&port| port != 0 && port > preferred)
        .find_map(|port| bind(port).ok())
        .map_or_else(|| bind(0), Ok)
}

struct State {
    stopped: AtomicBool,
    next_id: AtomicU64,
    /// Accepted connections by id: `None` while its tunnel connects.
    connections: Mutex<HashMap<u64, Option<TunnelHandle>>>,
    /// Called (on any thread) when connections open or close.
    changed: Box<dyn Fn() + Send + Sync>,
}

/// Stops listening and closes its connections when dropped.
pub struct Forwarder {
    local_port: u16,
    state: Arc<State>,
}

impl Forwarder {
    /// Listens on `listener` and relays each connection to `host:port` on
    /// the agent's machine.
    pub fn start(
        client: Arc<AgentClient>,
        host: String,
        port: u16,
        listener: TcpListener,
        changed: impl Fn() + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let local_port = listener.local_addr()?.port();
        let state = Arc::new(State {
            stopped: AtomicBool::new(false),
            next_id: AtomicU64::new(0),
            connections: Mutex::default(),
            changed: Box::new(changed),
        });
        let accepting = state.clone();
        thread::Builder::new()
            .name(format!("forward-{local_port}"))
            .spawn(move || {
                for stream in listener.incoming() {
                    if accepting.stopped.load(Ordering::Acquire) {
                        return;
                    }
                    match stream {
                        Ok(stream) => connect(&client, &host, port, stream, &accepting),
                        // E.g. a connection reset before it was accepted.
                        Err(_) => thread::sleep(Duration::from_millis(10)),
                    }
                }
            })?;
        Ok(Self { local_port, state })
    }

    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// Open connections (including those still connecting).
    pub fn connections(&self) -> usize {
        self.state.connections.lock().len()
    }
}

/// Opens a tunnel for an accepted connection on its own thread (connecting
/// may take seconds); a failed tunnel closes the connection.
fn connect(
    client: &Arc<AgentClient>,
    host: &str,
    port: u16,
    stream: TcpStream,
    state: &Arc<State>,
) {
    let id = state.next_id.fetch_add(1, Ordering::Relaxed);
    state.connections.lock().insert(id, None);
    (state.changed)();
    let (client, host, connection_state) = (client.clone(), host.to_owned(), state.clone());
    let spawned = thread::Builder::new()
        .name(format!("forward-connect-{port}"))
        .spawn(move || {
            let state = connection_state;
            let closed = {
                let state = state.clone();
                move |_error: Option<String>| {
                    let handle = state.connections.lock().remove(&id);
                    drop(handle);
                    (state.changed)();
                }
            };
            match client.tunnel(&host, port, stream, closed) {
                Ok(handle) => {
                    let mut connections = state.connections.lock();
                    // Unless it already closed (or forwarding stopped).
                    if let Some(slot) = connections.get_mut(&id) {
                        *slot = Some(handle);
                    }
                }
                Err(_) => {
                    state.connections.lock().remove(&id);
                    (state.changed)();
                }
            }
        });
    if spawned.is_err() {
        state.connections.lock().remove(&id);
        (state.changed)();
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.state.stopped.store(true, Ordering::Release);
        // Wakes the accept loop, which then drops the listener.
        TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, self.local_port)),
            Duration::from_secs(1),
        )
        .ok();
        let connections: Vec<_> = self.state.connections.lock().drain().collect();
        // Dropping the handles closes the tunnels.
        drop(connections);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        time::Instant,
    };

    /// An agent serving on pipes in this process.
    fn agent() -> Arc<AgentClient> {
        let (client_reader, agent_writer) = io::pipe().unwrap();
        let (agent_reader, client_writer) = io::pipe().unwrap();
        thread::spawn(move || next_code_agent::serve(agent_reader, agent_writer));
        let transport = remote::Transport {
            reader: Box::new(client_reader),
            writer: Box::new(client_writer),
            child: None,
            stderr: None,
        };
        let root = std::env::temp_dir();
        let (client, _) = AgentClient::connect(
            transport,
            "test agent",
            root.to_str().unwrap(),
            Duration::from_secs(10),
        )
        .unwrap();
        client
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn echo(stream: &mut TcpStream, text: &[u8]) {
        stream.write_all(text).unwrap();
        let mut back = vec![0; text.len()];
        stream.read_exact(&mut back).unwrap();
        assert_eq!(back, text);
    }

    #[test]
    fn connections_are_relayed_counted_and_closed_on_stop() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in server.incoming().flatten() {
                thread::spawn(move || {
                    io::copy(&mut &stream, &mut &stream).ok();
                });
            }
        });
        let changes = Arc::new(AtomicU64::new(0));
        let counted = changes.clone();
        let forwarder = Forwarder::start(
            agent(),
            "localhost".into(),
            port,
            bind_local(0, false).unwrap(),
            move || {
                counted.fetch_add(1, Ordering::Relaxed);
            },
        )
        .unwrap();
        let local = forwarder.local_port();
        let mut first = TcpStream::connect(("127.0.0.1", local)).unwrap();
        echo(&mut first, b"one");
        let mut second = TcpStream::connect(("127.0.0.1", local)).unwrap();
        echo(&mut second, b"two");
        wait_for("two connections", || forwarder.connections() == 2);
        drop(second);
        wait_for("the closed connection", || forwarder.connections() == 1);
        assert!(changes.load(Ordering::Relaxed) >= 3);

        drop(forwarder);
        first
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut buffer = [0; 8];
        assert!(matches!(first.read(&mut buffer), Ok(0) | Err(_)));
        wait_for("the listener to close", || {
            TcpStream::connect(("127.0.0.1", local)).is_err()
        });
    }

    #[test]
    fn a_taken_port_moves_to_the_next_free_one_unless_required() {
        let taken = bind_local(0, false).unwrap();
        let port = taken.local_addr().unwrap().port();
        let next = bind_local(port, false).unwrap();
        let chosen = next.local_addr().unwrap().port();
        assert_ne!(chosen, port);
        assert!(
            chosen > port || port > u16::MAX - NEXT_PORTS,
            "{chosen} after {port}"
        );
        let err = bind_local(port, true).unwrap_err();
        assert!(err.to_string().contains(&port.to_string()), "{err}");
        drop(taken);
        assert_eq!(
            bind_local(port, true).unwrap().local_addr().unwrap().port(),
            port
        );
    }
}
