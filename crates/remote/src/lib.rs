//! Editor side of remote projects: a client for `next_code_agent` and the
//! bootstraps that start it inside WSL or a container.

mod client;
pub mod container;
pub mod launch;
pub mod wsl;

pub use client::{
    AgentClient, Initialized, PipedProcess, REQUEST_TIMEOUT, RunOutput, StreamHandle, Transport,
    TunnelHandle, agent_path,
};
pub use launch::Connection;
pub use proto::ListeningPort;
