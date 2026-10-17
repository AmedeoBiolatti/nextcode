//! Workflows without the UI: the file format (YAML, see [`file`]; read
//! with positions by [`yaml`]), validation, and a run's state machine
//! ([`run::Run`]) that schedules steps by their `depends_on`, decides
//! readiness from output, exits, ports, delays and the user's Continue,
//! and stops commands with a grace period. Steps may be grouped (a step
//! with `steps`); [`tree`] flattens the groups with qualified ids
//! (`group/step`) and a run derives each group's state from its steps'.
//! The editor performs what the run asks for (open projects, run checks
//! and commands, interrupt and kill them, check artifacts) and feeds the
//! results back. [`layout`] places the steps of a workflow's graph (a
//! group as a card, or expanded around its own graph), [`source`] maps
//! steps to their text, [`vars`] reads `${steps.…}` variables,
//! [`inputs`] the parameters a run asks for (`${inputs.…}`), and
//! [`legacy`] converts files of the old JSON format.

pub mod file;
pub mod inputs;
pub mod legacy;
pub mod layout;
pub mod output;
pub mod run;
pub mod source;
pub mod tree;
pub mod vars;
pub mod yaml;

pub use file::{
    Artifact, ArtifactTarget, Condition, DEFAULT_STOP_GRACE, Dependency, MAX_STOP_GRACE,
    ProjectSpec, Ready, Step, TEMPLATE, Workflow, check, format_duration, parse,
};
pub use inputs::{Choice, Input, InputKind, InputValue, InputValues};
pub use layout::{GraphLayout, Metrics, View, layout_workflow};
pub use output::{AnsiStripper, OutputWatcher, urls_in};
pub use run::{ArtifactState, Change, CheckResult, Effect, Outcome, Run, Status, StepState};
pub use source::{
    InputSpan, Problem, StepSpan, input_spans, path_at, span_of, step_at, step_spans,
};
pub use tree::{StepNode, flatten};
