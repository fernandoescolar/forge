//! Rust: each Cargo package is a project, each module with tests a class (per test
//! target: the package's own code, or a file in `tests/`), and each `#[test]` function
//! (`#[tokio::test]` and friends too) a test. Runs `cargo test` and reads libtest's output.
//!
//! Ids are `package|target|path`: `target` is empty for unit tests and the integration
//! test's name otherwise; `path` is what libtest calls the test (`parser::tests::empty`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context as _, Result};
use regex::Regex;

use crate::discovery::{Kind, NO_ROW, TestClass, TestMethod, TestProject, find_where};
use crate::runner::{self, LiveLog, RunOutput, Scope};
use crate::trx::{CaseResult, Outcome};

pub fn discover(root: &Path) -> Vec<TestProject> {
    find_where(root, |p| p.file_name().is_some_and(|n| n == "Cargo.toml"), None).into_iter().filter_map(|manifest| load_package(&manifest)).collect()
}

/// The `name` in the manifest's `[package]` section (workspace-only manifests have none).
fn package_name(manifest: &Path) -> Option<String> {
    let text = std::fs::read_to_string(manifest).ok()?;
    let mut in_package = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
        } else if in_package {
            if let Some(value) = line.strip_prefix("name").map(str::trim_start).and_then(|l| l.strip_prefix('=')) {
                return Some(value.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

fn load_package(manifest: &Path) -> Option<TestProject> {
    let package = package_name(manifest)?;
    let dir = manifest.parent()?;
    let mut classes: BTreeMap<String, TestClass> = BTreeMap::new();
    let mut add = |target: &str, file: &PathBuf, modules: Vec<String>| {
        let Ok(source) = std::fs::read_to_string(file) else { return };
        for test in file_tests(&source) {
            let module: Vec<String> = modules.iter().cloned().chain(test.modules).collect();
            let module = module.join("::");
            let path = if module.is_empty() { test.name.clone() } else { format!("{module}::{}", test.name) };
            let fqn = format!("{package}|{target}|{module}");
            let class = classes.entry(fqn.clone()).or_insert_with(|| TestClass {
                name: class_label(target, &module),
                fqn,
                file: file.clone(),
                row: NO_ROW,
                tests: Vec::new(),
            });
            class.tests.push(TestMethod { fqn: format!("{package}|{target}|{path}"), name: test.name, file: file.clone(), row: test.row });
        }
    };
    let src = dir.join("src");
    for file in find_where(&src, |p| p.extension().is_some_and(|e| e == "rs"), Some("Cargo.toml")) {
        let rel = file.strip_prefix(&src).unwrap_or(&file);
        add("", &file, module_of(rel));
    }
    let tests = dir.join("tests");
    if let Ok(entries) = std::fs::read_dir(&tests) {
        let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for path in entries {
            if path.extension().is_some_and(|e| e == "rs") {
                let target = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                add(&target, &path, vec![]);
            } else if path.join("main.rs").is_file() {
                let target = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                add(&target, &path.join("main.rs"), vec![]);
            }
        }
    }
    Some(TestProject { name: package.clone(), path: manifest.to_path_buf(), kind: Kind::Rust { package }, classes: classes.into_values().collect() })
}

fn class_label(target: &str, module: &str) -> String {
    match (target, module) {
        ("", "") => "(crate root)".into(),
        ("", module) => module.into(),
        (target, "") => format!("tests/{target}"),
        (target, module) => format!("tests/{target} › {module}"),
    }
}

/// The module a file under `src/` is: `lib.rs`, `main.rs` and binaries are crate roots,
/// `a/mod.rs` and `a.rs` are `a`, `a/b.rs` is `a::b`.
fn module_of(rel: &Path) -> Vec<String> {
    let parts: Vec<String> = rel.with_extension("").components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    match parts.as_slice() {
        [root] if root == "lib" || root == "main" => vec![],
        [first, ..] if first == "bin" => vec![],
        [.., last] if last == "mod" => parts[..parts.len() - 1].to_vec(),
        _ => parts,
    }
}

#[derive(Debug, PartialEq)]
struct FoundTest {
    /// Inline modules around it (`tests`).
    modules: Vec<String>,
    name: String,
    /// Zero-based line of the `fn`.
    row: u32,
}

static TEST_ATTR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*#\[\s*(?:[\w]+::)*test\b").unwrap());
static FN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+(\w+)").unwrap());
static MOD_OPEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*\{").unwrap());

/// The test functions in a file, with the inline modules around each.
fn file_tests(source: &str) -> Vec<FoundTest> {
    let mut found = Vec::new();
    let mut depth = 0i32;
    let mut modules: Vec<(String, i32)> = Vec::new();
    let mut pending = false;
    for (row, line) in code_lines(source).iter().enumerate() {
        if TEST_ATTR.is_match(line) {
            pending = true;
        } else if let Some(name) = FN.captures(line).filter(|_| pending).map(|c| c[1].to_string()) {
            found.push(FoundTest { modules: modules.iter().map(|(m, _)| m.clone()).collect(), name, row: row as u32 });
            pending = false;
        } else if pending && !line.trim().is_empty() && !line.trim_start().starts_with("#[") {
            pending = false;
        }
        if let Some(name) = MOD_OPEN.captures(line).map(|c| c[1].to_string()) {
            modules.push((name, depth));
        }
        depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
        while modules.last().is_some_and(|(_, d)| depth <= *d) {
            modules.pop();
        }
    }
    found
}

/// The source's lines with comments and the contents of strings and characters blanked
/// out, so braces in them don't count.
fn code_lines(source: &str) -> Vec<String> {
    #[derive(PartialEq)]
    enum State {
        Code,
        LineComment,
        BlockComment(u32),
        Str,
        RawStr(usize),
    }
    let chars: Vec<char> = source.chars().collect();
    let mut lines = vec![String::new()];
    let mut state = State::Code;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '\n' {
            lines.push(String::new());
            if state == State::LineComment {
                state = State::Code;
            }
            i += 1;
            continue;
        }
        let line = lines.last_mut().unwrap();
        match state {
            State::Code => {
                if c == '/' && next == Some('/') {
                    state = State::LineComment;
                    i += 2;
                    continue;
                } else if c == '/' && next == Some('*') {
                    state = State::BlockComment(1);
                    i += 2;
                    continue;
                } else if c == '"' {
                    line.push('"');
                    state = State::Str;
                } else if c == 'r' && (next == Some('"') || next == Some('#')) && !chars.get(i.wrapping_sub(1)).is_some_and(|p| p.is_alphanumeric() || *p == '_') {
                    let hashes = chars[i + 1..].iter().take_while(|c| **c == '#').count();
                    if chars.get(i + 1 + hashes) == Some(&'"') {
                        line.push('"');
                        state = State::RawStr(hashes);
                        i += 2 + hashes;
                        continue;
                    }
                    line.push(c);
                } else if c == '\'' {
                    // A char literal ('{', '\n', '\u{7b}'), not a lifetime.
                    let end = if next == Some('\\') { chars[i + 2..].iter().position(|c| *c == '\'').map(|p| i + 2 + p) } else if chars.get(i + 2) == Some(&'\'') { Some(i + 2) } else { None };
                    match end {
                        Some(end) => {
                            line.push_str("' '");
                            i = end + 1;
                            continue;
                        }
                        None => line.push(c),
                    }
                } else {
                    line.push(c);
                }
            }
            State::LineComment => {}
            State::BlockComment(depth) => {
                if c == '*' && next == Some('/') {
                    state = if depth == 1 { State::Code } else { State::BlockComment(depth - 1) };
                    i += 2;
                    continue;
                } else if c == '/' && next == Some('*') {
                    state = State::BlockComment(depth + 1);
                    i += 2;
                    continue;
                }
            }
            State::Str => {
                if c == '\\' {
                    i += 2;
                    continue;
                } else if c == '"' {
                    line.push('"');
                    state = State::Code;
                }
            }
            State::RawStr(hashes) => {
                if c == '"' && chars[i + 1..].iter().take(hashes).filter(|c| **c == '#').count() == hashes {
                    line.push('"');
                    state = State::Code;
                    i += 1 + hashes;
                    continue;
                }
            }
        }
        i += 1;
    }
    lines
}

/// `package|target|path` → (target, path).
fn split_id(id: &str) -> Option<(&str, &str)> {
    let mut parts = id.splitn(3, '|');
    let (_, target, path) = (parts.next()?, parts.next()?, parts.next()?);
    Some((target, path))
}

/// The `cargo test` arguments for `scope`.
pub(crate) fn args(package: &str, scope: &Scope) -> Vec<String> {
    let mut args: Vec<String> = vec!["test".into(), "-p".into(), package.into(), "--no-fail-fast".into()];
    match scope {
        Scope::Project => {}
        Scope::Class(id) => {
            let Some((target, module)) = split_id(id) else { return args };
            if !target.is_empty() {
                args.extend(["--test".into(), target.into()]);
            }
            if !module.is_empty() {
                args.extend(["--".into(), format!("{module}::")]);
            }
        }
        Scope::Methods(ids) => {
            let tests: Vec<(&str, &str)> = ids.iter().filter_map(|id| split_id(id)).collect();
            let mut targets: Vec<&str> = tests.iter().map(|(t, _)| *t).collect();
            targets.sort();
            targets.dedup();
            if let [target] = targets.as_slice() {
                if !target.is_empty() {
                    args.extend(["--test".into(), target.to_string()]);
                }
            }
            args.extend(["--".into(), "--exact".into()]);
            args.extend(tests.iter().map(|(_, path)| path.to_string()));
        }
    }
    args
}

pub async fn run(project: &TestProject, scope: &Scope, live: LiveLog, env: &runner::Env) -> Result<RunOutput> {
    let Kind::Rust { package } = &project.kind else { anyhow::bail!("not a Cargo package") };
    let mut command = runner::command("cargo", env);
    command.args(args(package, scope)).env("CARGO_TERM_COLOR", "never").current_dir(project.dir());
    let output = runner::execute(command, live, None).await.context("failed to start `cargo test`; is Rust installed?")?;
    Ok(RunOutput { results: parse(package, &output.stdout, &output.stderr), log: output.log, success: output.success })
}

static RESULT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^test (\S+) \.\.\. (ok|FAILED|ignored)").unwrap());

/// Results from libtest's output. Cargo names each test binary on stderr (`Running
/// unittests src/lib.rs`, `Running tests/api.rs`, `Doc-tests`) in the order their
/// `running N tests` blocks appear on stdout.
pub fn parse(package: &str, stdout: &str, stderr: &str) -> Vec<CaseResult> {
    let mut targets = stderr.lines().filter_map(|line| {
        let line = line.trim();
        if line.starts_with("Doc-tests") {
            return Some(None);
        }
        let running = line.strip_prefix("Running ")?;
        if running.starts_with("unittests") {
            return Some(Some(String::new()));
        }
        let path = running.split(" (").next().unwrap_or(running);
        Some(Some(Path::new(path).components().nth(1).map(|c| Path::new(c.as_os_str()).with_extension("").to_string_lossy().into_owned()).unwrap_or_default()))
    });
    let mut current: Option<String> = None;
    let mut results: Vec<CaseResult> = Vec::new();
    let mut capturing: Option<(String, String)> = None;
    let mut failures: Vec<(String, String, String)> = Vec::new();
    for line in stdout.lines() {
        if let Some((name, text)) = &mut capturing {
            if line.starts_with("---- ") || line.starts_with("failures:") || line.starts_with("test result:") {
                if let Some(target) = &current {
                    failures.push((target.clone(), std::mem::take(name), std::mem::take(text).trim_end().to_string()));
                }
                capturing = None;
            } else {
                text.push_str(line);
                text.push('\n');
                continue;
            }
        }
        if line.starts_with("running ") && line.ends_with(" tests") || line == "running 1 test" {
            current = targets.next().flatten();
        } else if let Some(name) = line.strip_prefix("---- ").and_then(|l| l.strip_suffix(" stdout ----")) {
            capturing = Some((name.to_string(), String::new()));
        } else if let (Some(target), Some(captures)) = (&current, RESULT.captures(line)) {
            let outcome = match &captures[2] {
                "ok" => Outcome::Passed,
                "FAILED" => Outcome::Failed,
                _ => Outcome::Skipped,
            };
            let name = captures[1].to_string();
            results.push(CaseResult {
                method_fqn: format!("{package}|{target}|{name}"),
                display_name: name,
                framework: None,
                outcome,
                duration: Duration::ZERO,
                message: None,
                stack_trace: None,
            });
        }
    }
    for (target, name, text) in failures {
        let fqn = format!("{package}|{target}|{name}");
        if let Some(result) = results.iter_mut().find(|r| r.method_fqn == fqn) {
            result.message = Some(text);
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_tests_in_nested_modules() {
        let source = r##"
pub fn add(a: i32, b: i32) -> i32 { a + b }

const BRACES: &str = "{{{";
const RAW: &str = r#"
  { not code
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        let c = '{';
        assert_eq!(add(2, 2), 4); // }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn later() {}

    mod deep {
        #[test]
        fn inner() {}
    }

    fn helper() {}
}

#[test]
fn at_root() {}
"##;
        let found = file_tests(source);
        let summary: Vec<(String, &str, u32)> = found.iter().map(|t| (t.modules.join("::"), t.name.as_str(), t.row)).collect();
        assert_eq!(summary, [("tests".into(), "adds", 13), ("tests".into(), "later", 20), ("tests::deep".into(), "inner", 24), (String::new(), "at_root", 31)]);
    }

    #[test]
    fn modules_from_paths() {
        assert_eq!(module_of(Path::new("lib.rs")), Vec::<String>::new());
        assert_eq!(module_of(Path::new("parser/mod.rs")), ["parser"]);
        assert_eq!(module_of(Path::new("parser/lexer.rs")), ["parser", "lexer"]);
        assert_eq!(module_of(Path::new("bin/tool.rs")), Vec::<String>::new());
    }

    #[test]
    fn builds_arguments() {
        let base = ["test", "-p", "calc", "--no-fail-fast"];
        assert_eq!(args("calc", &Scope::Project), base);
        assert_eq!(args("calc", &Scope::Class("calc||parser::tests".into()))[4..], ["--", "parser::tests::"]);
        assert_eq!(args("calc", &Scope::Class("calc|api|".into()))[4..], ["--test", "api"]);
        assert_eq!(args("calc", &Scope::Methods(vec!["calc||tests::adds".into(), "calc||tests::subs".into()]))[4..], ["--", "--exact", "tests::adds", "tests::subs"]);
    }

    #[test]
    fn parses_libtest_output() {
        let stderr = "   Compiling calc v0.1.0\n    Finished `test` profile\n     Running unittests src/lib.rs (target/debug/deps/calc-1)\n     Running tests/api.rs (target/debug/deps/api-2)\n   Doc-tests calc\n";
        let stdout = "\nrunning 2 tests\ntest tests::adds ... ok\ntest tests::fails ... FAILED\n\nfailures:\n\n---- tests::fails stdout ----\n\nthread 'tests::fails' panicked at src/lib.rs:12:9:\nassertion failed\n\n\nfailures:\n    tests::fails\n\ntest result: FAILED. 1 passed; 1 failed\n\nrunning 1 test\ntest checks_api ... ignored\n\ntest result: ok.\n\nrunning 1 test\ntest src/lib.rs - add (line 3) ... ok\n";
        let results = parse("calc", stdout, stderr);
        let summary: Vec<(&str, Outcome)> = results.iter().map(|r| (r.method_fqn.as_str(), r.outcome)).collect();
        assert_eq!(summary, [("calc||tests::adds", Outcome::Passed), ("calc||tests::fails", Outcome::Failed), ("calc|api|checks_api", Outcome::Skipped)]);
        assert!(results[1].message.as_deref().unwrap().contains("panicked at src/lib.rs:12:9"));
    }

    /// Runs `cargo test` for real on a small package.
    #[test]
    fn runs_cargo_test() {
        if std::process::Command::new("cargo").arg("--version").output().is_err() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::create_dir_all(root.path().join("tests")).unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "[package]\nname = \"calc\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n").unwrap();
        std::fs::write(
            root.path().join("src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn adds() { assert_eq!(super::add(2, 2), 4); }\n\n    #[test]\n    fn broken() { assert_eq!(super::add(2, 2), 5, \"bad sum\"); }\n}\n",
        )
        .unwrap();
        std::fs::write(root.path().join("tests/api.rs"), "#[test]\nfn api_adds() { assert_eq!(calc::add(1, 1), 2); }\n").unwrap();
        let project = discover(root.path()).remove(0);
        assert_eq!(project.classes.len(), 2);
        let all = futures::executor::block_on(run(&project, &Scope::Project, Default::default(), &Default::default())).unwrap();
        let outcome = |fqn: &str| all.results.iter().find(|r| r.method_fqn == fqn).map(|r| r.outcome);
        assert_eq!(outcome("calc||tests::adds"), Some(Outcome::Passed), "{}", all.log);
        assert_eq!(outcome("calc||tests::broken"), Some(Outcome::Failed));
        assert_eq!(outcome("calc|api|api_adds"), Some(Outcome::Passed));
        assert!(all.results.iter().find(|r| r.method_fqn == "calc||tests::broken").unwrap().message.as_deref().unwrap_or_default().contains("bad sum"));
        let one = futures::executor::block_on(run(&project, &Scope::Methods(vec!["calc|api|api_adds".into()]), Default::default(), &Default::default())).unwrap();
        assert_eq!(one.results.iter().map(|r| r.method_fqn.as_str()).collect::<Vec<_>>(), ["calc|api|api_adds"], "{}", one.log);
    }
}
