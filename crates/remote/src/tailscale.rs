//! The machines of this computer's tailnet, from `tailscale status --json`:
//! each is an SSH host (`ssh`), reached by its MagicDNS name where the
//! tailnet has MagicDNS, else by its Tailscale address.

use std::{
    io::{self, Read},
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

use crate::launch::{decode_output, no_window};

/// A stuck daemon must not keep a list waiting.
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);

/// Another machine of the tailnet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// Its machine name (`HostName`).
    pub name: String,
    /// What `ssh` reaches it by: its MagicDNS name, else its first
    /// Tailscale address.
    pub address: String,
    /// `linux`, `windows`, `macOS`, `android`…
    pub os: String,
    pub online: bool,
    /// It runs Tailscale SSH (it published SSH host keys).
    pub tailscale_ssh: bool,
}

/// Tailscale's CLI: on PATH, else where the Windows and macOS apps put it.
fn program() -> PathBuf {
    let installed = if cfg!(windows) {
        std::env::var_os("ProgramFiles")
            .map(|dir| PathBuf::from(dir).join(r"Tailscale\tailscale.exe"))
    } else if cfg!(target_os = "macos") {
        Some(PathBuf::from(
            "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        ))
    } else {
        None
    };
    installed
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("tailscale"))
}

/// The tailnet's other machines, online ones first, then by name. Fails
/// when Tailscale is not installed or not connected.
pub fn peers() -> io::Result<Vec<Peer>> {
    let mut command = Command::new(program());
    no_window(&mut command);
    command
        .args(["status", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|err| io::Error::new(err.kind(), format!("Tailscale is not available: {err}")))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > STATUS_TIMEOUT {
            child.kill().ok();
            child.wait().ok();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "`tailscale status` did not answer",
            ));
        }
        thread::sleep(Duration::from_millis(20));
    };
    let bytes = reader
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("reading `tailscale status` panicked")))?;
    // Stopped or logged out, it still prints the status (and fails).
    let text = decode_output(&bytes);
    match parse_status(&text) {
        Err(_) if !status.success() => Err(io::Error::other(format!(
            "`tailscale status` failed ({status})"
        ))),
        parsed => parsed,
    }
}

fn parse_status(text: &str) -> io::Result<Vec<Peer>> {
    let invalid = |why: String| io::Error::new(io::ErrorKind::InvalidData, why);
    let status: Value = serde_json::from_str(text)
        .map_err(|err| invalid(format!("`tailscale status --json`: {err}")))?;
    let state = status["BackendState"].as_str().unwrap_or_default();
    if state != "Running" {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            format!("Tailscale is not connected ({state})"),
        ));
    }
    let magic_dns = status["CurrentTailnet"]["MagicDNSEnabled"]
        .as_bool()
        .unwrap_or(false);
    let text_of = |value: &Value| value.as_str().unwrap_or_default().to_owned();
    let mut peers: Vec<Peer> = status["Peer"]
        .as_object()
        .into_iter()
        .flat_map(|peers| peers.values())
        .filter_map(|peer| {
            let dns_name = text_of(&peer["DNSName"]).trim_end_matches('.').to_owned();
            let addresses: Vec<&str> = peer["TailscaleIPs"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let address = if magic_dns && !dns_name.is_empty() {
                dns_name
            } else {
                // IPv4 first: shorter, and no brackets needed with a port.
                addresses
                    .iter()
                    .find(|address| !address.contains(':'))
                    .or(addresses.first())?
                    .to_string()
            };
            Some(Peer {
                name: text_of(&peer["HostName"]),
                address,
                os: text_of(&peer["OS"]),
                online: peer["Online"].as_bool().unwrap_or(false),
                tailscale_ssh: peer["sshHostKeys"]
                    .as_array()
                    .is_some_and(|keys| !keys.is_empty()),
            })
        })
        .collect();
    peers.sort_by(|a, b| {
        b.online
            .cmp(&a.online)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(peers)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{
        "BackendState": "Running",
        "CurrentTailnet": {"Name": "me@example.com", "MagicDNSSuffix": "tail1.ts.net", "MagicDNSEnabled": true},
        "Self": {"HostName": "here", "DNSName": "here.tail1.ts.net.", "OS": "linux"},
        "Peer": {
            "nodekey:1": {"HostName": "phone", "DNSName": "phone.tail1.ts.net.", "OS": "android",
                          "TailscaleIPs": ["100.1.1.1", "fd7a::1"], "Online": false},
            "nodekey:2": {"HostName": "Box", "DNSName": "box.tail1.ts.net.", "OS": "linux",
                          "TailscaleIPs": ["100.2.2.2"], "Online": true, "sshHostKeys": ["ssh-ed25519 AAAA"]},
            "nodekey:3": {"HostName": "nameless", "DNSName": "", "OS": "linux",
                          "TailscaleIPs": ["fd7a::3", "100.3.3.3"], "Online": true},
            "nodekey:4": {"HostName": "ghost", "DNSName": "", "OS": "linux", "TailscaleIPs": []}
        }
    }"#;

    #[test]
    fn peers_are_listed_online_first_by_their_magic_dns_names() {
        let peers = parse_status(STATUS).unwrap();
        let summary: Vec<_> = peers
            .iter()
            .map(|peer| (peer.name.as_str(), peer.address.as_str(), peer.online))
            .collect();
        assert_eq!(
            summary,
            [
                ("Box", "box.tail1.ts.net", true),
                ("nameless", "100.3.3.3", true),
                ("phone", "phone.tail1.ts.net", false),
            ]
        );
        assert!(peers[0].tailscale_ssh);
        assert!(!peers[1].tailscale_ssh);
        assert_eq!(peers[2].os, "android");
    }

    #[test]
    fn without_magic_dns_peers_are_reached_by_address() {
        let status = STATUS.replace("\"MagicDNSEnabled\": true", "\"MagicDNSEnabled\": false");
        let peers = parse_status(&status).unwrap();
        assert_eq!(peers[0].address, "100.2.2.2");
        assert_eq!(peers[2].address, "100.1.1.1");
    }

    #[test]
    #[ignore = "needs Tailscale installed and connected"]
    fn this_machines_tailnet_is_listed() {
        let peers = peers().unwrap();
        for peer in &peers {
            println!("{peer:?}");
        }
        assert!(peers.iter().all(|peer| !peer.address.is_empty()));
    }

    #[test]
    fn a_tailnet_that_is_not_connected_is_an_error() {
        let stopped = STATUS.replace("\"Running\"", "\"Stopped\"");
        let err = parse_status(&stopped).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotConnected);
        assert!(err.to_string().contains("Stopped"));
        assert!(parse_status("not json").is_err());
    }
}
