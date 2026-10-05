//! TypeScript and JavaScript: each package whose `package.json` uses Jest or Vitest is a
//! project, each `*.test.*` / `*.spec.*` file a class, and each `it`/`test` (inside its
//! `describe`s) a test. Runs the package's own runner with its JSON reporter, which both
//! write in the same shape.
//!
//! Test ids are `<file> > <describe> > … > <test>`, with the file's canonical path.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context as _, Result};
use regex::Regex;
use serde_json::Value;

use crate::discovery::{Kind, NO_ROW, NodeRunner, TestClass, TestMethod, TestProject, find_where};
use crate::runner::{self, LiveLog, RunOutput, Scope};
use crate::trx::{CaseResult, Outcome};

static TEST_FILE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.(test|spec)\.[cm]?[jt]sx?$").unwrap());
const TITLE: &str = r#"(?:'((?:[^'\\]|\\.)*)'|"((?:[^"\\]|\\.)*)"|`([^`$]*)`)"#;
static DESCRIBE: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!(r"\b(?:describe|suite|context)(?:\.(?:only|skip|concurrent|sequential))*\s*\(\s*{TITLE}")).unwrap());
static TEST: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!(r"\b(?:it|test)(?:\.(?:only|skip|todo|concurrent|failing|fails))*\s*\(\s*{TITLE}")).unwrap());

pub fn discover(root: &Path) -> Vec<TestProject> {
    find_where(root, |p| p.file_name().is_some_and(|n| n == "package.json"), None).into_iter().filter_map(|manifest| load_package(&manifest)).collect()
}

/// The package's test runner, from its dependencies and `test` script.
fn runner_of(manifest: &Value) -> Option<NodeRunner> {
    let has = |name: &str| ["dependencies", "devDependencies", "peerDependencies"].iter().any(|k| manifest.get(k).and_then(|d| d.get(name)).is_some());
    let script = manifest.pointer("/scripts/test").and_then(Value::as_str).unwrap_or_default();
    if has("vitest") || script.contains("vitest") {
        Some(NodeRunner::Vitest)
    } else if has("jest") || script.contains("jest") {
        Some(NodeRunner::Jest)
    } else {
        None
    }
}

fn load_package(manifest: &Path) -> Option<TestProject> {
    let json: Value = serde_json::from_str(&std::fs::read_to_string(manifest).ok()?).ok()?;
    let runner = runner_of(&json)?;
    let dir = manifest.parent()?;
    let mut classes = Vec::new();
    for file in find_where(dir, |p| TEST_FILE.is_match(&p.to_string_lossy()), Some("package.json")) {
        let Ok(source) = std::fs::read_to_string(&file) else { continue };
        let id_path = canonical(&file);
        let tests: Vec<TestMethod> = file_tests(&source)
            .into_iter()
            .map(|(titles, row)| TestMethod { fqn: format!("{id_path} > {}", titles.join(" > ")), name: titles.join(" › "), file: file.clone(), row })
            .collect();
        if tests.is_empty() {
            continue;
        }
        let rel = file.strip_prefix(dir).unwrap_or(&file).to_string_lossy().replace('\\', "/");
        classes.push(TestClass { fqn: id_path, name: rel, file: file.clone(), row: NO_ROW, tests });
    }
    let name = json.get("name").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    Some(TestProject { name, path: manifest.to_path_buf(), kind: Kind::Node(runner), classes })
}

fn canonical(path: &Path) -> String {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()).to_string_lossy().into_owned()
}

fn title(captures: &regex::Captures) -> String {
    let raw = captures.get(1).or_else(|| captures.get(2)).or_else(|| captures.get(3)).map(|m| m.as_str()).unwrap_or_default();
    raw.replace("\\'", "'").replace("\\\"", "\"")
}

/// Each test's titles (its `describe`s, then its own) and zero-based line.
fn file_tests(source: &str) -> Vec<(Vec<String>, u32)> {
    let mut found = Vec::new();
    let mut depth = 0i32;
    let mut suites: Vec<(String, i32)> = Vec::new();
    for (row, (raw, code)) in source.lines().zip(code_lines(source)).enumerate() {
        if let Some(captures) = DESCRIBE.captures(raw) {
            suites.push((title(&captures), depth));
        } else if let Some(captures) = TEST.captures(raw) {
            let mut titles: Vec<String> = suites.iter().map(|(t, _)| t.clone()).collect();
            titles.push(title(&captures));
            found.push((titles, row as u32));
        }
        depth += code.matches('{').count() as i32 - code.matches('}').count() as i32;
        while suites.last().is_some_and(|(_, d)| depth <= *d) {
            suites.pop();
        }
    }
    found
}

/// Lines with comments, strings and template literals blanked, for counting braces.
fn code_lines(source: &str) -> Vec<String> {
    let mut lines = vec![String::new()];
    let mut quote: Option<char> = None;
    let mut block_comment = false;
    let chars: Vec<char> = source.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let (c, next) = (chars[i], chars.get(i + 1).copied());
        if c == '\n' {
            lines.push(String::new());
            if matches!(quote, Some('\'') | Some('"')) {
                quote = None;
            }
            i += 1;
            continue;
        }
        let line = lines.last_mut().unwrap();
        if block_comment {
            if c == '*' && next == Some('/') {
                block_comment = false;
                i += 1;
            }
        } else if let Some(q) = quote {
            if c == '\\' {
                i += 1;
            } else if c == q {
                quote = None;
            }
        } else if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        } else if c == '/' && next == Some('*') {
            block_comment = true;
            i += 1;
        } else if matches!(c, '\'' | '"' | '`') {
            quote = Some(c);
        } else {
            line.push(c);
        }
        i += 1;
    }
    lines
}

/// `<file> > a > b` → (file, ["a", "b"]).
fn split_id(id: &str) -> (&str, Vec<&str>) {
    let mut parts = id.split(" > ");
    let file = parts.next().unwrap_or_default();
    (file, parts.collect())
}

/// Files and the test-name pattern to run `scope`. Both runners match the pattern against
/// the test's full name: its describes and title, joined by spaces.
fn selection(scope: &Scope) -> (Vec<String>, Option<String>) {
    match scope {
        Scope::Project => (vec![], None),
        Scope::Class(file) => (vec![file.clone()], None),
        Scope::Methods(ids) => {
            let mut files: Vec<String> = ids.iter().map(|id| split_id(id).0.to_string()).collect();
            files.sort();
            files.dedup();
            let names: Vec<String> = ids.iter().map(|id| format!("^{}$", regex::escape(&split_id(id).1.join(" ")))).collect();
            (files, Some(names.join("|")))
        }
    }
}

pub async fn run(project: &TestProject, scope: &Scope, live: LiveLog, env: &runner::Env) -> Result<RunOutput> {
    let Kind::Node(runner) = project.kind else { anyhow::bail!("not a Jest or Vitest package") };
    let report = tempfile::Builder::new().prefix("forge-tests-").tempdir()?;
    let report_file = report.path().join("results.json");
    let (files, pattern) = selection(scope);
    let mut command = runner::command("npx", env);
    command.arg("--no-install");
    match runner {
        NodeRunner::Jest => {
            command.args(["jest", "--json", "--ci"]).arg(format!("--outputFile={}", report_file.display()));
            // Positional arguments are path patterns.
            command.args(files.iter().map(|f| regex::escape(f)));
        }
        NodeRunner::Vitest => {
            command.args(["vitest", "run", "--reporter=default", "--reporter=json"]).arg(format!("--outputFile.json={}", report_file.display()));
            // Positional arguments are path filters, relative to the package.
            command.args(files.iter().map(|f| Path::new(f).strip_prefix(&canonical(project.dir())).map(|r| r.to_string_lossy().into_owned()).unwrap_or_else(|_| f.clone())));
        }
    }
    if let Some(pattern) = pattern {
        command.arg("-t").arg(pattern);
    }
    command.env("FORCE_COLOR", "0").env("CI", "1").current_dir(project.dir());
    let output = runner::execute(command, live, None).await.context("failed to start the tests with npx; is Node.js installed?")?;
    let results = std::fs::read_to_string(&report_file).ok().map(|json| parse(&json)).unwrap_or_default();
    Ok(RunOutput { results, log: output.log, success: output.success })
}

/// The test runner's own entry point (`jest/bin/jest.js`, `vitest/vitest.mjs`), from the
/// package's `node_modules` or a workspace's above it.
fn runner_program(dir: &Path, runner: NodeRunner) -> Option<PathBuf> {
    let entry = match runner {
        NodeRunner::Jest => "node_modules/jest/bin/jest.js",
        NodeRunner::Vitest => "node_modules/vitest/vitest.mjs",
    };
    dir.ancestors().map(|d| d.join(entry)).find(|p| p.is_file())
}

/// The arguments that run `scope` in one process, without timeouts, so breakpoints hold.
fn debug_args(project: &TestProject, runner: NodeRunner, scope: &Scope) -> Vec<String> {
    let (files, pattern) = selection(scope);
    let mut args: Vec<String> = match runner {
        NodeRunner::Jest => {
            let mut args = vec!["--runInBand".into(), "--watchAll=false".into(), "--testTimeout=86400000".into()];
            args.extend(files.iter().map(|f| regex::escape(f)));
            args
        }
        NodeRunner::Vitest => {
            let mut args = vec!["run".into(), "--no-file-parallelism".into(), "--testTimeout=0".into()];
            args.extend(files.iter().map(|f| Path::new(f).strip_prefix(&canonical(project.dir())).map(|r| r.to_string_lossy().into_owned()).unwrap_or_else(|_| f.clone())));
            args
        }
    };
    if let Some(pattern) = pattern {
        args.extend(["-t".to_string(), pattern]);
    }
    args
}

/// A debug session for `scope`: the runner under js-debug (`pwa-node`), following the
/// processes it starts. `None` when the runner isn't installed in the package.
pub fn debug_scenario(project: &TestProject, scope: &Scope) -> Option<task::DebugScenario> {
    let Kind::Node(runner) = project.kind else { return None };
    let program = runner_program(project.dir(), runner)?;
    let config = serde_json::json!({
        "request": "launch",
        "type": "pwa-node",
        "program": program,
        "args": debug_args(project, runner, scope),
        "cwd": project.dir(),
        "console": "integratedTerminal",
        "autoAttachChildProcesses": true,
        "skipFiles": ["<node_internals>/**"],
    });
    Some(task::DebugScenario { adapter: "JavaScript".into(), label: format!("Debug tests in {}", project.name).into(), build: None, config, tcp_connection: None })
}

/// Results from Jest's (or Vitest's Jest-compatible) JSON report.
pub fn parse(json: &str) -> Vec<CaseResult> {
    let Ok(report) = serde_json::from_str::<Value>(json) else { return vec![] };
    let mut results = Vec::new();
    for file in report.get("testResults").and_then(Value::as_array).into_iter().flatten() {
        let path = canonical(&PathBuf::from(file.get("name").and_then(Value::as_str).unwrap_or_default()));
        for case in file.get("assertionResults").and_then(Value::as_array).into_iter().flatten() {
            let mut titles: Vec<String> = case.get("ancestorTitles").and_then(Value::as_array).into_iter().flatten().filter_map(|t| t.as_str().map(str::to_string)).collect();
            titles.push(case.get("title").and_then(Value::as_str).unwrap_or_default().to_string());
            let outcome = match case.get("status").and_then(Value::as_str) {
                Some("passed") => Outcome::Passed,
                Some("failed") => Outcome::Failed,
                _ => Outcome::Skipped,
            };
            let failures: Vec<&str> = case.get("failureMessages").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
            results.push(CaseResult {
                method_fqn: format!("{path} > {}", titles.join(" > ")),
                display_name: titles.join(" › "),
                framework: None,
                outcome,
                duration: Duration::from_secs_f64(case.get("duration").and_then(Value::as_f64).unwrap_or(0.).max(0.) / 1000.),
                message: (!failures.is_empty()).then(|| strip_ansi(&failures.join("\n\n"))),
                stack_trace: None,
            });
        }
    }
    results
}

fn strip_ansi(text: &str) -> String {
    static ANSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*m").unwrap());
    ANSI.replace_all(text, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"import { describe, it, expect } from 'vitest';

describe('math', () => {
  it('adds', () => {
    expect(1 + 1).toBe(2); // }
  });

  describe("with negatives", () => {
    test.skip(`subtracts`, () => {
      const s = "}{";
      expect(1 - 2).toBe(-1);
    });
  });
});

it('stands alone', async () => {});
"#;

    #[test]
    fn finds_nested_tests() {
        let found = file_tests(SOURCE);
        assert_eq!(found, [(vec!["math".into(), "adds".into()], 3), (vec!["math".into(), "with negatives".into(), "subtracts".into()], 8), (vec!["stands alone".to_string()], 15)]);
    }

    #[test]
    fn detects_the_runner() {
        let manifest = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        assert_eq!(runner_of(&manifest(r#"{"devDependencies": {"vitest": "^3"}}"#)), Some(NodeRunner::Vitest));
        assert_eq!(runner_of(&manifest(r#"{"devDependencies": {"jest": "^29"}}"#)), Some(NodeRunner::Jest));
        assert_eq!(runner_of(&manifest(r#"{"scripts": {"test": "jest --coverage"}}"#)), Some(NodeRunner::Jest));
        assert_eq!(runner_of(&manifest(r#"{"dependencies": {"react": "^19"}}"#)), None);
    }

    #[test]
    fn debugs_with_the_packages_runner() {
        let root = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(root.path()).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/vitest")).unwrap();
        std::fs::write(dir.join("node_modules/vitest/vitest.mjs"), "").unwrap();
        std::fs::create_dir_all(dir.join("web")).unwrap();
        let project = |runner| TestProject { name: "web".into(), path: dir.join("web/package.json"), kind: Kind::Node(runner), classes: vec![] };
        let file = dir.join("web/a.test.ts").to_string_lossy().into_owned();
        let scenario = debug_scenario(&project(NodeRunner::Vitest), &Scope::Methods(vec![format!("{file} > math > adds")])).expect("vitest is installed above the package");
        assert_eq!(scenario.adapter.as_ref(), "JavaScript");
        assert_eq!(scenario.config["program"], serde_json::json!(dir.join("node_modules/vitest/vitest.mjs")));
        assert_eq!(scenario.config["args"], serde_json::json!(["run", "--no-file-parallelism", "--testTimeout=0", "a.test.ts", "-t", "^math adds$"]), "vitest filters by paths relative to the package");
        assert!(debug_scenario(&project(NodeRunner::Jest), &Scope::Project).is_none(), "jest isn't installed");
    }

    #[test]
    fn selects_files_and_names() {
        let (files, pattern) = selection(&Scope::Methods(vec!["/p/a.test.ts > math > adds (1+1)".into(), "/p/a.test.ts > b".into()]));
        assert_eq!(files, ["/p/a.test.ts"]);
        assert_eq!(pattern.as_deref(), Some(r"^math adds \(1\+1\)$|^b$"));
        assert_eq!(selection(&Scope::Class("/p/a.test.ts".into())), (vec!["/p/a.test.ts".to_string()], None));
    }

    /// Runs a real package: `FORGE_TESTS_NODE_DIR=/path/to/package` with `math > adds`
    /// passing and `math > breaks` failing (Jest or Vitest, installed).
    #[test]
    #[ignore]
    fn runs_a_real_package() {
        let Ok(dir) = std::env::var("FORGE_TESTS_NODE_DIR") else { return };
        let project = discover(Path::new(&dir)).remove(0);
        let file = project.classes[0].fqn.clone();
        let all = futures::executor::block_on(run(&project, &Scope::Project, Default::default(), &Default::default())).unwrap();
        let outcome = |name: &str| all.results.iter().find(|r| r.method_fqn == format!("{file} > {name}")).map(|r| r.outcome);
        assert_eq!(outcome("math > adds"), Some(Outcome::Passed), "{}", all.log);
        assert_eq!(outcome("math > breaks"), Some(Outcome::Failed));
        let one = futures::executor::block_on(run(&project, &Scope::Methods(vec![format!("{file} > math > adds")]), Default::default(), &Default::default())).unwrap();
        let ran: Vec<_> = one.results.iter().filter(|r| r.outcome != Outcome::Skipped).map(|r| r.method_fqn.clone()).collect();
        assert_eq!(ran, [format!("{file} > math > adds")], "{}", one.log);
    }

    #[test]
    fn parses_the_json_report() {
        let json = r#"{"testResults": [{"name": "/nonexistent/a.test.ts", "assertionResults": [
            {"ancestorTitles": ["math"], "title": "adds", "status": "passed", "duration": 3, "failureMessages": []},
            {"ancestorTitles": ["math"], "title": "breaks", "status": "failed", "duration": null, "failureMessages": ["\u001b[31mExpected 2\u001b[39m"]},
            {"ancestorTitles": [], "title": "later", "status": "pending", "failureMessages": []}
        ]}]}"#;
        let results = parse(json);
        let summary: Vec<(&str, Outcome)> = results.iter().map(|r| (r.method_fqn.as_str(), r.outcome)).collect();
        assert_eq!(summary, [("/nonexistent/a.test.ts > math > adds", Outcome::Passed), ("/nonexistent/a.test.ts > math > breaks", Outcome::Failed), ("/nonexistent/a.test.ts > later", Outcome::Skipped)]);
        assert_eq!(results[1].message.as_deref(), Some("Expected 2"));
        assert_eq!(results[0].duration, Duration::from_millis(3));
    }
}
