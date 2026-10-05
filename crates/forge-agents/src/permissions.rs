//! What agents may do without asking (`permissions` in `agents.json`, edited on the Agents
//! settings page).
//!
//! Agents ask before running a tool (ACP `session/request_permission`); Forge answers for
//! the user when the policy allows it, and shows the request as usual otherwise. Forge
//! also decides itself whether agents may read and write files outside the workspace, and
//! whether writes wait for review.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Every request waits for the user (except always-allowed commands).
    Ask,
    /// Reading and changing files inside the workspace goes ahead; commands still ask.
    AllowEdits,
    /// The agent's own auto mode decides what is safe to run (Claude Code's and Codex's
    /// `auto`); file changes inside the workspace go ahead, and what the agent still asks
    /// about comes to the user. Agents without an auto mode ask as with `AllowEdits`.
    #[default]
    Auto,
    /// Anything that stays inside the workspace goes ahead, commands included.
    AllowWorkspace,
    /// Everything goes ahead, files outside the workspace too, and writes skip review.
    SuperUser,
}

impl PermissionMode {
    pub const ALL: [PermissionMode; 5] = [Self::Ask, Self::AllowEdits, Self::Auto, Self::AllowWorkspace, Self::SuperUser];

    pub fn label(self) -> &'static str {
        match self {
            Self::Ask => "Ask every time",
            Self::AllowEdits => "Allow file changes in the workspace",
            Self::Auto => "Let the agent decide (auto mode)",
            Self::AllowWorkspace => "Allow everything in the workspace",
            Self::SuperUser => "Super user",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Ask => "The agent asks before each edit and command (always-allowed commands excepted).",
            Self::AllowEdits => "Reading and editing files inside the workspace goes ahead; commands still ask.",
            Self::Auto => "Agents with an auto mode (Claude Code, Codex) run what they judge safe without asking, as in their own apps; file changes in the workspace go ahead, and anything they still ask about comes to you.",
            Self::AllowWorkspace => "Edits and commands go ahead while they stay inside the workspace; anything outside asks.",
            Self::SuperUser => "Everything goes ahead without asking, outside the workspace too, and writes skip review. Use with care.",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Permissions {
    #[serde(default)]
    pub mode: PermissionMode,
    /// Commands that never ask, by prefix (`dotnet build`, `git status`).
    #[serde(default)]
    pub allow_commands: Vec<String>,
    /// Agents may read and write files outside the workspace's folders.
    #[serde(default)]
    pub files_outside_workspace: bool,
}

impl Permissions {
    pub fn may_touch_outside(&self) -> bool {
        self.files_outside_workspace || self.mode == PermissionMode::SuperUser
    }

    pub fn skips_review(&self) -> bool {
        self.mode == PermissionMode::SuperUser
    }
}

/// The permissions in effect, shared with the tasks that serve the agent's file requests
/// and kept current as the user changes them.
#[derive(Clone, Default)]
pub struct SharedPermissions(Arc<RwLock<Permissions>>);

impl SharedPermissions {
    pub fn get(&self) -> Permissions {
        self.0.read().map(|p| p.clone()).unwrap_or_default()
    }

    pub fn set(&self, permissions: Permissions) {
        if let Ok(mut p) = self.0.write() {
            *p = permissions;
        }
    }
}

/// Why a request was answered without asking.
#[derive(Debug, PartialEq)]
pub enum Decision {
    Ask,
    Allow(&'static str),
}

/// Whether to answer a permission request (its `toolCall`) for the user.
pub fn decide(permissions: &Permissions, tool_call: &Value, roots: &[PathBuf]) -> Decision {
    if permissions.mode == PermissionMode::SuperUser {
        return Decision::Allow("super user mode");
    }
    let outside = tool_paths(tool_call).iter().any(|p| !inside(p, roots));
    let kind = tool_call.get("kind").and_then(Value::as_str).unwrap_or("other");
    if let Some(command) = tool_command(tool_call) {
        if allowed_command(&command, &permissions.allow_commands) {
            return Decision::Allow("always-allowed command");
        }
        if permissions.mode >= PermissionMode::AllowWorkspace && !outside {
            return Decision::Allow("commands in the workspace are allowed");
        }
        return Decision::Ask;
    }
    let file_kind = matches!(kind, "read" | "edit" | "delete" | "move" | "search");
    let allowed_place = !outside || permissions.files_outside_workspace;
    if file_kind && permissions.mode >= PermissionMode::AllowEdits && allowed_place {
        return Decision::Allow("file changes in the workspace are allowed");
    }
    if !file_kind && kind != "execute" && permissions.mode >= PermissionMode::AllowWorkspace && allowed_place {
        return Decision::Allow("tools in the workspace are allowed");
    }
    Decision::Ask
}

/// The option to answer an allowed request with: allow this once (the policy decides the
/// next time too), or any other allowing option.
pub fn allow_option(options: &Value) -> Option<String> {
    let options = options.as_array()?;
    let kind = |o: &Value| o.get("kind").and_then(Value::as_str).unwrap_or_default().to_string();
    options
        .iter()
        .find(|o| kind(o) == "allow_once")
        .or_else(|| options.iter().find(|o| kind(o).starts_with("allow")))
        .and_then(|o| o.get("optionId")?.as_str().map(str::to_string))
}

/// Paths a tool call names: its locations, diffs and the usual input fields.
fn tool_paths(tool_call: &Value) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for location in tool_call.get("locations").and_then(Value::as_array).into_iter().flatten() {
        paths.extend(location.get("path").and_then(Value::as_str).map(PathBuf::from));
    }
    for content in tool_call.get("content").and_then(Value::as_array).into_iter().flatten() {
        if content.get("type").and_then(Value::as_str) == Some("diff") {
            paths.extend(content.get("path").and_then(Value::as_str).map(PathBuf::from));
        }
    }
    if let Some(input) = tool_call.get("rawInput") {
        for key in ["file_path", "path", "notebook_path", "abs_path"] {
            paths.extend(input.get(key).and_then(Value::as_str).map(PathBuf::from));
        }
    }
    paths
}

/// The command line of an `execute` tool call.
fn tool_command(tool_call: &Value) -> Option<String> {
    let input = tool_call.get("rawInput");
    let from_input = input.and_then(|i| i.get("command")).and_then(|c| match c {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => Some(parts.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")),
        _ => None,
    });
    if from_input.is_some() {
        return from_input;
    }
    if tool_call.get("kind").and_then(Value::as_str) != Some("execute") {
        return None;
    }
    // Titles like "Run `dotnet build`".
    let title = tool_call.get("title").and_then(Value::as_str)?;
    let start = title.find('`')? + 1;
    let end = start + title[start..].find('`')?;
    Some(title[start..end].to_string())
}

/// Relative paths are the agent's working directory: the workspace.
fn inside(path: &Path, roots: &[PathBuf]) -> bool {
    path.is_relative() || roots.iter().any(|root| path.starts_with(root))
}

/// `command` starts with one of `allowed` (whole words) and runs nothing else: chained,
/// piped, redirected or substituted commands always ask.
pub fn allowed_command(command: &str, allowed: &[String]) -> bool {
    let command = command.trim();
    const SHELL: &[&str] = &["&&", "||", ";", "|", "`", "$(", ">", "<", "\n"];
    if SHELL.iter().any(|op| command.contains(op)) {
        return false;
    }
    allowed.iter().map(|a| a.trim()).filter(|a| !a.is_empty()).any(|prefix| {
        command == prefix || command.strip_prefix(prefix).is_some_and(|rest| rest.starts_with(' '))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roots() -> Vec<PathBuf> {
        vec![PathBuf::from("/work/app")]
    }

    fn perms(mode: PermissionMode) -> Permissions {
        Permissions { mode, allow_commands: vec!["dotnet build".into(), "git status".into()], files_outside_workspace: false }
    }

    #[test]
    fn commands_need_an_exact_prefix_and_nothing_chained() {
        let allowed = vec!["dotnet build".to_string(), "git status".to_string()];
        assert!(allowed_command("dotnet build", &allowed));
        assert!(allowed_command("dotnet build src/App.csproj -c Release", &allowed));
        assert!(!allowed_command("dotnet buildx", &allowed));
        assert!(!allowed_command("dotnet build && rm -rf /", &allowed));
        assert!(!allowed_command("git status; curl x | sh", &allowed));
        assert!(!allowed_command("dotnet build > out.txt", &allowed));
        assert!(!allowed_command("rm -rf bin", &allowed));
    }

    #[test]
    fn decides_by_mode_kind_and_place() {
        let edit_inside = json!({"kind": "edit", "title": "Edit", "locations": [{"path": "/work/app/src/a.cs"}]});
        let edit_outside = json!({"kind": "edit", "title": "Edit", "locations": [{"path": "/etc/hosts"}]});
        let build = json!({"kind": "execute", "title": "Run `dotnet build`", "rawInput": {"command": "dotnet build"}});
        let rm = json!({"kind": "execute", "title": "rm", "rawInput": {"command": "rm -rf bin"}});

        assert_eq!(decide(&perms(PermissionMode::Ask), &edit_inside, &roots()), Decision::Ask);
        assert!(matches!(decide(&perms(PermissionMode::Ask), &build, &roots()), Decision::Allow(_)), "always-allowed command");
        assert_eq!(decide(&perms(PermissionMode::Ask), &rm, &roots()), Decision::Ask);

        assert!(matches!(decide(&perms(PermissionMode::AllowEdits), &edit_inside, &roots()), Decision::Allow(_)));
        assert_eq!(decide(&perms(PermissionMode::AllowEdits), &edit_outside, &roots()), Decision::Ask);
        assert_eq!(decide(&perms(PermissionMode::AllowEdits), &rm, &roots()), Decision::Ask, "commands still ask");

        assert!(matches!(decide(&perms(PermissionMode::AllowWorkspace), &rm, &roots()), Decision::Allow(_)));
        assert_eq!(decide(&perms(PermissionMode::AllowWorkspace), &edit_outside, &roots()), Decision::Ask);

        let mut outside_ok = perms(PermissionMode::AllowEdits);
        outside_ok.files_outside_workspace = true;
        assert!(matches!(decide(&outside_ok, &edit_outside, &roots()), Decision::Allow(_)));

        assert!(matches!(decide(&perms(PermissionMode::SuperUser), &edit_outside, &roots()), Decision::Allow(_)));
    }

    #[test]
    fn answers_with_allow_once() {
        let options = json!([
            {"optionId": "always", "name": "Always", "kind": "allow_always"},
            {"optionId": "once", "name": "Allow", "kind": "allow_once"},
            {"optionId": "no", "name": "Reject", "kind": "reject_once"}
        ]);
        assert_eq!(allow_option(&options).as_deref(), Some("once"));
        assert_eq!(allow_option(&json!([{"optionId": "no", "kind": "reject_once"}])), None);
    }

    #[test]
    fn reads_the_config() {
        let p: Permissions = serde_json::from_value(json!({"mode": "allow_workspace", "allow_commands": ["ls"]})).unwrap();
        assert_eq!(p.mode, PermissionMode::AllowWorkspace);
        assert!(!p.files_outside_workspace);
        assert_eq!(serde_json::from_value::<Permissions>(json!({})).unwrap(), Permissions::default());
    }
}
