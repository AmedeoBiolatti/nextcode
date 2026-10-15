//! Headless run configurations and tests: `run_configurations`,
//! `debug_request`, `tests_discover`, `tests_run`. They use the same
//! loading, resolution and test engine as the GUI.

use super::*;
use crate::{
    run_config::{self, Active},
    test_explorer::engine,
};

impl Session {
    /// `run_configurations`, `debug_request`, `tests_*`.
    pub(super) fn run_command(
        &self,
        id: Value,
        command: &str,
        request: &Value,
        cx: &mut Context<Self>,
    ) {
        let file = request["file"]
            .as_str()
            .map(|file| self.project.path(file).to_string_lossy().into_owned());
        let active = Active {
            file,
            line: request["line"].as_u64().map(|line| line as usize + 1),
        };
        let name = request["name"].as_str().map(str::to_owned);
        let ids: Vec<String> = request["ids"]
            .as_array()
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let command = command.to_owned();
        self.blocking(id, cx, move |project| {
            let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidInput, message);
            match command.as_str() {
                "run_configurations" => {
                    let configs = run_config::load(project);
                    let runner = run_config::python_runner(project);
                    let rows: Vec<Value> = configs
                        .configurations
                        .iter()
                        .map(|config| {
                            let mut row = json!(run_config::describe(config));
                            row["command"] = match run_config::resolve(project, config, &active)
                                .and_then(|resolved| resolved.command(&runner))
                            {
                                Ok(command) => json!(run_config::terminal_line(project, &command)),
                                Err(err) => json!({ "error": err }),
                            };
                            row
                        })
                        .collect();
                    Ok(json!({ "configurations": rows, "problems": configs.problems }))
                }
                "debug_request" => {
                    let configs = run_config::load(project);
                    let config = match &name {
                        Some(name) => configs
                            .find(name)
                            .ok_or_else(|| invalid(format!("no configuration named `{name}`")))?,
                        None => configs
                            .selected()
                            .ok_or_else(|| invalid("no usable configuration".into()))?,
                    };
                    let resolved = run_config::resolve(project, config, &active).map_err(invalid)?;
                    let request = run_config::debug_request(project, &resolved).map_err(io::Error::other)?;
                    Ok(json!(request))
                }
                "tests_discover" => {
                    let settings = Settings::load().0.tests;
                    let discovery = engine::discover(project, &settings);
                    Ok(json!({
                        "nodes": discovery.tree.nodes,
                        "roots": discovery.tree.roots,
                        "errors": discovery.errors,
                        "results": engine::results_json(&discovery.tree, &discovery.results),
                    }))
                }
                "tests_run" => {
                    let settings = Settings::load().0.tests;
                    let discovery = engine::discover(project, &settings);
                    let tree = &discovery.tree;
                    let nodes: Vec<usize> = if ids.is_empty() {
                        tree.roots.clone()
                    } else {
                        ids.iter()
                            .map(|id| tree.get(id).ok_or_else(|| invalid(format!("unknown test `{id}`"))))
                            .collect::<io::Result<_>>()?
                    };
                    let report = engine::run_nodes(project, tree, &nodes, &settings);
                    let summaries = tree.summarize(&report.results, &Default::default());
                    let roots: Vec<Value> = tree
                        .roots
                        .iter()
                        .map(|&root| json!({ "id": tree.nodes[root].id, "summary": summaries[root] }))
                        .collect();
                    Ok(json!({
                        "results": engine::results_json(tree, &report.results),
                        "roots": roots,
                        "errors": report.errors,
                        "discovery_errors": discovery.errors,
                        "output": report.output,
                    }))
                }
                other => Err(invalid(format!("unknown command `{other}`"))),
            }
        });
    }
}
