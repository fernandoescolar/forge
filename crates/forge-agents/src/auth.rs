//! Signing in to agents without leaving Forge.
//!
//! Forge advertises terminal auth in `initialize` (`auth.terminal` and the older
//! `_meta["terminal-auth"]`, both as Zed does). Agents then list `authMethods` that say
//! which command performs the login; Forge runs it in an interactive Zed terminal inside
//! the agent panel and restarts the agent when it succeeds. Methods without a terminal
//! command are completed with ACP `authenticate`.

use ide_api::AgentSpec;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum AuthAction {
    /// Run this interactively; exit status 0 means signed in.
    Terminal { command: String, args: Vec<String>, env: Vec<(String, String)> },
    /// Ask the agent to do it (`authenticate { methodId }`).
    Authenticate,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuthMethod {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub action: AuthAction,
}

/// Reads `authMethods` from an `initialize` result.
pub fn parse_auth_methods(init: &Value, agent: &AgentSpec) -> Vec<AuthMethod> {
    let Some(methods) = init.get("authMethods").and_then(Value::as_array) else { return vec![] };
    methods
        .iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_string();
            let name = m.get("name").and_then(Value::as_str).unwrap_or(&id).to_string();
            let description = m.get("description").and_then(Value::as_str).map(|d| d.trim().to_string()).filter(|d| !d.is_empty());
            let action = if let Some(meta) = m.pointer("/_meta/terminal-auth") {
                // Pre-stabilisation form: the full command line.
                AuthAction::Terminal {
                    command: meta.get("command")?.as_str()?.to_string(),
                    args: strings(meta.get("args")),
                    env: env_pairs(meta.get("env")),
                }
            } else if m.get("type").and_then(Value::as_str) == Some("terminal") {
                // Stable form: run the agent's own command with extra args.
                let mut args = agent.args.clone();
                args.extend(strings(m.get("args")));
                let mut env = agent.env.clone();
                env.extend(env_pairs(m.get("env")));
                AuthAction::Terminal { command: agent.command.clone(), args, env }
            } else {
                AuthAction::Authenticate
            };
            Some(AuthMethod { id, name, description, action })
        })
        .collect()
}

fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).map(|xs| xs.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default()
}

/// Env as `{ "K": "V" }` or `[{ "name": "K", "value": "V" }]`.
fn env_pairs(v: Option<&Value>) -> Vec<(String, String)> {
    match v {
        Some(Value::Object(o)) => o.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect(),
        Some(Value::Array(xs)) => xs.iter().filter_map(|e| Some((e.get("name")?.as_str()?.to_string(), e.get("value")?.as_str()?.to_string()))).collect(),
        _ => vec![],
    }
}

/// Whether an error or message from an agent means "you need to sign in".
pub fn is_auth_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    ["authentication required", "auth_required", "failed to authenticate", "not logged in", "please run /login", "oauth session expired", "invalid api key"]
        .iter()
        .any(|needle| m.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn agent() -> AgentSpec {
        AgentSpec { id: "claude".into(), command: "npx".into(), args: vec!["-y".into(), "pkg".into()], env: vec![("A".into(), "1".into())], cwd: None }
    }

    #[test]
    fn parses_claude_style_methods() {
        // Shape returned by @agentclientprotocol/claude-agent-acp 0.85.
        let init = json!({"authMethods": [
            {"id": "claude-ai-login", "name": "Claude Subscription", "description": "Use Claude subscription ", "type": "terminal",
             "args": ["--cli", "auth", "login", "--claudeai"],
             "_meta": {"terminal-auth": {"command": "/usr/bin/node", "args": ["/x/claude-agent-acp", "--cli", "auth", "login", "--claudeai"], "label": "Claude Login"}}},
            {"id": "console-login", "name": "Anthropic Console", "type": "terminal", "args": ["--cli", "auth", "login", "--console"]},
            {"id": "oauth", "name": "Browser"}
        ]});
        let methods = parse_auth_methods(&init, &agent());
        assert_eq!(methods.len(), 3);
        assert_eq!(methods[0].description.as_deref(), Some("Use Claude subscription"));
        assert_eq!(methods[0].action, AuthAction::Terminal { command: "/usr/bin/node".into(), args: vec!["/x/claude-agent-acp".into(), "--cli".into(), "auth".into(), "login".into(), "--claudeai".into()], env: vec![] });
        assert_eq!(
            methods[1].action,
            AuthAction::Terminal { command: "npx".into(), args: vec!["-y".into(), "pkg".into(), "--cli".into(), "auth".into(), "login".into(), "--console".into()], env: vec![("A".into(), "1".into())] }
        );
        assert_eq!(methods[2].action, AuthAction::Authenticate);
        assert!(parse_auth_methods(&json!({}), &agent()).is_empty());
    }

    #[test]
    fn recognises_auth_errors() {
        assert!(is_auth_error("session/prompt failed (-32000): Authentication required"));
        assert!(is_auth_error("Failed to authenticate: OAuth session expired"));
        assert!(!is_auth_error("rate limited"));
    }
}
