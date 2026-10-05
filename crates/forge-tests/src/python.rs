//! Python: each project (a folder with `pyproject.toml`, `pytest.ini`, `setup.cfg`,
//! `tox.ini`, `setup.py` or `requirements.txt`) is a project, each `test_*.py` /
//! `*_test.py` file a class, and each `test*` function, or `test*` method of a `Test*` or
//! `TestCase` class, a test. Runs pytest with the project's virtual environment and a
//! small plugin that writes each test's report as a JSON line.
//!
//! Test ids are `<file>::<Class>::<test>` (pytest's node ids, with the file's canonical
//! path); parametrized cases report under their test.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context as _, Result};
use regex::Regex;
use serde_json::Value;

use crate::discovery::{Kind, NO_ROW, TestClass, TestMethod, TestProject, find_where};
use crate::runner::{self, LiveLog, RunOutput, Scope};
use crate::trx::{CaseResult, Outcome};

/// The files that make a folder a Python project, in the order one is picked as its manifest.
pub const MARKERS: &[&str] = &["pyproject.toml", "pytest.ini", "setup.cfg", "tox.ini", "setup.py", "requirements.txt"];

static FUNCTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:async\s+)?def\s+(test\w*)\s*\(").unwrap());
static CLASS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^class\s+(\w+)\s*(?:\(([^)]*)\))?\s*:").unwrap());

pub fn is_test_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py"))
}

pub fn discover(root: &Path) -> Vec<TestProject> {
    let mut manifests: Vec<PathBuf> = Vec::new();
    for marker in find_where(root, |p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| MARKERS.contains(&n)), None) {
        let dir = marker.parent().unwrap_or(root);
        if !manifests.iter().any(|m| m.parent() == Some(dir)) {
            manifests.push(manifest_in(dir).unwrap_or(marker));
        }
    }
    manifests.into_iter().filter_map(|manifest| load_project(&manifest)).collect()
}

/// The marker that names a project folder, if it is one.
pub fn manifest_in(dir: &Path) -> Option<PathBuf> {
    MARKERS.iter().map(|m| dir.join(m)).find(|p| p.is_file())
}

fn load_project(manifest: &Path) -> Option<TestProject> {
    let dir = manifest.parent()?;
    let mut classes = Vec::new();
    // Nested projects keep their own tests.
    let mut files = find_where(dir, is_test_file, None);
    files.retain(|f| f.ancestors().skip(1).take_while(|a| *a != dir).all(|a| manifest_in(a).is_none()));
    for file in files {
        let Ok(source) = std::fs::read_to_string(&file) else { continue };
        let id_path = canonical(&file);
        let tests: Vec<TestMethod> = file_tests(&source)
            .into_iter()
            .map(|(names, row)| TestMethod { fqn: format!("{id_path}::{}", names.join("::")), name: names.join(" › "), file: file.clone(), row })
            .collect();
        if tests.is_empty() {
            continue;
        }
        let rel = file.strip_prefix(dir).unwrap_or(&file).to_string_lossy().replace('\\', "/");
        classes.push(TestClass { fqn: id_path, name: rel, file: file.clone(), row: NO_ROW, tests });
    }
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Some(TestProject { name, path: manifest.to_path_buf(), kind: Kind::Python, classes })
}

fn canonical(path: &Path) -> String {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()).to_string_lossy().into_owned()
}

/// Each test's names (its class, then its own) and zero-based line, as pytest collects them
/// by default: module-level `test*` functions, and `test*` methods of `Test*` classes or
/// `unittest.TestCase` subclasses.
fn file_tests(source: &str) -> Vec<(Vec<String>, u32)> {
    let mut found = Vec::new();
    // The test class being read, and the indentation of its methods once known.
    let mut class: Option<(String, Option<usize>)> = None;
    for (row, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - trimmed.len();
        if indent == 0 {
            class = CLASS.captures(line).and_then(|c| {
                let name = c[1].to_string();
                let bases = c.get(2).map(|b| b.as_str()).unwrap_or_default();
                (name.starts_with("Test") || bases.contains("TestCase")).then_some((name, None))
            });
        } else if let Some((_, method_indent)) = &mut class {
            method_indent.get_or_insert(indent);
        }
        let Some(name) = FUNCTION.captures(line).map(|c| c[1].to_string()) else { continue };
        match &class {
            _ if indent == 0 => found.push((vec![name], row as u32)),
            Some((class, Some(method_indent))) if *method_indent == indent => found.push((vec![class.clone(), name], row as u32)),
            _ => {}
        }
    }
    found
}

/// The interpreter for a project in `dir`: the nearest `.venv` / `venv` (up to `root`),
/// or `python3` from `PATH`.
pub fn interpreter(dir: &Path, root: Option<&Path>) -> String {
    for ancestor in dir.ancestors() {
        for venv in [".venv", "venv", "env"] {
            let python = ancestor.join(venv).join(if cfg!(windows) { "Scripts/python.exe" } else { "bin/python" });
            if python.is_file() {
                return python.to_string_lossy().into_owned();
            }
        }
        if Some(ancestor) == root {
            break;
        }
    }
    "python3".into()
}

/// Writes one JSON line per test phase to `$FORGE_PYTEST_REPORT`.
const PLUGIN: &str = r#"import json, os

_report = open(os.environ["FORGE_PYTEST_REPORT"], "a", encoding="utf-8")

def pytest_runtest_logreport(report):
    _report.write(json.dumps({
        "nodeid": report.nodeid,
        "when": report.when,
        "outcome": report.outcome,
        "duration": report.duration,
        "message": report.longreprtext if report.failed else "",
    }) + "\n")
    _report.flush()
"#;

/// Node ids to pass to pytest for `scope`, relative to the project when possible.
fn selection(scope: &Scope, dir: &str) -> Vec<String> {
    let relative = |id: &str| id.strip_prefix(dir).map(|r| r.trim_start_matches('/').to_string()).unwrap_or_else(|| id.to_string());
    match scope {
        Scope::Project => vec![],
        Scope::Class(file) => vec![relative(file)],
        Scope::Methods(ids) => ids.iter().map(|id| relative(id)).collect(),
    }
}

pub async fn run(project: &TestProject, scope: &Scope, live: LiveLog, env: &runner::Env) -> Result<RunOutput> {
    let dir = project.dir();
    let work = tempfile::Builder::new().prefix("forge-tests-").tempdir()?;
    std::fs::write(work.path().join("forge_pytest.py"), PLUGIN)?;
    let report = work.path().join("results.jsonl");
    let existing = env.get("PYTHONPATH").cloned().or_else(|| std::env::var("PYTHONPATH").ok()).filter(|p| !p.is_empty());
    let python_path = match existing {
        Some(existing) => format!("{}:{existing}", work.path().display()),
        None => work.path().display().to_string(),
    };
    let mut command = runner::command(&interpreter(dir, None), env);
    command
        .args(["-m", "pytest", "-p", "forge_pytest", "--color=no", "-q"])
        .arg(format!("--rootdir={}", dir.display()))
        .args(selection(scope, &canonical(dir)))
        .env("FORGE_PYTEST_REPORT", &report)
        .env("PYTHONPATH", python_path)
        .env("PYTHONUNBUFFERED", "1")
        .current_dir(dir);
    let output = runner::execute(command, live, None).await.context("failed to start pytest; is Python installed?")?;
    let results = std::fs::read_to_string(&report).map(|lines| parse(&lines, &canonical(dir))).unwrap_or_default();
    // Exit code 5: nothing was collected, which is not a failure of the tests.
    let success = output.success || (results.is_empty() && output.log.contains("no tests ran"));
    Ok(RunOutput { results, log: output.log, success })
}

/// Per-test results from the plugin's report, whose node ids are relative to `dir`.
pub fn parse(lines: &str, dir: &str) -> Vec<CaseResult> {
    // (node id, outcome, duration, messages), in the order tests ran.
    let mut cases: Vec<(String, Outcome, f64, Vec<String>)> = Vec::new();
    for event in lines.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        let field = |k: &str| event.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let (node, when, outcome) = (field("nodeid"), field("when"), field("outcome"));
        let index = match cases.iter().position(|c| c.0 == node) {
            Some(index) => index,
            None => {
                cases.push((node, Outcome::Passed, 0., Vec::new()));
                cases.len() - 1
            }
        };
        let case = &mut cases[index];
        case.2 += event.get("duration").and_then(Value::as_f64).unwrap_or(0.).max(0.);
        match outcome.as_str() {
            "failed" => {
                case.1 = Outcome::Failed;
                let message = field("message");
                if !message.trim().is_empty() {
                    case.3.push(if when == "call" { message } else { format!("Error in {when}:\n{message}") });
                }
            }
            "skipped" if case.1 != Outcome::Failed => case.1 = Outcome::Skipped,
            _ => {}
        }
    }
    cases
        .into_iter()
        .map(|(node, outcome, duration, messages)| {
            // `a/test_x.py::TestA::test_b[1-2]` → `<dir>/a/test_x.py::TestA::test_b`, `TestA › test_b[1-2]`.
            let (path, names) = node.split_once("::").unwrap_or((&node, ""));
            let file = canonical(&Path::new(dir).join(path));
            let base = names.split_once('[').map(|(b, _)| b).unwrap_or(names);
            CaseResult {
                method_fqn: format!("{file}::{base}"),
                display_name: names.replace("::", " › "),
                framework: None,
                outcome,
                duration: Duration::from_secs_f64(duration),
                message: (!messages.is_empty()).then(|| messages.join("\n\n")),
                stack_trace: None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"import pytest
import unittest

def helper():
    pass

def test_adds():
    assert 1 + 1 == 2

@pytest.mark.parametrize("n", [1, 2])
async def test_async(n):
    pass

class TestMath:
    def test_subtracts(self):
        def test_inner():
            pass

    def not_a_test(self):
        pass

class Helpers:
    def test_ignored(self):
        pass

class LegacyCase(unittest.TestCase):
    def test_legacy(self):
        pass
"#;

    #[test]
    fn finds_functions_and_methods() {
        let found = file_tests(SOURCE);
        let expected: Vec<(Vec<String>, u32)> = vec![
            (vec!["test_adds".into()], 6),
            (vec!["test_async".into()], 10),
            (vec!["TestMath".into(), "test_subtracts".into()], 14),
            (vec!["LegacyCase".into(), "test_legacy".into()], 26),
        ];
        assert_eq!(found, expected);
    }

    #[test]
    fn discovers_projects_and_test_files() {
        let root = tempfile::tempdir().unwrap();
        let write = |path: &str, text: &str| {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("api/pyproject.toml", "[project]\nname = \"api\"\n");
        write("api/requirements.txt", "");
        write("api/tests/test_math.py", "def test_adds():\n    pass\n");
        write("api/app/math_test.py", "def test_other():\n    pass\n");
        write("api/.venv/lib/site/test_vendored.py", "def test_x():\n    pass\n");
        write("api/tools/requirements.txt", "");
        write("api/tools/test_tool.py", "def test_tool():\n    pass\n");
        write("scripts/test_loose.py", "def test_loose():\n    pass\n");

        let projects = discover(root.path());
        let found: Vec<(String, Vec<String>)> = projects.iter().map(|p| (p.name.clone(), p.classes.iter().map(|c| c.name.clone()).collect())).collect();
        assert_eq!(
            found,
            [("api".to_string(), vec!["app/math_test.py".to_string(), "tests/test_math.py".to_string()]), ("tools".to_string(), vec!["test_tool.py".to_string()])]
        );
        assert!(projects[0].path.ends_with("pyproject.toml"));
    }

    #[test]
    fn finds_the_virtual_environment() {
        let root = tempfile::tempdir().unwrap();
        let python = root.path().join(".venv/bin/python");
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();
        std::fs::write(&python, "").unwrap();
        let sub = root.path().join("pkg");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(interpreter(&sub, Some(root.path())), python.to_string_lossy());
        assert_eq!(interpreter(Path::new("/nonexistent/project"), Some(Path::new("/nonexistent"))), "python3");
    }

    #[test]
    fn selects_node_ids() {
        let ids = vec!["/p/tests/test_a.py::TestA::test_b".to_string(), "/elsewhere/test_c.py::test_d".to_string()];
        assert_eq!(selection(&Scope::Methods(ids), "/p"), ["tests/test_a.py::TestA::test_b", "/elsewhere/test_c.py::test_d"]);
        assert_eq!(selection(&Scope::Class("/p/test_a.py".into()), "/p"), ["test_a.py"]);
        assert!(selection(&Scope::Project, "/p").is_empty());
    }

    #[test]
    fn parses_the_report() {
        let lines = [
            r#"{"nodeid": "test_a.py::test_ok", "when": "setup", "outcome": "passed", "duration": 0.001, "message": ""}"#,
            r#"{"nodeid": "test_a.py::test_ok", "when": "call", "outcome": "passed", "duration": 0.002, "message": ""}"#,
            r#"{"nodeid": "test_a.py::test_ok", "when": "teardown", "outcome": "passed", "duration": 0.0, "message": ""}"#,
            r#"{"nodeid": "test_a.py::TestA::test_n[1]", "when": "call", "outcome": "failed", "duration": 0.0, "message": "assert 1 == 2"}"#,
            r#"{"nodeid": "test_a.py::TestA::test_n[2]", "when": "call", "outcome": "passed", "duration": 0.0, "message": ""}"#,
            r#"{"nodeid": "test_a.py::test_skip", "when": "setup", "outcome": "skipped", "duration": 0.0, "message": ""}"#,
            r#"{"nodeid": "test_a.py::test_fixture", "when": "setup", "outcome": "failed", "duration": 0.0, "message": "fixture 'db' not found"}"#,
        ]
        .join("\n");
        let results = parse(&lines, "/nonexistent");
        let summary: Vec<(&str, &str, Outcome)> = results.iter().map(|r| (r.method_fqn.as_str(), r.display_name.as_str(), r.outcome)).collect();
        assert_eq!(
            summary,
            [
                ("/nonexistent/test_a.py::test_ok", "test_ok", Outcome::Passed),
                ("/nonexistent/test_a.py::TestA::test_n", "TestA › test_n[1]", Outcome::Failed),
                ("/nonexistent/test_a.py::TestA::test_n", "TestA › test_n[2]", Outcome::Passed),
                ("/nonexistent/test_a.py::test_skip", "test_skip", Outcome::Skipped),
                ("/nonexistent/test_a.py::test_fixture", "test_fixture", Outcome::Failed),
            ]
        );
        assert_eq!(results[0].duration, Duration::from_millis(3));
        assert_eq!(results[1].message.as_deref(), Some("assert 1 == 2"));
        assert_eq!(results[4].message.as_deref(), Some("Error in setup:\nfixture 'db' not found"));
    }

    /// Runs a real project: `FORGE_TESTS_PYTHON_DIR=/path/to/project` with `test_math.py`
    /// holding `test_adds` (passing) and `test_breaks` (failing), pytest installed.
    #[test]
    #[ignore]
    fn runs_a_real_project() {
        let Ok(dir) = std::env::var("FORGE_TESTS_PYTHON_DIR") else { return };
        let project = discover(Path::new(&dir)).remove(0);
        let file = project.classes[0].fqn.clone();
        let all = futures::executor::block_on(run(&project, &Scope::Project, Default::default(), &Default::default())).unwrap();
        let outcome = |name: &str| all.results.iter().find(|r| r.method_fqn == format!("{file}::{name}")).map(|r| r.outcome);
        assert_eq!(outcome("test_adds"), Some(Outcome::Passed), "{}", all.log);
        assert_eq!(outcome("test_breaks"), Some(Outcome::Failed));
        let one = futures::executor::block_on(run(&project, &Scope::Methods(vec![format!("{file}::test_adds")]), Default::default(), &Default::default())).unwrap();
        let ran: Vec<_> = one.results.iter().map(|r| r.method_fqn.clone()).collect();
        assert_eq!(ran, [format!("{file}::test_adds")], "{}", one.log);
    }
}
