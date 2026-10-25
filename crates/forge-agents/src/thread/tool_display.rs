//! How a tool call shows in the thread: its title, command line, location, terminal and
//! output, read from ACP `tool_call` / `tool_call_update`.

use super::AgentLocation;
use serde_json::Value;
use std::path::PathBuf;

/// The last of a tool call's `locations`; lines are zero-based, as Zed reads them.
pub(super) fn tool_location(u: &Value) -> Option<AgentLocation> {
    let location = u.get("locations")?.as_array()?.last()?;
    Some(AgentLocation {
        path: PathBuf::from(location.get("path")?.as_str()?),
        line: location.get("line").and_then(Value::as_u64).map(|l| l as u32),
        kind: u.get("kind").and_then(Value::as_str).map(str::to_string),
    })
}

/// A tool call's title, and for a command, its command line when the title is the agent's
/// description of it (Claude Code's `rawInput.description`). Command lines lose the
/// `cd <folder> &&` agents put first, and say `.` for the thread's folder.
pub(super) fn tool_title(u: &Value, root: &std::path::Path) -> (Option<String>, Option<String>) {
    let title = u.get("title").and_then(Value::as_str).map(str::to_string);
    let input = u.get("rawInput");
    // Forge's own tools: their title, and what they were asked.
    let name = u.pointer("/_meta/claudeCode/toolName").or_else(|| u.get("name")).and_then(Value::as_str).or(title.as_deref());
    if let Some(forge_title) = name.and_then(crate::forge_mcp::tool_title).filter(|_| name.is_some_and(|n| n.starts_with("mcp__"))) {
        return (Some(forge_title), input.and_then(crate::forge_mcp::tool_summary));
    }
    let command = input.and_then(|i| i.get("command")).and_then(Value::as_str).map(str::to_string).or_else(|| (u.get("kind").and_then(Value::as_str) == Some("execute")).then(|| title.clone()).flatten());
    let Some(command) = command.filter(|c| !c.trim().is_empty()) else { return (title, None) };
    let command = tidy_command(&command, root);
    match input.and_then(|i| i.get("description")).and_then(Value::as_str).map(str::trim).filter(|d| !d.is_empty()) {
        Some(description) => (Some(description.to_string()), Some(command)),
        None => (Some(command), None),
    }
}

/// `command` as the user reads it: see [`tool_title`].
fn tidy_command(command: &str, root: &std::path::Path) -> String {
    // A command Forge's hook ran through its tools: what it ran, not Forge's command line.
    if let Some(described) = crate::shell_translate::describe(command) {
        return described;
    }
    let mut rest = command.trim();
    // `cd <dir> && …` / `cd <dir>; …`, maybe more than one.
    while let Some(after_cd) = rest.strip_prefix("cd ") {
        let cut = [after_cd.find("&&").map(|i| (i, 2)), after_cd.find(';').map(|i| (i, 1))].into_iter().flatten().min_by_key(|(i, _)| *i);
        let Some((i, len)) = cut else { break };
        rest = after_cd[i + len..].trim_start();
    }
    let root = root.to_string_lossy();
    if root.len() <= 1 {
        return rest.to_string();
    }
    rest.replace(&format!("{root}/"), "").replace(root.as_ref(), ".")
}

/// The terminal a tool call embeds (`{"type": "terminal", "terminalId"}` content).
pub(super) fn tool_terminal(u: &Value) -> Option<String> {
    u.get("content")?.as_array()?.iter().find_map(|c| (c.get("type")?.as_str()? == "terminal").then(|| c.get("terminalId")?.as_str().map(str::to_string)).flatten())
}

/// Lines of a tool call's text shown at most (the last ones: a command's ending matters).
const DETAIL_LINES: usize = 200;

/// What a tool call's content says: the diff target, or its first text block (a command's
/// output, often in a code fence, shown without it).
pub(super) fn tool_detail(u: &Value) -> Option<String> {
    let content = u.get("content")?.as_array()?;
    content.iter().find_map(|c| match c.get("type").and_then(Value::as_str)? {
        "diff" => Some(format!("edit {}", c.get("path")?.as_str()?)),
        "content" => {
            let text = unfence(c.pointer("/content/text")?.as_str()?);
            let lines: Vec<&str> = text.lines().collect();
            let shown = match lines.len().checked_sub(DETAIL_LINES) {
                Some(cut) if cut > 0 => format!("… {cut} more lines\n{}", lines[cut..].join("\n")),
                _ => text.to_string(),
            };
            (!shown.trim().is_empty()).then_some(shown)
        }
        _ => None,
    })
}

/// `text` without the code fence around it (```` ```console ```` … ```` ``` ````), if it has one.
fn unfence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else { return text };
    let Some(body_start) = rest.find('\n') else { return "" };
    let body = &rest[body_start + 1..];
    body.trim_end().strip_suffix("```").unwrap_or(body).trim_end_matches('\n')
}

#[cfg(test)]
mod detail_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn titles_commands_by_their_description() {
        let root = std::path::Path::new("/work/app");
        let call = json!({ "kind": "execute", "title": "cd /work/app && git status", "rawInput": { "command": "cd /work/app && git status", "description": "Show working tree status" } });
        assert_eq!(tool_title(&call, root), (Some("Show working tree status".into()), Some("git status".into())));
        let bare = json!({ "kind": "execute", "title": "cd \"/work/app\"; ls /work/app/src /tmp" });
        assert_eq!(tool_title(&bare, root), (Some("ls src /tmp".into()), None));
        let read = json!({ "kind": "read", "title": "Read main.rs" });
        assert_eq!(tool_title(&read, root), (Some("Read main.rs".into()), None));
        let forge = json!({ "kind": "other", "title": "mcp__forge__find_references", "rawInput": { "path": "src/a.rs", "line": 12, "symbol": "parse" } });
        assert_eq!(tool_title(&forge, root), (Some("Find references".into()), Some("src/a.rs:12 parse".into())));
        assert_eq!(tidy_command("cd sub", root), "cd sub", "a lone cd stays");
        assert_eq!(tidy_command("cd /work/app && cd src && cargo test", root), "cargo test");
        let exe = std::path::Path::new("/Apps/Forge/forge");
        let ran = |name: &'static str, args: Value| crate::shell_translate::exec_command(exe, "http://127.0.0.1:1/mcp/t", &(name, args));
        assert_eq!(tidy_command(&ran("read_file", json!({ "path": "src/a.rs", "lines": 2000 })), root), "Read a file src/a.rs");
        assert_eq!(tidy_command(&ran("run_command", json!({ "command": "cargo build", "tail": 20 })), root), "cargo build");
    }

    fn text(t: &str) -> Value {
        json!({ "content": [{ "type": "content", "content": { "type": "text", "text": t } }] })
    }

    #[test]
    fn shows_command_output_without_its_fence() {
        assert_eq!(tool_detail(&text("```console\nline 1\nline 2\n```")).as_deref(), Some("line 1\nline 2"));
        assert_eq!(tool_detail(&text("```\nok\n```\n")).as_deref(), Some("ok"));
        assert_eq!(tool_detail(&text("plain\ntext")).as_deref(), Some("plain\ntext"));
        assert_eq!(tool_detail(&text("```console\n```")), None, "nothing to show");
        let long: String = (1..=250).map(|i| format!("{i}\n")).collect();
        let shown = tool_detail(&text(&format!("```console\n{long}```"))).unwrap();
        assert!(shown.starts_with("… 50 more lines\n51\n") && shown.ends_with("\n250"), "{shown}");
        assert_eq!(tool_detail(&json!({ "content": [{ "type": "diff", "path": "/a.rs" }] })).as_deref(), Some("edit /a.rs"));
    }
}
