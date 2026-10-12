//! Instead of refusing a simple shell command that Forge has a tool for, runs the tool.
//! The Claude Code hook ([`crate::shell_guard`]) rewrites `cat a.rs`, `head -n 20 a.rs`,
//! `grep -rn foo src` or `git log -n 5` into `forge --agent-shell-exec <url> <call>`, which
//! calls the tool on Forge's MCP server (so it sees unsaved edits and keeps the files the
//! user hides from agents hidden) and prints its answer as the command's output.
//!
//! Only commands that map exactly are translated: one program, no pipes, redirections,
//! substitutions or chaining, and only options the tool has an equivalent for. Anything
//! else stays refused, with the tool to use instead. The output is the tool's, not the
//! program's (`cat` comes with line numbers, `grep` with Forge's search format).
//!
//! Two more kinds go through Forge though they aren't refused alone: test runs (`cargo test
//! parser`, `go test ./pkg/...`, `dotnet test`, `pytest a.py::b`) go to `run_tests`, so they
//! show in the Tests panel; and a command piped only into `grep`, then `head` or `tail`
//! (`cargo build 2>&1 | tail -20`) goes to `run_command` with those as `filter`, `head` or
//! `tail`, so the user watches it run. Test runs with options `run_tests` has no equivalent
//! for (`--features`, `--release`) run as they are.

use std::io::{BufRead as _, Read as _, Write as _};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::{Map, Value, json};

use std::path::Path;

use crate::shell_guard::{Line, basename, names_file, parse};

/// `forge --agent-shell-exec <url> <call>`: runs a translated command.
pub const EXEC_ARG: &str = "--agent-shell-exec";

/// Lines asked of `read_file` for a whole file (its maximum).
const MAX_LINES: u64 = 2000;

type Call = (&'static str, Value);

/// The tool call that does what `command` does, if there is an exact one.
pub(crate) fn translate(command: &str) -> Option<Call> {
    let command = command.trim();
    if command.contains("<<") || command.ends_with('&') {
        return None;
    }
    let line = parse(command);
    if line.segments.len() > 1 {
        return piped(command, &line);
    }
    let [segment] = line.segments.as_slice() else { return None };
    if !line.nested.is_empty() || segment.piped || !segment.inputs.is_empty() || segment.outputs.iter().any(|o| names_file(o)) {
        return None;
    }
    if segment.words.iter().any(|w| expands(w)) {
        return None;
    }
    let words = without_rtk(&segment.words);
    if let Some(call) = test_run(words) {
        return Some(call);
    }
    let (program, args) = words.split_first()?;
    match basename(program) {
        "cat" => cat(args),
        "head" => head_tail(args, false),
        "tail" => head_tail(args, true),
        "sed" => sed(args),
        "grep" => search(args, false),
        "rg" => search(args, true),
        "git" => match args.split_first()? {
            (sub, rest) if sub == "log" => git_log(rest),
            (sub, rest) if sub == "show" => git_show(rest),
            (sub, rest) if sub == "blame" => git_blame(rest),
            (sub, rest) if sub == "diff" => git_diff(rest),
            _ => None,
        },
        _ => None,
    }
}

/// The tools that may run for minutes: the shell tool gets its longest timeout for them,
/// and the call waits as long as it does.
pub(crate) fn takes_long(tool: &str) -> bool {
    matches!(tool, "run_tests" | "run_command")
}

/// `words` without the `rtk` (or `rtk proxy`) before the program.
fn without_rtk(words: &[String]) -> &[String] {
    match words {
        [rtk, proxy, rest @ ..] if basename(rtk) == "rtk" && proxy == "proxy" => rest,
        [rtk, rest @ ..] if basename(rtk) == "rtk" => rest,
        _ => words,
    }
}

/// `command | grep x | tail -n 20`: the command before the pipe, run with `run_command`
/// and the programs after it as its `filter` and `head` or `tail` (or, for a test run,
/// `run_tests`, which sums the results up itself). Only `grep`, then `head` or `tail`.
fn piped(command: &str, line: &Line) -> Option<Call> {
    let segments = &line.segments;
    let first_filter = segments.iter().rposition(|s| !s.piped || !is_output_filter(s))? + 1;
    let filters = &segments[first_filter..];
    if filters.is_empty() || filters.iter().any(|s| !s.inputs.is_empty() || s.outputs.iter().any(|o| names_file(o))) {
        return None;
    }
    let mut args = Map::new();
    for (ix, filter) in filters.iter().enumerate() {
        let words = without_rtk(&filter.words);
        let (program, rest) = words.split_first()?;
        match basename(program) {
            "grep" if ix == 0 => {
                args.insert("filter".into(), json!(grep_filter(rest)?));
            }
            name @ ("head" | "tail") if ix + 1 == filters.len() => {
                args.insert(name.into(), json!(line_count(rest)?));
            }
            _ => return None,
        }
    }
    let before: String = command.chars().take(filters[0].pipe_at).collect();
    let before = before.trim();
    // What runs must pass the guard on its own (`cat a.rs | grep x` stays refused).
    if before.is_empty() || crate::shell_guard::refusal(before).is_some() {
        return None;
    }
    if let [segment] = &segments[..first_filter] {
        let plain = line.nested.is_empty() && segment.inputs.is_empty() && !segment.outputs.iter().any(|o| names_file(o)) && !segment.words.iter().any(|w| expands(w));
        if let Some(call) = plain.then(|| test_run(without_rtk(&segment.words))).flatten() {
            return Some(call);
        }
    }
    args.insert("command".into(), json!(before));
    Some(("run_command", Value::Object(args)))
}

/// A program that only filters what is piped into it: `grep`, `head` or `tail`.
fn is_output_filter(segment: &crate::shell_guard::Segment) -> bool {
    without_rtk(&segment.words).first().is_some_and(|p| matches!(basename(p), "grep" | "head" | "tail"))
}

/// `grep`'s options and pattern, as `run_command`'s `filter` regex: `-E`, `-F` and `-i`.
fn grep_filter(args: &[String]) -> Option<String> {
    let (mut extended, mut fixed, mut insensitive, mut pattern) = (false, false, false, None);
    for arg in args {
        match arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
            Some(flags) => {
                for flag in flags.chars() {
                    match flag {
                        'E' => extended = true,
                        'F' => fixed = true,
                        'i' => insensitive = true,
                        _ => return None,
                    }
                }
            }
            None if pattern.is_none() => pattern = Some(arg.as_str()),
            None => return None,
        }
    }
    let pattern = pattern.filter(|p| !p.is_empty())?;
    let regex = if fixed {
        regex::escape(pattern)
    } else if extended {
        pattern.to_string()
    } else {
        match basic_regex(pattern)? {
            (regex, true) => regex,
            (text, false) => regex::escape(&text),
        }
    };
    regex::Regex::new(&regex).ok()?;
    Some(if insensitive { format!("(?i){regex}") } else { regex })
}

/// `head` and `tail`'s line count, without a file: `-n 20`, `-20`, `--lines=20`.
fn line_count(args: &[String]) -> Option<u64> {
    let mut count = 10;
    let mut words = args.iter();
    while let Some(word) = words.next() {
        if word == "-n" || word == "--lines" {
            count = digits(words.next()?)?;
        } else if let Some(n) = word.strip_prefix("--lines=").or_else(|| word.strip_prefix("-n")).or_else(|| word.strip_prefix('-')) {
            count = digits(n)?;
        } else {
            return None;
        }
    }
    Some(count.max(1))
}

/// A test run (`cargo test parser`, `go test ./pkg/...`, `dotnet test`, `npm test`, `npx
/// vitest run a.test.ts`, `pytest a.py::test_b`), as `run_tests` with the same tests.
fn test_run(words: &[String]) -> Option<Call> {
    let (program, args) = words.split_first()?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let tests = match (basename(program), args.as_slice()) {
        ("cargo", ["test", rest @ ..]) => cargo_test(rest)?,
        ("go", ["test", rest @ ..]) => go_test(rest)?,
        ("dotnet", ["test", rest @ ..]) => dotnet_test(rest)?,
        ("npm" | "pnpm" | "yarn", ["test" | "t"] | ["run", "test"]) => Map::new(),
        ("npx" | "pnpx", [runner @ ("jest" | "vitest"), rest @ ..]) => js_test(runner, rest)?,
        (runner @ ("jest" | "vitest"), rest) => js_test(runner, rest)?,
        ("pytest", rest) => pytest(rest)?,
        ("python" | "python3", ["-m", "pytest", rest @ ..]) => pytest(rest)?,
        _ => return None,
    };
    Some(("run_tests", Value::Object(tests)))
}

/// A test name `run_tests` can look for (its `name` is a substring): letters, digits, `_`,
/// `.` and `:`.
fn test_name(text: &str) -> Option<String> {
    let text = text.trim_start_matches('^').trim_end_matches('$');
    (!text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || "_.:".contains(c))).then(|| text.to_string())
}

/// A folder of the project, as `run_tests`' `path`: `./pkg/...` → `pkg`; `None` for the root.
fn test_path(word: &str) -> Option<Option<String>> {
    let path = word.trim_start_matches("./").trim_end_matches("...").trim_end_matches('/');
    match path {
        "" | "." => Some(None),
        path if is_path(path) => Some(Some(path.to_string())),
        _ => None,
    }
}

fn tests_args(path: Option<String>, name: Option<String>) -> Map<String, Value> {
    let mut args = Map::new();
    if let Some(path) = path {
        args.insert("path".into(), json!(path));
    }
    if let Some(name) = name {
        args.insert("name".into(), json!(name));
    }
    args
}

/// `cargo test [-p crate] [name] [-- --nocapture]`.
fn cargo_test(args: &[&str]) -> Option<Map<String, Value>> {
    let (mut path, mut name) = (None, None);
    let mut words = args.iter();
    while let Some(&word) = words.next() {
        match word {
            "--workspace" | "--all" | "-q" | "--quiet" => {}
            "-p" | "--package" => path = Some(package_dir(Path::new("."), words.next()?)?),
            "--" => {
                // The test binary's options: only those that change what is shown.
                for &option in words.by_ref() {
                    if !matches!(option, "--nocapture" | "--show-output" | "-q" | "--quiet") && !option.starts_with("--test-threads=") {
                        return None;
                    }
                }
            }
            w if w.starts_with("--package=") => path = Some(package_dir(Path::new("."), &w["--package=".len()..])?),
            w if !w.starts_with('-') && name.is_none() => name = Some(test_name(w)?),
            _ => return None,
        }
    }
    Some(tests_args(path, name))
}

/// The folder of the workspace's crate `name`, under `root`: where its Cargo.toml names it,
/// in the usual places.
fn package_dir(root: &Path, name: &str) -> Option<String> {
    let names_it = |dir: &str| {
        std::fs::read_to_string(root.join(dir).join("Cargo.toml")).is_ok_and(|toml| {
            toml.lines().map(str::trim).any(|l| l.strip_prefix("name").map(|v| v.trim_start().trim_start_matches('=').trim().trim_matches('"')) == Some(name))
        })
    };
    let candidates = [format!("crates/{name}"), name.to_string(), format!("packages/{name}"), format!("libs/{name}")];
    candidates.into_iter().find(|dir| is_path(dir) && names_it(dir))
}

/// `go test [./pkg/...] [-run Name] [-v] [-count=1]`.
fn go_test(args: &[&str]) -> Option<Map<String, Value>> {
    let (mut path, mut name, mut package) = (None, None, false);
    let mut words = args.iter();
    while let Some(&word) = words.next() {
        match word {
            "-v" | "-count=1" => {}
            "-count" if words.next() == Some(&"1") => {}
            "-run" => name = Some(test_name(words.next()?)?),
            w if w.starts_with("-run=") => name = Some(test_name(&w["-run=".len()..])?),
            w if !w.starts_with('-') && !package => {
                package = true;
                path = test_path(w)?;
            }
            _ => return None,
        }
    }
    Some(tests_args(path, name))
}

/// `dotnet test [folder|project] [--filter Name~x] [--no-restore] [-v q]`.
fn dotnet_test(args: &[&str]) -> Option<Map<String, Value>> {
    let (mut path, mut name, mut target) = (None, None, false);
    let mut words = args.iter();
    while let Some(&word) = words.next() {
        match word {
            "--no-restore" | "--nologo" => {}
            "-v" | "--verbosity" => {
                words.next()?;
            }
            "--filter" => {
                let filter = words.next()?;
                let wanted = ["FullyQualifiedName~", "Name~", "DisplayName~"].iter().find_map(|p| filter.strip_prefix(p)).unwrap_or(filter);
                name = Some(test_name(wanted)?);
            }
            w if !w.starts_with('-') && !target => {
                target = true;
                path = if w.ends_with(".sln") || w.ends_with(".slnx") {
                    None
                } else if w.ends_with(".csproj") || w.ends_with(".fsproj") || w.ends_with(".vbproj") {
                    match Path::new(w).parent().and_then(Path::to_str) {
                        Some(dir) => test_path(dir)?,
                        None => None,
                    }
                } else {
                    test_path(w)?
                };
            }
            _ => return None,
        }
    }
    Some(tests_args(path, name))
}

/// `jest [path] [-t name]`, `vitest [run] [path] [-t name]`.
fn js_test(runner: &str, args: &[&str]) -> Option<Map<String, Value>> {
    let args = if runner == "vitest" { args.strip_prefix(&["run"]).unwrap_or(args) } else { args };
    let (mut path, mut name, mut target) = (None, None, false);
    let mut words = args.iter();
    while let Some(&word) = words.next() {
        match word {
            "--run" => {}
            "-t" | "--testNamePattern" => name = Some(test_name(words.next()?)?),
            w if w.starts_with("--testNamePattern=") => name = Some(test_name(&w["--testNamePattern=".len()..])?),
            w if !w.starts_with('-') && !target => {
                target = true;
                path = test_path(w)?;
            }
            _ => return None,
        }
    }
    Some(tests_args(path, name))
}

/// `pytest [path[::Class::test]] [-k name] [-q] [-v] [-s]`.
fn pytest(args: &[&str]) -> Option<Map<String, Value>> {
    let (mut path, mut name, mut target) = (None, None, false);
    let mut words = args.iter();
    while let Some(&word) = words.next() {
        match word {
            "-q" | "-v" | "-vv" | "-s" => {}
            "-k" => name = Some(test_name(words.next()?)?),
            w if w.starts_with("--tb=") => {}
            w if !w.starts_with('-') && !target => {
                target = true;
                let mut parts = w.split("::");
                path = test_path(parts.next()?)?;
                if let Some(test) = parts.last() {
                    name = Some(test_name(test)?);
                }
            }
            _ => return None,
        }
    }
    Some(tests_args(path, name))
}

/// A word the shell would expand (`$HOME`, `$1`, `${x}`).
fn expands(word: &str) -> bool {
    word.split('$').skip(1).any(|rest| rest.starts_with(|c: char| c.is_ascii_alphanumeric() || "_{@*#?!".contains(c)))
}

/// A file or folder named as it is, inside the project: no globs, `~`, `..` or absolute paths.
fn is_path(word: &str) -> bool {
    !word.is_empty() && !word.starts_with(['-', '/', '~']) && !word.contains(['*', '?', '[', ']', '{', '}']) && !word.split('/').any(|part| part == "..")
}

/// A count, as digits only.
fn digits(text: &str) -> Option<u64> {
    (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())).then(|| text.parse().ok()).flatten()
}

fn cat(args: &[String]) -> Option<Call> {
    let [path] = args else { return None };
    is_path(path).then(|| ("read_file", json!({ "path": path, "lines": MAX_LINES })))
}

fn head_tail(args: &[String], tail: bool) -> Option<Call> {
    let (mut count, mut path) = (10, None);
    let mut words = args.iter();
    while let Some(word) = words.next() {
        if word == "-n" || word == "--lines" {
            count = digits(words.next()?)?;
        } else if let Some(n) = word.strip_prefix("--lines=").or_else(|| word.strip_prefix("-n")).or_else(|| word.strip_prefix('-')) {
            count = digits(n)?;
        } else if path.is_none() && is_path(word) {
            path = Some(word);
        } else {
            return None;
        }
    }
    let count = count.max(1);
    Some(("read_file", if tail { json!({ "path": path?, "tail": count }) } else { json!({ "path": path?, "lines": count }) }))
}

/// `sed -n 10,20p file` and `sed -n 5p file`.
fn sed(args: &[String]) -> Option<Call> {
    let [flag, script, path] = args else { return None };
    let script = script.strip_suffix('p').filter(|_| flag == "-n" && is_path(path))?;
    let (first, last) = match script.split_once(',') {
        Some((first, last)) => (digits(first)?, digits(last)?),
        None => (digits(script)?, digits(script)?),
    };
    (first >= 1 && last >= first).then(|| ("read_file", json!({ "path": path, "start_line": first, "lines": last - first + 1 })))
}

/// Globs for `search`'s `include`: `src` is a file or a folder.
fn path_globs(path: &str) -> Vec<String> {
    vec![path.to_string(), format!("{path}/**")]
}

/// `grep` and `rg` with the options `search` has an equivalent for.
fn search(args: &[String], rg: bool) -> Option<Call> {
    let (mut insensitive, mut word, mut fixed, mut extended, mut files_only, mut recursive) = (false, false, false, false, false, false);
    let (mut context, mut include, mut exclude) = (0u64, Vec::<String>::new(), Vec::<String>::new());
    let mut positional: Vec<&str> = Vec::new();
    let mut options_done = false;
    let mut i = 0;
    while let Some(arg) = args.get(i).map(String::as_str) {
        i += 1;
        if options_done || !arg.starts_with('-') || arg == "-" {
            positional.push(arg);
        } else if arg == "--" {
            options_done = true;
        } else if let Some(long) = arg.strip_prefix("--") {
            let (name, value) = long.split_once('=').map_or((long, None), |(n, v)| (n, Some(v)));
            match (name, value) {
                ("ignore-case", None) => insensitive = true,
                ("word-regexp", None) => word = true,
                ("fixed-strings", None) => fixed = true,
                ("files-with-matches", None) => files_only = true,
                ("line-number" | "with-filename" | "no-heading", None) | ("color", Some("never")) => {}
                ("extended-regexp", None) if !rg => extended = true,
                ("recursive" | "dereference-recursive", None) if !rg => recursive = true,
                ("include", Some(glob)) if !rg => include.push(glob.to_string()),
                ("exclude", Some(glob)) if !rg => exclude.push(glob.to_string()),
                ("glob", Some(glob)) if rg && !glob.starts_with('!') => include.push(glob.to_string()),
                ("context" | "after-context" | "before-context", Some(n)) => context = context.max(digits(n)?),
                _ => return None,
            }
        } else {
            let cluster: Vec<char> = arg[1..].chars().collect();
            let mut j = 0;
            while let Some(&c) = cluster.get(j) {
                j += 1;
                match c {
                    'i' => insensitive = true,
                    'w' => word = true,
                    'F' => fixed = true,
                    'l' => files_only = true,
                    'n' | 'H' => {}
                    'E' if !rg => extended = true,
                    'r' | 'R' if !rg => recursive = true,
                    'A' | 'B' | 'C' | 'g' => {
                        let attached: String = cluster[j..].iter().collect();
                        j = cluster.len();
                        let value = if attached.is_empty() {
                            i += 1;
                            args.get(i - 1)?.clone()
                        } else {
                            attached
                        };
                        match c {
                            'g' if rg && !value.starts_with('!') => include.push(value),
                            'g' => return None,
                            _ => context = context.max(digits(&value)?),
                        }
                    }
                    _ => return None,
                }
            }
        }
    }
    let (pattern, paths) = positional.split_first()?;
    // `grep foo` reads its input, not the project.
    if pattern.is_empty() || (!rg && paths.is_empty() && !recursive) {
        return None;
    }
    let mut globs = Vec::new();
    let mut whole_project = paths.is_empty();
    for path in paths {
        let path = path.trim_start_matches("./").trim_end_matches('/');
        if path.is_empty() || path == "." {
            whole_project = true;
        } else if is_path(path) {
            globs.extend(path_globs(path));
        } else {
            return None;
        }
    }
    if !whole_project {
        // A path and a glob would be a union in `search`, an intersection for grep.
        if !include.is_empty() {
            return None;
        }
        include = globs;
    }
    let (query, regex) = if rg || extended {
        ((*pattern).to_string(), !fixed)
    } else if fixed {
        ((*pattern).to_string(), false)
    } else {
        basic_regex(pattern)?
    };
    Some(("search", json!({
        "query": query, "regex": regex, "case_sensitive": !insensitive, "whole_word": word, "files_only": files_only,
        "context": context.min(10), "max_results": 1000, "include": include, "exclude": exclude,
    })))
}

/// grep's default patterns: text unless it has a regex's `.`, `*`, `[`, `^` or `$`; then
/// the regex it means (`+ ? | ( ) { }` are plain characters there). Not with `\`.
fn basic_regex(pattern: &str) -> Option<(String, bool)> {
    if pattern.contains('\\') {
        return None;
    }
    if !pattern.contains(['.', '*', '[', '^', '$']) {
        return Some((pattern.to_string(), false));
    }
    let mut regex = String::new();
    for c in pattern.chars() {
        if "+?|(){}".contains(c) {
            regex.push('\\');
        }
        regex.push(c);
    }
    Some((regex, true))
}

/// The words before and after a `--`, if the path is at most one.
fn split_path(args: &[String]) -> Option<(&[String], Option<&String>)> {
    match args.iter().position(|a| a == "--") {
        Some(at) => match &args[at + 1..] {
            [] => Some((&args[..at], None)),
            [path] if is_path(path) => Some((&args[..at], Some(path))),
            _ => None,
        },
        None => Some((args, None)),
    }
}

fn history(action: &str, fields: Map<String, Value>) -> Option<Call> {
    let mut args = Map::new();
    args.insert("action".into(), json!(action));
    args.extend(fields);
    Some(("git_history", Value::Object(args)))
}

fn git_log(args: &[String]) -> Option<Call> {
    let (words, mut path) = split_path(args)?;
    let mut fields = Map::new();
    let (mut text, mut regex, mut patch) = (None, None, false);
    let mut words = words.iter();
    while let Some(word) = words.next() {
        let mut value = |prefix: &str| word.strip_prefix(prefix).map(|v| if v.is_empty() { words.next().cloned() } else { Some(v.to_string()) });
        if word == "--oneline" {
        } else if word == "--stat" {
            fields.insert("stat".into(), json!(true));
        } else if word == "-p" || word == "--patch" {
            patch = true;
        } else if let Some(v) = value("--author=") {
            fields.insert("author".into(), json!(v?));
        } else if let Some(v) = value("--since=") {
            fields.insert("since".into(), json!(v?));
        } else if let Some(v) = value("--grep=") {
            fields.insert("message".into(), json!(v?));
        } else if let Some(v) = value("--max-count=") {
            fields.insert("limit".into(), json!(digits(&v?)?));
        } else if let Some(v) = value("-n") {
            fields.insert("limit".into(), json!(digits(&v?)?));
        } else if let Some(v) = value("-S") {
            text = Some(v?);
        } else if let Some(v) = value("-G") {
            regex = Some(v?);
        } else if let Some(n) = word.strip_prefix('-').and_then(digits) {
            fields.insert("limit".into(), json!(n));
        } else if path.is_none() && is_path(word) && std::path::Path::new(word).exists() {
            path = Some(word);
        } else {
            return None;
        }
    }
    if let Some(path) = path {
        fields.insert("path".into(), json!(path));
    }
    let action = match (text, regex) {
        (Some(_), Some(_)) => return None,
        (Some(text), None) => {
            fields.insert("text".into(), json!(text));
            "changes"
        }
        (None, Some(regex)) => {
            fields.insert("regex".into(), json!(regex));
            "changes"
        }
        (None, None) if patch => return None,
        (None, None) => "log",
    };
    if patch {
        fields.insert("patch".into(), json!(true));
    }
    history(action, fields)
}

fn git_show(args: &[String]) -> Option<Call> {
    let (words, path) = split_path(args)?;
    let mut fields = Map::new();
    let mut commit = None;
    for word in words {
        if word == "--stat" {
            fields.insert("stat_only".into(), json!(true));
        } else if commit.is_none() && !word.starts_with('-') {
            commit = Some(word.as_str());
        } else {
            return None;
        }
    }
    let commit = commit.unwrap_or("HEAD");
    if let Some((rev, file)) = commit.split_once(':').filter(|_| path.is_none() && !fields.contains_key("stat_only")) {
        if rev.is_empty() || !is_path(file) {
            return None;
        }
        return Some(("git_history", json!({ "action": "file_at", "commit": rev, "path": file })));
    }
    fields.insert("commit".into(), json!(commit));
    if let Some(path) = path {
        fields.insert("path".into(), json!(path));
    }
    history("show", fields)
}

fn git_blame(args: &[String]) -> Option<Call> {
    let (words, mut path) = split_path(args)?;
    let mut fields = Map::new();
    let mut words = words.iter();
    while let Some(word) = words.next() {
        if let Some(range) = word.strip_prefix("-L") {
            let range = if range.is_empty() { words.next()?.as_str() } else { range };
            let (start, end) = range.split_once(',')?;
            fields.insert("start_line".into(), json!(digits(start)?));
            fields.insert("end_line".into(), json!(digits(end)?));
        } else if path.is_none() && is_path(word) {
            path = Some(word);
        } else {
            return None;
        }
    }
    fields.insert("path".into(), json!(path?));
    history("blame", fields)
}

fn git_diff(args: &[String]) -> Option<Call> {
    let (words, path) = split_path(args)?;
    let mut fields = Map::new();
    let mut revisions: Vec<&str> = Vec::new();
    for word in words {
        if word == "--stat" {
            fields.insert("stat_only".into(), json!(true));
        } else if !word.starts_with('-') && !word.contains("...") {
            match word.split_once("..") {
                Some((from, to)) if revisions.is_empty() && !from.is_empty() && !to.is_empty() => revisions.extend([from, to]),
                Some(_) => return None,
                None => revisions.push(word),
            }
        } else {
            return None;
        }
    }
    match revisions[..] {
        [] => {}
        [from] => {
            fields.insert("from".into(), json!(from));
        }
        [from, to] => {
            fields.insert("from".into(), json!(from));
            fields.insert("to".into(), json!(to));
        }
        _ => return None,
    }
    if let Some(path) = path {
        fields.insert("path".into(), json!(path));
    }
    history("diff", fields)
}

/// The command that runs `call` through Forge's server at `url`.
pub(crate) fn exec_command(exe: &std::path::Path, url: &str, call: &Call) -> String {
    let payload = json!({ "name": call.0, "arguments": call.1 }).to_string();
    let hex: String = payload.bytes().map(|b| format!("{b:02x}")).collect();
    format!("\"{}\" {EXEC_ARG} {url} {hex}", exe.display())
}

/// A command line [`exec_command`] wrote, as the user reads it: the command for
/// `run_command`, else the tool and what it was asked (`Read a file src/a.rs`).
pub(crate) fn describe(command: &str) -> Option<String> {
    let mut words = command.split(' ').skip_while(|w| *w != EXEC_ARG).skip(2);
    let call: Value = serde_json::from_slice(&unhex(words.next()?)?).ok()?;
    let (name, args) = (call["name"].as_str()?, &call["arguments"]);
    if let Some(command) = args["command"].as_str().filter(|_| name == "run_command") {
        return Some(command.to_string());
    }
    let title = crate::forge_mcp::tool_title(name).unwrap_or_else(|| name.to_string());
    Some(match crate::forge_mcp::tool_summary(args) {
        Some(summary) => format!("{title} {summary}"),
        None => title,
    })
}

fn unhex(hex: &str) -> Option<Vec<u8>> {
    (hex.len() % 2 == 0).then_some(())?;
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok()).collect()
}

/// Runs a translated command (`args`: the server's URL and the call); the process's exit code.
pub fn run_exec(args: &[String]) -> i32 {
    let call = match args {
        [_, payload] => unhex(payload).and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()),
        _ => None,
    };
    let Some((url, call)) = args.first().zip(call) else {
        eprintln!("usage: forge {EXEC_ARG} <url> <call>");
        return 2;
    };
    let name = call["name"].as_str().unwrap_or_default();
    match call_tool(url, name, &call["arguments"]) {
        Ok((text, false)) => {
            println!("{text}");
            // A command's own exit code (`run_command` answers "Exit code 1." first).
            text.strip_prefix("Exit code ").and_then(|rest| rest.split('.').next()?.parse().ok()).unwrap_or(0)
        }
        Ok((text, true)) => {
            eprintln!("{text}");
            1
        }
        Err(e) => {
            eprintln!("Forge's tools couldn't be reached ({e}): use the `forge` MCP server's tools directly.");
            1
        }
    }
}

/// Calls a tool of Forge's MCP server: its text, and whether it is an error.
pub(crate) fn call_tool(url: &str, name: &str, arguments: &Value) -> Result<(String, bool), String> {
    let rest = url.strip_prefix("http://").ok_or("not an http URL")?;
    let (host, path) = rest.split_at(rest.find('/').ok_or("no path in the URL")?);
    let mut stream = TcpStream::connect(host).map_err(|e| e.to_string())?;
    // Tests and commands wait as long as the shell tool lets them run.
    let timeout = (!takes_long(name)).then_some(Duration::from_secs(120));
    stream.set_read_timeout(timeout).map_err(|e| e.to_string())?;
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": arguments } }).to_string();
    let request = format!("POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
    // The server keeps connections open: read as long as the answer says.
    let mut reader = std::io::BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status).map_err(|e| e.to_string())?;
    let mut length = 0;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).map_err(|e| e.to_string())? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some(value) = header.to_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().map_err(|_| "bad Content-Length")?;
        }
    }
    if !status.contains(" 200 ") {
        return Err(status.trim().to_string());
    }
    let mut answer = vec![0; length];
    reader.read_exact(&mut answer).map_err(|e| e.to_string())?;
    let answer: Value = serde_json::from_slice(&answer).map_err(|e| e.to_string())?;
    let result = answer.get("result").ok_or_else(|| answer["error"]["message"].as_str().unwrap_or("no result").to_string())?;
    let text = result["content"][0]["text"].as_str().unwrap_or_default().to_string();
    Ok((text, result["isError"].as_bool().unwrap_or(false)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(command: &str) -> Option<(String, Value)> {
        translate(command).map(|(name, args)| (name.to_string(), args))
    }

    #[test]
    fn translates_reads() {
        assert_eq!(call("cat src/main.rs"), Some(("read_file".into(), json!({ "path": "src/main.rs", "lines": 2000 }))));
        assert_eq!(call("head -n 20 a.rs").unwrap().1, json!({ "path": "a.rs", "lines": 20 }));
        assert_eq!(call("head -5 a.rs").unwrap().1, json!({ "path": "a.rs", "lines": 5 }));
        assert_eq!(call("head a.rs").unwrap().1, json!({ "path": "a.rs", "lines": 10 }));
        assert_eq!(call("tail -n30 a.rs").unwrap().1, json!({ "path": "a.rs", "tail": 30 }));
        assert_eq!(call("tail --lines=3 a.rs").unwrap().1, json!({ "path": "a.rs", "tail": 3 }));
        assert_eq!(call("sed -n 10,20p a.rs").unwrap().1, json!({ "path": "a.rs", "start_line": 10, "lines": 11 }));
        assert_eq!(call("sed -n '5p' a.rs").unwrap().1, json!({ "path": "a.rs", "start_line": 5, "lines": 1 }));
        assert_eq!(call("cat a.rs 2>&1").map(|c| c.0), Some("read_file".into()));
    }

    #[test]
    fn translates_searches() {
        let (name, args) = call("grep -rn parse src").unwrap();
        assert_eq!(name, "search");
        assert_eq!(args["query"], "parse");
        assert_eq!(args["regex"], false);
        assert_eq!(args["case_sensitive"], true);
        assert_eq!(args["include"], json!(["src", "src/**"]));
        let args = call("grep -rniw -C 2 'fn main' .").unwrap().1;
        assert_eq!((args["case_sensitive"].clone(), args["whole_word"].clone(), args["context"].clone(), args["include"].clone()), (json!(false), json!(true), json!(2), json!([])));
        assert_eq!(call("grep -rl foo --include=*.rs").unwrap().1["files_only"], true);
        assert_eq!(call("grep -rl foo --include=*.rs").unwrap().1["include"], json!(["*.rs"]));
        assert_eq!(call("grep -E 'a|b' x.rs").unwrap().1["regex"], true);
        assert_eq!(call("grep -F 'a.b' x.rs").unwrap().1["regex"], false);
        // grep's basic regexes: `.` is a regex, `(` isn't.
        let args = call("grep 'foo(.*)' x.rs").unwrap().1;
        assert_eq!((args["query"].clone(), args["regex"].clone()), (json!("foo\\(.*\\)"), json!(true)));
        let args = call("rg -n 'fn \\w+' crates/a").unwrap().1;
        assert_eq!((args["query"].clone(), args["regex"].clone(), args["include"].clone()), (json!("fn \\w+"), json!(true), json!(["crates/a", "crates/a/**"])));
        assert_eq!(call("rg -g '*.rs' foo").unwrap().1["include"], json!(["*.rs"]));
    }

    #[test]
    fn translates_history() {
        let log = call("git log -n 5 --author=ana").unwrap();
        assert_eq!(log, ("git_history".into(), json!({ "action": "log", "limit": 5, "author": "ana" })));
        assert_eq!(call("git log -S parse_config -- src/a.rs").unwrap().1, json!({ "action": "changes", "text": "parse_config", "path": "src/a.rs" }));
        assert_eq!(call("git show HEAD~2").unwrap().1, json!({ "action": "show", "commit": "HEAD~2" }));
        assert_eq!(call("git show --stat abc123 -- a.rs").unwrap().1, json!({ "action": "show", "stat_only": true, "commit": "abc123", "path": "a.rs" }));
        assert_eq!(call("git show HEAD~2:src/a.rs").unwrap().1, json!({ "action": "file_at", "commit": "HEAD~2", "path": "src/a.rs" }));
        assert_eq!(call("git blame -L 10,20 a.rs").unwrap().1, json!({ "action": "blame", "start_line": 10, "end_line": 20, "path": "a.rs" }));
        assert_eq!(call("git diff main..feature -- a.rs").unwrap().1, json!({ "action": "diff", "from": "main", "to": "feature", "path": "a.rs" }));
        assert_eq!(call("git diff --stat").unwrap().1, json!({ "action": "diff", "stat_only": true }));
    }

    #[test]
    fn translates_test_runs() {
        let tests = |command: &str| call(command).filter(|(name, _)| name == "run_tests").map(|(_, args)| args);
        assert_eq!(tests("cargo test"), Some(json!({})));
        assert_eq!(tests("cargo test parser::tests -- --nocapture"), Some(json!({ "name": "parser::tests" })));
        assert_eq!(tests("cargo test --workspace 2>&1"), Some(json!({})));
        assert_eq!(tests("go test ./..."), Some(json!({})));
        assert_eq!(tests("go test -v -run ^TestParse$ ./internal/parser/..."), Some(json!({ "path": "internal/parser", "name": "TestParse" })));
        assert_eq!(tests("dotnet test"), Some(json!({})));
        assert_eq!(tests("dotnet test tests/App.Tests/App.Tests.csproj --filter FullyQualifiedName~Parser"), Some(json!({ "path": "tests/App.Tests", "name": "Parser" })));
        assert_eq!(tests("dotnet test App.sln --no-restore -v q"), Some(json!({})));
        assert_eq!(tests("npm test"), Some(json!({})));
        assert_eq!(tests("pnpm run test"), Some(json!({})));
        assert_eq!(tests("npx vitest run src/a.test.ts -t parses"), Some(json!({ "path": "src/a.test.ts", "name": "parses" })));
        assert_eq!(tests("pytest tests/test_a.py::TestA::test_b -q"), Some(json!({ "path": "tests/test_a.py", "name": "test_b" })));
        assert_eq!(tests("python3 -m pytest -k parse"), Some(json!({ "name": "parse" })));
        assert_eq!(tests("rtk cargo test"), Some(json!({})));
        // Piped into a filter: run_tests sums the results up itself.
        assert_eq!(tests("cargo test parser 2>&1 | tail -30"), Some(json!({ "name": "parser" })));
        // What run_tests can't do runs as it is.
        for command in ["cargo test --features x", "cargo test --release", "cargo test -p no-such-crate", "cargo test -- --exact a", "go test -race ./...", "pytest -x", "npm test -- --watch", "dotnet test --filter Category=Slow", "cd sub && cargo test", "cargo test $NAME"] {
            assert_eq!(call(command), None, "{command:?}");
            assert_eq!(crate::shell_guard::refusal(command), None, "{command:?} runs");
        }
    }

    #[test]
    fn finds_a_crates_folder() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("crates/forge-x")).unwrap();
        std::fs::write(root.path().join("crates/forge-x/Cargo.toml"), "[package]\nname = \"forge-x\"\n").unwrap();
        assert_eq!(package_dir(root.path(), "forge-x").as_deref(), Some("crates/forge-x"));
        assert_eq!(package_dir(root.path(), "forge-y"), None);
        assert_eq!(package_dir(root.path(), "../x"), None);
    }

    #[test]
    fn translates_piped_commands() {
        assert_eq!(call("cargo build 2>&1 | tail -20"), Some(("run_command".into(), json!({ "command": "cargo build 2>&1", "tail": 20 }))));
        assert_eq!(call("npm ls |& head -n 5").unwrap().1, json!({ "command": "npm ls", "head": 5 }));
        assert_eq!(call("cd web && npm run build | grep -i 'error|warn' | tail -n 10").unwrap().1, json!({ "command": "cd web && npm run build", "filter": "(?i)error\\|warn", "tail": 10 }));
        assert_eq!(call("make | grep -E 'error|warn'").unwrap().1, json!({ "command": "make", "filter": "error|warn" }));
        assert_eq!(call("make | grep -F 'a.b'").unwrap().1, json!({ "command": "make", "filter": "a\\.b" }));
        assert_eq!(call("make | grep error").unwrap().1, json!({ "command": "make", "filter": "error" }));
        for command in ["make | grep -v x", "make | tail -f", "make | sort", "make | tail -5 | grep x", "make | head -5 | tail -2", "cat a.rs | grep x", "git log | head", "make | tail -5 > out.txt", "| tail"] {
            assert_eq!(call(command), None, "{command:?}");
            assert!(crate::shell_guard::refusal(command).is_some(), "{command:?} stays refused");
        }
    }

    #[test]
    fn leaves_the_rest_to_the_refusal() {
        for command in [
            "cat a.rs b.rs",
            "cat",
            "cat *.rs",
            "cat ~/notes",
            "cat /etc/hosts",
            "cat ../a.rs",
            "cat $FILE",
            "cat a.rs | wc -l",
            "cat a.rs > b.rs",
            "cat < a.rs",
            "cat <<EOF\nhi\nEOF",
            "cd src && cat a.rs",
            "cat a.rs &",
            "echo $(cat a.rs)",
            "tail -f log.txt",
            "tail -n +5 a.rs",
            "head -c 10 a.rs",
            "sed -i s/a/b/ a.rs",
            "sed -n 's/a/b/p' a.rs",
            "grep foo",
            "grep -v foo a.rs",
            "grep -c foo a.rs",
            "grep -o foo a.rs",
            "grep 'a\\|b' a.rs",
            "grep -r foo src --include=*.rs",
            "grep foo /etc/hosts",
            "rg -g '!target' foo",
            "rg -t rust foo",
            "git log --graph",
            "git log -p",
            "git log nonexistent-branch-or-path",
            "git -C x log",
            "git diff --cached",
            "git diff a...b",
            "git blame",
            "git status",
            "find . -name '*.rs'",
            "ls -R",
        ] {
            assert_eq!(call(command), None, "{command:?} should stay refused");
        }
    }

    #[test]
    fn what_is_translated_was_refused() {
        for command in ["cat a.rs", "head -n 3 a.rs", "tail -3 a.rs", "sed -n 1,2p a.rs", "grep -rn x src", "rg x", "git log -n 3", "git show HEAD", "git blame a.rs", "git diff main..x"] {
            assert!(translate(command).is_some() && crate::shell_guard::refusal(command).is_some(), "{command}");
        }
    }

    #[test]
    fn calls_are_passed_in_the_command_line() {
        let call = ("read_file", json!({ "path": "a b.rs", "lines": 2000 }));
        let command = exec_command(std::path::Path::new("/Apps/Forge/forge"), "http://127.0.0.1:1/mcp/t", &call);
        let words: Vec<&str> = command.split(' ').collect();
        assert_eq!(&words[..3], ["\"/Apps/Forge/forge\"", EXEC_ARG, "http://127.0.0.1:1/mcp/t"]);
        assert_eq!(words.len(), 4, "no spaces or quotes to escape: {command}");
        let back: Value = serde_json::from_slice(&unhex(words[3]).unwrap()).unwrap();
        assert_eq!(back, json!({ "name": "read_file", "arguments": { "path": "a b.rs", "lines": 2000 } }));
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex("abc"), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn runs_the_tool_on_the_server() {
        use futures::StreamExt as _;
        let project = tempfile::tempdir().unwrap();
        let (registration, mut requests) = crate::forge_mcp::register(&tokio::runtime::Handle::current(), project.path().to_path_buf()).unwrap();
        let url = registration.acp_server()["url"].as_str().unwrap().to_string();
        let asked = tokio::task::spawn_blocking({
            let url = url.clone();
            move || call_tool(&url, "read_file", &json!({ "path": "a.rs" }))
        });
        let request = requests.next().await.unwrap();
        assert_eq!((request.name.as_str(), &request.args), ("read_file", &json!({ "path": "a.rs" })));
        request.reply.send(Ok("1  fn main() {}".into())).unwrap();
        assert_eq!(asked.await.unwrap(), Ok(("1  fn main() {}".to_string(), false)));

        let failed = tokio::task::spawn_blocking({
            let url = url.clone();
            move || call_tool(&url, "read_file", &json!({}))
        });
        requests.next().await.unwrap().reply.send(Err("`path` is missing.".into())).unwrap();
        assert_eq!(failed.await.unwrap(), Ok(("`path` is missing.".to_string(), true)));

        drop(registration);
        assert!(tokio::task::spawn_blocking(move || call_tool(&url, "read_file", &json!({}))).await.unwrap().is_err(), "closed thread");
    }
}
