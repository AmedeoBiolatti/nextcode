//! Debugging through the Debug Adapter Protocol: what to debug
//! ([`DebugRequest`]), the adapters, sessions, breakpoints and their UI.

pub mod adapters;
pub mod console;
pub mod panel;
pub mod session;
pub mod store;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};

gpui::actions!(
    debugger,
    [
        StartDebugging,
        DebugCurrentFile,
        Continue,
        Pause,
        StepOver,
        StepInto,
        StepOut,
        Stop,
        Restart,
        ToggleBreakpoint,
        EditBreakpoint,
        RemoveAllBreakpoints,
        ToggleAllBreakpoints,
        ToggleDebugPanel,
        ToggleDebugConsole,
        DownloadCodeLldb,
        AddWatch,
        SubmitConsoleInput,
        PreviousConsoleInput,
        NextConsoleInput,
        ClearConsole,
        ConfirmSetVariable,
        CancelSetVariable,
    ]
);

/// Which debug adapter runs a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum DebugAdapterKind {
    /// debugpy (`python -m debugpy.adapter`).
    Python,
    /// CodeLLDB or lldb-dap (Rust, C, C++).
    Lldb,
    /// Any adapter speaking DAP over stdio.
    Custom { command: String, args: Vec<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DebugRequestKind {
    Launch,
    Attach,
}

/// What to debug: the adapter and its `launch`/`attach` arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DebugRequest {
    /// Shown in the Debug panel and status bar.
    pub name: String,
    pub adapter: DebugAdapterKind,
    pub request: DebugRequestKind,
    /// Adapter-specific `launch`/`attach` arguments (`program`, `args`,
    /// `cwd` as the adapter sees it, `env`, `module`, `justMyCode`,
    /// `stopOnEntry`, ...). Defaults for the adapter are filled in when the
    /// session starts.
    pub configuration: serde_json::Value,
}
