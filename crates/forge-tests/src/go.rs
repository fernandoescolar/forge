//! Go: each module (`go.mod`) is a project, each package with `_test.go` files a class,
//! and each `func TestXxx(t *testing.T)` a test. Runs `go test -json`.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context as _, Result};
use regex::Regex;
use serde_json::Value;

use crate::discovery::{Kind, NO_ROW, TestClass, TestMethod, TestProject, find_where};
use crate::runner::{self, LiveLog, RunOutput, Scope};
use crate::trx::{CaseResult, Outcome};

static TEST_FN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^func\s+(Test(?:[A-Z0-9_]\w*)?)\s*\(\s*\w+\s+\*testing\.T\s*\)").unwrap());

pub fn discover(root: &Path) -> Vec<TestProject> {
    find_where(root, |p| p.file_name().is_some_and(|n| n == "go.mod"), None).into_iter().filter_map(|go_mod| load_module(&go_mod)).collect()
}

pub(crate) fn module_path(go_mod: &Path) -> Option<String> {
    let text = std::fs::read_to_string(go_mod).ok()?;
    text.lines().find_map(|l| l.trim().strip_prefix("module ")).map(|m| m.trim().trim_matches('"').to_string())
}

fn load_module(go_mod: &Path) -> Option<TestProject> {
    let module = module_path(go_mod)?;
    let dir = go_mod.parent()?;
    let mut packages: BTreeMap<String, TestClass> = BTreeMap::new();
    for file in find_where(dir, |p| p.to_string_lossy().ends_with("_test.go"), Some("go.mod")) {
        let Ok(source) = std::fs::read_to_string(&file) else { continue };
        let rel = file.parent().and_then(|d| d.strip_prefix(dir).ok()).map(|d| d.to_string_lossy().replace('\\', "/")).unwrap_or_default();
        let import = if rel.is_empty() { module.clone() } else { format!("{module}/{rel}") };
        for (row, line) in source.lines().enumerate() {
            let Some(name) = TEST_FN.captures(line).map(|c| c[1].to_string()) else { continue };
            let class = packages.entry(import.clone()).or_insert_with(|| TestClass {
                fqn: import.clone(),
                // The module's own package is named like the module.
                name: if rel.is_empty() { module.rsplit('/').next().unwrap_or(&module).to_string() } else { rel.clone() },
                file: file.clone(),
                row: NO_ROW,
                tests: Vec::new(),
            });
            class.tests.push(TestMethod { fqn: format!("{import}.{name}"), name, file: file.clone(), row: row as u32 });
        }
    }
    let name = module.rsplit('/').next().unwrap_or(&module).to_string();
    Some(TestProject { name, path: go_mod.to_path_buf(), kind: Kind::Go, classes: packages.into_values().collect() })
}

/// `./rel/dir` for a package import path within `module`.
fn package_arg(import: &str, module: &str) -> String {
    match import.strip_prefix(module) {
        Some("") | None => ".".into(),
        Some(rest) => format!(".{rest}"),
    }
}

/// The `go test` arguments (after `-json`) for `scope`.
pub(crate) fn args(scope: &Scope, module: &str) -> Vec<String> {
    match scope {
        Scope::Project => vec!["./...".into()],
        Scope::Class(import) => vec![package_arg(import, module)],
        Scope::Methods(fqns) => {
            let mut packages: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for fqn in fqns {
                if let Some((import, name)) = fqn.rsplit_once('.') {
                    packages.entry(package_arg(import, module)).or_default().push(regex::escape(name));
                }
            }
            let names: Vec<String> = packages.values().flatten().cloned().collect();
            let mut args: Vec<String> = packages.into_keys().collect();
            args.push("-run".into());
            args.push(format!("^({})$", names.join("|")));
            args
        }
    }
}

pub async fn run(project: &TestProject, scope: &Scope, live: LiveLog, env: &runner::Env) -> Result<RunOutput> {
    let module = module_path(&project.path).context("no module line in go.mod")?;
    let mut command = runner::command("go", env);
    command.args(["test", "-json", "-count=1"]).args(args(scope, &module)).current_dir(project.dir());
    let output = runner::execute(command, live, Some(readable)).await.context("failed to start `go test`; is Go installed?")?;
    Ok(RunOutput { results: parse(&output.stdout), log: output.log, success: output.success })
}

/// The human part of a `go test -json` line.
fn readable(line: &str) -> Option<String> {
    let event: Value = serde_json::from_str(line).ok()?;
    event.get("Output").and_then(Value::as_str).map(str::to_string)
}

/// Per-test results from `go test -json` output. Subtests count toward their test.
pub fn parse(stdout: &str) -> Vec<CaseResult> {
    let mut output: HashMap<(String, String), String> = HashMap::new();
    let mut results = Vec::new();
    for event in stdout.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        let field = |k: &str| event.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let (package, test, action) = (field("Package"), field("Test"), field("Action"));
        if test.is_empty() {
            continue;
        }
        let top = test.split('/').next().unwrap_or(&test).to_string();
        match action.as_str() {
            "output" => {
                let text = field("Output");
                let noise = ["=== RUN", "=== PAUSE", "=== CONT", "=== NAME"].iter().any(|p| text.trim_start().starts_with(p));
                if !noise {
                    output.entry((package, top)).or_default().push_str(&text);
                }
            }
            "pass" | "fail" | "skip" if test == top => {
                let outcome = match action.as_str() {
                    "pass" => Outcome::Passed,
                    "fail" => Outcome::Failed,
                    _ => Outcome::Skipped,
                };
                let text = output.remove(&(package.clone(), top.clone())).unwrap_or_default();
                let duration = Duration::from_secs_f64(event.get("Elapsed").and_then(Value::as_f64).unwrap_or(0.).max(0.));
                results.push(CaseResult {
                    method_fqn: format!("{package}.{top}"),
                    display_name: top,
                    framework: None,
                    outcome,
                    duration,
                    message: (outcome == Outcome::Failed && !text.trim().is_empty()).then(|| text.trim_end().to_string()),
                    stack_trace: None,
                });
            }
            _ => {}
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_packages_and_tests() {
        let root = tempfile::tempdir().unwrap();
        let write = |path: &str, text: &str| {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("go.mod", "module example.com/calc\n\ngo 1.22\n");
        write("calc_test.go", "package calc\n\nimport \"testing\"\n\nfunc TestAdd(t *testing.T) {}\nfunc TestMain(m *testing.M) {}\nfunc helper(t *testing.T) {}\n");
        write("internal/parse/parse_test.go", "package parse\nimport \"testing\"\nfunc TestParse(t *testing.T) {}\nfunc Test_edge(tt *testing.T) {}\n");
        let projects = discover(root.path());
        assert_eq!(projects.len(), 1);
        let project = &projects[0];
        assert_eq!((project.name.as_str(), &project.kind), ("calc", &Kind::Go));
        let classes: Vec<(&str, Vec<&str>)> = project.classes.iter().map(|c| (c.fqn.as_str(), c.tests.iter().map(|t| t.fqn.as_str()).collect())).collect();
        assert_eq!(
            classes,
            [
                ("example.com/calc", vec!["example.com/calc.TestAdd"]),
                ("example.com/calc/internal/parse", vec!["example.com/calc/internal/parse.TestParse", "example.com/calc/internal/parse.Test_edge"]),
            ]
        );
        assert_eq!(project.classes[0].tests[0].row, 4);
    }

    #[test]
    fn builds_arguments() {
        let m = "example.com/calc";
        assert_eq!(args(&Scope::Project, m), ["./..."]);
        assert_eq!(args(&Scope::Class("example.com/calc/internal/parse".into()), m), ["./internal/parse"]);
        assert_eq!(args(&Scope::Methods(vec!["example.com/calc.TestAdd".into(), "example.com/calc.TestSub".into()]), m), [".", "-run", "^(TestAdd|TestSub)$"]);
    }

    #[test]
    fn parses_go_test_json() {
        let out = [
            r#"{"Action":"run","Package":"ex/calc","Test":"TestAdd"}"#,
            r#"{"Action":"output","Package":"ex/calc","Test":"TestAdd","Output":"=== RUN   TestAdd\n"}"#,
            r#"{"Action":"pass","Package":"ex/calc","Test":"TestAdd","Elapsed":0.01}"#,
            r#"{"Action":"output","Package":"ex/calc","Test":"TestSub/neg","Output":"    calc_test.go:12: got 1, want 2\n"}"#,
            r#"{"Action":"fail","Package":"ex/calc","Test":"TestSub/neg","Elapsed":0}"#,
            r#"{"Action":"fail","Package":"ex/calc","Test":"TestSub","Elapsed":0.02}"#,
            r#"{"Action":"skip","Package":"ex/calc","Test":"TestLater","Elapsed":0}"#,
            r#"{"Action":"fail","Package":"ex/calc","Elapsed":0.3}"#,
        ]
        .join("\n");
        let results = parse(&out);
        let summary: Vec<(&str, Outcome)> = results.iter().map(|r| (r.method_fqn.as_str(), r.outcome)).collect();
        assert_eq!(summary, [("ex/calc.TestAdd", Outcome::Passed), ("ex/calc.TestSub", Outcome::Failed), ("ex/calc.TestLater", Outcome::Skipped)]);
        assert_eq!(results[1].message.as_deref(), Some("    calc_test.go:12: got 1, want 2"));
        assert_eq!(readable(r#"{"Action":"output","Output":"ok  \tex/calc\n"}"#).as_deref(), Some("ok  \tex/calc\n"));
    }

    /// Runs `go test` for real on a small module.
    #[test]
    fn runs_go_test() {
        if std::process::Command::new("go").arg("version").output().is_err() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("go.mod"), "module example.com/calc\n\ngo 1.21\n").unwrap();
        std::fs::write(root.path().join("calc.go"), "package calc\n\nfunc Add(a, b int) int { return a + b }\n").unwrap();
        std::fs::write(
            root.path().join("calc_test.go"),
            "package calc\n\nimport \"testing\"\n\nfunc TestAdd(t *testing.T) {\n\tif Add(2, 2) != 4 {\n\t\tt.Fatal(\"no\")\n\t}\n}\n\nfunc TestBroken(t *testing.T) {\n\tt.Errorf(\"want %d, got %d\", 5, Add(2, 2))\n}\n",
        )
        .unwrap();
        let project = discover(root.path()).remove(0);
        let all = futures::executor::block_on(run(&project, &Scope::Project, Default::default(), &Default::default())).unwrap();
        let outcome = |fqn: &str| all.results.iter().find(|r| r.method_fqn == fqn).map(|r| r.outcome);
        assert_eq!(outcome("example.com/calc.TestAdd"), Some(Outcome::Passed), "{}", all.log);
        assert_eq!(outcome("example.com/calc.TestBroken"), Some(Outcome::Failed));
        assert!(all.log.contains("want 5, got 4"), "readable output: {}", all.log);
        let one = futures::executor::block_on(run(&project, &Scope::Methods(vec!["example.com/calc.TestAdd".into()]), Default::default(), &Default::default())).unwrap();
        assert_eq!(one.results.len(), 1);
    }
}
