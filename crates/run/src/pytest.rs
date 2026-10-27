//! What pytest prints: `--collect-only -q` node ids and collection
//! errors, JUnit XML results (`--junitxml`, `junit_family=xunit1`), and
//! where a test is defined in its file.

use std::collections::BTreeMap;

use crate::tree::{Status, TestResult};

/// Arguments for a collection run.
pub const COLLECT_ARGS: &[&str] = &["--collect-only", "-q", "--continue-on-collection-errors"];

/// Arguments for a test run reporting to `report` (JUnit XML).
pub fn run_args(report: &str) -> Vec<String> {
    [
        "-q",
        "--continue-on-collection-errors",
        "-o",
        "junit_family=xunit1",
        "-o",
        "junit_logging=all",
    ]
    .iter()
    .map(|&arg| arg.to_owned())
    .chain([format!("--junitxml={report}")])
    .collect()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Collection {
    pub node_ids: Vec<String>,
    /// Files that failed to import, with pytest's report.
    pub errors: Vec<(String, String)>,
}

/// `pytest --collect-only -q` stdout: node ids up to the first blank
/// line, then `ERROR collecting <file>` sections.
pub fn parse_collection(stdout: &str) -> Collection {
    let mut collection = Collection::default();
    let mut lines = stdout.lines();
    for line in lines.by_ref() {
        if line.trim().is_empty() {
            break;
        }
        if line.contains("::") && !line.starts_with(' ') {
            collection.node_ids.push(line.to_owned());
        }
    }
    let mut current: Option<(String, Vec<&str>)> = None;
    let finish = |current: &mut Option<(String, Vec<&str>)>, errors: &mut Vec<(String, String)>| {
        if let Some((file, lines)) = current.take() {
            errors.push((file, lines.join("\n").trim().to_owned()));
        }
    };
    for line in lines {
        let header = line.trim_matches('_').trim();
        if line.starts_with('_') && header.starts_with("ERROR collecting ") {
            finish(&mut current, &mut collection.errors);
            current = Some((header["ERROR collecting ".len()..].to_owned(), Vec::new()));
        } else if line.starts_with("====") {
            finish(&mut current, &mut collection.errors);
        } else if let Some((_, lines)) = &mut current {
            lines.push(line);
        }
    }
    finish(&mut current, &mut collection.errors);
    collection
}

/// A node id's parts: file, classes, function, parameters (`[1-2]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeId<'a> {
    pub file: &'a str,
    pub classes: Vec<&'a str>,
    pub function: &'a str,
    pub params: Option<&'a str>,
}

/// Splits `tests/a.py::TestX::test_y[a::b]` (parameters may hold `::`).
pub fn split_node_id(id: &str) -> Option<NodeId<'_>> {
    let (path, params) = match id.find('[') {
        Some(start) if id.ends_with(']') => (&id[..start], Some(&id[start..])),
        _ => (id, None),
    };
    let mut parts: Vec<&str> = path.split("::").collect();
    if parts.len() < 2 {
        return None;
    }
    let function = parts.pop()?;
    let file = parts.remove(0);
    Some(NodeId {
        file,
        classes: parts,
        function,
        params,
    })
}

/// Results by node id from a JUnit XML report (xunit1: `file` given).
/// File-level failures (collection errors) are keyed by the file.
pub fn parse_junit(xml: &str) -> Result<BTreeMap<String, TestResult>, String> {
    let document =
        roxmltree::Document::parse(xml).map_err(|err| format!("pytest report: {err}"))?;
    let mut results = BTreeMap::new();
    for case in document
        .descendants()
        .filter(|node| node.has_tag_name("testcase"))
    {
        let name = case.attribute("name").unwrap_or_default();
        let classname = case.attribute("classname").unwrap_or_default();
        let file = case
            .attribute("file")
            .map(|file| file.replace('\\', "/"))
            .unwrap_or_else(|| module_file(classname));
        let id = if classname.is_empty() {
            // A module that failed to import: `name` is its dotted path.
            file.clone()
        } else {
            let module = file.trim_end_matches(".py").replace('/', ".");
            let classes = classname
                .strip_prefix(&module)
                .map(|rest| rest.trim_start_matches('.'))
                .unwrap_or_default();
            let mut id = file.clone();
            for class in classes.split('.').filter(|class| !class.is_empty()) {
                id.push_str("::");
                id.push_str(class);
            }
            format!("{id}::{name}")
        };
        let mut result = TestResult {
            status: Status::Passed,
            message: None,
            output: None,
            duration: case.attribute("time").and_then(|time| time.parse().ok()),
        };
        let mut output = Vec::new();
        for child in case.children().filter(|child| child.is_element()) {
            let text = child.text().unwrap_or_default().trim();
            match child.tag_name().name() {
                tag @ ("failure" | "error" | "skipped") => {
                    result.status = match tag {
                        "failure" => Status::Failed,
                        "error" => Status::Error,
                        _ => Status::Skipped,
                    };
                    result.message = child
                        .attribute("message")
                        .map(|message| message.lines().next().unwrap_or_default().to_owned());
                    if !text.is_empty() {
                        output.insert(0, text.to_owned());
                    }
                }
                "system-out" | "system-err" => {
                    let captured = captured_text(text);
                    if !captured.is_empty() {
                        output.push(captured);
                    }
                }
                _ => {}
            }
        }
        if !output.is_empty() {
            result.output = Some(output.join("\n\n"));
        }
        // Setup and call can both report: the worst wins.
        match results.get(&id) {
            Some(existing) if severity(existing) >= severity(&result) => {}
            _ => {
                results.insert(id, result);
            }
        }
    }
    Ok(results)
}

fn severity(result: &TestResult) -> u8 {
    match result.status {
        Status::Error => 4,
        Status::Failed => 3,
        Status::Skipped => 2,
        _ => 1,
    }
}

/// `tests.sub.test_b` → `tests/sub/test_b.py` (without `file`).
fn module_file(classname: &str) -> String {
    format!("{}.py", classname.replace('.', "/"))
}

/// `junit_logging=all` sections minus their empty headers.
fn captured_text(text: &str) -> String {
    let mut out = Vec::new();
    let mut section: Option<(&str, Vec<&str>)> = None;
    let mut flush = |section: &mut Option<(&str, Vec<&str>)>| {
        if let Some((header, lines)) = section.take() {
            let body = lines.join("\n");
            if !body.trim().is_empty() {
                out.push(format!("{header}\n{}", body.trim_end()));
            }
        }
    };
    for line in text.lines() {
        let trimmed = line.trim_matches('-').trim();
        if line.starts_with("---") && trimmed.starts_with("Captured ") {
            flush(&mut section);
            section = Some((trimmed, Vec::new()));
        } else if let Some((_, lines)) = &mut section {
            lines.push(line);
        } else if !line.trim().is_empty() {
            section = Some(("Output", vec![line]));
        }
    }
    flush(&mut section);
    out.join("\n\n")
}

/// The 0-based line defining `function` inside `classes` (nested by
/// indentation), or the innermost class found.
pub fn find_test_line(source: &str, classes: &[&str], function: &str) -> Option<usize> {
    let mut wanted = classes
        .iter()
        .map(|class| (true, *class))
        .collect::<Vec<_>>();
    if !function.is_empty() {
        wanted.push((false, function));
    }
    let mut found = None;
    // Indentation the next definition must exceed.
    let mut indent: Option<usize> = None;
    let mut step = 0;
    for (index, line) in source.lines().enumerate() {
        let Some((is_class, name)) = wanted.get(step) else {
            break;
        };
        let body = line.trim_start();
        let depth = line.len() - body.len();
        if !body.is_empty()
            && indent.is_some_and(|indent| depth <= indent)
            && !body.starts_with('#')
        {
            // Left the class without finding the rest.
            if !body.starts_with('@') && !body.starts_with(')') {
                return found;
            }
        }
        let definition = if *is_class {
            body.strip_prefix("class ")
        } else {
            body.strip_prefix("def ")
                .or_else(|| body.strip_prefix("async def "))
        };
        let matches = definition.is_some_and(|rest| {
            rest.strip_prefix(name)
                .is_some_and(|after| after.starts_with(['(', ':', ' ']))
        });
        if matches && indent.is_none_or(|indent| depth > indent) {
            found = Some(index);
            indent = Some(depth);
            step += 1;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_lists_node_ids_and_errors() {
        let stdout = "tests/sub/test_b.py::test_b
tests/test_a.py::test_double[1-2]
tests/test_a.py::TestGroup::test_inside

==================================== ERRORS ====================================
_______________________ ERROR collecting tests/test_c.py _______________________
ImportError while importing test module '/tmp/rt_py/tests/test_c.py'.
tests/test_c.py:1: in <module>
    import nonexistent_mod
E   ModuleNotFoundError: No module named 'nonexistent_mod'
=========================== short test summary info ============================
ERROR tests/test_c.py
3 tests collected, 1 error in 0.03s
";
        let collection = parse_collection(stdout);
        assert_eq!(
            collection.node_ids,
            [
                "tests/sub/test_b.py::test_b",
                "tests/test_a.py::test_double[1-2]",
                "tests/test_a.py::TestGroup::test_inside"
            ]
        );
        assert_eq!(collection.errors.len(), 1);
        assert_eq!(collection.errors[0].0, "tests/test_c.py");
        assert!(
            collection.errors[0]
                .1
                .ends_with("No module named 'nonexistent_mod'")
        );
        let empty = parse_collection("\nno tests collected in 0.01s\n");
        assert_eq!(empty, Collection::default());
    }

    #[test]
    fn node_ids_split_around_parameters() {
        let id = split_node_id("tests/a.py::TestX::Inner::test_y[a::b-1]").unwrap();
        assert_eq!(id.file, "tests/a.py");
        assert_eq!(id.classes, ["TestX", "Inner"]);
        assert_eq!(id.function, "test_y");
        assert_eq!(id.params, Some("[a::b-1]"));
        assert_eq!(split_node_id("tests/a.py::test_z").unwrap().params, None);
        assert_eq!(split_node_id("tests/a.py"), None);
    }

    #[test]
    fn junit_reports_map_to_node_ids() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?><testsuites name="pytest tests"><testsuite name="pytest" errors="1" failures="1" skipped="1" tests="5">
<testcase classname="" name="tests.test_c" file="tests/test_c.py" time="0.000"><error message="collection failure">ImportError while importing test module
E   ModuleNotFoundError: No module named 'nonexistent_mod'</error></testcase>
<testcase classname="tests.sub.test_b" name="test_b" file="tests/sub/test_b.py" line="0" time="0.001"><system-out>--------------------------------- Captured Log ---------------------------------

--------------------------------- Captured Out ---------------------------------
hello from b

</system-out><system-err>--------------------------------- Captured Err ---------------------------------

</system-err></testcase>
<testcase classname="tests.test_a.TestGroup" name="test_skipped" file="tests/test_a.py" line="16" time="0.000"><skipped type="pytest.skip" message="not today">/tmp/rt_py/tests/test_a.py:17: not today</skipped></testcase>
<testcase classname="tests.test_a" name="test_double[3-7]" file="tests/test_a.py" line="8" time="0.002"><failure message="assert (3 * 2) == 7&#10;more">x = 3, y = 7
E       assert (3 * 2) == 7</failure></testcase>
<testcase classname="tests.test_a" name="test_fixture" file="tests/test_a.py" time="0.0"><error message="failed on setup with &quot;boom&quot;">fixture error</error></testcase>
<testcase classname="tests.test_a" name="test_fixture" file="tests/test_a.py" time="0.0" />
</testsuite></testsuites>"#;
        let results = parse_junit(xml).unwrap();
        let ids: Vec<&str> = results.keys().map(String::as_str).collect();
        assert_eq!(
            ids,
            [
                "tests/sub/test_b.py::test_b",
                "tests/test_a.py::TestGroup::test_skipped",
                "tests/test_a.py::test_double[3-7]",
                "tests/test_a.py::test_fixture",
                "tests/test_c.py"
            ]
        );
        assert_eq!(results["tests/test_c.py"].status, Status::Error);
        let passed = &results["tests/sub/test_b.py::test_b"];
        assert_eq!(passed.status, Status::Passed);
        assert_eq!(passed.output.as_deref(), Some("Captured Out\nhello from b"));
        assert_eq!(passed.duration, Some(0.001));
        let skipped = &results["tests/test_a.py::TestGroup::test_skipped"];
        assert_eq!(
            (skipped.status, skipped.message.as_deref()),
            (Status::Skipped, Some("not today"))
        );
        let failed = &results["tests/test_a.py::test_double[3-7]"];
        assert_eq!(failed.status, Status::Failed);
        assert_eq!(failed.message.as_deref(), Some("assert (3 * 2) == 7"));
        assert!(failed.output.as_deref().unwrap().ends_with("== 7"));
        assert_eq!(
            results["tests/test_a.py::test_fixture"].status,
            Status::Error
        );
        assert!(parse_junit("<not xml").is_err());
    }

    #[test]
    fn test_lines_follow_class_nesting() {
        let source = "import pytest

def test_pass():
    pass

class TestGroup:
    @pytest.mark.skip
    def test_skipped(self):
        pass

    class TestInner:
        async def test_pass(self):
            pass

def test_after():
    pass
";
        assert_eq!(find_test_line(source, &[], "test_pass"), Some(2));
        assert_eq!(
            find_test_line(source, &["TestGroup"], "test_skipped"),
            Some(7)
        );
        assert_eq!(
            find_test_line(source, &["TestGroup", "TestInner"], "test_pass"),
            Some(11)
        );
        assert_eq!(find_test_line(source, &["TestGroup"], ""), Some(5));
        assert_eq!(find_test_line(source, &[], "test_after"), Some(14));
        // Not in the class: the class line is the best answer.
        assert_eq!(
            find_test_line(source, &["TestGroup"], "test_after"),
            Some(5)
        );
    }
}
