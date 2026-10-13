//! Editor side of remote projects: a client for `next_code_agent` and the
//! bootstraps that start it inside WSL or a container.

mod client;
pub mod container;
pub mod launch;
pub mod wsl;

pub use client::{
    AgentClient, Initialized, REQUEST_TIMEOUT, RunOutput, StreamHandle, Transport, agent_path,
};
pub use launch::Connection;
