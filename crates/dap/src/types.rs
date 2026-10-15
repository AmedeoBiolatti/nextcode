//! The subset of the Debug Adapter Protocol's types the editor uses
//! (<https://microsoft.github.io/debug-adapter-protocol/specification>).
//! Unknown fields are ignored; optional ones may be missing.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

/// A request with typed arguments and response body.
pub trait Request {
    const COMMAND: &'static str;
    type Arguments: Serialize;
    type Response: DeserializeOwned;
}

macro_rules! requests {
    ($($name:ident = $command:literal ($arguments:ty) -> $response:ty;)*) => {
        $(
            pub enum $name {}
            impl Request for $name {
                const COMMAND: &'static str = $command;
                type Arguments = $arguments;
                type Response = $response;
            }
        )*
    };
}

requests! {
    Initialize = "initialize" (InitializeArguments) -> Capabilities;
    ConfigurationDone = "configurationDone" (Value) -> Value;
    SetBreakpoints = "setBreakpoints" (SetBreakpointsArguments) -> SetBreakpointsResponse;
    SetFunctionBreakpoints = "setFunctionBreakpoints" (SetFunctionBreakpointsArguments) -> SetBreakpointsResponse;
    SetExceptionBreakpoints = "setExceptionBreakpoints" (SetExceptionBreakpointsArguments) -> Value;
    Threads = "threads" (Value) -> ThreadsResponse;
    StackTrace = "stackTrace" (StackTraceArguments) -> StackTraceResponse;
    Scopes = "scopes" (ScopesArguments) -> ScopesResponse;
    Variables = "variables" (VariablesArguments) -> VariablesResponse;
    SetVariable = "setVariable" (SetVariableArguments) -> SetVariableResponse;
    Evaluate = "evaluate" (EvaluateArguments) -> EvaluateResponse;
    Continue = "continue" (ThreadArguments) -> Value;
    Next = "next" (ThreadArguments) -> Value;
    StepIn = "stepIn" (ThreadArguments) -> Value;
    StepOut = "stepOut" (ThreadArguments) -> Value;
    Pause = "pause" (ThreadArguments) -> Value;
    Restart = "restart" (Value) -> Value;
    Terminate = "terminate" (TerminateArguments) -> Value;
    Disconnect = "disconnect" (DisconnectArguments) -> Value;
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeArguments {
    #[serde(rename = "clientID")]
    pub client_id: String,
    pub client_name: String,
    #[serde(rename = "adapterID")]
    pub adapter_id: String,
    pub locale: String,
    pub lines_start_at1: bool,
    pub columns_start_at1: bool,
    pub path_format: String,
    pub supports_variable_type: bool,
    pub supports_variable_paging: bool,
    pub supports_run_in_terminal_request: bool,
    pub supports_start_debugging_request: bool,
    pub supports_progress_reporting: bool,
    pub supports_invalidated_event: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Capabilities {
    pub supports_configuration_done_request: bool,
    pub supports_function_breakpoints: bool,
    pub supports_conditional_breakpoints: bool,
    pub supports_hit_conditional_breakpoints: bool,
    pub supports_evaluate_for_hovers: bool,
    pub exception_breakpoint_filters: Vec<ExceptionBreakpointsFilter>,
    pub supports_set_variable: bool,
    pub supports_restart_request: bool,
    pub supports_log_points: bool,
    pub supports_terminate_request: bool,
    pub support_terminate_debuggee: bool,
    pub supports_value_formatting_options: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExceptionBreakpointsFilter {
    pub filter: String,
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub default: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_reference: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation_hint: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceBreakpoint {
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hit_condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetBreakpointsArguments {
    pub source: Source,
    pub breakpoints: Vec<SourceBreakpoint>,
    pub source_modified: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SetBreakpointsResponse {
    pub breakpoints: Vec<Breakpoint>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Breakpoint {
    pub id: Option<i64>,
    pub verified: bool,
    pub message: Option<String>,
    pub source: Option<Source>,
    pub line: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionBreakpoint {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SetFunctionBreakpointsArguments {
    pub breakpoints: Vec<FunctionBreakpoint>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SetExceptionBreakpointsArguments {
    pub filters: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Thread {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ThreadsResponse {
    pub threads: Vec<Thread>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StackTraceArguments {
    pub thread_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_frame: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub levels: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackFrame {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub source: Option<Source>,
    #[serde(default)]
    pub line: u32,
    #[serde(default)]
    pub column: u32,
    #[serde(default)]
    pub presentation_hint: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StackTraceResponse {
    pub stack_frames: Vec<StackFrame>,
    pub total_frames: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopesArguments {
    pub frame_id: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Scope {
    pub name: String,
    pub variables_reference: i64,
    #[serde(default)]
    pub expensive: bool,
    #[serde(default)]
    pub named_variables: Option<u32>,
    #[serde(default)]
    pub indexed_variables: Option<u32>,
    #[serde(default)]
    pub presentation_hint: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ScopesResponse {
    pub scopes: Vec<Scope>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VariablesArguments {
    pub variables_reference: i64,
    /// `"indexed"` or `"named"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Variable {
    pub name: String,
    #[serde(default)]
    pub value: String,
    #[serde(default, rename = "type")]
    pub type_name: Option<String>,
    #[serde(default)]
    pub variables_reference: i64,
    #[serde(default)]
    pub named_variables: Option<u32>,
    #[serde(default)]
    pub indexed_variables: Option<u32>,
    #[serde(default)]
    pub evaluate_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct VariablesResponse {
    pub variables: Vec<Variable>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetVariableArguments {
    pub variables_reference: i64,
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SetVariableResponse {
    pub value: String,
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    pub variables_reference: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluateArguments {
    pub expression: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<i64>,
    /// `"watch"`, `"repl"`, `"hover"`, `"clipboard"`.
    pub context: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluateResponse {
    pub result: String,
    #[serde(default, rename = "type")]
    pub type_name: Option<String>,
    #[serde(default)]
    pub variables_reference: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadArguments {
    pub thread_id: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminateArguments {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisconnectArguments {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminate_debuggee: Option<bool>,
}

// Events.

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StoppedEvent {
    pub reason: String,
    pub description: Option<String>,
    pub thread_id: Option<i64>,
    pub text: Option<String>,
    pub all_threads_stopped: bool,
    pub hit_breakpoint_ids: Vec<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ContinuedEvent {
    pub thread_id: i64,
    pub all_threads_continued: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ThreadEvent {
    pub reason: String,
    pub thread_id: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OutputEvent {
    /// `console` (default), `important`, `stdout`, `stderr`, `telemetry`.
    pub category: Option<String>,
    pub output: String,
    pub source: Option<Source>,
    pub line: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct BreakpointEvent {
    pub reason: String,
    pub breakpoint: Breakpoint,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExitedEvent {
    pub exit_code: i64,
}

// Reverse requests.

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RunInTerminalArguments {
    pub kind: Option<String>,
    pub title: Option<String>,
    pub cwd: String,
    pub args: Vec<String>,
    pub env: Option<serde_json::Map<String, Value>>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct StartDebuggingArguments {
    pub configuration: Value,
    pub request: String,
}
