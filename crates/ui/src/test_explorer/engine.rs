//! Discovering and running tests where the project lives (blocking; the
//! panel and headless mode call these on background threads): pytest
//! through `uv run` or the folder's interpreter, cargo through `cargo
//! test`, in each folder of the project. With several folders the shown
//! tree has a top-level node per folder and ids prefixed `folder › `;
//! runs are planned with each folder's own tree, in that folder. Parsing
//! is in the `run` crate.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
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
    project::{Project, root_label},
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
    /// Package directories by package (id-prefixed like the tree); cargo
    /// tests run there.
    pub package_dirs: HashMap<String, String>,
    /// With several folders, each folder's own tree (indexed like the
    /// project's folders), which runs are planned with. Empty with one.
    pub folders: Vec<TestTree>,
}

/// The folders' names as the tree shows them (later duplicates numbered).
fn folder_names(project: &Project) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for (index, root) in project.roots().iter().enumerate() {
        let name = match names.contains(&root.name) {
            true => format!("{} ({})", root.name, index + 1),
            false => root.name.clone(),
        };
        names.push(name);
    }
    names
}

/// The id prefix of folder `root`'s tests: `api › ` with several
/// folders, empty with one.
fn id_prefix(project: &Project, names: &[String], root: usize) -> String {
    match project.is_multi_root() {
        true => root_label(&names[root], ""),
        false => String::new(),
    }
}

/// The shown id of folder `root`'s test `id` (a pytest node id or path,
/// a cargo test id), as `RunTarget::Ids` takes it.
pub fn test_id(project: &Project, root: usize, id: &str) -> String {
    format!("{}{id}", id_prefix(project, &folder_names(project), root))
}

/// The folder of a shown id and the id within it (`None` for the folder's
/// own node). With one folder, every id is the first folder's as is.
fn split_id<'a>(
    project: &Project,
    names: &[String],
    id: &'a str,
) -> Option<(usize, Option<&'a str>)> {
    if !project.is_multi_root() {
        return Some((0, Some(id)));
    }
    names.iter().enumerate().find_map(|(root, name)| {
        if id == name {
            return Some((root, None));
        }
        let local = id.strip_prefix(&root_label(name, ""))?;
        Some((root, Some(local)))
    })
}

/// Folder `root`'s test settings (its layered settings).
fn settings_in(project: &Project, root: usize) -> TestSettings {
    project
        .settings_for(Some(&project.roots()[root].path))
        .tests
        .clone()
}

/// Whether pytest applies to folder `root`: a Python project (or pytest
/// config).
fn has_pytest(project: &Project, root: usize) -> bool {
    [
        "pyproject.toml",
        "pytest.ini",
        "setup.cfg",
        "tox.ini",
        "conftest.py",
    ]
    .iter()
    .any(|file| matches!(project.kind(&project.resolve_in(root, file)), Ok(Some(_))))
}

fn has_cargo(project: &Project, root: usize) -> bool {
    matches!(
        project.kind(&project.resolve_in(root, "Cargo.toml")),
        Ok(Some(_))
    )
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

/// Finds every folder's tests. Blocks (collection, cargo test builds).
pub fn discover(project: &Project) -> Discovery {
    let mut discovery = match project.is_multi_root() {
        true => merge(
            project,
            (0..project.roots().len())
                .map(|root| discover_in(project, root))
                .collect(),
        ),
        false => discover_in(project, 0),
    };
    if discovery.tree.nodes.is_empty() && discovery.errors.is_empty() {
        discovery.errors.push(
            "No tests found (pytest needs pyproject.toml or pytest.ini; cargo needs Cargo.toml)."
                .into(),
        );
    }
    discovery
}

/// Folder `root`'s tests, with ids as in that folder alone.
fn discover_in(project: &Project, root: usize) -> Discovery {
    let settings = settings_in(project, root);
    let mut discovery = Discovery::default();
    if has_pytest(project, root) {
        discover_pytest(project, root, &settings, &mut discovery);
    }
    if has_cargo(project, root) {
        discover_cargo(project, root, &settings, &mut discovery);
    }
    discovery
}

/// One discovery from each folder's (in folder order): a top-level node
/// per folder with tests, holding them with prefixed ids (pytest's
/// folder-relative paths made absolute); results, package directories
/// and errors follow.
fn merge(project: &Project, folders: Vec<Discovery>) -> Discovery {
    let names = folder_names(project);
    let mut merged = Discovery::default();
    for (root, folder) in folders.into_iter().enumerate() {
        let prefix = id_prefix(project, &names, root);
        if !folder.tree.nodes.is_empty() {
            merged.tree.graft(
                &names[root],
                &names[root],
                &prefix,
                &folder.tree,
                |node, file| match node.tool {
                    Tool::Pytest => project
                        .resolve_in(root, file)
                        .to_string_lossy()
                        .into_owned(),
                    Tool::Cargo => file.to_owned(),
                },
            );
        }
        let prefixed = |id: String| format!("{prefix}{id}");
        merged.results.extend(
            folder
                .results
                .into_iter()
                .map(|(id, result)| (prefixed(id), result)),
        );
        merged.package_dirs.extend(
            folder
                .package_dirs
                .into_iter()
                .map(|(package, dir)| (prefixed(package), dir)),
        );
        merged.errors.extend(
            folder
                .errors
                .into_iter()
                .map(|error| format!("{}: {error}", names[root])),
        );
        merged.folders.push(folder.tree);
    }
    merged
}

fn discover_pytest(
    project: &Project,
    root: usize,
    settings: &TestSettings,
    discovery: &mut Discovery,
) {
    let runner = run_config::python_runner(project, root);
    let args = pytest::COLLECT_ARGS
        .iter()
        .map(|&arg| arg.to_owned())
        .chain(settings.pytest_args.iter().cloned())
        .collect();
    let (program, args) = pytest_command(&runner, args);
    let dir = &project.roots()[root].path;
    let output = match project.run_program_in(dir, &program, &args, &[], None, RUN_TIMEOUT) {
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
                .or_insert_with(|| project.read_text(&project.path_in(root, file)).ok());
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

fn discover_cargo(
    project: &Project,
    root: usize,
    settings: &TestSettings,
    discovery: &mut Discovery,
) {
    let packages = match run_config::cargo_packages(project, root) {
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
    let dir = &project.roots()[root].path;
    let output = match project.run_program_in(dir, "cargo", &args, &[], None, BUILD_TIMEOUT) {
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
            let listed = project.run_program_in(
                dir,
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

/// Shown `nodes` grouped by folder: (folder, its own tree, the nodes
/// there). A folder's node stands for all its tests.
fn by_folder<'a>(
    project: &Project,
    tree: &'a TestTree,
    folders: &'a [TestTree],
    nodes: &[usize],
) -> Vec<(usize, &'a TestTree, Vec<usize>)> {
    if folders.is_empty() {
        return vec![(0, tree, nodes.to_vec())];
    }
    let names = folder_names(project);
    let mut grouped: Vec<(usize, &TestTree, Vec<usize>)> = Vec::new();
    for &node in nodes {
        let Some((root, local)) = split_id(project, &names, &tree.nodes[node].id) else {
            continue;
        };
        let Some(folder) = folders.get(root) else {
            continue;
        };
        let local: Vec<usize> = match local {
            None => folder.roots.clone(),
            Some(id) => folder.get(id).into_iter().collect(),
        };
        match grouped.iter_mut().find(|(of, _, _)| *of == root) {
            Some((_, _, nodes)) => nodes.extend(local),
            None => grouped.push((root, folder, local)),
        }
    }
    grouped
}

/// The tests (shown ids) a run of `nodes` reports on.
pub fn planned_leaves(
    project: &Project,
    tree: &TestTree,
    folders: &[TestTree],
    nodes: &[usize],
) -> HashSet<String> {
    let names = folder_names(project);
    by_folder(project, tree, folders, nodes)
        .into_iter()
        .flat_map(|(root, folder, nodes)| {
            let prefix = id_prefix(project, &names, root);
            folder
                .plan(&nodes)
                .into_iter()
                .flat_map(|(_, leaves)| leaves)
                .map(move |leaf| format!("{prefix}{leaf}"))
        })
        .collect()
}

/// Runs `nodes` (indices into the shown `tree`; `folders` as discovered),
/// each folder's in that folder. Blocks.
pub fn run_nodes(
    project: &Project,
    tree: &TestTree,
    folders: &[TestTree],
    nodes: &[usize],
) -> RunReport {
    let names = folder_names(project);
    let mut report = RunReport::default();
    for (root, folder, nodes) in by_folder(project, tree, folders, nodes) {
        let part = run_in(project, root, folder, &nodes, &settings_in(project, root));
        let prefix = id_prefix(project, &names, root);
        report.results.extend(
            part.results
                .into_iter()
                .map(|(id, result)| (format!("{prefix}{id}"), result)),
        );
        if !part.output.is_empty() {
            if !report.output.is_empty() {
                report.output.push_str("\n\n");
            }
            report.output.push_str(&part.output);
        }
        report.errors.extend(
            part.errors
                .into_iter()
                .map(|error| match project.is_multi_root() {
                    true => format!("{}: {error}", names[root]),
                    false => error,
                }),
        );
    }
    report
}

/// Runs `nodes` of folder `root`'s own tree there.
fn run_in(
    project: &Project,
    root: usize,
    tree: &TestTree,
    nodes: &[usize],
    settings: &TestSettings,
) -> RunReport {
    let mut report = RunReport::default();
    for (spec, leaves) in tree.plan(nodes) {
        let outcome = match &spec {
            RunSpec::Pytest { target } => run_pytest(project, root, target, settings, &mut report),
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
                        root,
                        &package,
                        &target,
                        filter.as_deref().map(|filter| (filter, *exact)),
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
    root: usize,
    targets: &str,
    settings: &TestSettings,
    report: &mut RunReport,
) -> Result<(), String> {
    let runner = run_config::python_runner(project, root);
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
        .run_program_in(
            &project.roots()[root].path,
            &program,
            &args,
            &[],
            None,
            RUN_TIMEOUT,
        )
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

/// Runs `cargo test` for `target` of `package` in folder `root`, limited by
/// `filter` (test names, one per line, and whether they match exactly).
fn run_cargo(
    project: &Project,
    root: usize,
    package: &str,
    target: &CargoTarget,
    filter: Option<(&str, bool)>,
    settings: &TestSettings,
    report: &mut RunReport,
) -> Result<(), String> {
    let mut args: Vec<String> = vec!["test".into(), "--package".into(), package.into()];
    args.extend(target.args());
    args.push("--no-fail-fast".into());
    args.extend(settings.cargo_test_args.iter().cloned());
    if let Some((filter, exact)) = filter {
        args.push("--".into());
        if exact {
            args.push("--exact".into());
        }
        args.extend(filter.lines().map(str::to_owned));
    }
    let output = project
        .run_program_in(
            &project.roots()[root].path,
            "cargo",
            &args,
            &[],
            None,
            RUN_TIMEOUT,
        )
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
/// test binary (built first) under LLDB with the node's filter, in the
/// node's folder. Blocks.
pub fn debug_request(
    project: &Project,
    node: &Node,
    package_dirs: &HashMap<String, String>,
) -> Result<DebugRequest, String> {
    let names = folder_names(project);
    let folder = match split_id(project, &names, &node.id) {
        Some((folder, Some(_))) => folder,
        _ => return Err("pick tests inside a folder to debug".into()),
    };
    let root = project.roots()[folder].path.to_string_lossy().into_owned();
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
            let program = run_config::cargo_executable(project, folder, &args, true)?;
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
                    "cwd": package_dirs
                        .get(&format!("{}{package}", id_prefix(project, &names, folder)))
                        .cloned()
                        .unwrap_or(root),
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// One folder's discovery: pytest `ids`, plus a file that failed to
    /// collect.
    fn folder(ids: &[&str]) -> Discovery {
        let mut discovery = Discovery::default();
        let ids: Vec<String> = ids.iter().map(|&id| id.to_owned()).collect();
        discovery
            .tree
            .add_pytest(&ids, &["tests/test_bad.py".into()], |_, _, _| Some(3));
        discovery.results.insert(
            "tests/test_bad.py".into(),
            TestResult {
                status: Status::Error,
                message: Some("collection failed".into()),
                output: None,
                duration: None,
            },
        );
        discovery.errors.push("pytest: warning".into());
        discovery
    }

    fn folders() -> (PathBuf, PathBuf, Project) {
        let base = std::env::temp_dir().join("next_code_engine_roots");
        let (api, web) = (base.join("api"), base.join("web"));
        let project = Project::local_folders(&[&api, &web]);
        (api, web, project)
    }

    #[test]
    fn each_folder_gets_a_top_level_node_with_its_tests() {
        let (api, web, project) = folders();
        let merged = merge(
            &project,
            vec![
                folder(&["tests/test_a.py::test_one"]),
                folder(&["tests/test_a.py::test_one", "tests/test_a.py::test_two"]),
            ],
        );
        let tree = &merged.tree;
        let top: Vec<&str> = tree
            .roots
            .iter()
            .map(|&root| tree.nodes[root].label.as_str())
            .collect();
        assert_eq!(top, ["api", "web"]);
        // The same test in both folders stays two tests, each in its folder.
        let api_test = tree.get("api › tests/test_a.py::test_one").unwrap();
        let web_test = tree
            .get(&test_id(&project, 1, "tests/test_a.py::test_one"))
            .unwrap();
        assert_eq!(tree.leaves(tree.roots[0]), [api_test]);
        assert_eq!(tree.leaves(tree.roots[1]).len(), 2);
        assert_eq!(
            tree.nodes[web_test].path.as_deref().map(PathBuf::from),
            Some(web.join("tests/test_a.py"))
        );
        assert_eq!(
            nodes_in_file(tree, &project, &api.join("tests/test_a.py")),
            [(api_test, &tree.nodes[api_test])]
        );
        assert!(merged.results.contains_key("web › tests/test_bad.py"));
        assert!(merged.errors.contains(&"api: pytest: warning".to_owned()));
        assert_eq!(merged.folders.len(), 2);

        // Runs are planned per folder, with the folder's own ids.
        let grouped = by_folder(&project, tree, &merged.folders, &[web_test, tree.roots[0]]);
        let planned: Vec<(usize, Vec<String>)> = grouped
            .iter()
            .map(|(root, folder, nodes)| {
                let ids = folder
                    .plan(nodes)
                    .into_iter()
                    .flat_map(|(spec, _)| match spec {
                        RunSpec::Pytest { target } => vec![target],
                        RunSpec::Cargo { .. } => vec![],
                    })
                    .collect();
                (*root, ids)
            })
            .collect();
        assert_eq!(
            planned,
            [
                (1, vec!["tests/test_a.py::test_one".to_owned()]),
                (0, vec![String::new()]),
            ]
        );
        let leaves = planned_leaves(&project, tree, &merged.folders, &[tree.roots[1]]);
        assert_eq!(
            leaves,
            HashSet::from([
                "web › tests/test_a.py::test_one".to_owned(),
                "web › tests/test_a.py::test_two".to_owned(),
            ])
        );

        // Debugging runs in the test's folder; a folder node is not a target.
        let request = debug_request(&project, &tree.nodes[web_test], &merged.package_dirs).unwrap();
        assert_eq!(
            request.configuration["cwd"].as_str().map(PathBuf::from),
            Some(web.clone())
        );
        assert_eq!(
            request.configuration["args"],
            json!(["tests/test_a.py::test_one"])
        );
        assert!(debug_request(&project, &tree.nodes[tree.roots[0]], &merged.package_dirs).is_err());
    }

    #[test]
    fn folders_without_tests_have_no_node_but_keep_their_place() {
        let (_, _, project) = folders();
        let merged = merge(
            &project,
            vec![Discovery::default(), folder(&["test_x.py::test_x"])],
        );
        assert_eq!(merged.tree.roots.len(), 1);
        assert_eq!(merged.tree.nodes[merged.tree.roots[0]].id, "web");
        let test = merged.tree.get("web › test_x.py::test_x").unwrap();
        let grouped = by_folder(&project, &merged.tree, &merged.folders, &[test]);
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].0, 1);
    }

    #[test]
    fn one_folder_keeps_plain_ids() {
        let project = Project::local(std::env::temp_dir().join("next_code_engine_root"));
        let discovery = folder(&["tests/test_a.py::test_one"]);
        assert_eq!(test_id(&project, 0, "tests/test_a.py"), "tests/test_a.py");
        let test = discovery.tree.get("tests/test_a.py::test_one").unwrap();
        let grouped = by_folder(&project, &discovery.tree, &[], &[test]);
        assert_eq!(grouped.len(), 1);
        assert_eq!((grouped[0].0, grouped[0].2.clone()), (0, vec![test]));
        assert_eq!(
            planned_leaves(&project, &discovery.tree, &[], &[test]),
            HashSet::from(["tests/test_a.py::test_one".to_owned()])
        );
    }
}
