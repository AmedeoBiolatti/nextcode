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
            .map(|file| self.path(request, file).to_string_lossy().into_owned());
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
                    // Each configuration runs in its own folder.
                    let mut runners = HashMap::new();
                    let rows: Vec<Value> = configs
                        .configurations
                        .iter()
                        .map(|config| {
                            let folder = configs.folder(&config.name);
                            let runner = runners
                                .entry(folder)
                                .or_insert_with(|| run_config::python_runner(project, folder));
                            let mut row = json!(run_config::describe(config));
                            row["command"] = match run_config::resolve(project, folder, config, &active)
                                .and_then(|resolved| resolved.command(runner))
                            {
                                Ok(command) => {
                                    json!(run_config::terminal_line(project, folder, &command))
                                }
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
                    let folder = configs.folder(&config.name);
                    let resolved =
                        run_config::resolve(project, folder, config, &active).map_err(invalid)?;
                    let request = run_config::debug_request(project, folder, &resolved)
                        .map_err(io::Error::other)?;
                    Ok(json!(request))
                }
                "tests_discover" => {
                    let discovery = engine::discover(project);
                    Ok(json!({
                        "nodes": discovery.tree.nodes,
                        "roots": discovery.tree.roots,
                        "errors": discovery.errors,
                        "results": engine::results_json(&discovery.tree, &discovery.results),
                    }))
                }
                "tests_run" => {
                    let discovery = engine::discover(project);
                    let tree = &discovery.tree;
                    let nodes: Vec<usize> = if ids.is_empty() {
                        tree.roots.clone()
                    } else {
                        ids.iter()
                            .map(|id| tree.get(id).ok_or_else(|| invalid(format!("unknown test `{id}`"))))
                            .collect::<io::Result<_>>()?
                    };
                    let report = engine::run_nodes(project, tree, &discovery.folders, &nodes);
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
