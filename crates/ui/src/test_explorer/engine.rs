//! Discovering and running tests where the project lives (blocking; the
//! panel and headless mode call these on background threads): pytest
//! through `uv run` or the project's interpreter, cargo through
//! `cargo test`. Parsing is in the `run` crate.

use std::{
    collections::{BTreeMap, HashMap},
    io,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use remote::RunOutput;
use run::{
    CargoTarget, Node, PythonRunner, RunSpec, Status, TestResult, TestTree, Tool, cargo, pytest,
    tree::cargo_test_id,
};
use serde_json::json;

use crate::{
    debugger::{DebugAdapterKind, DebugRequest, DebugRequestKind},
    project::Project,
    run_config::{self, BUILD_TIMEOUT, tail},
    settings::TestSettings,
};

/// Long test suites run this long at most.
const RUN_TIMEOUT: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Default)]
pub struct Discovery {
    pub tree: TestTree,
    /// Results known from discovery: files that failed to collect.
    pub results: HashMap<String, TestResult>,
    /// What went wrong (shown above the tree).
    pub errors: Vec<String>,
    /// Package directories (cargo tests run there).
    pub package_dirs: HashMap<String, String>,
}

/// Whether pytest applies: a Python project (or pytest config).
fn has_pytest(project: &Project) -> bool {
    [
        "pyproject.toml",
        "pytest.ini",
        "setup.cfg",
        "tox.ini",
        "conftest.py",
    ]
    .iter()
    .any(|file| matches!(project.kind(&project.resolve(file)), Ok(Some(_))))
}

fn has_cargo(project: &Project) -> bool {
    matches!(project.kind(&project.resolve("Cargo.toml")), Ok(Some(_)))
}

/// `pytest <args>` through uv or an interpreter.
fn pytest_command(runner: &PythonRunner, args: Vec<String>) -> (String, Vec<String>) {
    match runner {
        PythonRunner::Uv => (
            "uv".into(),
            ["run", "pytest"]
                .map(str::to_owned)
                .into_iter()
                .chain(args)
                .collect(),
        ),
        PythonRunner::Interpreter(python) => (
            python.clone(),
            ["-m", "pytest"]
                .map(str::to_owned)
                .into_iter()
                .chain(args)
                .collect(),
        ),
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Finds tests. Blocks (collection, a cargo test build).
pub fn discover(project: &Project, settings: &TestSettings) -> Discovery {
    let mut discovery = Discovery::default();
    if has_pytest(project) {
        discover_pytest(project, settings, &mut discovery);
    }
    if has_cargo(project) {
        discover_cargo(project, settings, &mut discovery);
    }
    if discovery.tree.nodes.is_empty() && discovery.errors.is_empty() {
        discovery.errors.push(
            "No tests found (pytest needs pyproject.toml or pytest.ini; cargo needs Cargo.toml)."
                .into(),
        );
    }
    discovery
}

fn discover_pytest(project: &Project, settings: &TestSettings, discovery: &mut Discovery) {
    let runner = run_config::python_runner(project);
    let args = pytest::COLLECT_ARGS
        .iter()
        .map(|&arg| arg.to_owned())
        .chain(settings.pytest_args.iter().cloned())
        .collect();
    let (program, args) = pytest_command(&runner, args);
    let output = match project.run_program(&program, &args, &[], None, RUN_TIMEOUT) {
        Ok(output) => output,
        Err(err) => {
            discovery.errors.push(format!("pytest: {program}: {err}"));
            return;
        }
    };
    let stdout = text(&output.stdout);
    let collection = pytest::parse_collection(&stdout);
    // 0: collected, 1: some errors, 5: none collected.
    if collection.node_ids.is_empty()
        && collection.errors.is_empty()
        && output.code != Some(5)
        && output.code != Some(0)
    {
        discovery.errors.push(format!(
            "pytest collection failed ({}):\n{}",
            exit_text(&output),
            tail(&format!("{}\n{stdout}", text(&output.stderr)), 20)
        ));
        return;
    }
    let mut files: HashMap<String, Option<String>> = HashMap::new();
    let error_files: Vec<String> = collection
        .errors
        .iter()
        .map(|(file, _)| file.clone())
        .collect();
    discovery.tree.add_pytest(
        &collection.node_ids,
        &error_files,
        |file, classes, function| {
            let source = files
                .entry(file.to_owned())
                .or_insert_with(|| project.read_text(&project.path(file)).ok());
            pytest::find_test_line(source.as_deref()?, classes, function)
        },
    );
    for (file, report) in collection.errors {
        discovery.results.insert(
            file,
            TestResult {
                status: Status::Error,
                message: Some("collection failed".into()),
                output: Some(report),
                duration: None,
            },
        );
    }
}

fn exit_text(output: &RunOutput) -> String {
    match output.code {
        Some(code) => format!("exit code {code}"),
        None => "killed".into(),
    }
}

fn discover_cargo(project: &Project, settings: &TestSettings, discovery: &mut Discovery) {
    let packages = match run_config::cargo_packages(project) {
        Ok(packages) => packages,
        Err(err) => {
            discovery.errors.push(err);
            return;
        }
    };
    for package in &packages {
        discovery
            .package_dirs
            .insert(package.name.clone(), package.dir.clone());
    }
    let args: Vec<String> = [
        "test",
        "--no-run",
        "--workspace",
        "--tests",
        "--message-format=json-render-diagnostics",
    ]
    .map(str::to_owned)
    .into_iter()
    .chain(settings.cargo_test_args.iter().cloned())
    .collect();
    let output = match project.run_program("cargo", &args, &[], None, BUILD_TIMEOUT) {
        Ok(output) => output,
        Err(err) => {
            discovery.errors.push(format!("cargo test --no-run: {err}"));
            return;
        }
    };
    let build = cargo::parse_build(&text(&output.stdout));
    if output.code != Some(0) {
        discovery.errors.push(format!(
            "cargo test --no-run failed ({}):\n{}",
            exit_text(&output),
            tail(&text(&output.stderr), 30)
        ));
    }
    let mut files: HashMap<String, Option<String>> = HashMap::new();
    // In metadata order: packages, then their targets.
    for package in &packages {
        for target in &package.targets {
            let Some(artifact) = build.artifacts.iter().find(|artifact| {
                artifact.test
                    && artifact.target == target.target
                    && same_path(&artifact.src_path, &target.src_path)
            }) else {
                continue;
            };
            let listed = project.run_program(
                &artifact.executable,
                &["--list".into(), "--format".into(), "terse".into()],
                &[],
                None,
                Duration::from_secs(60),
            );
            let tests = match listed {
                Ok(output) if output.code == Some(0) => cargo::parse_list(&text(&output.stdout)),
                Ok(output) => {
                    discovery.errors.push(format!(
                        "{} {}: --list failed: {}",
                        package.name,
                        target.target.label(),
                        tail(&text(&output.stderr), 5)
                    ));
                    continue;
                }
                Err(err) => {
                    discovery.errors.push(format!(
                        "{} {}: {err}",
                        package.name,
                        target.target.label()
                    ));
                    continue;
                }
            };
            if tests.is_empty() {
                continue;
            }
            discovery.tree.add_cargo_target(
                &package.name,
                &target.target,
                &tests,
                |modules, name| {
                    cargo::module_files(&target.src_path, modules)
                        .into_iter()
                        .find_map(|file| {
                            let source = files
                                .entry(file.clone())
                                .or_insert_with(|| project.read_text(Path::new(&file)).ok());
                            let line = cargo::find_fn_line(source.as_deref()?, name)?;
                            Some((file, line))
                        })
                },
            );
        }
    }
}

fn same_path(a: &str, b: &str) -> bool {
    let normalize = |path: &str| path.replace('\\', "/").to_lowercase();
    a == b || (cfg!(windows) && normalize(a) == normalize(b))
}

/// What a test run produced.
#[derive(Debug, Clone, Default)]
pub struct RunReport {
    pub results: HashMap<String, TestResult>,
    /// Every command's output, in order.
    pub output: String,
    pub errors: Vec<String>,
}

/// Runs `nodes` (indices into `tree`). Blocks.
pub fn run_nodes(
    project: &Project,
    tree: &TestTree,
    nodes: &[usize],
    settings: &TestSettings,
) -> RunReport {
    let mut report = RunReport::default();
    for (spec, leaves) in tree.plan(nodes) {
        let outcome = match &spec {
            RunSpec::Pytest { target } => run_pytest(project, target, settings, &mut report),
            RunSpec::Cargo {
                package,
                target,
                filter,
                exact,
            } => {
                // Every target runs; the first failure is reported.
                let mut outcome = Ok(());
                for (package, target) in cargo_targets(tree, package.as_deref(), target.as_ref()) {
                    let run = run_cargo(
                        project,
                        &package,
                        &target,
                        filter.as_deref(),
                        *exact,
                        settings,
                        &mut report,
                    );
                    if outcome.is_ok() {
                        outcome = run;
                    }
                }
                outcome
            }
        };
        let failure = outcome.err();
        if let Some(error) = &failure {
            report.errors.push(error.clone());
        }
        // Asked for but not reported: the run broke before them.
        for leaf in leaves {
            report.results.entry(leaf).or_insert_with(|| TestResult {
                status: if failure.is_some() {
                    Status::Error
                } else {
                    Status::NotRun
                },
                message: failure
                    .as_ref()
                    .map(|error| error.lines().next().unwrap_or_default().to_owned())
                    .or(Some("not reported by the test run".into())),
                output: failure.clone(),
                duration: None,
            });
        }
    }
    report
}

/// The (package, target) pairs a cargo spec covers.
fn cargo_targets(
    tree: &TestTree,
    package: Option<&str>,
    target: Option<&CargoTarget>,
) -> Vec<(String, CargoTarget)> {
    if let (Some(package), Some(target)) = (package, target) {
        return vec![(package.to_owned(), target.clone())];
    }
    tree.nodes
        .iter()
        .filter(|node| node.kind == run::NodeKind::Target)
        .filter_map(|node| match &node.run {
            RunSpec::Cargo {
                package: Some(node_package),
                target: Some(node_target),
                ..
            } if package.is_none_or(|package| package == node_package) => {
                Some((node_package.clone(), node_target.clone()))
            }
            _ => None,
        })
        .collect()
}

/// A report file where the project lives.
fn report_path(project: &Project) -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_nanos());
    let name = format!("next_code-pytest-{}-{stamp}.xml", std::process::id());
    if project.is_remote() {
        format!("/tmp/{name}")
    } else {
        std::env::temp_dir()
            .join(name)
            .to_string_lossy()
            .into_owned()
    }
}

fn run_pytest(
    project: &Project,
    targets: &str,
    settings: &TestSettings,
    report: &mut RunReport,
) -> Result<(), String> {
    let runner = run_config::python_runner(project);
    let xml_path = report_path(project);
    let args = pytest::run_args(&xml_path)
        .into_iter()
        .chain(settings.pytest_args.iter().cloned())
        .chain(
            targets
                .lines()
                .filter(|target| !target.is_empty())
                .map(str::to_owned),
        )
        .collect();
    let (program, args) = pytest_command(&runner, args);
    let output = project
        .run_program(&program, &args, &[], None, RUN_TIMEOUT)
        .map_err(|err| format!("pytest: {program}: {err}"))?;
    append_output(report, &format!("{program} {}", args.join(" ")), &output);
    let xml_file = Path::new(&xml_path);
    let xml = project.read_text(xml_file);
    project.remove(xml_file, false).ok();
    match xml {
        Ok(xml) => {
            report.results.extend(pytest::parse_junit(&xml)?);
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Err(format!(
            "pytest wrote no report ({}):\n{}",
            exit_text(&output),
            tail(
                &format!("{}\n{}", text(&output.stdout), text(&output.stderr)),
                20
            )
        )),
        Err(err) => Err(format!("reading pytest's report: {err}")),
    }
}

fn run_cargo(
    project: &Project,
    package: &str,
    target: &CargoTarget,
    filter: Option<&str>,
    exact: bool,
    settings: &TestSettings,
    report: &mut RunReport,
) -> Result<(), String> {
    let mut args: Vec<String> = vec!["test".into(), "--package".into(), package.into()];
    args.extend(target.args());
    args.push("--no-fail-fast".into());
    args.extend(settings.cargo_test_args.iter().cloned());
    if let Some(filter) = filter {
        args.push("--".into());
        if exact {
            args.push("--exact".into());
        }
        args.extend(filter.lines().map(str::to_owned));
    }
    let output = project
        .run_program("cargo", &args, &[], None, RUN_TIMEOUT)
        .map_err(|err| format!("cargo test: {err}"))?;
    append_output(report, &format!("cargo {}", args.join(" ")), &output);
    let results = cargo::parse_results(&text(&output.stdout));
    if results.is_empty() && output.code != Some(0) {
        return Err(format!(
            "cargo test failed ({}):\n{}",
            exit_text(&output),
            tail(&text(&output.stderr), 30)
        ));
    }
    report.results.extend(
        results
            .into_iter()
            .map(|(name, result)| (cargo_test_id(package, target, &name), result)),
    );
    Ok(())
}

fn append_output(report: &mut RunReport, command: &str, output: &RunOutput) {
    if !report.output.is_empty() {
        report.output.push_str("\n\n");
    }
    report.output.push_str(&format!("$ {command}\n"));
    report.output.push_str(text(&output.stdout).trim_end());
    let stderr = text(&output.stderr);
    if !stderr.trim().is_empty() {
        report.output.push('\n');
        report.output.push_str(stderr.trim_end());
    }
    report
        .output
        .push_str(&format!("\n[{}]", exit_text(output)));
}

/// The debug request for a test node: pytest under debugpy, or the cargo
/// test binary (built first) under LLDB with the node's filter. Blocks.
pub fn debug_request(
    project: &Project,
    node: &Node,
    package_dirs: &HashMap<String, String>,
) -> Result<DebugRequest, String> {
    let root = project.root().to_string_lossy().into_owned();
    match &node.run {
        RunSpec::Pytest { target } => Ok(DebugRequest {
            name: format!(
                "pytest {}",
                if target.is_empty() {
                    "(all)"
                } else {
                    &node.label
                }
            ),
            adapter: DebugAdapterKind::Python,
            request: DebugRequestKind::Launch,
            configuration: json!({
                "module": "pytest",
                "args": target.lines().filter(|t| !t.is_empty()).collect::<Vec<_>>(),
                "cwd": root,
                "justMyCode": false,
            }),
        }),
        RunSpec::Cargo {
            package: Some(package),
            target: Some(target),
            filter,
            exact,
        } => {
            let mut args: Vec<String> = vec![
                "test".into(),
                "--no-run".into(),
                "--package".into(),
                package.clone(),
            ];
            args.extend(target.args());
            args.push("--message-format=json-render-diagnostics".into());
            let program = run_config::cargo_executable(project, &args, true)?;
            let mut test_args: Vec<String> = Vec::new();
            if let Some(filter) = filter {
                if *exact {
                    test_args.push("--exact".into());
                }
                test_args.extend(filter.lines().map(str::to_owned));
            }
            test_args.push("--nocapture".into());
            Ok(DebugRequest {
                name: format!(
                    "cargo test {}",
                    filter
                        .as_deref()
                        .map_or(target.label(), |f| f.replace('\n', " "))
                ),
                adapter: DebugAdapterKind::Lldb,
                request: DebugRequestKind::Launch,
                configuration: json!({
                    "program": program,
                    "args": test_args,
                    "cwd": package_dirs.get(package).cloned().unwrap_or(root),
                }),
            })
        }
        RunSpec::Cargo { .. } => Err("pick one test target (or a test in it) to debug".into()),
    }
}

/// Results as JSON-friendly rows (headless).
pub fn results_json(tree: &TestTree, results: &HashMap<String, TestResult>) -> serde_json::Value {
    let sorted: BTreeMap<&String, &TestResult> = results.iter().collect();
    let rows: Vec<serde_json::Value> = sorted
        .into_iter()
        .map(|(id, result)| {
            json!({
                "id": id,
                "in_tree": tree.get(id).is_some(),
                "status": result.status,
                "message": result.message,
                "output": result.output,
                "duration": result.duration,
            })
        })
        .collect();
    json!(rows)
}

/// Test nodes defined in `path` (project side) with a known line: tests,
/// parametrized functions and classes (gutter markers, test at cursor).
pub fn nodes_in_file<'a>(
    tree: &'a TestTree,
    project: &Project,
    path: &Path,
) -> Vec<(usize, &'a Node)> {
    tree.nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| {
            node.line.is_some()
                && matches!(node.kind, run::NodeKind::Test | run::NodeKind::Function | run::NodeKind::Class)
                && node.path.as_deref().is_some_and(|node_path| project.path(node_path) == path)
                // Parametrized cases share their function's line.
                && !(node.tool == Tool::Pytest
                    && node.kind == run::NodeKind::Test
                    && node.parent.is_some_and(|parent| tree.nodes[parent].kind == run::NodeKind::Function))
        })
        .collect()
}
