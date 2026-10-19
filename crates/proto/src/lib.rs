//! Wire protocol between the editor and `next_code_agent`, the headless
//! process that performs project operations where the files live (inside a
//! WSL distro).
//!
//! # Transport
//!
//! The agent's stdin carries client → agent traffic and its stdout carries
//! agent → client traffic; nothing listens on a network port.
//!
//! 1. The agent first writes one text line, the [banner]:
//!    `NEXT_CODE_AGENT <protocol> <agent version>\n`. Clients skip anything
//!    printed before it (login scripts may write to stdout) and refuse to go
//!    on if the protocol differs from [`PROTOCOL_VERSION`].
//! 2. After the banner, both directions carry frames: a little-endian `u32`
//!    length followed by a postcard-encoded [`ClientMessage`] or
//!    [`AgentMessage`].
//! 3. The first request must be [`Request::Initialize`]; the agent rejects
//!    clients speaking another protocol version there too.
//!
//! Every request has a client-chosen id and gets exactly one
//! [`AgentMessage::Response`]. Streaming requests ([`Request::Watch`],
//! [`Request::Search`], [`Request::Spawn`], [`Request::Tunnel`]) then send
//! [`AgentMessage::Event`]s with the same id until a final event
//! ([`Event::is_final`]) or until the client sends [`ClientMessage::Cancel`].
//!
//! # Tunnels
//!
//! [`Request::Tunnel`] opens a TCP connection from the agent's machine (port
//! forwarding). Bytes flow as [`ClientMessage::Input`] (client → target) and
//! [`Event::TunnelData`] (target → client), each direction under a credit
//! window of [`TUNNEL_WINDOW`] bytes: a sender may have at most that many
//! bytes unacknowledged, and the receiver grants more as it delivers them
//! ([`ClientMessage::Credit`], [`Event::TunnelCredit`]). So a fast transfer
//! never queues more than the window per tunnel, and the shared connection
//! keeps serving other requests (no head-of-line blocking: neither side's
//! reader ever waits for a slow socket). [`ClientMessage::CloseInput`] and
//! [`Event::TunnelEof`] are half-closes; [`Event::TunnelClosed`] is final.
//!
//! # Pinpoint
//!
//! [`Request::Pinpoint`] is the one stream the agent drives: while it runs,
//! `next_code_agent pinpoint …` processes on the agent's machine (CLI agents
//! inside WSL or a container) reach the editor through it. Each of their
//! requests arrives as an [`Event::Pinpoint`] with an agent-chosen `call`
//! number, and the client answers it with [`ClientMessage::PinpointReply`].
//! Both carry JSON text (the pinpoint control commands), opaque to the
//! protocol.
//!
//! # Host services
//!
//! [`Request::HostServices`] is the other stream the agent drives: the
//! agent's machine borrows the editor's git credentials and ssh-agent (as
//! VS Code's dev containers do). Each use arrives as an [`Event::HostCall`]
//! with an agent-chosen `call` number, answered with
//! [`ClientMessage::HostReply`]; [`Event::SshAgentClosed`] tells that a
//! forwarded ssh-agent connection ended.
//!
//! Shutdown is closing the agent's stdin: the agent stops its watchers and
//! searches, terminates the processes it started, and exits.

pub mod credential;

use std::io::{self, Read, Write};

pub use search::{SearchEvent, SearchLimits, SearchQuery};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
pub use watcher::Batch;
pub use worktree::{DirEntry, FileKind};

/// Bumped on every incompatible change to the messages below.
pub const PROTOCOL_VERSION: u32 = 7;

/// Bytes a tunnel's sender may have in flight in each direction before the
/// receiver grants more (see the crate docs).
pub const TUNNEL_WINDOW: u32 = 1024 * 1024;

pub const BANNER_PREFIX: &str = "NEXT_CODE_AGENT";

/// Frames larger than this are treated as corruption.
pub const MAX_FRAME_LEN: usize = 256 * 1024 * 1024;

/// The first line the agent writes, before any frame.
pub fn banner() -> String {
    format!(
        "{BANNER_PREFIX} {PROTOCOL_VERSION} {}\n",
        env!("CARGO_PKG_VERSION")
    )
}

/// What an agent announced in its banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Banner {
    pub protocol: u32,
    pub version: String,
}

/// Parses a banner line (without its line break); `None` for other text.
pub fn parse_banner(line: &str) -> Option<Banner> {
    let mut words = line.trim_end_matches('\r').split(' ');
    if words.next()? != BANNER_PREFIX {
        return None;
    }
    let protocol = words.next()?.parse().ok()?;
    let version = words.next()?.to_owned();
    Some(Banner { protocol, version })
}

pub type RequestId = u64;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMessage {
    Request {
        id: RequestId,
        request: Request,
    },
    /// Ends a stream: stops a watch or search, or terminates a process
    /// (closing its stdin first and killing it if it does not exit soon).
    Cancel {
        id: RequestId,
    },
    /// Bytes for a spawned process's stdin or pty, or for a tunnel's target.
    Input {
        id: RequestId,
        data: Vec<u8>,
    },
    /// New size of a spawned process's pty.
    Resize {
        id: RequestId,
        size: PtySize,
    },
    /// Closes a spawned process's stdin (end of input; it keeps running), or
    /// a tunnel's sending direction (the target reads end of file).
    CloseInput {
        id: RequestId,
    },
    /// The client delivered this many bytes of a tunnel's data: the agent
    /// may send as many more.
    Credit {
        id: RequestId,
        bytes: u32,
    },
    /// The answer to the [`Event::Pinpoint`] numbered `call` of the
    /// [`Request::Pinpoint`] stream `id`: JSON text.
    PinpointReply {
        id: RequestId,
        call: u64,
        reply: String,
    },
    /// The answer to the [`Event::HostCall`] numbered `call` of the
    /// [`Request::HostServices`] stream `id`: the git credential text or the
    /// ssh-agent reply message, or why there is none.
    HostReply {
        id: RequestId,
        call: u64,
        reply: Result<Vec<u8>, String>,
    },
}

/// Paths are absolute POSIX paths on the agent's side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    /// Must come first. Fails unless `root` is an existing directory.
    Initialize {
        protocol: u32,
        root: String,
    },
    ReadDir {
        path: String,
    },
    /// Every non-ignored file under `root`, root-relative.
    ListFiles {
        root: String,
    },
    ReadFile {
        path: String,
    },
    WriteFile {
        path: String,
        data: Vec<u8>,
    },
    Stat {
        path: String,
    },
    FindExecutable {
        command: String,
        root: String,
    },
    PythonInterpreter {
        root: String,
    },
    /// Streams [`Event::Watch`] until cancelled.
    Watch {
        root: String,
        debounce_ms: u64,
    },
    /// Streams [`Event::Search`]; the final one is [`SearchEvent::Done`].
    Search {
        root: String,
        query: SearchQuery,
        limits: SearchLimits,
    },
    /// Streams output events; the final one is [`Event::Exited`].
    Spawn(SpawnRequest),
    /// Connects to `host:port` from the agent's machine and relays bytes
    /// both ways (see the crate docs); answers once connected. `localhost`
    /// and loopback addresses try both 127.0.0.1 and ::1, so servers bound
    /// to either family are reached. Streams [`Event::TunnelData`],
    /// [`Event::TunnelCredit`] and [`Event::TunnelEof`]; the final event is
    /// [`Event::TunnelClosed`].
    Tunnel {
        host: String,
        port: u16,
    },
    /// TCP sockets listening on the agent's machine ([`Response::Ports`];
    /// empty where `/proc/net/tcp` does not exist).
    ListeningPorts,
    /// Serves pinpoint requests of the agent's machine until cancelled (see
    /// the crate docs): streams [`Event::Pinpoint`]; never ends by itself.
    /// `roots` are the project's folders, which pick the editor serving a
    /// caller's working directory.
    Pinpoint {
        roots: Vec<String>,
    },
    /// Lends the editor's machine's git credentials and ssh-agent to the
    /// agent's machine until cancelled (see the crate docs): sets up git
    /// there, then streams [`Event::HostCall`] and
    /// [`Event::SshAgentClosed`]; never ends by itself.
    HostServices(HostServices),
}

/// What [`Request::HostServices`] lends. Everything off removes the git
/// credential helper an earlier stream configured.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostServices {
    /// Answer git credential requests (`next_code_agent git-credential`,
    /// configured as the user's `credential.helper` where none is set).
    pub git_credentials: bool,
    /// Serve an ssh-agent socket (`SSH_AUTH_SOCK` of the processes the
    /// agent starts) relayed to the editor's machine's ssh-agent.
    pub ssh_agent: bool,
    /// The editor's machine's `user.name` / `user.email`, set in the
    /// user's global git config where unset there.
    pub git_user_name: Option<String>,
    pub git_user_email: Option<String>,
}

/// Something the agent's machine asks of the editor's machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostRequest {
    /// A git credential helper call: `credential` is the credential
    /// protocol's text (see [`credential`]); `get` answers with one.
    GitCredential {
        operation: credential::Operation,
        credential: String,
    },
    /// One ssh-agent request message (without its length prefix) on the
    /// forwarded connection `connection`; answered with the reply message.
    SshAgent { connection: u64, message: Vec<u8> },
}

/// A listening TCP socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListeningPort {
    /// The bound address (`0.0.0.0`, `::`, `127.0.0.1`, …).
    pub address: std::net::IpAddr,
    pub port: u16,
    /// The owning process, when the agent may read its file descriptors.
    pub pid: Option<u32>,
    /// The owning process's command line.
    pub command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnRequest {
    /// `None` starts the user's login shell (only with a pty).
    pub program: Option<String>,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    /// Run in a pseudo-terminal of this size instead of with piped stdio.
    pub pty: Option<PtySize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtySize {
    pub columns: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Initialized {
        /// Canonical project root.
        root: String,
        home: String,
    },
    Entries(Vec<DirEntry>),
    Files(Vec<String>),
    Contents(Vec<u8>),
    Kind(Option<FileKind>),
    Path(Option<String>),
    Ports(Vec<ListeningPort>),
    /// The request succeeded and has nothing to return; for streaming
    /// requests, the stream started.
    Done,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AgentMessage {
    Response {
        id: RequestId,
        result: Result<Response, RemoteError>,
    },
    Event {
        id: RequestId,
        event: Event,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    Watch(Batch),
    Search(SearchEvent),
    /// Process output; a pty's output arrives as stdout.
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// The process exited (`None`: killed by a signal) after all its output.
    Exited {
        code: Option<i32>,
    },
    /// Bytes the tunnel's target sent.
    TunnelData(Vec<u8>),
    /// The agent wrote this many of the client's bytes to the target: the
    /// client may send as many more.
    TunnelCredit(u32),
    /// The target ended its output (half-close); the client may still send.
    TunnelEof,
    /// The tunnel is closed, after all its data; `error` when it broke.
    TunnelClosed {
        error: Option<String>,
    },
    /// A pinpoint request (JSON text), answered with
    /// [`ClientMessage::PinpointReply`] carrying the same `call`.
    Pinpoint {
        call: u64,
        request: String,
    },
    /// A use of the editor's machine, answered with
    /// [`ClientMessage::HostReply`] carrying the same `call`.
    HostCall {
        call: u64,
        request: HostRequest,
    },
    /// The forwarded ssh-agent connection `connection` ended.
    SshAgentClosed {
        connection: u64,
    },
}

impl Event {
    /// Whether this is the last event of its stream.
    pub fn is_final(&self) -> bool {
        matches!(
            self,
            Self::Exited { .. }
                | Self::Search(SearchEvent::Done { .. })
                | Self::TunnelClosed { .. }
        )
    }
}

/// An I/O error, transported with its kind so callers can react to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteError {
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorKind {
    NotFound,
    PermissionDenied,
    AlreadyExists,
    NotADirectory,
    IsADirectory,
    InvalidInput,
    InvalidData,
    Unsupported,
    Other,
}

impl From<&io::Error> for RemoteError {
    fn from(err: &io::Error) -> Self {
        use io::ErrorKind as Io;
        let kind = match err.kind() {
            Io::NotFound => ErrorKind::NotFound,
            Io::PermissionDenied => ErrorKind::PermissionDenied,
            Io::AlreadyExists => ErrorKind::AlreadyExists,
            Io::NotADirectory => ErrorKind::NotADirectory,
            Io::IsADirectory => ErrorKind::IsADirectory,
            Io::InvalidInput => ErrorKind::InvalidInput,
            Io::InvalidData => ErrorKind::InvalidData,
            Io::Unsupported => ErrorKind::Unsupported,
            _ => ErrorKind::Other,
        };
        Self {
            kind,
            message: err.to_string(),
        }
    }
}

impl From<RemoteError> for io::Error {
    fn from(err: RemoteError) -> Self {
        use io::ErrorKind as Io;
        let kind = match err.kind {
            ErrorKind::NotFound => Io::NotFound,
            ErrorKind::PermissionDenied => Io::PermissionDenied,
            ErrorKind::AlreadyExists => Io::AlreadyExists,
            ErrorKind::NotADirectory => Io::NotADirectory,
            ErrorKind::IsADirectory => Io::IsADirectory,
            ErrorKind::InvalidInput => Io::InvalidInput,
            ErrorKind::InvalidData => Io::InvalidData,
            ErrorKind::Unsupported => Io::Unsupported,
            ErrorKind::Other => Io::Other,
        };
        io::Error::new(kind, err.message)
    }
}

/// One message as a complete frame (length prefix included).
pub fn encode_frame(message: &impl Serialize) -> io::Result<Vec<u8>> {
    let mut frame = postcard::to_extend(message, vec![0; 4]).map_err(io::Error::other)?;
    let len = frame.len() - 4;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("message of {len} bytes exceeds the frame limit"),
        ));
    }
    frame[..4].copy_from_slice(&(len as u32).to_le_bytes());
    Ok(frame)
}

/// Writes one frame and flushes.
pub fn write_frame(writer: &mut impl Write, message: &impl Serialize) -> io::Result<()> {
    writer.write_all(&encode_frame(message)?)?;
    writer.flush()
}

/// Reads one frame; `None` when the stream ends cleanly between frames.
pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    let mut filled = 0;
    while filled < len.len() {
        match reader.read(&mut len[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes exceeds the limit"),
        ));
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body)?;
    postcard::from_bytes(&body)
        .map(Some)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banners_round_trip_and_reject_other_lines() {
        let line = banner();
        let parsed = parse_banner(line.trim_end_matches('\n')).unwrap();
        assert_eq!(parsed.protocol, PROTOCOL_VERSION);
        assert_eq!(parsed.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            parse_banner("NEXT_CODE_AGENT 7 0.9.0\r").map(|b| b.protocol),
            Some(7)
        );
        assert_eq!(parse_banner("Welcome to Ubuntu"), None);
        assert_eq!(parse_banner("NEXT_CODE_AGENT x 1"), None);
    }

    #[test]
    fn frames_round_trip_and_end_cleanly_only_between_frames() {
        let messages = [
            ClientMessage::Request {
                id: 1,
                request: Request::WriteFile {
                    path: "/home/me/é.txt".into(),
                    data: vec![0, 255, 10],
                },
            },
            ClientMessage::Resize {
                id: 2,
                size: PtySize {
                    columns: 120,
                    rows: 40,
                },
            },
            ClientMessage::Request {
                id: 3,
                request: Request::Tunnel {
                    host: "db".into(),
                    port: 5432,
                },
            },
            ClientMessage::Credit {
                id: 3,
                bytes: TUNNEL_WINDOW,
            },
            ClientMessage::Request {
                id: 4,
                request: Request::Pinpoint {
                    roots: vec!["/home/me/p".into()],
                },
            },
            ClientMessage::PinpointReply {
                id: 4,
                call: 9,
                reply: r#"{"ok":true}"#.into(),
            },
            ClientMessage::Request {
                id: 5,
                request: Request::HostServices(HostServices {
                    git_credentials: true,
                    ssh_agent: true,
                    git_user_name: Some("Ada".into()),
                    git_user_email: None,
                }),
            },
            ClientMessage::HostReply {
                id: 5,
                call: 1,
                reply: Err("no".into()),
            },
        ];
        let mut wire = Vec::new();
        for message in &messages {
            write_frame(&mut wire, message).unwrap();
        }
        let mut reader = wire.as_slice();
        for message in &messages {
            assert_eq!(
                read_frame::<ClientMessage>(&mut reader).unwrap().as_ref(),
                Some(message)
            );
        }
        assert_eq!(read_frame::<ClientMessage>(&mut reader).unwrap(), None);

        let truncated = &wire[..wire.len() - 1];
        let mut reader = truncated;
        for _ in 1..messages.len() {
            read_frame::<ClientMessage>(&mut reader).unwrap();
        }
        let err = read_frame::<ClientMessage>(&mut reader).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn oversized_frames_are_rejected_before_allocating() {
        let wire = u32::MAX.to_le_bytes();
        let err = read_frame::<AgentMessage>(&mut wire.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn io_errors_keep_their_kind_across_the_wire() {
        let original = io::Error::new(io::ErrorKind::NotFound, "no such file: /x");
        let back: io::Error = RemoteError::from(&original).into();
        assert_eq!(back.kind(), io::ErrorKind::NotFound);
        assert_eq!(back.to_string(), "no such file: /x");
    }
}
