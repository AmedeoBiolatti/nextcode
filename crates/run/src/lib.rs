//! Run configurations and tests, without the UI: the configuration model
//! and its files (`.vscode/launch.json`, `.next_code/run.json`), `${…}`
//! variables, the commands configurations run, and the parsers for what
//! pytest and cargo print (collection, results, build artifacts). The
//! editor runs the commands where the project lives and feeds the output
//! back here.

pub mod cargo;
pub mod config;
pub mod pytest;
pub mod shell;
pub mod tree;

pub use config::{
    Adapter, CargoCommand, CargoTarget, Command, CommandLine, Configuration, Context, Kind,
    PythonRunner, PythonTarget, Resolved, Source,
};
pub use tree::{Node, NodeKind, RunSpec, Status, Summary, TestResult, TestTree, Tool};
