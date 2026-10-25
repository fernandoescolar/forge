//! The tools of Forge's MCP server for what agents otherwise do with shell commands:
//! reading part of a file (`head`, `tail`, `sed -n`), searching the project (`grep`,
//! `rg`), looking through git history (`git log -S`, `git show`, `git blame`), and
//! keeping only part of a command's output (`| tail`, `| grep`). They need no permission
//! (they only read), answer from unsaved edits too, and leave out the files the user keeps
//! from agents.

use std::fmt::Write as _;
use std::path::Path;

use futures::StreamExt as _;
use gpui::{AppContext as _, AsyncWindowContext};
use project::search::{SearchQuery, SearchResult};
use serde_json::Value;
use util::paths::PathMatcher;

use crate::agent_ignore::AgentIgnore;
use crate::forge_mcp::ToolReply;
use crate::forge_tools::{Ide, open, str_arg};

/// Lines `read_file` returns when not told how many.
const DEFAULT_LINES: u32 = 250;
const MAX_LINES: u32 = 2000;
/// Characters of git's output passed on, at most.
const MAX_GIT_OUTPUT: usize = 60_000;

fn number(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

fn flag(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// A string or a list of strings (`include: "*.rs"` or `["src/**", "tests/**"]`).
fn strings(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::String(s)) => s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect(),
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect(),
        _ => Vec::new(),
    }
}

fn kept_from_agents(ide: &Ide, path: &Path) -> Result<(), String> {
    if AgentIgnore::load(&ide.root).denies(path) {
        return Err(format!("{} is kept from agents: ask the user if you need something from it.", ide.show(path)));
    }
    Ok(())
}

/// `read_file`: lines of a file, numbered, as Forge has it (unsaved edits included).
pub(crate) async fn read_file(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let path = str_arg(&args, "path").ok_or("`path` is missing.")?;
    let path = ide.resolve(&path)?;
    kept_from_agents(&ide, &path)?;
    let buffer = open(&ide, &path, cx).await?;
    let text = cx.update(|_, cx| buffer.read(cx).text()).map_err(|_| "The project is closed.".to_string())?;
    Ok(numbered_lines(&ide.show(&path), &text, number(&args, "start_line"), number(&args, "lines"), number(&args, "tail")))
}

/// `text`'s lines, numbered: `lines` from `start` (1-based), or the last `tail` ones.
fn numbered_lines(name: &str, text: &str, start: Option<u64>, lines: Option<u64>, tail: Option<u64>) -> String {
    let all: Vec<&str> = text.lines().collect();
    let total = all.len();
    if total == 0 {
        return format!("{name} is empty.");
    }
    let count = lines.or(tail).unwrap_or(DEFAULT_LINES as u64).clamp(1, MAX_LINES as u64) as usize;
    let first = match (tail, start) {
        (Some(_), _) => total.saturating_sub(count),
        (None, Some(start)) => (start.max(1) as usize - 1).min(total),
        (None, None) => 0,
    };
    let last = (first + count).min(total);
    if first >= total {
        return format!("{name} has {total} lines.");
    }
    let width = last.to_string().len();
    let mut out = format!("{name}: lines {}-{last} of {total}\n", first + 1);
    for (i, line) in all[first..last].iter().enumerate() {
        let _ = writeln!(out, "{:>width$}  {line}", first + i + 1);
    }
    if last < total {
        let _ = write!(out, "… {} more lines (start_line: {})", total - last, last + 1);
    }
    out.trim_end().to_string()
}

/// `search`: Forge's project search (the Search panel's), over the project's files and the
/// unsaved edits, skipping ignored files and the ones kept from agents.
pub(crate) async fn search(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let query = args.get("query").and_then(Value::as_str).filter(|q| !q.is_empty()).ok_or("`query` is missing.")?.to_string();
    let regex = flag(&args, "regex");
    let (case_sensitive, whole_word, files_only) = (flag(&args, "case_sensitive"), flag(&args, "whole_word"), flag(&args, "files_only"));
    let context = number(&args, "context").unwrap_or(0).min(10) as u32;
    let max = number(&args, "max_results").unwrap_or(100).clamp(1, 1000) as usize;
    let (include, exclude) = (strings(&args, "include"), strings(&args, "exclude"));
    let results = ide
        .project
        .update(cx, |project, cx| {
            let style = project.path_style(cx);
            let include = PathMatcher::new(&include, style).map_err(|e| format!("Bad `include` pattern: {e}"))?;
            let exclude = PathMatcher::new(&exclude, style).map_err(|e| format!("Bad `exclude` pattern: {e}"))?;
            let query = if regex {
                SearchQuery::regex(&query, whole_word, case_sensitive, false, false, include, exclude, false, None)
            } else {
                SearchQuery::text(&query, whole_word, case_sensitive, false, include, exclude, false, None)
            }
            .map_err(|e| format!("Bad query: {e}"))?;
            Ok::<_, String>(project.search(query, cx))
        })
        .map_err(|_| "The project is closed.".to_string())??;
    let kept = AgentIgnore::load(&ide.root);
    let rx = results.rx;
    futures::pin_mut!(rx);
    let (mut out, mut matches, mut files, mut more) = (String::new(), 0usize, 0usize, false);
    while let Some(result) = rx.next().await {
        let (buffer, ranges) = match result {
            SearchResult::Buffer { buffer, ranges } if !ranges.is_empty() => (buffer, ranges),
            SearchResult::LimitReached => {
                more = true;
                break;
            }
            _ => continue,
        };
        if matches >= max {
            more = true;
            break;
        }
        let found = cx
            .update(|_, cx| {
                let buffer = buffer.read(cx);
                let path = buffer.file().and_then(|f| f.as_local().map(|l| l.abs_path(cx)))?;
                let snapshot = buffer.snapshot();
                let rows: Vec<u32> = ranges.iter().map(|r| language::ToPoint::to_point(&r.start, &snapshot).row).collect();
                Some((path, snapshot.text(), rows))
            })
            .map_err(|_| "The project is closed.".to_string())?;
        let Some((path, text, mut rows)) = found else { continue };
        if kept.denies(&path) {
            continue;
        }
        rows.dedup();
        files += 1;
        let name = ide.show(&path);
        if files_only {
            let _ = writeln!(out, "{name} ({} matching lines)", rows.len());
            matches += rows.len();
            continue;
        }
        let lines: Vec<&str> = text.lines().collect();
        let mut shown_until = 0u32;
        for row in rows {
            if matches >= max {
                more = true;
                break;
            }
            matches += 1;
            let from = row.saturating_sub(context).max(shown_until);
            let to = (row + context).min(lines.len().saturating_sub(1) as u32);
            if context > 0 && from > shown_until && shown_until > 0 {
                out.push_str("--\n");
            }
            for r in from..=to {
                let separator = if r == row { ':' } else { '-' };
                let _ = writeln!(out, "{name}{separator}{}{separator} {}", r + 1, lines.get(r as usize).unwrap_or(&""));
            }
            shown_until = to + 1;
        }
    }
    drop(results.task_handle);
    if files == 0 {
        return Ok(format!("No matches for `{query}`."));
    }
    let tail = if more { format!("\n… stopped at {max} matches: narrow it with `include`, or raise `max_results`.") } else { String::new() };
    Ok(format!("{matches} matches in {files} files\n{}{tail}", out.trim_end()))
}

/// Arguments to git that must not be taken for options.
fn plain(value: &str, what: &str) -> Result<String, String> {
    if value.starts_with('-') {
        return Err(format!("`{what}` can't start with `-`."));
    }
    Ok(value.to_string())
}

/// `git_history`: the repository's history, read-only.
pub(crate) async fn git_history(ide: Ide, args: Value, cx: &mut AsyncWindowContext) -> ToolReply {
    let action = str_arg(&args, "action").unwrap_or_else(|| "log".into());
    let path = str_arg(&args, "path");
    if let Some(path) = &path {
        kept_from_agents(&ide, &ide.resolve(path)?)?;
    }
    let git_args = git_args(&action, &args, path.as_deref())?;
    let root = ide.root.clone();
    let output = cx
        .background_spawn(async move { util::command::new_command("git").args(&git_args).current_dir(&root).env("GIT_PAGER", "cat").output().await })
        .await
        .map_err(|e| format!("Couldn't run git: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        return Err(format!("git failed: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    let text = stdout.trim_end();
    if text.is_empty() {
        return Ok("Nothing found.".into());
    }
    if text.len() > MAX_GIT_OUTPUT {
        let cut = text.floor_char_boundary(MAX_GIT_OUTPUT);
        return Ok(format!("{}\n… cut at {MAX_GIT_OUTPUT} characters: narrow it with `path`, `limit` or `stat_only`.", &text[..cut]));
    }
    Ok(text.to_string())
}

/// The `git` command line for a `git_history` call.
fn git_args(action: &str, args: &Value, path: Option<&str>) -> Result<Vec<String>, String> {
    let limit = number(args, "limit").unwrap_or(20).clamp(1, 500);
    let log_format = || vec!["--date=short".to_string(), "--format=%h %ad %an%n    %s".to_string(), format!("-n{limit}")];
    let mut git: Vec<String> = vec!["--no-optional-locks".into()];
    match action {
        "log" | "changes" => {
            git.push("log".into());
            git.extend(log_format());
            if let Some(author) = str_arg(args, "author") {
                git.push(format!("--author={author}"));
            }
            if let Some(since) = str_arg(args, "since") {
                git.push(format!("--since={since}"));
            }
            if let Some(message) = str_arg(args, "message") {
                git.extend(["-i".into(), format!("--grep={message}")]);
            }
            if action == "changes" {
                // Commits that add or remove the text (pickaxe), or whose diff matches the regex.
                match (args.get("text").and_then(Value::as_str), args.get("regex").and_then(Value::as_str)) {
                    (Some(text), _) if !text.is_empty() => git.push(format!("-S{text}")),
                    (_, Some(regex)) if !regex.is_empty() => git.push(format!("-G{regex}")),
                    _ => return Err("`changes` needs `text` (added or removed) or `regex` (in the changed lines).".into()),
                }
                git.push(if flag(args, "patch") { "-p".into() } else { "--name-status".into() });
            } else if flag(args, "stat") {
                git.push("--stat".into());
            }
            if let Some(path) = path {
                if action == "log" {
                    git.push("--follow".into());
                }
                git.extend(["--".into(), path.into()]);
            }
        }
        "show" => {
            let commit = plain(&str_arg(args, "commit").ok_or("`show` needs `commit`.")?, "commit")?;
            git.extend(["show".into(), "--date=short".into(), "--format=commit %H%nAuthor: %an <%ae>%nDate:   %ad%n%n%B".into()]);
            if flag(args, "stat_only") {
                git.push("--stat".into());
            }
            git.push(commit);
            if let Some(path) = path {
                git.extend(["--".into(), path.into()]);
            }
        }
        "file_at" => {
            let commit = plain(&str_arg(args, "commit").ok_or("`file_at` needs `commit`.")?, "commit")?;
            let path = path.ok_or("`file_at` needs `path`.")?;
            git.extend(["show".into(), format!("{commit}:{}", path.trim_start_matches("./"))]);
        }
        "blame" => {
            let path = path.ok_or("`blame` needs `path`.")?;
            git.extend(["blame".into(), "--date=short".into()]);
            if let Some(start) = number(args, "start_line").filter(|l| *l >= 1) {
                let end = number(args, "end_line").filter(|e| *e >= start).unwrap_or(start + 40);
                git.push(format!("-L{start},{end}"));
            }
            git.extend(["--".into(), path.into()]);
        }
        "diff" => {
            git.push("diff".into());
            if flag(args, "stat_only") {
                git.push("--stat".into());
            }
            if let Some(from) = str_arg(args, "from") {
                git.push(plain(&from, "from")?);
            }
            if let Some(to) = str_arg(args, "to") {
                git.push(plain(&to, "to")?);
            }
            if let Some(path) = path {
                git.extend(["--".into(), path.into()]);
            }
        }
        other => return Err(format!("Unknown action `{other}`: use log, changes, show, file_at, blame or diff.")),
    }
    Ok(git)
}

/// What `run_command` passes on of a command's output: the status line, then the lines
/// matching `filter`, the first `head` or the last `tail` of the rest. The user still sees
/// the whole output in the terminal.
pub(crate) fn filter_output(reply: &str, filter: Option<&regex::Regex>, head: Option<usize>, tail: Option<usize>) -> String {
    if filter.is_none() && head.is_none() && tail.is_none() {
        return reply.to_string();
    }
    let (status, body) = reply.split_once('\n').unwrap_or((reply, ""));
    let mut lines: Vec<&str> = body.lines().filter(|l| filter.is_none_or(|f| f.is_match(l))).collect();
    let matched = lines.len();
    let mut notes = Vec::new();
    if let Some(filter) = filter {
        notes.push(format!("{matched} lines match `{filter}`"));
    }
    if let Some(head) = head.filter(|h| *h < lines.len()) {
        notes.push(format!("the first {head} of {}", lines.len()));
        lines.truncate(head);
    } else if let Some(tail) = tail.filter(|t| *t < lines.len()) {
        notes.push(format!("the last {tail} of {}", lines.len()));
        lines.drain(..lines.len() - tail);
    }
    let note = if notes.is_empty() { String::new() } else { format!("({})\n", notes.join(", ")) };
    format!("{status}\n{note}{}", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_a_range_or_the_end() {
        let text = (1..=10).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let head = numbered_lines("a.txt", &text, None, Some(3), None);
        assert_eq!(head, "a.txt: lines 1-3 of 10\n1  line 1\n2  line 2\n3  line 3\n… 7 more lines (start_line: 4)");
        let tail = numbered_lines("a.txt", &text, None, None, Some(2));
        assert_eq!(tail, "a.txt: lines 9-10 of 10\n 9  line 9\n10  line 10");
        assert!(numbered_lines("a.txt", &text, Some(5), Some(2), None).starts_with("a.txt: lines 5-6 of 10\n5  line 5"));
        assert_eq!(numbered_lines("a.txt", &text, Some(40), None, None), "a.txt has 10 lines.");
    }

    #[test]
    fn builds_read_only_git_commands() {
        let args = git_args("changes", &json!({ "text": "parse_config", "limit": 5 }), Some("src/a.rs")).unwrap();
        assert_eq!(args[1..], ["log", "--date=short", "--format=%h %ad %an%n    %s", "-n5", "-Sparse_config", "--name-status", "--", "src/a.rs"]);
        let blame = git_args("blame", &json!({ "start_line": 10, "end_line": 12 }), Some("a.rs")).unwrap();
        assert_eq!(blame[1..], ["blame", "--date=short", "-L10,12", "--", "a.rs"]);
        assert_eq!(git_args("file_at", &json!({ "commit": "HEAD~2" }), Some("./a.rs")).unwrap()[1..], ["show", "HEAD~2:a.rs"]);
        assert!(git_args("show", &json!({ "commit": "--output=/tmp/x" }), None).unwrap_err().contains("can't start with `-`"));
        assert!(git_args("changes", &json!({}), None).is_err());
        assert!(git_args("push", &json!({}), None).unwrap_err().contains("Unknown action"));
    }

    #[test]
    fn keeps_part_of_a_commands_output() {
        let reply = "Exit code 1.\ncompiling a\nerror: x\ncompiling b\nerror: y\ndone";
        let errors = regex::Regex::new("^error").unwrap();
        assert_eq!(filter_output(reply, Some(&errors), None, None), "Exit code 1.\n(2 lines match `^error`)\nerror: x\nerror: y");
        assert_eq!(filter_output(reply, None, None, Some(2)), "Exit code 1.\n(the last 2 of 5)\nerror: y\ndone");
        assert_eq!(filter_output(reply, None, Some(1), None), "Exit code 1.\n(the first 1 of 5)\ncompiling a");
        assert_eq!(filter_output(reply, None, None, None), reply);
    }
}
