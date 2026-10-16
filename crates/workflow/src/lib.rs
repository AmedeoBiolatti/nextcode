//! Workflows without the UI: the file format (JSON with comments, see
//! [`file`]), validation, and a run's state machine ([`run::Run`]) that
//! schedules steps by their `needs` and decides readiness from output,
//! exits, ports, delays and the user's Continue. The editor performs what
//! the run asks for (open projects, run checks and commands) and feeds the
//! results back. [`layout`] places the steps of a workflow's graph and
//! [`source`] maps steps to their text.

pub mod file;
pub mod layout;
pub mod output;
pub mod run;
pub mod source;

pub use file::{ProjectSpec, Ready, Step, TEMPLATE, Workflow, check, parse};
pub use layout::{GraphLayout, Metrics, View, layout_workflow};
pub use output::{AnsiStripper, OutputWatcher, urls_in};
pub use run::{Change, CheckResult, Effect, Outcome, Run, Status, StepState};
pub use source::{Problem, StepSpan, step_at, step_spans};
