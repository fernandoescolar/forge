//! The server's `forge-host` calls, as Forge makes them (`forge_host::Host::call`): the
//! parameters as one JSON line on stdin, events back as JSON lines on stdout.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

/// Calls `method` with `params` and returns (the logs, the result or error).
fn call(method: &str, params: Value, data: &Path) -> (Vec<String>, Result<Value, String>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_forge-remote-server"))
        .args(["forge-host", method])
        .env("XDG_DATA_HOME", data)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, "{params}").unwrap();
    // Kept open, as the client does while it waits.
    let output = child.wait_with_output().unwrap();
    drop(stdin);
    let mut logs = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let event: Value = serde_json::from_str(line).unwrap_or_else(|_| panic!("not an event: {line}"));
        if let Some(text) = event.get("log").and_then(Value::as_str) {
            logs.push(text.to_string());
        } else if let Some(value) = event.get("ok") {
            return (logs, Ok(value.clone()));
        } else if let Some(error) = event.get("err").and_then(Value::as_str) {
            return (logs, Err(error.to_string()));
        }
    }
    panic!("no answer: {}", String::from_utf8_lossy(&output.stdout));
}

fn write(root: &Path, path: &str, text: &str) -> PathBuf {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    path
}

fn files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() { pending.push(path) } else { files.push(path) }
        }
    }
    files
}

fn rust_crate(root: &Path) {
    write(root, "Cargo.toml", "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    write(root, "src/main.rs", "fn main() { println!(\"{}\", twice(2)); }\n\nfn twice(x: i32) -> i32 { x * 2 }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn doubles() {\n        assert_eq!(super::twice(2), 4);\n    }\n}\n");
}

#[test]
fn discovers_tests_and_run_targets_where_the_files_are() {
    let (project, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let root = project.path().canonicalize().unwrap();
    rust_crate(&root);
    let listed = json!({ "listed": [[root, files(&root)]] });

    let (_, tests) = call("tests.discover", listed.clone(), data.path());
    let tests = tests.unwrap();
    assert_eq!(tests[0]["name"], "demo", "{tests}");
    assert_eq!(tests[0]["classes"][0]["tests"][0]["name"], "doubles", "{tests}");

    let (_, targets) = call("run.discover", listed, data.path());
    let targets = targets.unwrap();
    assert_eq!(targets[0]["kind"], "Cargo", "{targets}");
    assert_eq!(targets[0]["dir"], json!(root), "{targets}");
}

#[test]
fn runs_tests_and_streams_their_output() {
    let (project, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let root = project.path().canonicalize().unwrap();
    rust_crate(&root);
    let (_, tests) = call("tests.discover", json!({ "listed": [[root, files(&root)]] }), data.path());
    let test_project = tests.unwrap()[0].clone();
    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let params = json!({ "project": test_project, "scope": "Project", "env": env, "options": { "build": {}, "coverage": false, "settings": null } });

    let (logs, output) = call("tests.run", params, data.path());
    let output = output.unwrap();
    assert_eq!(output["success"], true, "{output}");
    assert_eq!(output["results"][0]["outcome"], "Passed", "{output}");
    assert!(logs.concat().contains("doubles"), "the run's output comes back as it runs: {logs:?}");
}

#[test]
fn loads_a_dotnet_solution() {
    let (project, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let root = project.path().canonicalize().unwrap();
    write(&root, "App/App.csproj", r#"<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><TargetFramework>net8.0</TargetFramework><OutputType>Exe</OutputType></PropertyGroup></Project>"#);
    write(&root, "App/Program.cs", "System.Console.WriteLine(1);\n");
    write(&root, "App.slnx", "<Solution>\n  <Project Path=\"App/App.csproj\" />\n</Solution>\n");
    let params = json!({
        "roots": [root],
        "chosen": null,
        "saved": null,
        "options": {
            "eval": { "configuration": "Debug", "platform": "AnyCPU", "msbuild_version": "17.11.9", "overrides": [] },
            "tree": { "items": { "ignored": ["bin", "obj"] }, "nest_by_name": true },
            "solution_search_depth": 4
        }
    });

    let (_, loaded) = call("dotnet.load", params, data.path());
    let loaded = loaded.unwrap();
    assert_eq!(loaded["solution"]["path"], json!(root.join("App.slnx")), "{loaded}");
    let tree = serde_json::to_string(&loaded["tree"]).unwrap();
    assert!(tree.contains("Program.cs"), "the tree lists the project's files: {tree}");
}

#[test]
fn says_what_it_does_not_know() {
    let data = tempfile::tempdir().unwrap();
    let (_, answer) = call("no.such.method", json!({}), data.path());
    assert!(answer.unwrap_err().contains("no `no.such.method`"));
}
