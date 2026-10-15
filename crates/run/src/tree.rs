//! The test tree shown by the Tests panel: pytest (folders, files,
//! classes, parametrized functions, cases) and cargo (packages, targets,
//! modules, tests), with how to run each node and status aggregation.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;

use crate::{config::CargoTarget, pytest};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tool {
    Pytest,
    Cargo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Root,
    Folder,
    File,
    Class,
    /// A parametrized test function grouping its cases.
    Function,
    Package,
    Target,
    Module,
    Test,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    NotRun,
    Running,
    Passed,
    Failed,
    /// Could not run (collection or fixture error).
    Error,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TestResult {
    pub status: Status,
    /// One line: assertion, panic or skip reason.
    pub message: Option<String>,
    /// Traceback and captured output.
    pub output: Option<String>,
    /// Seconds.
    pub duration: Option<f64>,
}

/// How to run a node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum RunSpec {
    /// `pytest <targets>` (newline-separated); empty for every test.
    Pytest { target: String },
    /// `cargo test` of one target (every target of every package when
    /// `package` is `None`), filtered by a test path prefix, or exact
    /// names (newline-separated).
    Cargo {
        package: Option<String>,
        target: Option<CargoTarget>,
        filter: Option<String>,
        exact: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Node {
    pub id: String,
    pub label: String,
    pub kind: NodeKind,
    pub tool: Tool,
    pub parent: Option<usize>,
    pub depth: usize,
    pub children: Vec<usize>,
    /// Project-relative (pytest) or absolute (cargo) file.
    pub path: Option<String>,
    /// 0-based line of the definition.
    pub line: Option<usize>,
    pub run: RunSpec,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TestTree {
    pub nodes: Vec<Node>,
    /// Top-level nodes (one per tool).
    pub roots: Vec<usize>,
    #[serde(skip)]
    index: HashMap<String, usize>,
}

/// Per-node counts and the status they add up to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub status: Status,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub total: usize,
}

impl TestTree {
    pub fn get(&self, id: &str) -> Option<usize> {
        self.index.get(id).copied()
    }

    fn add(&mut self, parent: Option<usize>, node: Node) -> usize {
        if let Some(&existing) = self.index.get(&node.id) {
            return existing;
        }
        let index = self.nodes.len();
        self.index.insert(node.id.clone(), index);
        match parent {
            Some(parent) => self.nodes[parent].children.push(index),
            None => self.roots.push(index),
        }
        self.nodes.push(Node {
            parent,
            depth: parent.map_or(0, |parent| self.nodes[parent].depth + 1),
            ..node
        });
        index
    }

    fn node(id: String, label: String, kind: NodeKind, tool: Tool, run: RunSpec) -> Node {
        Node {
            id,
            label,
            kind,
            tool,
            parent: None,
            depth: 0,
            children: Vec::new(),
            path: None,
            line: None,
            run,
        }
    }

    /// Adds pytest's collection (node ids relative to the project root)
    /// and files that failed to collect; `line` finds a definition
    /// (file, classes, function).
    pub fn add_pytest(
        &mut self,
        node_ids: &[String],
        error_files: &[String],
        mut line: impl FnMut(&str, &[&str], &str) -> Option<usize>,
    ) {
        let pytest = |target: &str| RunSpec::Pytest {
            target: target.to_owned(),
        };
        let root = self.add(
            None,
            Self::node(
                "pytest".into(),
                "pytest".into(),
                NodeKind::Root,
                Tool::Pytest,
                pytest(""),
            ),
        );
        let file_node = |tree: &mut Self, file: &str| -> usize {
            let mut parent = root;
            let mut prefix = String::new();
            let mut segments: Vec<&str> = file.split('/').collect();
            let name = segments.pop().unwrap_or(file);
            for segment in segments {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(segment);
                parent = tree.add(
                    Some(parent),
                    Self::node(
                        prefix.clone(),
                        segment.to_owned(),
                        NodeKind::Folder,
                        Tool::Pytest,
                        pytest(&prefix),
                    ),
                );
            }
            let mut node = Self::node(
                file.to_owned(),
                name.to_owned(),
                NodeKind::File,
                Tool::Pytest,
                pytest(file),
            );
            node.path = Some(file.to_owned());
            tree.add(Some(parent), node)
        };
        for id in node_ids {
            let Some(parts) = pytest::split_node_id(id) else {
                continue;
            };
            let mut parent = file_node(self, parts.file);
            let mut prefix = parts.file.to_owned();
            for (depth, class) in parts.classes.iter().enumerate() {
                prefix = format!("{prefix}::{class}");
                let mut node = Self::node(
                    prefix.clone(),
                    (*class).to_owned(),
                    NodeKind::Class,
                    Tool::Pytest,
                    pytest(&prefix),
                );
                node.path = Some(parts.file.to_owned());
                if self.get(&prefix).is_none() {
                    node.line = line(parts.file, &parts.classes[..=depth], "");
                }
                parent = self.add(Some(parent), node);
            }
            let function_id = format!("{prefix}::{}", parts.function);
            let function_line = line(parts.file, &parts.classes, parts.function);
            match parts.params {
                Some(params) => {
                    if self.get(&function_id).is_none() {
                        let mut group = Self::node(
                            function_id.clone(),
                            parts.function.to_owned(),
                            NodeKind::Function,
                            Tool::Pytest,
                            pytest(&function_id),
                        );
                        group.path = Some(parts.file.to_owned());
                        group.line = function_line;
                        self.add(Some(parent), group);
                    }
                    let group = self.get(&function_id).unwrap_or(parent);
                    let mut case = Self::node(
                        id.clone(),
                        params.to_owned(),
                        NodeKind::Test,
                        Tool::Pytest,
                        pytest(id),
                    );
                    case.path = Some(parts.file.to_owned());
                    case.line = self.nodes[group].line;
                    self.add(Some(group), case);
                }
                None => {
                    let mut test = Self::node(
                        id.clone(),
                        parts.function.to_owned(),
                        NodeKind::Test,
                        Tool::Pytest,
                        pytest(id),
                    );
                    test.path = Some(parts.file.to_owned());
                    test.line = function_line;
                    self.add(Some(parent), test);
                }
            }
        }
        for file in error_files {
            file_node(self, file);
        }
    }

    /// Adds one cargo test target's tests (paths like `tests::adds`);
    /// `line` finds a test (module path, function name).
    pub fn add_cargo_target(
        &mut self,
        package: &str,
        target: &CargoTarget,
        tests: &[String],
        mut line: impl FnMut(&[&str], &str) -> Option<(String, usize)>,
    ) {
        let root = self.add(
            None,
            Self::node(
                "cargo".into(),
                "cargo".into(),
                NodeKind::Root,
                Tool::Cargo,
                RunSpec::Cargo {
                    package: None,
                    target: None,
                    filter: None,
                    exact: false,
                },
            ),
        );
        let spec = |filter: Option<String>, exact: bool| RunSpec::Cargo {
            package: Some(package.to_owned()),
            target: Some(target.clone()),
            filter,
            exact,
        };
        let package_node = self.add(
            Some(root),
            Self::node(
                package.to_owned(),
                package.to_owned(),
                NodeKind::Package,
                Tool::Cargo,
                RunSpec::Cargo {
                    package: Some(package.to_owned()),
                    target: None,
                    filter: None,
                    exact: false,
                },
            ),
        );
        let target_id = format!("{package}/{}", target.label().replace(' ', ":"));
        let target_node = self.add(
            Some(package_node),
            Self::node(
                target_id.clone(),
                target.label(),
                NodeKind::Target,
                Tool::Cargo,
                spec(None, false),
            ),
        );
        for test in tests {
            let mut parts: Vec<&str> = test.split("::").collect();
            let Some(name) = parts.pop() else {
                continue;
            };
            let mut parent = target_node;
            for depth in 0..parts.len() {
                let path = parts[..=depth].join("::");
                parent = self.add(
                    Some(parent),
                    Self::node(
                        format!("{target_id}::{path}::"),
                        parts[depth].to_owned(),
                        NodeKind::Module,
                        Tool::Cargo,
                        spec(Some(format!("{path}::")), false),
                    ),
                );
            }
            let mut node = Self::node(
                format!("{target_id}::{test}"),
                name.to_owned(),
                NodeKind::Test,
                Tool::Cargo,
                spec(Some(test.clone()), true),
            );
            if let Some((path, found)) = line(&parts, name) {
                node.path = Some(path);
                node.line = Some(found);
            }
            self.add(Some(parent), node);
        }
    }

    /// The tests (leaves) under `index`, itself included.
    pub fn leaves(&self, index: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack = vec![index];
        while let Some(node) = stack.pop() {
            let children = &self.nodes[node].children;
            if children.is_empty() {
                if self.nodes[node].kind == NodeKind::Test {
                    out.push(node);
                }
            } else {
                stack.extend(children.iter().rev());
            }
        }
        out
    }

    /// Nodes in display order (depth first).
    pub fn preorder(&self) -> Vec<usize> {
        let mut out = Vec::with_capacity(self.nodes.len());
        let mut stack: Vec<usize> = self.roots.iter().rev().copied().collect();
        while let Some(node) = stack.pop() {
            out.push(node);
            stack.extend(self.nodes[node].children.iter().rev());
        }
        out
    }

    /// Status and counts per node from leaf results (and a node's own
    /// result, e.g. a file that failed to import); `running` leaves show
    /// as running.
    pub fn summarize(
        &self,
        results: &HashMap<String, TestResult>,
        running: &HashSet<String>,
    ) -> Vec<Summary> {
        let mut summaries = vec![Summary::default(); self.nodes.len()];
        // Children come after their parents: fold in reverse.
        for index in (0..self.nodes.len()).rev() {
            let node = &self.nodes[index];
            let own = if running.contains(&node.id) {
                Some(Status::Running)
            } else {
                results.get(&node.id).map(|result| result.status)
            };
            let summary = if node.kind == NodeKind::Test {
                let status = own.unwrap_or_default();
                Summary {
                    status,
                    passed: usize::from(status == Status::Passed),
                    failed: usize::from(matches!(status, Status::Failed | Status::Error)),
                    skipped: usize::from(status == Status::Skipped),
                    total: 1,
                }
            } else {
                let mut sum = Summary::default();
                let mut any_running = false;
                for &child in &node.children {
                    let child = summaries[child];
                    sum.passed += child.passed;
                    sum.failed += child.failed;
                    sum.skipped += child.skipped;
                    sum.total += child.total;
                    any_running |= child.status == Status::Running;
                }
                sum.status = if any_running || own == Some(Status::Running) {
                    Status::Running
                } else if matches!(own, Some(Status::Error | Status::Failed)) {
                    Status::Error
                } else if sum.failed > 0 {
                    Status::Failed
                } else if sum.total > 0 && sum.passed + sum.skipped == sum.total {
                    if sum.passed > 0 {
                        Status::Passed
                    } else {
                        Status::Skipped
                    }
                } else {
                    Status::NotRun
                };
                if own == Some(Status::Error) {
                    sum.failed = sum.failed.max(1);
                }
                sum
            };
            summaries[index] = summary;
        }
        summaries
    }

    /// Nodes to show for `filter` (case-insensitive label/id match): the
    /// matches, their ancestors and descendants. `None` without a filter.
    pub fn filtered(&self, filter: &str) -> Option<HashSet<usize>> {
        let needle = filter.trim().to_lowercase();
        if needle.is_empty() {
            return None;
        }
        let mut shown = HashSet::new();
        for (index, node) in self.nodes.iter().enumerate() {
            if node.label.to_lowercase().contains(&needle)
                || node.id.to_lowercase().contains(&needle)
            {
                let mut stack = vec![index];
                while let Some(next) = stack.pop() {
                    if shown.insert(next) {
                        stack.extend(&self.nodes[next].children);
                    }
                }
                let mut parent = node.parent;
                while let Some(up) = parent {
                    shown.insert(up);
                    parent = self.nodes[up].parent;
                }
            }
        }
        Some(shown)
    }

    /// Runs covering `nodes` with as few commands as possible: one
    /// pytest run with every target, one cargo run per target (or for
    /// everything), each with the leaves it reports on.
    pub fn plan(&self, nodes: &[usize]) -> Vec<(RunSpec, Vec<String>)> {
        let mut pytest_targets: Vec<String> = Vec::new();
        let mut pytest_all = false;
        let mut pytest_leaves = Vec::new();
        let mut cargo: BTreeMap<(Option<String>, Option<String>), CargoRequest> = BTreeMap::new();
        // Covered by an ancestor also asked for: skipped.
        let selected: HashSet<usize> = nodes.iter().copied().collect();
        for &index in nodes {
            let mut parent = self.nodes[index].parent;
            let mut covered = false;
            while let Some(up) = parent {
                covered |= selected.contains(&up);
                parent = self.nodes[up].parent;
            }
            if covered {
                continue;
            }
            match &self.nodes[index].run {
                RunSpec::Pytest { target } => {
                    pytest_all |= target.is_empty();
                    if !target.is_empty() {
                        pytest_targets.push(target.clone());
                    }
                    pytest_leaves.extend(
                        self.leaves(index)
                            .into_iter()
                            .map(|leaf| self.nodes[leaf].id.clone()),
                    );
                }
                RunSpec::Cargo {
                    package,
                    target,
                    filter,
                    exact,
                } => {
                    let key = (package.clone(), target.as_ref().map(CargoTarget::label));
                    let request = cargo.entry(key).or_insert_with(|| CargoRequest {
                        package: package.clone(),
                        target: target.clone(),
                        exact: Vec::new(),
                        prefixes: Vec::new(),
                        whole: false,
                    });
                    match filter {
                        Some(filter) if *exact => request.exact.push(filter.clone()),
                        Some(filter) => request.prefixes.push(filter.clone()),
                        None => request.whole = true,
                    }
                }
            }
        }
        let mut plan = Vec::new();
        if pytest_all || !pytest_targets.is_empty() {
            let target = if pytest_all {
                String::new()
            } else {
                pytest_targets.join("\n")
            };
            pytest_leaves.sort();
            pytest_leaves.dedup();
            plan.push((RunSpec::Pytest { target }, pytest_leaves));
        }
        for request in cargo.into_values() {
            let CargoRequest {
                package,
                target,
                mut exact,
                prefixes,
                whole,
            } = request;
            exact.sort();
            exact.dedup();
            let scope = self.cargo_scope(package.as_deref(), target.as_ref());
            let leaves: Vec<String> = scope
                .iter()
                .flat_map(|&node| self.leaves(node))
                .map(|leaf| self.nodes[leaf].id.clone())
                .collect();
            if whole || (exact.is_empty() && prefixes.is_empty()) {
                plan.push((
                    RunSpec::Cargo {
                        package,
                        target,
                        filter: None,
                        exact: false,
                    },
                    leaves,
                ));
                continue;
            }
            // Prefix filters, then exact names (libtest takes one mode).
            for prefix in &prefixes {
                let covered = leaves
                    .iter()
                    .filter(|leaf| {
                        cargo_test_path(leaf).is_some_and(|path| path.starts_with(prefix.as_str()))
                    })
                    .cloned()
                    .collect();
                plan.push((
                    RunSpec::Cargo {
                        package: package.clone(),
                        target: target.clone(),
                        filter: Some(prefix.clone()),
                        exact: false,
                    },
                    covered,
                ));
            }
            let exact: Vec<String> = exact
                .into_iter()
                .filter(|name| {
                    !prefixes
                        .iter()
                        .any(|prefix| name.starts_with(prefix.as_str()))
                })
                .collect();
            if !exact.is_empty() {
                let covered = leaves
                    .iter()
                    .filter(|leaf| {
                        cargo_test_path(leaf)
                            .is_some_and(|path| exact.iter().any(|name| name == path))
                    })
                    .cloned()
                    .collect();
                plan.push((
                    RunSpec::Cargo {
                        package: package.clone(),
                        target: target.clone(),
                        filter: Some(exact.join("\n")),
                        exact: true,
                    },
                    covered,
                ));
            }
        }
        plan
    }

    /// The nodes a cargo spec without filter covers.
    fn cargo_scope(&self, package: Option<&str>, target: Option<&CargoTarget>) -> Vec<usize> {
        let Some(package) = package else {
            return self.get("cargo").into_iter().collect();
        };
        match target {
            None => self.get(package).into_iter().collect(),
            Some(target) => self
                .get(&format!("{package}/{}", target.label().replace(' ', ":")))
                .into_iter()
                .collect(),
        }
    }
}

/// Cargo tests asked for in one target (or package, or everything).
struct CargoRequest {
    package: Option<String>,
    target: Option<CargoTarget>,
    /// Exact test paths.
    exact: Vec<String>,
    /// Module path prefixes (`tests::`).
    prefixes: Vec<String>,
    /// The whole scope.
    whole: bool,
}

/// `tests::adds` of a cargo leaf id `demo/lib::tests::adds`.
pub fn cargo_test_path(id: &str) -> Option<&str> {
    id.split_once("::").map(|(_, path)| path)
}

/// The leaf id of `test` in a cargo target.
pub fn cargo_test_id(package: &str, target: &CargoTarget, test: &str) -> String {
    format!("{package}/{}::{test}", target.label().replace(' ', ":"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pytest_tree() -> TestTree {
        let ids: Vec<String> = [
            "tests/sub/test_b.py::test_b",
            "tests/test_a.py::test_pass",
            "tests/test_a.py::test_double[1-2]",
            "tests/test_a.py::test_double[2-4]",
            "tests/test_a.py::TestGroup::test_inside",
            "test_top.py::test_top",
        ]
        .map(str::to_owned)
        .into();
        let mut tree = TestTree::default();
        tree.add_pytest(
            &ids,
            &["tests/test_c.py".into()],
            |file, classes, function| Some(file.len() + classes.len() * 100 + function.len()),
        );
        tree
    }

    fn labels(tree: &TestTree) -> Vec<String> {
        tree.preorder()
            .into_iter()
            .map(|index| {
                let node = &tree.nodes[index];
                format!("{}{}", "  ".repeat(node.depth), node.label)
            })
            .collect()
    }

    #[test]
    fn pytest_ids_build_folders_files_classes_and_parameter_groups() {
        let tree = pytest_tree();
        assert_eq!(
            labels(&tree),
            [
                "pytest",
                "  tests",
                "    sub",
                "      test_b.py",
                "        test_b",
                "    test_a.py",
                "      test_pass",
                "      test_double",
                "        [1-2]",
                "        [2-4]",
                "      TestGroup",
                "        test_inside",
                "    test_c.py",
                "  test_top.py",
                "    test_top",
            ]
        );
        let group = &tree.nodes[tree.get("tests/test_a.py::test_double").unwrap()];
        assert_eq!(group.kind, NodeKind::Function);
        assert_eq!(
            group.line,
            Some("tests/test_a.py".len() + "test_double".len())
        );
        let case = &tree.nodes[tree.get("tests/test_a.py::test_double[2-4]").unwrap()];
        assert_eq!(case.line, group.line);
        assert_eq!(
            case.run,
            RunSpec::Pytest {
                target: "tests/test_a.py::test_double[2-4]".into()
            }
        );
        let class = &tree.nodes[tree.get("tests/test_a.py::TestGroup").unwrap()];
        assert_eq!(class.line, Some("tests/test_a.py".len() + 100));
        assert_eq!(tree.leaves(tree.get("tests").unwrap()).len(), 5);
    }

    fn result(status: Status) -> TestResult {
        TestResult {
            status,
            message: None,
            output: None,
            duration: None,
        }
    }

    #[test]
    fn statuses_aggregate_up_the_tree() {
        let tree = pytest_tree();
        let mut results: HashMap<String, TestResult> = HashMap::new();
        let status =
            |results: &HashMap<String, TestResult>, running: &HashSet<String>, id: &str| {
                tree.summarize(results, running)[tree.get(id).unwrap()]
            };
        let none = HashSet::new();
        assert_eq!(status(&results, &none, "pytest").status, Status::NotRun);
        for id in [
            "tests/test_a.py::test_pass",
            "tests/test_a.py::test_double[1-2]",
            "tests/sub/test_b.py::test_b",
        ] {
            results.insert(id.into(), result(Status::Passed));
        }
        results.insert(
            "tests/test_a.py::test_double[2-4]".into(),
            result(Status::Skipped),
        );
        // Partially run: not a verdict for the whole file.
        assert_eq!(
            status(&results, &none, "tests/test_a.py").status,
            Status::NotRun
        );
        assert_eq!(
            status(&results, &none, "tests/test_a.py::test_double").status,
            Status::Passed
        );
        assert_eq!(status(&results, &none, "tests/sub").status, Status::Passed);
        results.insert(
            "tests/test_a.py::TestGroup::test_inside".into(),
            result(Status::Failed),
        );
        let file = status(&results, &none, "tests/test_a.py");
        assert_eq!(
            file,
            Summary {
                status: Status::Failed,
                passed: 2,
                failed: 1,
                skipped: 1,
                total: 4
            }
        );
        let running: HashSet<String> = ["tests/sub/test_b.py::test_b".to_owned()].into();
        assert_eq!(status(&results, &running, "tests").status, Status::Running);
        results.insert("tests/test_c.py".into(), result(Status::Error));
        let errored = status(&results, &none, "tests/test_c.py");
        assert_eq!((errored.status, errored.failed), (Status::Error, 1));
        assert_eq!(status(&results, &none, "pytest").failed, 2);
        // Skipped only.
        let mut skipped = HashMap::new();
        skipped.insert("test_top.py::test_top".to_owned(), result(Status::Skipped));
        assert_eq!(
            tree.summarize(&skipped, &none)[tree.get("test_top.py").unwrap()].status,
            Status::Skipped
        );
    }

    #[test]
    fn filter_keeps_ancestors_and_descendants() {
        let tree = pytest_tree();
        let shown = tree.filtered("Group").unwrap();
        let mut ids: Vec<&str> = shown
            .iter()
            .map(|&index| tree.nodes[index].id.as_str())
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            [
                "pytest",
                "tests",
                "tests/test_a.py",
                "tests/test_a.py::TestGroup",
                "tests/test_a.py::TestGroup::test_inside"
            ]
        );
        assert!(tree.filtered("  ").is_none());
    }

    fn cargo_tree() -> TestTree {
        let mut tree = TestTree::default();
        let lib = CargoTarget::Lib;
        tree.add_cargo_target(
            "demo",
            &lib,
            &[
                "tests::adds".into(),
                "tests::fails".into(),
                "tests::nested::deep".into(),
                "top".into(),
            ],
            |modules, name| Some((format!("src/{}.rs", modules.join("/")), name.len())),
        );
        tree.add_cargo_target(
            "demo",
            &CargoTarget::Test("it".into()),
            &["integration".into()],
            |_, _| None,
        );
        tree
    }

    #[test]
    fn cargo_tests_build_packages_targets_and_modules() {
        let tree = cargo_tree();
        assert_eq!(
            labels(&tree),
            [
                "cargo",
                "  demo",
                "    lib",
                "      tests",
                "        adds",
                "        fails",
                "        nested",
                "          deep",
                "      top",
                "    test it",
                "      integration",
            ]
        );
        let deep = &tree.nodes[tree.get("demo/lib::tests::nested::deep").unwrap()];
        assert_eq!(
            (deep.path.as_deref(), deep.line),
            (Some("src/tests/nested.rs"), Some(4))
        );
        assert_eq!(
            tree.nodes[tree.get("demo/lib::tests::").unwrap()].run,
            RunSpec::Cargo {
                package: Some("demo".into()),
                target: Some(CargoTarget::Lib),
                filter: Some("tests::".into()),
                exact: false
            }
        );
        assert_eq!(
            cargo_test_id("demo", &CargoTarget::Test("it".into()), "integration"),
            "demo/test:it::integration"
        );
        assert!(tree.get("demo/test:it::integration").is_some());
    }

    #[test]
    fn plans_group_runs_per_tool_and_target() {
        let tree = pytest_tree();
        let pick = |ids: &[&str]| {
            ids.iter()
                .map(|id| tree.get(id).unwrap())
                .collect::<Vec<_>>()
        };
        let plan = tree.plan(&pick(&[
            "tests/test_a.py::test_pass",
            "tests/sub",
            "tests/sub/test_b.py::test_b",
        ]));
        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan[0],
            (
                RunSpec::Pytest {
                    target: "tests/test_a.py::test_pass\ntests/sub".into()
                },
                vec![
                    "tests/sub/test_b.py::test_b".into(),
                    "tests/test_a.py::test_pass".into()
                ]
            )
        );
        assert_eq!(
            tree.plan(&pick(&["pytest"]))[0].0,
            RunSpec::Pytest {
                target: String::new()
            }
        );

        let tree = cargo_tree();
        let pick = |ids: &[&str]| {
            ids.iter()
                .map(|id| tree.get(id).unwrap())
                .collect::<Vec<_>>()
        };
        let plan = tree.plan(&pick(&[
            "demo/lib::tests::adds",
            "demo/lib::top",
            "demo/lib::tests::nested::",
            "demo/lib::tests::nested::deep",
            "demo/test:it",
        ]));
        let specs: Vec<(Option<String>, bool, usize)> = plan
            .iter()
            .map(|(spec, leaves)| match spec {
                RunSpec::Cargo { filter, exact, .. } => (filter.clone(), *exact, leaves.len()),
                RunSpec::Pytest { .. } => panic!("cargo only"),
            })
            .collect();
        assert_eq!(
            specs,
            [
                (Some("tests::nested::".into()), false, 1),
                (Some("tests::adds\ntop".into()), true, 2),
                (None, false, 1)
            ]
        );
        let all = tree.plan(&pick(&["cargo", "demo/lib::top"]));
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1.len(), 5);
    }
}
