//! What cargo and libtest print: `cargo metadata`, `--message-format=json`
//! build messages (executables, errors), `--list --format terse` and test
//! results, plus finding a test function's line in its sources.

use std::collections::BTreeMap;

use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::{
    config::CargoTarget,
    tree::{Status, TestResult},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Package {
    pub name: String,
    /// Directory of its `Cargo.toml`.
    pub dir: String,
    pub targets: Vec<Target>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Target {
    pub name: String,
    pub target: CargoTarget,
    pub src_path: String,
}

/// The workspace members of `cargo metadata --no-deps --format-version 1`.
pub fn parse_metadata(json: &str) -> Result<Vec<Package>, String> {
    let root: Value = serde_json::from_str(json).map_err(|err| format!("cargo metadata: {err}"))?;
    let members: Vec<&str> = root["workspace_members"]
        .as_array()
        .map(|ids| ids.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let packages = root["packages"]
        .as_array()
        .ok_or("cargo metadata: no packages")?;
    Ok(packages
        .iter()
        .filter(|package| {
            members.is_empty()
                || package["id"]
                    .as_str()
                    .is_some_and(|id| members.contains(&id))
        })
        .filter_map(|package| {
            let manifest = package["manifest_path"].as_str()?;
            let dir = manifest
                .rfind(['/', '\\'])
                .map_or(manifest, |end| &manifest[..end]);
            Some(Package {
                name: package["name"].as_str()?.to_owned(),
                dir: dir.to_owned(),
                targets: package["targets"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|target| {
                        let name = target["name"].as_str()?;
                        let kind = target["kind"].as_array()?.first()?.as_str()?;
                        Some(Target {
                            name: name.to_owned(),
                            target: CargoTarget::from_kind(kind, name)?,
                            src_path: target["src_path"].as_str()?.to_owned(),
                        })
                    })
                    .collect(),
            })
        })
        .collect())
}

/// An executable cargo built.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Artifact {
    pub target: CargoTarget,
    pub src_path: String,
    pub executable: String,
    /// Built as a test harness (`cargo test --no-run`).
    pub test: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Build {
    pub artifacts: Vec<Artifact>,
    /// `build-finished`'s verdict, if cargo got that far.
    pub success: Option<bool>,
    /// Rendered compiler errors.
    pub errors: Vec<String>,
}

impl Build {
    /// The single executable to debug, or why there is none.
    pub fn executable(&self, test: bool) -> Result<&Artifact, String> {
        if self.success == Some(false) || !self.errors.is_empty() {
            let errors = self.errors.join("\n");
            return Err(if errors.is_empty() {
                "the build failed".into()
            } else {
                format!("the build failed:\n{}", errors.trim_end())
            });
        }
        let candidates: Vec<&Artifact> = self.artifacts.iter().filter(|a| a.test == test).collect();
        match candidates.as_slice() {
            [single] => Ok(single),
            [] => Err("cargo built no executable for this configuration".into()),
            several => Err(format!(
                "cargo built {} executables ({}); pick one target (bin, test or lib) in the configuration",
                several.len(),
                several
                    .iter()
                    .map(|artifact| artifact.target.label())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

/// Lines of `cargo build|test --message-format=json…` stdout.
pub fn parse_build(stdout: &str) -> Build {
    let mut build = Build::default();
    for line in stdout.lines() {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match message["reason"].as_str() {
            Some("compiler-artifact") => {
                let Some(executable) = message["executable"].as_str() else {
                    continue;
                };
                let target = &message["target"];
                let name = target["name"].as_str().unwrap_or_default();
                let kind = target["kind"][0].as_str().unwrap_or_default();
                if let Some(cargo_target) = CargoTarget::from_kind(kind, name) {
                    build.artifacts.push(Artifact {
                        target: cargo_target,
                        src_path: target["src_path"].as_str().unwrap_or_default().to_owned(),
                        executable: executable.to_owned(),
                        test: message["profile"]["test"].as_bool().unwrap_or(false),
                    });
                }
            }
            Some("compiler-message") if message["message"]["level"] == "error" => {
                if let Some(rendered) = message["message"]["rendered"].as_str() {
                    build.errors.push(rendered.to_owned());
                }
            }
            Some("build-finished") => build.success = message["success"].as_bool(),
            _ => {}
        }
    }
    build
}

/// Test names from `<test binary> --list --format terse` (`name: test`).
pub fn parse_list(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| line.strip_suffix(": test"))
        .map(str::to_owned)
        .collect()
}

/// Results from libtest's stdout (`test a::b ... ok`), with each failed
/// test's captured output and panic message.
pub fn parse_results(stdout: &str) -> BTreeMap<String, TestResult> {
    let mut results = BTreeMap::new();
    for line in stdout.lines() {
        let Some(rest) = line.strip_prefix("test ") else {
            continue;
        };
        let Some((name, verdict)) = rest.split_once(" ... ") else {
            continue;
        };
        let name = name.strip_suffix(" - should panic").unwrap_or(name);
        let (status, message) = match verdict.trim() {
            "ok" => (Status::Passed, None),
            "FAILED" => (Status::Failed, None),
            "ignored" => (Status::Skipped, None),
            other => match other.strip_prefix("ignored, ") {
                Some(reason) => (Status::Skipped, Some(reason.to_owned())),
                None => continue,
            },
        };
        results.insert(
            name.to_owned(),
            TestResult {
                status,
                message,
                output: None,
                duration: None,
            },
        );
    }
    // `---- name stdout ----` blocks, up to the next block or the list of
    // failures.
    let mut current: Option<(String, Vec<&str>)> = None;
    let mut flush = |current: &mut Option<(String, Vec<&str>)>| {
        if let Some((name, lines)) = current.take()
            && let Some(result) = results.get_mut(&name)
        {
            let output = lines.join("\n").trim().to_owned();
            result.message = panic_message(&output).or(result.message.take());
            result.output = Some(output);
        }
    };
    for line in stdout.lines() {
        if let Some(name) = line
            .strip_prefix("---- ")
            .and_then(|rest| rest.strip_suffix(" stdout ----"))
        {
            flush(&mut current);
            current = Some((name.to_owned(), Vec::new()));
        } else if line == "failures:" || line.starts_with("test result: ") {
            flush(&mut current);
        } else if let Some((_, lines)) = &mut current {
            lines.push(line);
        }
    }
    flush(&mut current);
    results
}

/// The line after `thread '…' panicked at file:line:col:`.
fn panic_message(output: &str) -> Option<String> {
    let mut lines = output.lines();
    lines.find(|line| line.contains("panicked at"))?;
    let message = lines.next()?.trim();
    (!message.is_empty()).then(|| message.to_owned())
}

/// Files that may hold module `modules` of the crate whose root file is
/// `src_path` (`/`- or `\`-separated as given), best first: the module's
/// own file, then each enclosing module's file (inline `mod`s), then the
/// root.
pub fn module_files(src_path: &str, modules: &[&str]) -> Vec<String> {
    let separator = if src_path.contains('\\') && !src_path.contains('/') {
        '\\'
    } else {
        '/'
    };
    let (dir, file) = match src_path.rfind(['/', '\\']) {
        Some(end) => (&src_path[..end], &src_path[end + 1..]),
        None => ("", src_path),
    };
    // Children of `lib.rs`/`main.rs`/`mod.rs` live beside them; of
    // `foo.rs`, in `foo/`.
    let base = match file {
        "lib.rs" | "main.rs" | "mod.rs" => dir.to_owned(),
        other => format!("{dir}{separator}{}", other.trim_end_matches(".rs")),
    };
    let mut candidates = Vec::new();
    for depth in (1..=modules.len()).rev() {
        let path = modules[..depth].join(&separator.to_string());
        candidates.push(format!("{base}{separator}{path}.rs"));
        candidates.push(format!("{base}{separator}{path}{separator}mod.rs"));
    }
    candidates.push(src_path.to_owned());
    candidates
}

/// The 0-based line of `fn name`, if any.
pub fn find_fn_line(source: &str, name: &str) -> Option<usize> {
    let pattern = Regex::new(&format!(r"\bfn\s+{}\s*[<(]", regex::escape(name))).ok()?;
    let found = pattern.find(source)?;
    Some(source[..found.start()].matches('\n').count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_lists_workspace_members_with_targets() {
        let json = r#"{
            "packages": [{
                "id": "path+file:///p#demo@0.1.0", "name": "demo",
                "manifest_path": "/p/Cargo.toml",
                "targets": [
                    { "name": "demo", "kind": ["lib"], "src_path": "/p/src/lib.rs" },
                    { "name": "tool", "kind": ["bin"], "src_path": "/p/src/bin/tool.rs" },
                    { "name": "it", "kind": ["test"], "src_path": "/p/tests/it.rs" },
                    { "name": "build-script-build", "kind": ["custom-build"], "src_path": "/p/build.rs" }
                ]
            }],
            "workspace_members": ["path+file:///p#demo@0.1.0"]
        }"#;
        let packages = parse_metadata(json).unwrap();
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].dir, "/p");
        let targets: Vec<CargoTarget> = packages[0]
            .targets
            .iter()
            .map(|t| t.target.clone())
            .collect();
        assert_eq!(
            targets,
            [
                CargoTarget::Lib,
                CargoTarget::Bin("tool".into()),
                CargoTarget::Test("it".into())
            ]
        );
    }

    const BUILD: &str = r#"{"reason":"compiler-artifact","package_id":"path+file:///p#demo@0.1.0","target":{"kind":["lib"],"crate_types":["lib"],"name":"demo","src_path":"/p/src/lib.rs","test":true},"profile":{"test":false},"executable":null,"fresh":true}
{"reason":"compiler-artifact","package_id":"path+file:///p#demo@0.1.0","target":{"kind":["bin"],"name":"tool","src_path":"/p/src/bin/tool.rs"},"profile":{"test":true},"executable":"/t/debug/deps/tool-9f27","fresh":false}
{"reason":"compiler-artifact","package_id":"path+file:///p#demo@0.1.0","target":{"kind":["lib"],"name":"demo","src_path":"/p/src/lib.rs"},"profile":{"test":true},"executable":"/t/debug/deps/demo-0157","fresh":false}
{"reason":"compiler-artifact","package_id":"path+file:///p#demo@0.1.0","target":{"kind":["bin"],"name":"tool","src_path":"/p/src/bin/tool.rs"},"profile":{"test":false},"executable":"/t/debug/tool","fresh":false}
{"reason":"build-finished","success":true}"#;

    #[test]
    fn build_messages_give_executables() {
        let build = parse_build(BUILD);
        assert_eq!(build.success, Some(true));
        assert_eq!(build.artifacts.len(), 3);
        assert_eq!(build.executable(false).unwrap().executable, "/t/debug/tool");
        assert!(
            build
                .executable(true)
                .unwrap_err()
                .contains("2 executables (bin tool, lib)")
        );
        let failed = parse_build(
            r#"{"reason":"compiler-message","message":{"level":"error","rendered":"error[E0308]: mismatched types\n"}}
{"reason":"compiler-message","message":{"level":"warning","rendered":"warning: unused\n"}}
{"reason":"build-finished","success":false}"#,
        );
        assert_eq!(
            failed.executable(false).unwrap_err(),
            "the build failed:\nerror[E0308]: mismatched types"
        );
    }

    #[test]
    fn list_and_results_parse() {
        assert_eq!(
            parse_list("tests::adds: test\ntests::nested::deep: test\nbench_x: benchmark\n\n"),
            ["tests::adds", "tests::nested::deep"]
        );
        let stdout = "
running 5 tests
test tests::ignored ... ignored, slow
test tests::plain_ignored ... ignored
test tests::adds ... ok
test tests::panics - should panic ... ok
test tests::fails ... FAILED

failures:

---- tests::fails stdout ----
some output

thread 'tests::fails' (133494) panicked at src/lib.rs:11:18:
assertion `left == right` failed: bad math
  left: 3
 right: 4
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    tests::fails

test result: FAILED. 2 passed; 1 failed; 2 ignored; 0 measured; 0 filtered out; finished in 0.00s
";
        let results = parse_results(stdout);
        assert_eq!(results.len(), 5);
        assert_eq!(results["tests::adds"].status, Status::Passed);
        assert_eq!(results["tests::panics"].status, Status::Passed);
        assert_eq!(results["tests::ignored"].status, Status::Skipped);
        assert_eq!(results["tests::ignored"].message.as_deref(), Some("slow"));
        assert_eq!(results["tests::plain_ignored"].message, None);
        let failed = &results["tests::fails"];
        assert_eq!(failed.status, Status::Failed);
        assert_eq!(
            failed.message.as_deref(),
            Some("assertion `left == right` failed: bad math")
        );
        let output = failed.output.as_deref().unwrap();
        assert!(output.starts_with("some output"), "{output}");
        assert!(output.ends_with("display a backtrace"), "{output}");
    }

    #[test]
    fn module_files_and_fn_lines() {
        assert_eq!(
            module_files("/p/src/lib.rs", &["tests", "nested"]),
            [
                "/p/src/tests/nested.rs",
                "/p/src/tests/nested/mod.rs",
                "/p/src/tests.rs",
                "/p/src/tests/mod.rs",
                "/p/src/lib.rs"
            ]
        );
        assert_eq!(
            module_files(r"C:\p\src\parser.rs", &["tests"]),
            [
                r"C:\p\src\parser\tests.rs",
                r"C:\p\src\parser\tests\mod.rs",
                r"C:\p\src\parser.rs"
            ]
        );
        assert_eq!(module_files("/p/tests/it.rs", &[]), ["/p/tests/it.rs"]);
        let source = "mod tests {\n    #[test]\n    fn adds_more() {}\n    #[test]\n    async fn adds<T>() {}\n}\n";
        assert_eq!(find_fn_line(source, "adds"), Some(4));
        assert_eq!(find_fn_line(source, "missing"), None);
    }
}
