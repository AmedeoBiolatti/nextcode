//! Workflows without the UI: the file format (JSON with comments, see
//! [`file`]), validation, and a run's state machine ([`run::Run`]) that
//! schedules steps by their `needs` and decides readiness from output,
//! exits, ports, delays and the user's Continue. The editor performs what
//! the run asks for (open projects, run checks and commands) and feeds the
//! results back.

pub mod file;
pub mod output;
pub mod run;

pub use file::{ProjectSpec, Ready, Step, TEMPLATE, Workflow, parse};
pub use output::{AnsiStripper, OutputWatcher, urls_in};
pub use run::{Change, CheckResult, Effect, Outcome, Run, Status, StepState};
