//! TCP tunnels: connections the agent opens on its machine for the client
//! (port forwarding), relayed under the protocol's credit windows.

use std::{
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs},
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

use parking_lot::{Condvar, Mutex};
use proto::{Event, PtySize, TUNNEL_WINDOW};

use crate::{EventSink, process::Process};

/// Per address tried.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest data event.
const CHUNK: usize = 64 * 1024;

/// The addresses tried for `host:port`, in order: what `host` resolves to,
/// then, for `localhost` and loopback/unspecified addresses, both 127.0.0.1
/// and ::1 (a server may listen on only one of them, as VS Code's forwarder
/// also allows for).
pub(crate) fn candidates(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    let mut addresses: Vec<SocketAddr> = (host, port).to_socket_addrs()?.collect();
    let local = host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified());
    if local {
        for ip in [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ] {
            let address = SocketAddr::new(ip, port);
            if !addresses.contains(&address) {
                addresses.push(address);
            }
        }
    }
    Ok(addresses)
}

fn connect(host: &str, port: u16) -> io::Result<TcpStream> {
    let addresses = candidates(host.trim_matches(['[', ']']), port)
        .map_err(|err| io::Error::new(err.kind(), format!("cannot resolve {host}: {err}")))?;
    let mut last = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(err) => last = Some(err),
        }
    }
    let err = last.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no address"));
    Err(io::Error::new(
        err.kind(),
        format!("cannot connect to {host}:{port}: {err}"),
    ))
}

pub(crate) struct Tunnel {
    /// One handle shared by both relay threads (`&TcpStream` reads and
    /// writes). Never `try_clone`d: on Windows a socket duplicated right
    /// after a non-blocking connect can consider itself unconnected, and its
    /// `shutdown` then fails (WSAENOTCONN), losing the half-close.
    socket: Arc<TcpStream>,
    /// Client bytes for the writer thread; dropping it half-closes.
    input: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    /// Bytes the agent may still send to the client; `None` once closed.
    credit: Mutex<Option<u64>>,
    credit_changed: Condvar,
}

impl Tunnel {
    /// Waits for credit; the most to read next, `None` once closed.
    fn wait_credit(&self) -> Option<usize> {
        let mut credit = self.credit.lock();
        loop {
            match *credit {
                None => return None,
                Some(0) => self.credit_changed.wait(&mut credit),
                Some(bytes) => return Some(bytes.min(CHUNK as u64) as usize),
            }
        }
    }

    fn spend(&self, bytes: usize) {
        if let Some(credit) = &mut *self.credit.lock() {
            *credit = credit.saturating_sub(bytes as u64);
        }
    }

    fn close(&self) {
        self.input.lock().take();
        *self.credit.lock() = None;
        self.credit_changed.notify_all();
        self.socket.shutdown(Shutdown::Both).ok();
    }
}

/// Connects (blocking up to [`CONNECT_TIMEOUT`] per address) and starts
/// relaying; the final event comes once both directions ended.
pub(crate) fn open(host: &str, port: u16, sink: EventSink) -> io::Result<Arc<Tunnel>> {
    let socket = Arc::new(connect(host, port)?);
    socket.set_nodelay(true).ok();
    let (input, chunks) = mpsc::channel::<Vec<u8>>();
    let tunnel = Arc::new(Tunnel {
        socket: socket.clone(),
        input: Mutex::new(Some(input)),
        credit: Mutex::new(Some(u64::from(TUNNEL_WINDOW))),
        credit_changed: Condvar::new(),
    });
    let sink = Arc::new(sink);
    let error: Arc<Mutex<Option<String>>> = Arc::default();
    let fail = {
        let (tunnel, error) = (tunnel.clone(), error.clone());
        move |err: io::Error| {
            error.lock().get_or_insert_with(|| err.to_string());
            tunnel.close();
        }
    };

    let writer = {
        let (sink, fail, socket) = (sink.clone(), fail.clone(), socket.clone());
        thread::Builder::new()
            .name("agent-tunnel-out".into())
            .spawn(move || {
                for chunk in chunks {
                    if let Err(err) = (&*socket).write_all(&chunk) {
                        fail(err);
                        return;
                    }
                    sink.send(Event::TunnelCredit(chunk.len() as u32));
                }
                // The client half-closed (or the tunnel closed). A lost
                // half-close would leave the target waiting for ever.
                if let Err(err) = socket.shutdown(Shutdown::Write)
                    && err.kind() != io::ErrorKind::NotConnected
                {
                    fail(err);
                }
            })?
    };
    let reader = {
        let (sink, tunnel) = (sink.clone(), tunnel.clone());
        thread::Builder::new()
            .name("agent-tunnel-in".into())
            .spawn(move || {
                let mut buffer = vec![0; CHUNK];
                while let Some(allowed) = tunnel.wait_credit() {
                    match (&*socket).read(&mut buffer[..allowed]) {
                        Ok(0) => {
                            sink.send(Event::TunnelEof);
                            return;
                        }
                        Ok(n) => {
                            tunnel.spend(n);
                            sink.send(Event::TunnelData(buffer[..n].to_vec()));
                        }
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                        Err(err) => {
                            fail(err);
                            return;
                        }
                    }
                }
            })?
    };
    thread::Builder::new()
        .name("agent-tunnel".into())
        .spawn(move || {
            reader.join().ok();
            writer.join().ok();
            let error = error.lock().take();
            sink.finish(Event::TunnelClosed { error });
        })?;
    Ok(tunnel)
}

/// The client's side of the stream: input is bytes for the target.
impl Process for Tunnel {
    fn write(&self, data: Vec<u8>) {
        if let Some(input) = &*self.input.lock() {
            input.send(data).ok();
        }
    }

    fn close_input(&self) {
        self.input.lock().take();
    }

    fn resize(&self, _: PtySize) {}

    fn terminate(&self) {
        self.close();
    }

    fn kill(&self) {
        self.close();
    }

    fn credit(&self, bytes: u32) {
        if let Some(credit) = &mut *self.credit.lock() {
            *credit += u64::from(bytes);
            self.credit_changed.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_hosts_try_both_loopback_families() {
        let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, 80));
        let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, 80));
        for host in ["127.0.0.1", "0.0.0.0", "::1", "::"] {
            let found = candidates(host, 80).unwrap();
            assert!(
                found.contains(&v4) && found.contains(&v6),
                "{host}: {found:?}"
            );
        }
        assert_eq!(candidates("127.0.0.1", 80).unwrap()[0], v4);
        assert_eq!(
            candidates("10.1.2.3", 80).unwrap(),
            [SocketAddr::from(([10, 1, 2, 3], 80))]
        );
    }
}
