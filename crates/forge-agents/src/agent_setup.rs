//! What Forge adds when it starts an agent it knows, so each one works through Forge: told
//! about Forge's tools before it acts, and refused the shell commands Forge has a tool for
//! (`grep`, `cat`, `sed -i`, `git log`…), with that tool named where the agent lets a
//! reason through. Each agent takes it its own way:
//!
//! - Claude Code: the session's `_meta` (system prompt, a `PreToolUse` hook), see
//!   [`crate::forge_mcp::session_meta`].
//! - Antigravity (Forge's agy adapter): `_meta.forge.shellHook`, installed as an agy plugin.
//! - GitHub Copilot CLI: a plugin (`--plugin-dir`) with a `preToolUse` hook, Forge's
//!   instructions as an `*.instructions.md` (`COPILOT_CUSTOM_INSTRUCTIONS_DIRS`), and the
//!   `forge` MCP server's instructions kept (`--allow-all-mcp-server-instructions`).
//! - Codex: Forge's instructions as developer instructions (the session's `_meta`), and
//!   execpolicy rules forbidding those commands with the reason. Codex reads rules only from
//!   `$CODEX_HOME/rules`, so Forge runs it with a `CODEX_HOME` of its own, linked to the
//!   user's sign-in, config, `AGENTS.md` and rules: what Codex does outside Forge is
//!   unchanged (its session history inside Forge is kept apart).
//! - Gemini CLI: a policy denying those commands (`--admin-policy`); in ACP mode it
//!   doesn't pass the reason on, so the instructions Forge sends with the first message
//!   say why.
//! - OpenCode: a plugin of Forge's (`plugin` in an `OPENCODE_CONFIG_CONTENT` the user's
//!   config stays under) whose `tool.execute.before` hook runs Forge's shell hook and
//!   takes its answer like Claude Code does: a refusal fails the call with the tool to use,
//!   a test run is rewritten into `run_tests`. The plugin also takes the `forge` MCP
//!   server out of OpenCode's Code Mode, so Forge's tools are function tools the model
//!   sees directly, and the config names Forge's instructions as an instructions file.
//!
//! The files they read are written to Forge's data folder, every time an agent starts.

use std::path::{Path, PathBuf};

use ide_api::AgentSpec;
use serde_json::json;

use crate::shell_guard;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AgentKind {
    Claude,
    Codex,
    Copilot,
    Gemini,
    Antigravity,
    OpenCode,
    Other,
}

/// Which agent `spec` starts, from its command and arguments.
pub fn kind(spec: &AgentSpec) -> AgentKind {
    let program = Path::new(&spec.command).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let has = |part: &str| spec.args.iter().any(|a| a.contains(part)) || spec.command.contains(part);
    if has("claude-agent-acp") || has("claude-code-acp") {
        AgentKind::Claude
    } else if has("codex-acp") {
        AgentKind::Codex
    } else if has("@github/copilot") || program == "copilot" {
        AgentKind::Copilot
    } else if has("@google/gemini-cli") || program == "gemini" {
        AgentKind::Gemini
    } else if has("agy-acp") {
        AgentKind::Antigravity
    } else if has("opencode-ai") || program == "opencode" {
        AgentKind::OpenCode
    } else {
        AgentKind::Other
    }
}

/// Adds what the agent `spec` starts needs to work through Forge. `exe` is this Forge
/// (the hook runs it), `dir` where the agents' files go, `url` the thread's Forge MCP
/// server, which the hook runs simple commands through (see [`crate::shell_translate`]).
pub fn prepare(spec: &mut AgentSpec, exe: &Path, dir: &Path, url: Option<&str>) {
    let hook = shell_guard::hook_command(exe);
    match kind(spec) {
        AgentKind::Copilot => {
            let Ok(root) = write_copilot_files(&dir.join("copilot"), &hook) else { return };
            add_arg(spec, "--allow-all-mcp-server-instructions", None);
            add_arg(spec, "--plugin-dir", Some(&root.join("forge-guard").to_string_lossy()));
            add_to_list_env(spec, "COPILOT_CUSTOM_INSTRUCTIONS_DIRS", &root.join("instructions").to_string_lossy(), ',');
        }
        AgentKind::Codex => {
            let source = spec.env.iter().rev().find(|(k, _)| k == "CODEX_HOME").map(|(_, v)| PathBuf::from(v)).or_else(|| std::env::var_os("CODEX_HOME").map(PathBuf::from)).or_else(|| dirs_home().map(|h| h.join(".codex")));
            let home = dir.join("codex").join("home");
            // Already Forge's (prepared before).
            let Some(source) = source.filter(|s| *s != home) else { return };
            match write_codex_home(&home, &source) {
                Ok(()) => {
                    spec.env.retain(|(k, _)| k != "CODEX_HOME");
                    spec.env.push(("CODEX_HOME".into(), home.to_string_lossy().into_owned()));
                }
                Err(e) => log::warn!("couldn't set up Codex's folder for Forge: {e}"),
            }
        }
        AgentKind::Gemini => {
            let policy = dir.join("gemini").join("forge-policy.toml");
            if write(&policy, &gemini_policy()).is_ok() {
                add_arg(spec, "--admin-policy", Some(&policy.to_string_lossy()));
            }
        }
        AgentKind::OpenCode => {
            let root = dir.join("opencode");
            if let Err(e) = write_opencode_files(&root, exe) {
                log::warn!("couldn't set up OpenCode's files for Forge: {e}");
                return;
            }
            let config = json!({ "plugin": [file_url(&root.join("plugins").join("forge-guard"))], "instructions": [root.join("forge.md").to_string_lossy()] });
            add_to_json_env(spec, "OPENCODE_CONFIG_CONTENT", &config);
            // One plugin file serves every thread: each OpenCode gets its thread's URL here.
            if let Some(url) = url {
                spec.env.retain(|(k, _)| k != OPENCODE_URL_VAR);
                spec.env.push((OPENCODE_URL_VAR.into(), url.into()));
            }
        }
        AgentKind::Claude | AgentKind::Antigravity | AgentKind::Other => {}
    }
}

/// Where Forge's OpenCode plugin reads the thread's Forge MCP server URL.
const OPENCODE_URL_VAR: &str = "FORGE_MCP_URL";

/// `<root>/plugins/forge-guard/index.js` (OpenCode loads a plugin from a folder) and
/// `<root>/forge.md` (Forge's instructions).
fn write_opencode_files(root: &Path, exe: &Path) -> std::io::Result<()> {
    write(&root.join("plugins").join("forge-guard").join("index.js"), &opencode_plugin(exe))?;
    write(&root.join("forge.md"), &format!("{}\n", instructions()))
}

/// Forge's OpenCode plugin: its `tool.execute.before` hook runs Forge's shell hook
/// (`exe`, with [`shell_guard::HOOK_ARG`] and the URL in [`OPENCODE_URL_VAR`]) on the
/// shell tool's command, in Claude Code's formats, and takes its answer: a refusal
/// fails the call with the reason, a changed command replaces the agent's. It also takes
/// the `forge` MCP server out of Code Mode, so its tools are function tools.
fn opencode_plugin(exe: &Path) -> String {
    format!(
        r#"// Written by Forge each time it starts OpenCode: its guard for shell commands.
import {{ spawn }} from "node:child_process"

const FORGE = {exe}
const HOOK_ARG = {hook_arg}
const SERVER = {server}
const URL_VAR = {url_var}

// Forge's answer to a shell command ("" lets it run): a JSON with a decision and reason,
// and the command to run instead, when there is one.
function hook(command) {{
  return new Promise((resolve) => {{
    const url = process.env[URL_VAR]
    const args = url ? [HOOK_ARG, url] : [HOOK_ARG]
    let child
    try {{
      child = spawn(FORGE, args, {{ stdio: ["pipe", "pipe", "ignore"] }})
    }} catch {{
      return resolve("")
    }}
    let out = ""
    const timer = setTimeout(() => {{
      child.kill()
      resolve("")
    }}, 10000)
    child.stdout.on("data", (data) => (out += data))
    child.on("close", () => {{
      clearTimeout(timer)
      resolve(out)
    }})
    child.on("error", () => {{
      clearTimeout(timer)
      resolve("")
    }})
    child.stdin.on("error", () => {{}})
    child.stdin.end(JSON.stringify({{ tool_input: {{ command }} }}))
  }})
}}

export default {{
  id: "forge-guard",
  async setup(ctx) {{
    // Forge's tools as function tools, not behind Code Mode's `execute`.
    await ctx.mcp.transform((editor) => {{
      editor.update(SERVER, (server) => {{
        server.codemode = false
      }})
    }})
    ctx.tool.hook("execute.before", async (event) => {{
      if (event.tool !== "shell" && event.tool !== "bash") return
      const input = event.input
      if (!input || typeof input.command !== "string") return
      const out = await hook(input.command)
      if (!out.trim()) return
      let answer
      try {{
        answer = JSON.parse(out).hookSpecificOutput
      }} catch {{
        return
      }}
      if (!answer) return
      if (answer.permissionDecision === "deny") throw new Error(answer.permissionDecisionReason)
      if (answer.updatedInput) Object.assign(input, answer.updatedInput)
    }})
  }},
}}
"#,
        exe = json!(exe.to_string_lossy()),
        hook_arg = json!(shell_guard::HOOK_ARG),
        server = json!(crate::forge_mcp::SERVER_NAME),
        url_var = json!(OPENCODE_URL_VAR),
    )
}

/// `path` as a `file://` URL.
fn file_url(path: &Path) -> String {
    url::Url::from_file_path(path).map(|u| u.to_string()).unwrap_or_else(|_| format!("file://{}", path.to_string_lossy().replace('\\', "/")))
}

/// Adds `additions` to a JSON object in an environment variable: the user's entries stay,
/// and lists get the new values (once).
fn add_to_json_env(spec: &mut AgentSpec, name: &str, additions: &serde_json::Value) {
    let current = spec.env.iter().rev().find(|(k, _)| k == name).and_then(|(_, v)| serde_json::from_str::<serde_json::Value>(v).ok());
    let mut merged = match current {
        Some(serde_json::Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    for (key, value) in additions.as_object().into_iter().flatten() {
        match (merged.get_mut(key), value) {
            (Some(serde_json::Value::Array(items)), serde_json::Value::Array(new)) => {
                for value in new {
                    if !items.contains(value) {
                        items.push(value.clone());
                    }
                }
            }
            (Some(_), _) => {}
            (None, value) => {
                merged.insert(key.clone(), value.clone());
            }
        }
    }
    spec.env.retain(|(k, _)| k != name);
    spec.env.push((name.into(), serde_json::Value::Object(merged).to_string()));
}

/// The longest a Node timer waits (2³¹ − 1 ms, about 24 days); a longer one fires at once.
const NODE_MAX_TIMEOUT_MS: u64 = 2_147_483_647;

/// How long the agent `spec` starts waits for a tool of Forge's to answer (`ask_user` waits
/// for the user): `seconds`, or as long as it can when unset (an `MCP_TOOL_TIMEOUT` of the
/// user's own wins then). Claude Code takes it from `MCP_TOOL_TIMEOUT`; its ACP adapter
/// drops a per-server timeout.
pub fn set_tool_timeout(spec: &mut AgentSpec, seconds: Option<u64>) {
    const VAR: &str = "MCP_TOOL_TIMEOUT";
    if kind(spec) != AgentKind::Claude {
        return;
    }
    let ms = match seconds {
        Some(seconds) => seconds.saturating_mul(1000).clamp(1000, NODE_MAX_TIMEOUT_MS),
        None if spec.env.iter().any(|(k, _)| k == VAR) => return,
        None => NODE_MAX_TIMEOUT_MS,
    };
    spec.env.retain(|(k, _)| k != VAR);
    spec.env.push((VAR.into(), ms.to_string()));
}

/// What every agent is told: Forge's tools and why to use them (the `forge` MCP server's
/// instructions).
fn instructions() -> String {
    crate::forge_mcp::INSTRUCTIONS.to_string()
}

fn write(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)
}

/// `<root>/forge-guard` (the plugin with the hook) and `<root>/instructions`.
fn write_copilot_files(root: &Path, hook: &str) -> std::io::Result<PathBuf> {
    let plugin = root.join("forge-guard");
    write(&plugin.join("plugin.json"), &json!({ "name": "forge-guard", "version": "1.0.0", "description": "Forge's guard for shell commands" }).to_string())?;
    let hooks = json!({ "version": 1, "hooks": { "preToolUse": [{ "type": "command", "bash": hook, "powershell": format!("& {hook}"), "timeoutSec": 10, "matcher": "bash" }] } });
    write(&plugin.join("hooks.json"), &serde_json::to_string_pretty(&hooks).unwrap_or_default())?;
    write(&root.join("instructions").join("forge.instructions.md"), &format!("---\napplyTo: \"**\"\n---\n\n{}\n", instructions()))?;
    Ok(root.to_path_buf())
}

pub(crate) fn dirs_home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

/// What Forge's `CODEX_HOME` takes from the user's: their sign-in, config and instructions.
const CODEX_SHARED: &[&str] = &["auth.json", "config.toml", "AGENTS.md", "AGENTS.override.md", "prompts", "skills"];

/// Forge's `CODEX_HOME` at `home`: `rules/forge.rules`, and links to what the user's
/// (`source`) has. The sign-in is linked even before there is one, so signing in from
/// Forge signs the user's Codex in too.
fn write_codex_home(home: &Path, source: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(home.join("rules"))?;
    std::fs::create_dir_all(source)?;
    for name in CODEX_SHARED {
        let from = source.join(name);
        if from.exists() || *name == "auth.json" {
            link(&from, &home.join(name))?;
        }
    }
    // The user's own rules, beside Forge's.
    if let Ok(entries) = std::fs::read_dir(source.join("rules")) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name != "forge.rules" {
                link(&entry.path(), &home.join("rules").join(&name))?;
            }
        }
    }
    write(&home.join("rules").join("forge.rules"), &shell_guard::codex_rules())
}

/// `to` → `from`: a link, made again if it points elsewhere; a real file there (Codex
/// replaced the link) is left alone.
fn link(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(to) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if std::fs::read_link(to).ok().as_deref() == Some(from) {
                return Ok(());
            }
            std::fs::remove_file(to)?;
        }
        Ok(_) => return Ok(()),
        Err(_) => {}
    }
    #[cfg(unix)]
    return std::os::unix::fs::symlink(from, to);
    #[cfg(windows)]
    {
        // Links need a privilege on Windows: copy what is there instead.
        let linked = if from.is_dir() { std::os::windows::fs::symlink_dir(from, to) } else { std::os::windows::fs::symlink_file(from, to) };
        if linked.is_err() && from.is_file() {
            std::fs::copy(from, to)?;
        }
        Ok(())
    }
}

/// Gemini's policy: the shell commands Forge has a tool for (see
/// [`shell_guard::gemini_command_regex`]), denied.
fn gemini_policy() -> String {
    let reason = "Forge, the IDE you run in, has tools for this: read files with your own tools or the forge MCP server's read_file, \
search with its search, read git history with its git_history, change files with your own edit tools (not the shell), \
and run commands with its run_command (with tail, head or filter instead of pipes).";
    format!(
        "# Written by Forge each time it starts Gemini, from its shell guard.\n[[rule]]\ntoolName = \"run_shell_command\"\ncommandRegex = {}\ndecision = \"deny\"\npriority = 900\ndenyMessage = {}\n",
        toml_string(&shell_guard::gemini_command_regex()),
        toml_string(reason)
    )
}

/// A TOML basic string.
fn toml_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Adds `flag` (and its value) unless the user's arguments already have it.
fn add_arg(spec: &mut AgentSpec, flag: &str, value: Option<&str>) {
    if spec.args.iter().any(|a| a == flag || a.starts_with(&format!("{flag}="))) {
        return;
    }
    spec.args.push(flag.into());
    spec.args.extend(value.map(String::from));
}

/// Adds `value` to a list environment variable (keeping the user's entries).
fn add_to_list_env(spec: &mut AgentSpec, name: &str, value: &str, separator: char) {
    match spec.env.iter_mut().rev().find(|(k, _)| k == name) {
        Some((_, current)) if !current.split(separator).any(|v| v == value) => *current = format!("{current}{separator}{value}"),
        Some(_) => {}
        None => spec.env.push((name.into(), value.into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(command: &str, args: &[&str]) -> AgentSpec {
        AgentSpec { id: "x".into(), command: command.into(), args: args.iter().map(|a| a.to_string()).collect(), env: vec![], cwd: None }
    }

    #[test]
    fn knows_the_agents() {
        assert_eq!(kind(&spec("npx", &["-y", "@agentclientprotocol/claude-agent-acp"])), AgentKind::Claude);
        assert_eq!(kind(&spec("npx", &["-y", "@zed-industries/codex-acp"])), AgentKind::Codex);
        assert_eq!(kind(&spec("npx", &["-y", "@agentclientprotocol/codex-acp"])), AgentKind::Codex);
        assert_eq!(kind(&spec("npx", &["-y", "@github/copilot", "--acp", "--stdio"])), AgentKind::Copilot);
        assert_eq!(kind(&spec("/opt/homebrew/bin/gemini", &["--experimental-acp"])), AgentKind::Gemini);
        assert_eq!(kind(&spec("npx", &["-y", "@forge-ide/agy-acp"])), AgentKind::Antigravity);
        assert_eq!(kind(&spec("opencode", &["acp"])), AgentKind::OpenCode);
        assert_eq!(kind(&spec("/home/me/.opencode/bin/opencode", &["acp"])), AgentKind::OpenCode);
        assert_eq!(kind(&spec("npx", &["-y", "opencode-ai", "acp"])), AgentKind::OpenCode);
        assert_eq!(kind(&spec("python3", &["mock-acp-agent.py"])), AgentKind::Other);
    }

    #[test]
    fn sets_up_copilot_codex_and_gemini() {
        let dir = tempfile::tempdir().unwrap();
        let exe = Path::new("/Apps/Forge/forge");

        let mut copilot = spec("npx", &["-y", "@github/copilot", "--acp", "--stdio"]);
        copilot.env.push(("COPILOT_CUSTOM_INSTRUCTIONS_DIRS".into(), "/mine".into()));
        prepare(&mut copilot, exe, dir.path(), None);
        prepare(&mut copilot, exe, dir.path(), None);
        let plugin = dir.path().join("copilot/forge-guard");
        assert_eq!(copilot.args[4..], ["--allow-all-mcp-server-instructions".to_string(), "--plugin-dir".into(), plugin.to_string_lossy().into_owned()], "once");
        let hooks: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(plugin.join("hooks.json")).unwrap()).unwrap();
        assert_eq!(hooks["hooks"]["preToolUse"][0]["bash"], "\"/Apps/Forge/forge\" --agent-shell-hook");
        let instructions_dir = dir.path().join("copilot/instructions");
        assert_eq!(copilot.env, [("COPILOT_CUSTOM_INSTRUCTIONS_DIRS".to_string(), format!("/mine,{}", instructions_dir.display()))]);
        assert!(std::fs::read_to_string(instructions_dir.join("forge.instructions.md")).unwrap().starts_with("---\napplyTo: \"**\"\n---\n\nYou are working inside Forge"));

        let users = dir.path().join("user-codex");
        std::fs::create_dir_all(users.join("rules")).unwrap();
        std::fs::write(users.join("config.toml"), "model = \"x\"").unwrap();
        std::fs::write(users.join("rules/mine.rules"), "").unwrap();
        let mut codex = spec("npx", &["-y", "@agentclientprotocol/codex-acp"]);
        codex.env.push(("CODEX_HOME".into(), users.to_string_lossy().into_owned()));
        prepare(&mut codex, exe, dir.path(), None);
        prepare(&mut codex, exe, dir.path(), None);
        let home = dir.path().join("codex/home");
        assert_eq!(codex.env, [("CODEX_HOME".to_string(), home.to_string_lossy().into_owned())]);
        assert_eq!(std::fs::read_to_string(home.join("config.toml")).unwrap(), "model = \"x\"", "the user's config");
        assert_eq!(std::fs::read_link(home.join("auth.json")).unwrap(), users.join("auth.json"), "signing in signs the user's Codex in");
        assert!(home.join("rules/mine.rules").exists() && !home.join("skills").exists());
        let rules = std::fs::read_to_string(home.join("rules/forge.rules")).unwrap();
        assert!(rules.contains(r#"prefix_rule(pattern=[["grep","/bin/grep","#) && rules.contains(r#"decision="forbidden", justification="Forge has a tool for this. Search with the `search` tool"#), "{rules}");

        let mut gemini = spec("gemini", &["--acp"]);
        prepare(&mut gemini, exe, dir.path(), None);
        let policy = dir.path().join("gemini/forge-policy.toml");
        assert_eq!(gemini.args[1..], ["--admin-policy".to_string(), policy.to_string_lossy().into_owned()]);
        let policy = std::fs::read_to_string(&policy).unwrap();
        assert!(policy.contains("toolName = \"run_shell_command\"") && !policy.contains("commandRegex = \"^"), "{policy}");

        let mut other = spec("python3", &["agent.py"]);
        prepare(&mut other, exe, dir.path(), None);
        assert_eq!(other.args, ["agent.py"]);
    }

    #[test]
    fn sets_up_opencode() {
        let dir = tempfile::tempdir().unwrap();
        let exe = Path::new("/Apps/Forge/forge");
        let url = "http://127.0.0.1:5/mcp/t";
        let mut opencode = spec("opencode", &["acp"]);
        opencode.env.push(("OPENCODE_CONFIG_CONTENT".into(), r#"{"model":"x","plugin":["mine"]}"#.into()));
        prepare(&mut opencode, exe, dir.path(), Some(url));
        prepare(&mut opencode, exe, dir.path(), Some(url));
        assert_eq!(opencode.args, ["acp"]);
        let root = dir.path().join("opencode");
        let plugin = root.join("plugins/forge-guard");
        let env = |name: &str| opencode.env.iter().filter(|(k, _)| k == name).map(|(_, v)| v.clone()).collect::<Vec<_>>();
        assert_eq!(env("FORGE_MCP_URL"), [url], "once");
        let [content] = env("OPENCODE_CONFIG_CONTENT").try_into().unwrap();
        let config: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(config["model"], "x", "the user's own config stays");
        assert_eq!(config["plugin"], json!(["mine", format!("file://{}", plugin.display())]));
        assert_eq!(config["instructions"], json!([root.join("forge.md").to_string_lossy()]));
        let code = std::fs::read_to_string(plugin.join("index.js")).unwrap();
        assert!(code.contains(r#"const FORGE = "/Apps/Forge/forge""#) && code.contains(r#"const HOOK_ARG = "--agent-shell-hook""#) && code.contains("server.codemode = false"), "{code}");
        assert!(code.contains(r#"tool_input: { command }"#) && code.contains("answer.updatedInput"), "{code}");
        assert!(std::fs::read_to_string(root.join("forge.md")).unwrap().starts_with("You are working inside Forge"));

        let mut bare = spec("npx", &["-y", "opencode-ai", "acp"]);
        prepare(&mut bare, exe, dir.path(), None);
        assert!(bare.env.iter().all(|(k, _)| k != "FORGE_MCP_URL"), "no thread server, no URL");
        let config: serde_json::Value = serde_json::from_str(&bare.env.iter().find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT").unwrap().1).unwrap();
        assert_eq!(config["plugin"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn waits_for_forge_tools_as_long_as_asked() {
        let claude = || spec("npx", &["-y", "@agentclientprotocol/claude-agent-acp"]);
        let timeout = |spec: &AgentSpec| spec.env.iter().find(|(k, _)| k == "MCP_TOOL_TIMEOUT").map(|(_, v)| v.clone());
        let mut unset = claude();
        set_tool_timeout(&mut unset, None);
        assert_eq!(timeout(&unset).as_deref(), Some("2147483647"), "until the user answers");
        let mut users = claude();
        users.env.push(("MCP_TOOL_TIMEOUT".into(), "5000".into()));
        set_tool_timeout(&mut users, None);
        assert_eq!(timeout(&users).as_deref(), Some("5000"), "the user's own");
        set_tool_timeout(&mut users, Some(300));
        assert_eq!((timeout(&users).as_deref(), users.env.len()), (Some("300000"), 1), "the setting wins");
        let mut other = spec("python3", &["agent.py"]);
        set_tool_timeout(&mut other, Some(300));
        assert!(other.env.is_empty());
    }

    #[test]
    fn quotes_toml_strings() {
        assert_eq!(toml_string("say \"hi\"\n\\d"), r#""say \"hi\"\n\\d""#);
    }
}
