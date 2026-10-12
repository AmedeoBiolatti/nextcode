//! Editor side of remote projects: a client for `next_code_agent` and the
//! bootstrap that starts it inside WSL.

mod client;
pub mod wsl;

pub use client::{
    AgentClient, Initialized, REQUEST_TIMEOUT, RunOutput, StreamHandle, Transport, agent_path,
};
